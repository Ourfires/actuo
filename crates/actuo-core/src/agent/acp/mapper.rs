//! Maps ACP messages (as JSON) to actuo events. Pure: no I/O, so it is tested
//! directly on messages recorded from Claude Code and Codex.
//!
//! Rules:
//! - Message and thought text goes out only as `content.delta`; `item.started`
//!   opens the item empty and `item.completed` carries the final text once. A new
//!   kind of chunk, a new `messageId` (both agents send one per message) or a tool
//!   call closes the open text item.
//! - Tool calls are kept as snapshots; `tool_call_update` fields are merged in, and
//!   each change sends the whole item. `completed` or `failed` sends `item.completed`.
//! - The end of a turn completes every open item; a tool call still pending or in
//!   progress gets status `cancelled`.
//! - `available_commands_update` becomes `session.commands`, only when the list
//!   changes. `user_message_chunk` is dropped (`turn.started` has the text).
//!   Anything else becomes `agent.update`.
//! - Cost is the agent's running total per process; a turn's cost is the difference.

use super::super::{Mapped, TurnEnd};
use actuo_protocol::events::*;
use actuo_protocol::ids::{ItemId, PermissionOptionId, RequestId};
use actuo_protocol::types::*;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Message,
    Thought,
}

struct OpenText {
    id: ItemId,
    kind: TextKind,
    /// The agent's `messageId`, when it sends one.
    message_id: Option<String>,
    /// Marked as an informational notice (`_meta.claudeCode.kind`), not the
    /// agent's own words.
    informational: bool,
    text: String,
}

struct Tool {
    id: ItemId,
    call: ToolCall,
    done: bool,
}

#[derive(Default)]
pub struct Mapper {
    turn: u32,
    next_item: u32,
    next_request: u32,
    text: Option<OpenText>,
    /// By the agent's `toolCallId`.
    tools: HashMap<String, Tool>,
    plan: Option<(ItemId, Plan)>,
    commands: Option<Vec<AgentCommand>>,
    cost_total: Option<Cost>,
    cost_at_turn_start: f64,
    /// A cost came in during this turn: without one, the turn's cost isn't known.
    cost_reported: bool,
}

impl Mapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts numbering items and requests for `turn`: item `3.1`, request `3.r1`.
    pub fn begin_turn(&mut self, turn: u32) {
        self.turn = turn;
        self.next_item = 0;
        self.next_request = 0;
        self.tools.clear();
        self.cost_at_turn_start = self.cost_total.as_ref().map_or(0.0, |c| c.amount);
        self.cost_reported = false;
    }

    /// A `session/update` notification's params.
    pub fn update(&mut self, params: &Value) -> Vec<Mapped> {
        let raw = json!({"jsonrpc": "2.0", "method": "session/update", "params": params});
        let u = &params["update"];
        let kind = u["sessionUpdate"].as_str().unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                let text_kind = if kind == "agent_message_chunk" {
                    TextKind::Message
                } else {
                    TextKind::Thought
                };
                match u["content"]["text"].as_str() {
                    Some(text) if u["content"]["type"] == "text" => {
                        let message_id = u["messageId"].as_str().map(str::to_string);
                        let informational = u["_meta"]["claudeCode"]["kind"] == "informational";
                        self.chunk(text_kind, message_id, informational, text, raw, &mut out)
                    }
                    // Images and resources in messages go out as `agent.update`.
                    _ => out.push(agent_update(kind, u, raw)),
                }
            }
            "user_message_chunk" => {}
            "tool_call" => {
                self.close_text(&mut out);
                let id = str_field(u, "toolCallId");
                let mut call = ToolCall {
                    tool_kind: ToolKind::Other,
                    title: String::new(),
                    status: ToolStatus::Pending,
                    raw_input: None,
                    raw_output: None,
                    content: vec![],
                    locations: vec![],
                };
                merge_tool(&mut call, u);
                let item_id = match self.tools.get(&id) {
                    Some(t) => t.id.clone(),
                    None => self.new_item(),
                };
                let done = is_done(call.status);
                self.tools.insert(
                    id,
                    Tool {
                        id: item_id.clone(),
                        call: call.clone(),
                        done,
                    },
                );
                out.push(item(
                    EventBody::ItemStarted,
                    &item_id,
                    Item::ToolCall(call.clone()),
                    Some(raw),
                ));
                if done {
                    out.push(item(
                        EventBody::ItemCompleted,
                        &item_id,
                        Item::ToolCall(call),
                        None,
                    ));
                }
            }
            "tool_call_update" => {
                self.close_text(&mut out);
                let (item_id, call, newly_done, started) = self.merge_into_tool(u);
                let body = if started {
                    EventBody::ItemStarted
                } else if newly_done {
                    EventBody::ItemCompleted
                } else {
                    EventBody::ItemUpdated
                };
                out.push(item(
                    body,
                    &item_id,
                    Item::ToolCall(call.clone()),
                    Some(raw),
                ));
                if started && newly_done {
                    out.push(item(
                        EventBody::ItemCompleted,
                        &item_id,
                        Item::ToolCall(call),
                        None,
                    ));
                }
            }
            "plan" => {
                let plan = Plan {
                    entries: u["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|e| PlanEntry {
                            content: e["content"].as_str().unwrap_or_default().to_string(),
                            priority: e["priority"].as_str().map(str::to_string),
                            status: match e["status"].as_str() {
                                Some("in_progress") => PlanEntryStatus::InProgress,
                                Some("completed") => PlanEntryStatus::Completed,
                                _ => PlanEntryStatus::Pending,
                            },
                        })
                        .collect(),
                };
                let (id, body) = match &self.plan {
                    Some((id, _)) => (
                        id.clone(),
                        EventBody::ItemUpdated as fn(ItemData) -> EventBody,
                    ),
                    None => (
                        self.new_item(),
                        EventBody::ItemStarted as fn(ItemData) -> EventBody,
                    ),
                };
                self.plan = Some((id.clone(), plan.clone()));
                out.push(item(body, &id, Item::Plan(plan), Some(raw)));
            }
            "usage_update" => {
                if let Some(cost) = u.get("cost").and_then(parse_cost) {
                    self.cost_total = Some(cost);
                    self.cost_reported = true;
                }
                out.push(Mapped {
                    item_id: None,
                    body: EventBody::UsageUpdated(UsageUpdatedData {
                        context_used: u["used"].as_u64().unwrap_or_default(),
                        context_size: u["size"].as_u64().unwrap_or_default(),
                    }),
                    raw: Some(raw),
                });
            }
            "available_commands_update" => {
                let commands: Vec<AgentCommand> = u["availableCommands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| AgentCommand {
                        name: c["name"].as_str().unwrap_or_default().to_string(),
                        description: c["description"].as_str().map(str::to_string),
                        input_hint: c["input"]["hint"].as_str().map(str::to_string),
                    })
                    .collect();
                if self.commands.as_ref() != Some(&commands) {
                    self.commands = Some(commands.clone());
                    out.push(Mapped {
                        item_id: None,
                        body: EventBody::SessionCommands(SessionCommandsData { commands }),
                        raw: Some(raw),
                    });
                }
            }
            _ => out.push(agent_update(kind, u, raw)),
        }
        out
    }

    /// A `session/request_permission` request's params. Returns actuo's request
    /// id and the events (`request.opened`, after `item.started` if the tool call
    /// wasn't seen before).
    pub fn permission(&mut self, params: &Value) -> (RequestId, Vec<Mapped>) {
        let raw =
            json!({"jsonrpc": "2.0", "method": "session/request_permission", "params": params});
        let mut out = Vec::new();
        self.close_text(&mut out);
        let tc = &params["toolCall"];
        let (item_id, call, _, started) = self.merge_into_tool(tc);
        if started {
            out.push(item(
                EventBody::ItemStarted,
                &item_id,
                Item::ToolCall(call.clone()),
                None,
            ));
        }
        self.next_request += 1;
        let request_id = RequestId(format!("{}.r{}", self.turn, self.next_request));
        let options = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| {
                Some(PermissionOption {
                    option_id: PermissionOptionId(o["optionId"].as_str()?.to_string()),
                    name: o["name"].as_str().unwrap_or_default().to_string(),
                    // An option of a kind ACP v1 doesn't have can't be chosen by kind; drop it.
                    kind: serde_json::from_value(o["kind"].clone()).ok()?,
                })
            })
            .collect();
        out.push(Mapped {
            item_id: Some(item_id),
            body: EventBody::RequestOpened(RequestOpenedData {
                request_id: request_id.clone(),
                tool_call: call,
                options,
            }),
            raw: Some(raw),
        });
        (request_id, out)
    }

    /// The `session/prompt` result. Completes open items and reports how the turn ended.
    pub fn end_turn(&mut self, result: &Value) -> (Vec<Mapped>, TurnEnd) {
        let out = self.close_all();
        let stop_reason =
            serde_json::from_value(result["stopReason"].clone()).unwrap_or(StopReason::EndTurn);
        let mut tokens = parse_tokens(&result["usage"]);
        // Claude's adapter answers all zeros when a cancelled turn settles before
        // the SDK's result: nothing was counted, so it isn't known.
        if stop_reason == StopReason::Cancelled && tokens.as_ref().is_some_and(all_zero) {
            tokens = None;
        }
        let end = TurnEnd {
            stop_reason,
            tokens,
            cost: self
                .cost_total
                .as_ref()
                .filter(|_| self.cost_reported)
                .map(|total| Cost {
                    amount: round_cost(total.amount - self.cost_at_turn_start),
                    currency: total.currency.clone(),
                }),
            raw: Some(json!({"jsonrpc": "2.0", "result": result})),
        };
        (out, end)
    }

    /// The prompt failed or the agent went away: completes open items.
    pub fn close_all(&mut self) -> Vec<Mapped> {
        let mut out = Vec::new();
        self.close_text(&mut out);
        let mut open: Vec<_> = self.tools.values_mut().filter(|t| !t.done).collect();
        open.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        for t in open {
            t.done = true;
            if matches!(t.call.status, ToolStatus::Pending | ToolStatus::InProgress) {
                t.call.status = ToolStatus::Cancelled;
            }
            out.push(item(
                EventBody::ItemCompleted,
                &t.id,
                Item::ToolCall(t.call.clone()),
                None,
            ));
        }
        if let Some((id, plan)) = self.plan.take() {
            out.push(item(EventBody::ItemCompleted, &id, Item::Plan(plan), None));
        }
        out
    }

    /// Appends a text chunk to the open message or thought, or starts a new one
    /// when the chunk belongs to another: another kind, another `messageId` (one
    /// with an id and one without are different messages; agents put the id on
    /// every chunk of a message), or a notice next to the agent's own words.
    fn chunk(
        &mut self,
        kind: TextKind,
        message_id: Option<String>,
        informational: bool,
        text: &str,
        raw: Value,
        out: &mut Vec<Mapped>,
    ) {
        let new_message = |t: &OpenText| {
            t.kind != kind || t.message_id != message_id || t.informational != informational
        };
        if self.text.as_ref().is_some_and(new_message) {
            self.close_text(out);
        }
        if self.text.is_none() {
            let id = self.new_item();
            out.push(item(
                EventBody::ItemStarted,
                &id,
                text_item(kind, String::new()),
                None,
            ));
            self.text = Some(OpenText {
                id,
                kind,
                message_id,
                informational,
                text: String::new(),
            });
        }
        let open = self.text.as_mut().expect("opened above");
        open.text.push_str(text);
        out.push(Mapped {
            item_id: Some(open.id.clone()),
            body: EventBody::ContentDelta(ContentDeltaData {
                text: text.to_string(),
            }),
            raw: Some(raw),
        });
    }

    fn close_text(&mut self, out: &mut Vec<Mapped>) {
        if let Some(t) = self.text.take() {
            out.push(item(
                EventBody::ItemCompleted,
                &t.id,
                text_item(t.kind, t.text),
                None,
            ));
        }
    }

    /// Merges a tool call update into its snapshot, creating it if unknown. Returns
    /// the item id, the snapshot, whether it just became done, and whether it is new.
    fn merge_into_tool(&mut self, u: &Value) -> (ItemId, ToolCall, bool, bool) {
        let id = str_field(u, "toolCallId");
        let started = !self.tools.contains_key(&id);
        if started {
            let item_id = self.new_item();
            self.tools.insert(
                id.clone(),
                Tool {
                    id: item_id,
                    call: ToolCall {
                        tool_kind: ToolKind::Other,
                        title: String::new(),
                        status: ToolStatus::Pending,
                        raw_input: None,
                        raw_output: None,
                        content: vec![],
                        locations: vec![],
                    },
                    done: false,
                },
            );
        }
        let tool = self.tools.get_mut(&id).expect("inserted above");
        merge_tool(&mut tool.call, u);
        let newly_done = !tool.done && is_done(tool.call.status);
        tool.done |= newly_done;
        (tool.id.clone(), tool.call.clone(), newly_done, started)
    }

    fn new_item(&mut self) -> ItemId {
        self.next_item += 1;
        ItemId(format!("{}.{}", self.turn, self.next_item))
    }
}

fn merge_tool(call: &mut ToolCall, u: &Value) {
    if let Some(k) = u.get("kind").filter(|v| !v.is_null()) {
        call.tool_kind = serde_json::from_value(k.clone()).unwrap_or(ToolKind::Other);
    }
    if let Some(t) = u["title"].as_str() {
        call.title = t.to_string();
    }
    if let Some(s) = u.get("status").filter(|v| !v.is_null())
        && let Ok(s) = serde_json::from_value(s.clone())
    {
        call.status = s;
    }
    if let Some(v) = u.get("rawInput").filter(|v| !v.is_null()) {
        call.raw_input = Some(v.clone());
    }
    if let Some(v) = u.get("rawOutput").filter(|v| !v.is_null()) {
        call.raw_output = Some(v.clone());
    }
    if let Some(items) = u["content"].as_array() {
        call.content = items.iter().map(tool_content).collect();
    }
    if let Some(locs) = u["locations"].as_array() {
        call.locations = locs
            .iter()
            .filter_map(|l| {
                Some(Location {
                    path: l["path"].as_str()?.to_string(),
                    line: l["line"].as_u64().and_then(|n| u32::try_from(n).ok()),
                })
            })
            .collect();
    }
}

fn tool_content(c: &Value) -> ToolContent {
    match c["type"].as_str() {
        Some("diff") => ToolContent::Diff {
            path: c["path"].as_str().unwrap_or_default().to_string(),
            old_text: c["oldText"].as_str().map(str::to_string),
            new_text: c["newText"].as_str().unwrap_or_default().to_string(),
        },
        Some("content") if c["content"]["type"] == "text" => ToolContent::Text {
            text: c["content"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
        _ => ToolContent::Other { value: c.clone() },
    }
}

fn is_done(status: ToolStatus) -> bool {
    matches!(status, ToolStatus::Completed | ToolStatus::Failed)
}

fn text_item(kind: TextKind, text: String) -> Item {
    match kind {
        TextKind::Message => Item::Message(TextItem { text }),
        TextKind::Thought => Item::Thought(TextItem { text }),
    }
}

fn item(body: fn(ItemData) -> EventBody, id: &ItemId, item: Item, raw: Option<Value>) -> Mapped {
    Mapped {
        item_id: Some(id.clone()),
        body: body(ItemData { item }),
        raw,
    }
}

fn agent_update(kind: &str, u: &Value, raw: Value) -> Mapped {
    Mapped {
        item_id: None,
        body: EventBody::AgentUpdate(AgentUpdateData {
            kind: kind.to_string(),
            data: u.clone(),
        }),
        raw: Some(raw),
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

fn parse_cost(v: &Value) -> Option<Cost> {
    Some(Cost {
        amount: v["amount"].as_f64()?,
        currency: v["currency"].as_str().unwrap_or("USD").to_string(),
    })
}

/// Floating-point differences of running totals, rounded to 1e-9.
fn round_cost(x: f64) -> f64 {
    (x * 1e9).round() / 1e9
}

fn all_zero(t: &TokenUsage) -> bool {
    [
        t.input,
        t.output,
        t.cached_read,
        t.cached_write,
        t.thought,
        t.total,
    ]
    .iter()
    .all(|n| n.unwrap_or(0) == 0)
}

/// Best effort: the `usage` object of a `session/prompt` result, in the shapes
/// Claude Code and Codex send. Anything else gives `None`.
pub fn parse_tokens(usage: &Value) -> Option<TokenUsage> {
    let obj = usage.as_object()?;
    let get = |k: &str| obj.get(k).and_then(Value::as_u64);
    let tokens = TokenUsage {
        input: get("inputTokens"),
        output: get("outputTokens"),
        cached_read: get("cachedReadTokens"),
        cached_write: get("cachedWriteTokens"),
        thought: get("thoughtTokens"),
        total: get("totalTokens"),
    };
    (tokens != TokenUsage::default()).then_some(tokens)
}

#[cfg(test)]
mod tests;

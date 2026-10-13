//! How `actuo attach` prints events: one line each, except message and thought
//! text, which streams as it arrives.

use actuo_protocol::events::{Event, EventBody};
use actuo_protocol::types::{Item, ToolCall, ToolContent};
use std::io::Write;

#[derive(Default)]
pub struct Printer {
    /// A text item is open: its deltas are being printed on one line.
    in_text: bool,
}

impl Printer {
    pub fn print(&mut self, e: &Event, out: &mut impl Write) {
        let turn = e.turn.map(|t| format!("t{t} ")).unwrap_or_default();
        match &e.body {
            EventBody::ContentDelta(d) => {
                crate::exit_if_stdout_closed(write!(out, "{}", d.text));
                crate::exit_if_stdout_closed(out.flush());
                return;
            }
            EventBody::ItemStarted(d) if matches!(d.item, Item::Message(_) | Item::Thought(_)) => {
                self.end_text(out);
                let who = if matches!(d.item, Item::Message(_)) {
                    "agent"
                } else {
                    "thinking"
                };
                crate::exit_if_stdout_closed(write!(out, "{turn}{who}: "));
                crate::exit_if_stdout_closed(out.flush());
                self.in_text = true;
                return;
            }
            EventBody::ItemCompleted(d)
                if matches!(d.item, Item::Message(_) | Item::Thought(_)) =>
            {
                self.end_text(out);
                return;
            }
            // Context usage changes with nearly every message, and agent updates
            // (titles, config option changes) aren't worth a line in a terminal.
            EventBody::UsageUpdated(_) | EventBody::AgentUpdate(_) => return,
            _ => {}
        }
        self.end_text(out);
        let line = match &e.body {
            EventBody::SessionState(d) => match &d.reason {
                Some(r) => format!("state {} ({r})", name(&d.state)),
                None => format!("state {}", name(&d.state)),
            },
            EventBody::SessionAgent(d) => {
                let name = d
                    .agent_info
                    .as_ref()
                    .map(|i| format!("{} {}", i.name, i.version.clone().unwrap_or_default()))
                    .unwrap_or_else(|| "agent".into());
                let config: Vec<_> = d
                    .applied_config
                    .iter()
                    .map(|c| {
                        format!(
                            "{}={}",
                            c.id,
                            value(c.current.as_ref().unwrap_or(&c.requested))
                        )
                    })
                    .collect();
                format!(
                    "{name}: mode {}{}{}",
                    d.applied_mode.as_deref().unwrap_or("(agent's default)"),
                    if config.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", config.join(" "))
                    },
                    match (d.can_resume, d.can_load) {
                        (true, _) => ", can resume",
                        (false, true) => ", can load",
                        _ => ", can't resume or load",
                    }
                )
            }
            EventBody::SessionCommands(d) => format!("{} slash commands", d.commands.len()),
            EventBody::TurnStarted(d) => {
                format!("{turn}> {}", d.text.lines().last().unwrap_or_default())
            }
            EventBody::TurnCompleted(d) => {
                let files: Vec<_> = d
                    .files
                    .iter()
                    .map(|f| format!("{} {}", name(&f.status), f.path))
                    .collect();
                let mut s = format!("{turn}turn ended: {}", name(&d.stop_reason));
                if !files.is_empty() {
                    s.push_str(&format!(", files: {}", files.join(", ")));
                }
                if let Some(c) = &d.cost {
                    s.push_str(&format!(", cost {:.4} {}", c.amount, c.currency));
                }
                if let Some(t) = d.tokens.as_ref().and_then(|t| t.total) {
                    s.push_str(&format!(", {t} tokens"));
                }
                s
            }
            EventBody::ItemStarted(d) | EventBody::ItemUpdated(d) | EventBody::ItemCompleted(d) => {
                match &d.item {
                    Item::ToolCall(t) => format!("{turn}tool {}", tool(t)),
                    Item::Plan(p) => format!("{turn}plan: {} entries", p.entries.len()),
                    _ => return,
                }
            }
            EventBody::RequestOpened(d) => {
                let mut s = format!(
                    "{turn}PERMISSION {} for {}",
                    d.request_id,
                    tool(&d.tool_call)
                );
                for (i, o) in d.options.iter().enumerate() {
                    s.push_str(&format!(
                        "\n  {}. {} [{}] {}",
                        i + 1,
                        o.option_id,
                        name(&o.kind),
                        o.name
                    ));
                }
                s
            }
            EventBody::RequestResolved(d) => format!(
                "{turn}request {} {} by {}",
                d.request_id,
                match (&d.option_id, d.option_kind) {
                    (Some(o), Some(kind)) => format!("{} ({o})", answer(kind)),
                    (Some(o), None) => format!("answered ({o})"),
                    _ => "cancelled".into(),
                },
                name(&d.by)
            ),
            EventBody::CheckpointCreated(d) => {
                format!(
                    "{turn}checkpoint {} {}",
                    d.n,
                    &d.commit[..d.commit.len().min(10)]
                )
            }
            EventBody::CheckpointReverted(d) => format!(
                "{turn}reverted to checkpoint {}: restored {:?}, deleted {:?}",
                d.to_n, d.restored, d.deleted
            ),
            EventBody::Error(d) => format!(
                "{turn}ERROR {}: {}{}",
                d.kind.as_str(),
                d.message,
                if d.fatal { " (fatal)" } else { "" }
            ),
            EventBody::RunStep(d) => format!(
                "run step {} {}{}: {}",
                d.n,
                serde_json::to_value(d.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                d.command
                    .as_deref()
                    .map(|c| format!(" `{c}`"))
                    .unwrap_or_default(),
                match d.exit_code {
                    Some(0) => "passed".to_string(),
                    Some(c) => format!("exit {c}"),
                    None => "stopped".to_string(),
                }
            ),
            EventBody::ContentDelta(_) | EventBody::UsageUpdated(_) | EventBody::AgentUpdate(_) => {
                return;
            }
        };
        crate::exit_if_stdout_closed(writeln!(out, "{line}"));
    }

    /// Asks for an answer to a request that is open now (not one in the replay).
    pub fn ask(&mut self, request: &actuo_protocol::ids::RequestId, out: &mut impl Write) {
        self.end_text(out);
        crate::exit_if_stdout_closed(writeln!(
            out,
            "  answer {request} with a number or optionId, or c to cancel"
        ));
    }

    fn end_text(&mut self, out: &mut impl Write) {
        if self.in_text {
            crate::exit_if_stdout_closed(writeln!(out));
            self.in_text = false;
        }
    }
}

/// What choosing an option of this kind means.
fn answer(kind: actuo_protocol::types::PermissionOptionKind) -> &'static str {
    use actuo_protocol::types::PermissionOptionKind::*;
    match kind {
        AllowOnce => "allowed once",
        AllowAlways => "allowed always",
        RejectOnce => "rejected",
        RejectAlways => "rejected always",
    }
}

/// A unit enum's wire name, e.g. `end_turn`.
pub(crate) fn name(v: &impl serde::Serialize) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn tool(t: &ToolCall) -> String {
    let mut s = format!("{} {} {:?}", name(&t.tool_kind), name(&t.status), t.title);
    if let Some(cmd) = t.raw_input.as_ref().and_then(|i| i["command"].as_str()) {
        s.push_str(&format!(" $ {}", cmd.lines().next().unwrap_or_default()));
    }
    for c in &t.content {
        if let ToolContent::Diff { path, .. } = c {
            s.push_str(&format!(" (diff {path})"));
        }
    }
    s
}

fn value(v: &actuo_protocol::types::ConfigValue) -> String {
    match v {
        actuo_protocol::types::ConfigValue::Bool(b) => b.to_string(),
        actuo_protocol::types::ConfigValue::String(s) => s.clone(),
    }
}

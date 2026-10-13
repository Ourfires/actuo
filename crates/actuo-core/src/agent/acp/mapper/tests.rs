use super::*;

/// Feeds a recorded turn (one JSON-RPC message per line, as the agent sent them)
/// through the mapper, the way the driver does.
fn replay(fixture: &str) -> (Vec<Mapped>, TurnEnd) {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let mut m = Mapper::new();
    m.begin_turn(1);
    let mut out = Vec::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let msg: Value = serde_json::from_str(line).unwrap();
        match msg["method"].as_str() {
            Some("session/update") => out.extend(m.update(&msg["params"])),
            Some("session/request_permission") => out.extend(m.permission(&msg["params"]).1),
            Some(_) => {} // other notifications (e.g. Claude's _auth/status_update) are ignored
            None => {
                let (events, end) = m.end_turn(&msg["result"]);
                out.extend(events);
                return (out, end);
            }
        }
    }
    panic!("no prompt response in {fixture}");
}

fn types(events: &[Mapped]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            serde_json::to_value(&e.body).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn count(events: &[Mapped], ty: &str) -> usize {
    types(events).iter().filter(|t| *t == ty).count()
}

fn items<'a>(events: &'a [Mapped], ty: &str) -> Vec<(&'a ItemId, &'a Item)> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ItemStarted(d) if ty == "item.started" => {
                Some((e.item_id.as_ref()?, &d.item))
            }
            EventBody::ItemUpdated(d) if ty == "item.updated" => {
                Some((e.item_id.as_ref()?, &d.item))
            }
            EventBody::ItemCompleted(d) if ty == "item.completed" => {
                Some((e.item_id.as_ref()?, &d.item))
            }
            _ => None,
        })
        .collect()
}

fn request(events: &[Mapped]) -> (&ItemId, &RequestOpenedData) {
    events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::RequestOpened(d) => Some((e.item_id.as_ref().unwrap(), d)),
            _ => None,
        })
        .unwrap()
}

fn option_summary(d: &RequestOpenedData) -> Vec<(&str, PermissionOptionKind)> {
    d.options
        .iter()
        .map(|o| (o.option_id.0.as_str(), o.kind))
        .collect()
}

#[test]
fn claude_turn() {
    let (events, end) = replay("claude-turn1.jsonl");

    // Text: 38 chunks, one message item, no item.updated for it.
    assert_eq!(count(&events, "content.delta"), 38);
    let messages: Vec<_> = items(&events, "item.completed")
        .into_iter()
        .filter_map(|(id, i)| match i {
            Item::Message(t) => Some((id, t.text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]
            .1
            .starts_with("Created `NOTES.md` with two lines:")
    );
    assert!(
        items(&events, "item.updated")
            .iter()
            .all(|(_, i)| matches!(i, Item::ToolCall(_)))
    );

    // Two tool calls: Read then Write, with their final titles, completed.
    let done: Vec<_> = items(&events, "item.completed")
        .into_iter()
        .filter_map(|(_, i)| match i {
            Item::ToolCall(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].title, "Read README.md");
    assert_eq!(done[0].tool_kind, ToolKind::Read);
    assert_eq!(done[1].title, "Write NOTES.md");
    assert_eq!(done[1].tool_kind, ToolKind::Edit);
    assert_eq!(done[1].status, ToolStatus::Completed);
    assert!(matches!(
        &done[1].content[0],
        ToolContent::Diff { path, .. } if path == "/work/NOTES.md"
    ));

    // The permission request is about the Write tool call.
    let (item_id, req) = request(&events);
    assert_eq!(req.request_id, RequestId::from("1.r1"));
    assert_eq!(req.tool_call.title, "Write NOTES.md");
    assert_eq!(
        option_summary(req),
        vec![
            ("allow-once", PermissionOptionKind::AllowOnce),
            ("allow-with-updates", PermissionOptionKind::AllowAlways),
            ("reject", PermissionOptionKind::RejectOnce),
        ]
    );
    let write_item = items(&events, "item.started")
        .into_iter()
        .find(|(_, i)| matches!(i, Item::ToolCall(t) if t.tool_kind == ToolKind::Edit))
        .unwrap()
        .0;
    assert_eq!(item_id, write_item);

    // Repeated identical command lists give one session.commands.
    assert_eq!(count(&events, "session.commands"), 1);
    assert_eq!(count(&events, "usage.updated"), 8);
    assert_eq!(count(&events, "agent.update"), 0);

    // Tokens from the raw response, cost as the running total's difference.
    assert_eq!(end.stop_reason, StopReason::EndTurn);
    let tokens = end.tokens.unwrap();
    assert_eq!(tokens.total, Some(120246));
    assert_eq!(tokens.cached_write, Some(34518));
    assert_eq!(tokens.thought, None);
    let cost = end.cost.unwrap();
    assert_eq!(cost.currency, "USD");
    assert!((cost.amount - 0.3045884).abs() < 1e-9);
}

#[test]
fn codex_turn() {
    let (events, end) = replay("codex-turn1.jsonl");

    // Text before and after the tool calls: two message items.
    assert_eq!(count(&events, "content.delta"), 73);
    let messages = items(&events, "item.completed")
        .into_iter()
        .filter(|(_, i)| matches!(i, Item::Message(_)))
        .count();
    assert_eq!(messages, 2);

    // A command, not a diff: kind execute, the command in rawInput. Its only content
    // is a terminal reference, although actuo doesn't offer terminals.
    let (_, req) = request(&events);
    assert_eq!(req.tool_call.tool_kind, ToolKind::Execute);
    assert!(matches!(
        req.tool_call.content.as_slice(),
        [ToolContent::Other { value }] if value["type"] == "terminal"
    ));
    assert!(
        req.tool_call.raw_input.as_ref().unwrap()["command"]
            .as_str()
            .unwrap()
            .contains("> NOTES.md")
    );
    assert_eq!(
        option_summary(req),
        vec![
            ("allow_once", PermissionOptionKind::AllowOnce),
            (
                "accept_execpolicy_amendment",
                PermissionOptionKind::AllowAlways
            ),
            ("cancel", PermissionOptionKind::RejectOnce),
        ]
    );

    // Tool calls start in_progress (no pending stage) and all complete.
    let started: Vec<_> = items(&events, "item.started")
        .into_iter()
        .filter_map(|(_, i)| match i {
            Item::ToolCall(t) => Some(t.status),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec![ToolStatus::InProgress; 4]);
    let completed = items(&events, "item.completed")
        .into_iter()
        .filter(|(_, i)| matches!(i, Item::ToolCall(t) if t.status == ToolStatus::Completed))
        .count();
    assert_eq!(completed, 4);

    // Codex's thread status updates are passed on as agent.update.
    assert_eq!(count(&events, "agent.update"), 5);

    // No cost; tokens including thought tokens.
    assert_eq!(end.cost, None);
    let tokens = end.tokens.unwrap();
    assert_eq!(tokens.total, Some(17304));
    assert_eq!(tokens.thought, Some(0));
}

fn update(m: &mut Mapper, u: Value) -> Vec<Mapped> {
    m.update(&json!({"sessionId": "s", "update": u}))
}

fn chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
}

#[test]
fn switching_between_thought_and_message_closes_the_open_item() {
    let mut m = Mapper::new();
    m.begin_turn(2);
    let mut ev = Vec::new();
    ev.extend(update(&mut m, chunk("agent_thought_chunk", "hmm ")));
    ev.extend(update(&mut m, chunk("agent_thought_chunk", "ok")));
    ev.extend(update(&mut m, chunk("agent_message_chunk", "Hi")));
    ev.extend(m.end_turn(&json!({"stopReason": "end_turn"})).0);
    assert_eq!(
        types(&ev),
        vec![
            "item.started",
            "content.delta",
            "content.delta",
            "item.completed",
            "item.started",
            "content.delta",
            "item.completed"
        ]
    );
    assert_eq!(ev[0].item_id, Some(ItemId::from("2.1")));
    assert!(
        matches!(&ev[3].body, EventBody::ItemCompleted(d) if d.item == Item::Thought(TextItem { text: "hmm ok".into() }))
    );
    assert_eq!(ev[4].item_id, Some(ItemId::from("2.2")));
}

#[test]
fn a_new_message_id_starts_a_new_message() {
    let mut m = Mapper::new();
    m.begin_turn(1);
    let msg = |id: &str, text: &str| {
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id,
               "content": {"type": "text", "text": text}})
    };
    let mut ev = Vec::new();
    ev.extend(update(&mut m, msg("a", "BAN")));
    ev.extend(update(&mut m, msg("a", "ANA")));
    ev.extend(update(&mut m, msg("b", "I'm using")));
    ev.extend(update(&mut m, msg("b", " the shell")));
    ev.extend(m.end_turn(&json!({"stopReason": "end_turn"})).0);
    let texts: Vec<_> = ev
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ItemCompleted(d) => match &d.item {
                Item::Message(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["BANANA", "I'm using the shell"]);
}

/// A notice, a second notice, an error with its own message id and a reply can
/// come as chunks of one turn; joined, they'd read as one message. The
/// informational flag and the ids tell them apart.
#[test]
fn notices_and_messages_with_and_without_ids_are_separate() {
    let mut m = Mapper::new();
    m.begin_turn(1);
    let notice = json!({"sessionUpdate": "agent_message_chunk",
        "_meta": {"claudeCode": {"kind": "informational", "level": "notice"}},
        "content": {"type": "text", "text": "**Notice:** retrying once"}});
    let with_id = json!({"sessionUpdate": "agent_message_chunk", "messageId": "8b22",
        "content": {"type": "text", "text": "API Error: overloaded"}});
    let mut ev = Vec::new();
    ev.extend(update(&mut m, notice));
    ev.extend(update(
        &mut m,
        json!({"sessionUpdate": "usage_update", "used": 1, "size": 2}),
    ));
    ev.extend(update(
        &mut m,
        chunk("agent_message_chunk", "**Note:** using another model."),
    ));
    ev.extend(update(&mut m, with_id));
    ev.extend(update(
        &mut m,
        chunk("agent_message_chunk", "Trying again."),
    ));
    ev.extend(update(&mut m, chunk("agent_message_chunk", " More.")));
    ev.extend(m.end_turn(&json!({"stopReason": "end_turn"})).0);
    let texts: Vec<_> = ev
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ItemCompleted(d) => match &d.item {
                Item::Message(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "**Notice:** retrying once",
            "**Note:** using another model.",
            "API Error: overloaded",
            "Trying again. More.",
        ]
    );
}

#[test]
fn end_of_turn_completes_open_tool_calls_and_the_plan() {
    let mut m = Mapper::new();
    m.begin_turn(1);
    update(
        &mut m,
        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "x", "kind": "execute"}),
    );
    update(
        &mut m,
        json!({"sessionUpdate": "plan", "entries": [{"content": "a", "status": "pending"}]}),
    );
    let ev = update(
        &mut m,
        json!({"sessionUpdate": "plan", "entries": [{"content": "a", "status": "completed"}]}),
    );
    assert_eq!(types(&ev), vec!["item.updated"]);
    let (ev, end) = m.end_turn(&json!({"stopReason": "cancelled"}));
    assert_eq!(types(&ev), vec!["item.completed", "item.completed"]);
    assert!(matches!(&ev[0].body, EventBody::ItemCompleted(d)
        if matches!(&d.item, Item::ToolCall(t) if t.status == ToolStatus::Cancelled)));
    assert_eq!(end.stop_reason, StopReason::Cancelled);
}

#[test]
fn cost_is_the_difference_within_one_process() {
    let mut m = Mapper::new();
    m.begin_turn(1);
    update(
        &mut m,
        json!({"sessionUpdate": "usage_update", "used": 1, "size": 9,
                          "cost": {"amount": 0.3046, "currency": "USD"}}),
    );
    assert!(
        (m.end_turn(&json!({"stopReason": "end_turn"}))
            .1
            .cost
            .unwrap()
            .amount
            - 0.3046)
            .abs()
            < 1e-12
    );
    m.begin_turn(2);
    update(
        &mut m,
        json!({"sessionUpdate": "usage_update", "used": 2, "size": 9,
                          "cost": {"amount": 0.3336, "currency": "USD"}}),
    );
    let second = m
        .end_turn(&json!({"stopReason": "end_turn"}))
        .1
        .cost
        .unwrap();
    assert!((second.amount - 0.029).abs() < 1e-12);
    // A turn with no cost report of its own: its cost isn't known (not 0).
    m.begin_turn(3);
    assert_eq!(m.end_turn(&json!({"stopReason": "end_turn"})).1.cost, None);
}

/// Claude's adapter answers zeros for a cancelled turn it settled before the SDK's
/// result: unknown. Zeros on a finished turn, and real numbers on a cancelled one,
/// stay.
#[test]
fn a_cancelled_turn_with_zero_usage_is_unknown() {
    let zeros = json!({"inputTokens": 0, "outputTokens": 0, "cachedReadTokens": 0,
                       "cachedWriteTokens": 0, "totalTokens": 0});
    let mut m = Mapper::new();
    m.begin_turn(1);
    let end = m
        .end_turn(&json!({"stopReason": "cancelled", "usage": zeros}))
        .1;
    assert_eq!(end.tokens, None);
    m.begin_turn(2);
    let end = m
        .end_turn(&json!({"stopReason": "end_turn", "usage": zeros}))
        .1;
    assert_eq!(end.tokens.unwrap().total, Some(0));
    m.begin_turn(3);
    let some = json!({"inputTokens": 5, "outputTokens": 0, "totalTokens": 5});
    let end = m
        .end_turn(&json!({"stopReason": "cancelled", "usage": some}))
        .1;
    assert_eq!(end.tokens.unwrap().total, Some(5));
}

#[test]
fn tokens_are_best_effort() {
    assert_eq!(parse_tokens(&Value::Null), None);
    assert_eq!(parse_tokens(&json!("lots")), None);
    assert_eq!(parse_tokens(&json!({"prompt_tokens": 5})), None);
    assert_eq!(
        parse_tokens(&json!({"totalTokens": 5})).unwrap().total,
        Some(5)
    );
    // A response without usage or with an odd one is never an error.
    let (_, end) = Mapper::new().end_turn(&json!({"stopReason": "end_turn", "usage": [1, 2]}));
    assert_eq!(end.tokens, None);
}

#[test]
fn unknown_updates_and_odd_options() {
    let mut m = Mapper::new();
    m.begin_turn(1);
    let ev = update(
        &mut m,
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
    );
    assert!(matches!(&ev[0].body, EventBody::AgentUpdate(d) if d.kind == "current_mode_update"));
    assert!(update(&mut m, chunk("user_message_chunk", "replayed")).is_empty());

    // A permission request about an unseen tool call starts the item; an option of
    // an unknown kind is dropped.
    let (id, ev) = m.permission(&json!({
        "sessionId": "s",
        "toolCall": {"toolCallId": "t9", "title": "Run", "kind": "execute"},
        "options": [{"optionId": "a", "name": "A", "kind": "allow_once"},
                    {"optionId": "z", "name": "Z", "kind": "allow_forever"}]
    }));
    assert_eq!(id, RequestId::from("1.r1"));
    assert_eq!(types(&ev), vec!["item.started", "request.opened"]);
    let EventBody::RequestOpened(d) = &ev[1].body else {
        panic!()
    };
    assert_eq!(d.options.len(), 1);
}

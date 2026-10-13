//! Wire shapes: what the JSON looks like, and that it reads back into the same value.

use actuo_protocol::events::*;
use actuo_protocol::ids::*;
use actuo_protocol::methods::*;
use actuo_protocol::types::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    value: &T,
    expected: Value,
) {
    let json = serde_json::to_value(value).unwrap();
    assert_eq!(json, expected);
    let back: T = serde_json::from_value(json).unwrap();
    assert_eq!(&back, value);
}

fn event(body: EventBody) -> Event {
    Event {
        seq: 7,
        session_id: SessionId::from("s1"),
        ts: "2026-09-30T12:00:00Z".into(),
        turn: Some(2),
        item_id: Some(ItemId::from("i3")),
        body,
        raw: None,
    }
}

#[test]
fn event_envelope_has_type_and_data() {
    round_trip(
        &event(EventBody::ContentDelta(ContentDeltaData {
            text: "hi".into(),
        })),
        json!({
            "seq": 7, "sessionId": "s1", "ts": "2026-09-30T12:00:00Z", "turn": 2, "itemId": "i3",
            "type": "content.delta", "data": {"text": "hi"}
        }),
    );
}

#[test]
fn request_opened_carries_tool_call_and_options() {
    let tool_call = ToolCall {
        tool_kind: ToolKind::Execute,
        title: "Run command".into(),
        status: ToolStatus::Pending,
        raw_input: Some(json!({"command": "printf x > NOTES.md", "cwd": "/w"})),
        raw_output: None,
        content: vec![],
        locations: vec![],
    };
    round_trip(
        &event(EventBody::RequestOpened(RequestOpenedData {
            request_id: RequestId::from("r1"),
            tool_call,
            options: vec![PermissionOption {
                option_id: PermissionOptionId::from("cancel"),
                name: "No".into(),
                kind: PermissionOptionKind::RejectOnce,
            }],
        })),
        json!({
            "seq": 7, "sessionId": "s1", "ts": "2026-09-30T12:00:00Z", "turn": 2, "itemId": "i3",
            "type": "request.opened",
            "data": {
                "requestId": "r1",
                "toolCall": {
                    "toolKind": "execute", "title": "Run command", "status": "pending",
                    "rawInput": {"command": "printf x > NOTES.md", "cwd": "/w"},
                    "content": [], "locations": []
                },
                "options": [{"optionId": "cancel", "name": "No", "kind": "reject_once"}]
            }
        }),
    );
}

#[test]
fn item_is_tagged_by_kind() {
    round_trip(
        &event(EventBody::ItemUpdated(ItemData {
            item: Item::ToolCall(ToolCall {
                tool_kind: ToolKind::Edit,
                title: "Write NOTES.md".into(),
                status: ToolStatus::InProgress,
                raw_input: None,
                raw_output: None,
                content: vec![ToolContent::Diff {
                    path: "/w/NOTES.md".into(),
                    old_text: None,
                    new_text: "x\n".into(),
                }],
                locations: vec![Location {
                    path: "/w/NOTES.md".into(),
                    line: None,
                }],
            }),
        })),
        json!({
            "seq": 7, "sessionId": "s1", "ts": "2026-09-30T12:00:00Z", "turn": 2, "itemId": "i3",
            "type": "item.updated",
            "data": {"item": {
                "kind": "tool_call", "toolKind": "edit", "title": "Write NOTES.md", "status": "in_progress",
                "content": [{"type": "diff", "path": "/w/NOTES.md", "newText": "x\n"}],
                "locations": [{"path": "/w/NOTES.md"}]
            }}
        }),
    );
    round_trip(
        &Item::Message(TextItem {
            text: "hello".into(),
        }),
        json!({"kind": "message", "text": "hello"}),
    );
}

#[test]
fn turn_completed_and_request_resolved() {
    round_trip(
        &event(EventBody::TurnCompleted(TurnCompletedData {
            stop_reason: StopReason::ActuoStopped,
            tokens: None,
            cost: Some(Cost {
                amount: 0.029,
                currency: "USD".into(),
            }),
            files: vec![FileChange {
                path: "NOTES.md".into(),
                status: FileStatus::Added,
            }],
        })),
        json!({
            "seq": 7, "sessionId": "s1", "ts": "2026-09-30T12:00:00Z", "turn": 2, "itemId": "i3",
            "type": "turn.completed",
            "data": {
                "stopReason": "actuo_stopped",
                "cost": {"amount": 0.029, "currency": "USD"},
                "files": [{"path": "NOTES.md", "status": "added"}]
            }
        }),
    );
    round_trip(
        &RequestResolvedData {
            request_id: RequestId::from("r1"),
            outcome: RequestOutcome::Cancelled,
            option_id: None,
            option_kind: None,
            by: ResolvedBy::Interrupt,
            client: None,
        },
        json!({"requestId": "r1", "outcome": "cancelled", "by": "interrupt"}),
    );
    round_trip(
        &RequestResolvedData {
            request_id: RequestId::from("r1"),
            outcome: RequestOutcome::Cancelled,
            option_id: None,
            option_kind: None,
            by: ResolvedBy::Client,
            client: Some("lane".into()),
        },
        json!({"requestId": "r1", "outcome": "cancelled", "by": "client", "client": "lane"}),
    );
}

#[test]
fn session_start_defaults() {
    let params: SessionStartParams =
        serde_json::from_value(json!({"projectId": "p1", "agent": "claude"})).unwrap();
    assert_eq!(params.agent.as_deref(), Some("claude"));
    assert!(params.worktree);
    assert_eq!(params.level, Level::Ask);
    assert!(!params.clean);
    assert!(params.config.is_empty());

    let params: SessionStartParams = serde_json::from_value(json!({
        "projectId": "p1", "agent": "codex", "worktree": false, "level": "edits",
        "config": {"model": "some-model", "fast-mode": "off", "flag": true}
    }))
    .unwrap();
    assert!(!params.worktree);
    assert_eq!(
        params.config["model"],
        ConfigValue::String("some-model".into())
    );
    assert_eq!(params.config["flag"], ConfigValue::Bool(true));

    // Without an agent: actuo uses the settings' `sessions.agent`.
    let params: SessionStartParams = serde_json::from_value(json!({"projectId": "p1"})).unwrap();
    assert_eq!(params.agent, None);
    assert_eq!(
        serde_json::to_value(&params).unwrap(),
        json!({"projectId": "p1", "level": "ask", "clean": false})
    );
}

#[test]
fn respond_with_option_or_cancel() {
    round_trip(
        &SessionRespondParams {
            session_id: SessionId::from("s1"),
            request_id: RequestId::from("r1"),
            option_id: Some(PermissionOptionId::from("allow-once")),
            cancel: false,
        },
        json!({"sessionId": "s1", "requestId": "r1", "optionId": "allow-once"}),
    );
    round_trip(
        &SessionRespondParams {
            session_id: SessionId::from("s1"),
            request_id: RequestId::from("r1"),
            option_id: None,
            cancel: true,
        },
        json!({"sessionId": "s1", "requestId": "r1", "cancel": true}),
    );
}

#[test]
fn session_detail_flattens_summary() {
    let detail = SessionDetail {
        summary: SessionSummary {
            id: SessionId::from("s1"),
            project_id: ProjectId::from("p1"),
            agent: "claude".into(),
            agent_version: Some("0.84.0".into()),
            level: Level::Ask,
            clean: false,
            cwd: "/w".into(),
            worktree_path: Some("/w".into()),
            branch: Some("actuo/s1".into()),
            base: None,
            sandbox: None,
            web_tools: true,
            telemetry: true,
            state: SessionState::Detached,
            end_reason: None,
            title: None,
            turn_count: 1,
            head_seq: 40,
            created_at: "2026-09-30T12:00:00Z".into(),
            updated_at: "2026-09-30T12:01:00Z".into(),
        },
        config: Default::default(),
        turns: vec![],
        open_requests: vec![],
        commands: vec![AgentCommand {
            name: "compact".into(),
            description: None,
            input_hint: None,
        }],
    };
    let json = serde_json::to_value(&detail).unwrap();
    assert_eq!(json["state"], "detached");
    assert_eq!(json["turnCount"], 1);
    assert_eq!(json["commands"][0]["name"], "compact");
    assert_eq!(
        serde_json::from_value::<SessionDetail>(json).unwrap(),
        detail
    );
}

#[test]
fn a_check_step_with_its_failed_packages_and_the_baseline_step() {
    use actuo_protocol::runs::{CheckStep, RunStepKind};
    let step = CheckStep {
        n: 2,
        kind: RunStepKind::Check,
        command: "pnpm turbo test".into(),
        exit_code: Some(1),
        timed_out: false,
        output_tail: None,
        duration_ms: 10,
        peak_kb: None,
        peak_process_kb: None,
        diagnosis: None,
        failed_packages: Some(vec!["@a/x".into()]),
    };
    round_trip(
        &step,
        json!({
            "n": 2, "kind": "check", "command": "pnpm turbo test", "exitCode": 1,
            "timedOut": false, "durationMs": 10, "failedPackages": ["@a/x"]
        }),
    );
    // Without failed packages: absent.
    let without: CheckStep = serde_json::from_value(json!({
        "n": 1, "kind": "check", "command": "true", "timedOut": false, "durationMs": 0
    }))
    .unwrap();
    assert_eq!(without.failed_packages, None);
    assert_eq!(
        serde_json::to_value(RunStepKind::Baseline).unwrap(),
        json!("baseline")
    );
}

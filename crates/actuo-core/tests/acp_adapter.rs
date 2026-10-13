//! The ACP backend against the scripted fake agent.

mod common;

use actuo_core::agent::acp::AcpBackend;
use actuo_core::agent::{AgentBackend, AgentError, AgentEvent, AgentHandle, AgentSpec, Mapped};
use actuo_protocol::events::EventBody;
use actuo_protocol::ids::{PermissionOptionId, RequestId};
use actuo_protocol::types::{ConfigValue, Item, StopReason, ToolStatus};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Setup {
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("script.json"), script.to_string()).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        Self { _dir: dir, root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn spawn(&self) -> (AgentHandle, UnboundedReceiver<AgentEvent>) {
        AcpBackend
            .spawn(AgentSpec {
                command: common::fake_agent().display().to_string(),
                args: vec![
                    "--script".into(),
                    self.root.join("script.json").display().to_string(),
                    "--state".into(),
                    self.root.join("state").display().to_string(),
                    "--log".into(),
                    self.root.join("log.jsonl").display().to_string(),
                ],
                env: Default::default(),
                clear_env: false,
                cwd: self.work(),
                stderr_log: Some(self.root.join("stderr.log")),
                srt_records: None,
            })
            .unwrap()
    }

    fn logged(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["method"] == method)
            .map(|v| v["params"].clone())
            .collect()
    }
}

fn script() -> Value {
    json!({
        "loadSession": true,
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "configOptions": [{"id": "model", "name": "Model", "category": "model",
                           "values": ["small", "large"], "current": "small"}],
        "commands": [{"name": "compact", "description": "Compact"}],
        "turns": [
            {"match": "notes", "steps": [
                {"thought": "Reading"},
                {"message": "Writing "},
                {"message": "NOTES.md"},
                {"toolCall": {"id": "t1", "title": "Write NOTES.md", "kind": "edit",
                              "diff": {"path": "NOTES.md", "newText": "notes\n"}}},
                {"permission": {"toolCallId": "t1",
                    "options": [{"id": "allow-once", "name": "Yes", "kind": "allow_once"},
                                {"id": "reject", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "NOTES.md", "content": "notes\n"}},
                                {"toolUpdate": {"id": "t1", "status": "completed"}}]}},
                {"usage": {"used": 100, "size": 1000, "cost": 0.25}}
            ]},
            {"match": "long", "steps": [
                {"toolCall": {"id": "t2", "title": "Run command", "kind": "execute",
                              "rawInput": {"command": "python3 write.py"}}},
                {"permission": {"toolCallId": "t2",
                    "options": [{"id": "allow_once", "name": "Yes", "kind": "allow_once"},
                                {"id": "cancel", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "part1.txt", "content": "1\n"}}]}}
            ]},
            {"match": "crash", "steps": [{"message": "bye"}, {"sleepMs": 200}, {"exit": 3}]}
        ]
    })
}

async fn next(rx: &mut UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("an agent event within 10 s")
        .expect("event stream open")
}

/// Collects mapped events until `stop` returns true for one (included).
async fn until(
    rx: &mut UnboundedReceiver<AgentEvent>,
    mut stop: impl FnMut(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let e = next(rx).await;
        let done = stop(&e);
        out.push(e);
        if done {
            return out;
        }
    }
}

fn opened_request(events: &[AgentEvent]) -> Option<RequestId> {
    events.iter().find_map(|e| match e {
        AgentEvent::Event(Mapped {
            body: EventBody::RequestOpened(d),
            ..
        }) => Some(d.request_id.clone()),
        _ => None,
    })
}

fn body_types(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Event(m) => Some(
                serde_json::to_value(&m.body).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn setup_and_a_turn_with_a_permission() {
    let s = Setup::new(script());
    let (agent, mut rx) = s.spawn();
    assert!(agent.pid() > 0);
    assert!(actuo_core::process::start_time(agent.pid()).is_some());

    let info = agent.initialize().await.unwrap();
    assert!(info.can_load && info.can_resume);
    assert_eq!(info.implementation.unwrap().name, "fake-agent");

    let meta = json!({"claudeCode": {"options": {"allowDangerouslySkipPermissions": false}}});
    let setup = agent.new_session(Some(meta.clone())).await.unwrap();
    assert_eq!(setup.current_mode.as_deref(), Some("ask"));
    assert_eq!(setup.modes.len(), 2);
    assert_eq!(setup.config_options[0].id, "model");
    assert_eq!(s.logged("session/new")[0]["_meta"], meta);

    agent.set_mode("edits").await.unwrap();
    assert!(agent.set_mode("nope").await.is_err());
    let options = agent
        .set_config("model", ConfigValue::String("large".into()))
        .await
        .unwrap();
    assert_eq!(
        options[0].current_value,
        Some(ConfigValue::String("large".into()))
    );

    // The command list arrives after session/new.
    let commands = until(
        &mut rx,
        |e| matches!(e, AgentEvent::Event(m) if matches!(m.body, EventBody::SessionCommands(_))),
    )
    .await;
    assert_eq!(body_types(&commands), vec!["session.commands"]);

    agent.prompt(1, "write notes").await.unwrap();
    let before = until(&mut rx, |e| {
        opened_request(std::slice::from_ref(e)).is_some()
    })
    .await;
    let request = opened_request(&before).unwrap();
    assert_eq!(request, RequestId::from("1.r1"));
    agent
        .respond(request, Some(PermissionOptionId::from("allow-once")))
        .unwrap();
    let after = until(&mut rx, |e| matches!(e, AgentEvent::TurnEnded(_))).await;

    assert_eq!(
        std::fs::read_to_string(s.work().join("NOTES.md")).unwrap(),
        "notes\n"
    );
    let all: Vec<_> = before.iter().chain(&after).cloned().collect();
    assert_eq!(
        body_types(&all),
        vec![
            "item.started", // thought
            "content.delta",
            "item.completed", // thought, before the message
            "item.started",   // message
            "content.delta",
            "content.delta",
            "item.completed", // message, before the tool call
            "item.started",   // tool call
            "request.opened",
            "item.completed", // tool call completed
            "usage.updated",
        ]
    );
    // Every event keeps the agent's message, except the ones the mapper made up.
    let AgentEvent::Event(delta) = &before[1] else {
        panic!()
    };
    assert_eq!(delta.raw.as_ref().unwrap()["method"], "session/update");
    let message = all.iter().find_map(|e| match e {
        AgentEvent::Event(Mapped {
            body: EventBody::ItemCompleted(d),
            ..
        }) => match &d.item {
            Item::Message(t) => Some(t.text.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(message.as_deref(), Some("Writing NOTES.md"));
    let AgentEvent::TurnEnded(end) = after.last().unwrap() else {
        panic!()
    };
    assert_eq!(end.stop_reason, StopReason::EndTurn);
    assert_eq!(end.cost.as_ref().unwrap().amount, 0.25);
    assert_eq!(end.tokens, None); // the fake sends no usage in its response

    agent.shutdown().await;
    let exited = until(&mut rx, |e| matches!(e, AgentEvent::Exited(_))).await;
    assert!(matches!(exited.last(), Some(AgentEvent::Exited(_))));
    assert!(!actuo_core::process::group_alive(agent.pid()));
}

#[tokio::test]
async fn cancel_answers_the_pending_request_and_ends_cancelled() {
    let s = Setup::new(script());
    let (agent, mut rx) = s.spawn();
    agent.initialize().await.unwrap();
    agent.new_session(None).await.unwrap();

    agent.prompt(1, "long task").await.unwrap();
    let before = until(&mut rx, |e| {
        opened_request(std::slice::from_ref(e)).is_some()
    })
    .await;
    agent.cancel().unwrap();
    agent
        .respond(opened_request(&before).unwrap(), None)
        .unwrap();
    let after = until(&mut rx, |e| matches!(e, AgentEvent::TurnEnded(_))).await;

    let AgentEvent::TurnEnded(end) = after.last().unwrap() else {
        panic!()
    };
    assert_eq!(end.stop_reason, StopReason::Cancelled);
    // The tool call that never ran is completed with the turn, as cancelled.
    let tool = after.iter().find_map(|e| match e {
        AgentEvent::Event(Mapped {
            body: EventBody::ItemCompleted(d),
            ..
        }) => match &d.item {
            Item::ToolCall(t) => Some(t.status),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(tool, Some(ToolStatus::Cancelled));
    assert!(!s.work().join("part1.txt").exists());
    assert_eq!(s.logged("session/cancel").len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn load_drops_the_replay_and_resume_reattaches() {
    let s = Setup::new(script());
    let (agent, mut rx) = s.spawn();
    agent.initialize().await.unwrap();
    let id = agent.new_session(None).await.unwrap().agent_session_id;
    agent.prompt(1, "hello").await.unwrap();
    until(&mut rx, |e| matches!(e, AgentEvent::TurnEnded(_))).await;
    agent.shutdown().await;

    // A new process loads the session: the agent replays "hello"/"ok", which must
    // not come out as events.
    let (agent, mut rx) = s.spawn();
    agent.initialize().await.unwrap();
    let setup = agent.load_session(&id, None).await.unwrap();
    assert_eq!(setup.agent_session_id, id);
    agent.prompt(2, "again").await.unwrap();
    let turn = until(&mut rx, |e| matches!(e, AgentEvent::TurnEnded(_))).await;
    assert_eq!(
        body_types(&turn),
        vec!["item.started", "content.delta", "item.completed"]
    );
    assert!(matches!(&turn[0], AgentEvent::Event(m) if m.item_id.as_ref().unwrap().0 == "2.1"));
    agent.shutdown().await;

    let (agent, _rx) = s.spawn();
    agent.initialize().await.unwrap();
    agent.resume_session(&id, None).await.unwrap();
    assert!(agent.resume_session("nope", None).await.is_err());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_crash_is_reported_with_its_exit_code() {
    let s = Setup::new(script());
    let (agent, mut rx) = s.spawn();
    agent.initialize().await.unwrap();
    agent.new_session(None).await.unwrap();
    agent.prompt(1, "crash").await.unwrap();
    let events = until(&mut rx, |e| matches!(e, AgentEvent::Exited(_))).await;
    // The open message is completed before the exit is reported. (The sleep in the
    // script lets the chunk out first; output an agent hasn't flushed is lost.)
    assert_eq!(
        body_types(&events),
        vec![
            "session.commands",
            "item.started",
            "content.delta",
            "item.completed"
        ]
    );
    let AgentEvent::Exited(exit) = events.last().unwrap() else {
        panic!()
    };
    assert_eq!(exit.code, Some(3));
    assert!(matches!(agent.prompt(2, "x").await, Err(AgentError::Gone)));
}

#[tokio::test]
async fn auth_required_is_its_own_error() {
    let s = Setup::new(json!({"authRequired": true}));
    let (agent, _rx) = s.spawn();
    agent.initialize().await.unwrap();
    assert!(matches!(
        agent.new_session(None).await,
        Err(AgentError::AuthRequired(_))
    ));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_missing_command_is_a_spawn_error() {
    let err = AcpBackend
        .spawn(AgentSpec {
            command: "/nonexistent/agent".into(),
            args: vec![],
            env: Default::default(),
            clear_env: false,
            cwd: std::env::temp_dir(),
            stderr_log: None,
            srt_records: None,
        })
        .err()
        .unwrap();
    assert!(matches!(err, AgentError::Spawn(_)));
}

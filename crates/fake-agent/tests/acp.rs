//! Drives the fake agent as an ACP v1 client, as actuo does.

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionId, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, ConnectionTo, Responder};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

type Permission = (
    RequestPermissionRequest,
    Responder<RequestPermissionResponse>,
);

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    script: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

fn setup(script: Value) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("script.json");
    std::fs::write(&path, script.to_string()).unwrap();
    std::fs::create_dir(root.join("work")).unwrap();
    Setup {
        script: path,
        state: root.join("state"),
        log: root.join("log.jsonl"),
        root,
        _dir: dir,
    }
}

fn agent(s: &Setup) -> AcpAgent {
    AcpAgent::new(AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-agent")).args([
        "--script".to_string(),
        s.script.display().to_string(),
        "--state".into(),
        s.state.display().to_string(),
        "--log".into(),
        s.log.display().to_string(),
    ]))
}

/// Connects to a fresh fake-agent process and runs `body`. Updates and permission
/// requests go to the channels.
async fn with_agent<T: Send + 'static>(
    s: &Setup,
    updates: mpsc::UnboundedSender<Value>,
    permissions: mpsc::UnboundedSender<Permission>,
    body: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<T, agent_client_protocol::Error>,
) -> Result<T, agent_client_protocol::Error> {
    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                let _ = updates.send(serde_json::to_value(&n.update).unwrap());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                let _ = permissions.send((req, responder));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent(s), body)
        .await
}

fn text(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(t.to_string()))]
}

fn cwd(s: &Setup) -> PathBuf {
    s.root.join("work")
}

async fn start(cx: &ConnectionTo<Agent>, cwd: &Path) -> SessionId {
    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await
        .unwrap();
    cx.send_request(NewSessionRequest::new(cwd.to_path_buf()))
        .block_task()
        .await
        .unwrap()
        .session_id
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(v) = rx.try_recv() {
        out.push(v);
    }
    out
}

fn kinds(updates: &[Value]) -> Vec<&str> {
    updates
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap())
        .collect()
}

fn logged_methods(s: &Setup) -> Vec<String> {
    std::fs::read_to_string(&s.log)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn notes_script() -> Value {
    json!({
        "loadSession": true,
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "configOptions": [{"id": "model", "name": "Model", "category": "model",
                           "values": ["small", "large"], "current": "small"}],
        "commands": [{"name": "compact", "description": "Compact the context"}],
        "turns": [
            {"match": "notes", "steps": [
                {"thought": "Reading README"},
                {"message": "Writing "},
                {"message": "NOTES.md"},
                {"toolCall": {"id": "t1", "title": "Write NOTES.md", "kind": "edit",
                              "diff": {"path": "NOTES.md", "newText": "notes\n"}}},
                {"permission": {"toolCallId": "t1",
                    "options": [{"id": "allow-once", "name": "Yes", "kind": "allow_once"},
                                {"id": "reject", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "NOTES.md", "content": "notes\n"}},
                                {"toolUpdate": {"id": "t1", "status": "completed"}}],
                    "rejected": [{"toolUpdate": {"id": "t1", "status": "failed"}}]}},
                {"usage": {"used": 100, "size": 1000, "cost": 0.25}},
                {"title": "Notes"}
            ]},
            {"match": "long", "steps": [
                {"toolCall": {"id": "t2", "title": "Run command", "kind": "execute",
                              "rawInput": {"command": "python3 write.py"}}},
                {"permission": {"toolCallId": "t2",
                    "options": [{"id": "allow_once", "name": "Yes, proceed", "kind": "allow_once"},
                                {"id": "cancel", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "part1.txt", "content": "1\n"}}]}}
            ]},
            {"match": "wait", "steps": [{"message": "working"}, "waitForCancel"]},
            {"match": "crash", "steps": [{"exit": 3}]}
        ]
    })
}

#[tokio::test]
async fn turn_with_permission_writes_the_file() {
    let s = setup(notes_script());
    let (utx, mut urx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    let stop = with_agent(&s, utx, ptx, async |cx| {
        let id = start(&cx, &work).await;
        let turn = cx
            .send_request(PromptRequest::new(id, text("write notes")))
            .block_task();
        let answer = async {
            let (req, responder): Permission = prx.recv().await.unwrap();
            assert_eq!(req.options.len(), 2);
            let allow = req.options[0].option_id.clone();
            responder
                .respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(allow)),
                ))
                .unwrap();
        };
        let (response, ()) = tokio::join!(turn, answer);
        Ok(response?.stop_reason)
    })
    .await
    .unwrap();

    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(
        std::fs::read_to_string(cwd(&s).join("NOTES.md")).unwrap(),
        "notes\n"
    );
    let updates = drain(&mut urx);
    assert_eq!(
        kinds(&updates),
        vec![
            "available_commands_update",
            "agent_thought_chunk",
            "agent_message_chunk",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
            "usage_update",
            "session_info_update",
        ]
    );
    let tool_call = &updates[4];
    assert_eq!(tool_call["content"][0]["type"], "diff");
    assert_eq!(
        tool_call["content"][0]["path"],
        cwd(&s).join("NOTES.md").display().to_string()
    );
    assert_eq!(updates[6]["cost"]["amount"], 0.25);
    assert_eq!(
        logged_methods(&s),
        vec!["started", "initialize", "session/new", "session/prompt"]
    );
}

#[tokio::test]
async fn cancel_with_a_pending_permission_ends_the_turn_cancelled() {
    let s = setup(notes_script());
    let (utx, _urx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    let stop = with_agent(&s, utx, ptx, async |cx| {
        let id = start(&cx, &work).await;
        let turn = cx
            .send_request(PromptRequest::new(id.clone(), text("long task")))
            .block_task();
        let cancel = async {
            let (_req, responder): Permission = prx.recv().await.unwrap();
            cx.send_notification(CancelNotification::new(id.clone()))
                .unwrap();
            responder
                .respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
                .unwrap();
        };
        let (response, ()) = tokio::join!(turn, cancel);
        Ok(response?.stop_reason)
    })
    .await
    .unwrap();
    assert_eq!(stop, StopReason::Cancelled);
    assert!(!cwd(&s).join("part1.txt").exists());
    assert!(logged_methods(&s).contains(&"session/cancel".to_string()));
}

#[tokio::test]
async fn wait_for_cancel() {
    let s = setup(notes_script());
    let (utx, mut urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    let stop = with_agent(&s, utx, ptx, async |cx| {
        let id = start(&cx, &work).await;
        let turn = cx
            .send_request(PromptRequest::new(id.clone(), text("wait here")))
            .block_task();
        let cancel = async {
            // Cancel once the turn has visibly started.
            while urx.recv().await.unwrap()["sessionUpdate"] != "agent_message_chunk" {}
            cx.send_notification(CancelNotification::new(id.clone()))
                .unwrap();
        };
        let (response, ()) = tokio::join!(turn, cancel);
        Ok(response?.stop_reason)
    })
    .await
    .unwrap();
    assert_eq!(stop, StopReason::Cancelled);
}

#[tokio::test]
async fn load_replays_and_resume_does_not_after_a_restart() {
    let s = setup(notes_script());
    let work = cwd(&s);

    // First process: a session with one turn.
    let (utx, _urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let id = with_agent(&s, utx, ptx, async |cx| {
        let id = start(&cx, &work).await;
        cx.send_request(PromptRequest::new(id.clone(), text("hello")))
            .block_task()
            .await?;
        Ok(id)
    })
    .await
    .unwrap();

    // Second process: load replays the history, then the session goes on.
    let (utx, mut urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let stop = with_agent(&s, utx, ptx, async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        cx.send_request(LoadSessionRequest::new(id.clone(), work.clone()))
            .block_task()
            .await?;
        let r = cx
            .send_request(PromptRequest::new(id.clone(), text("again")))
            .block_task()
            .await?;
        Ok(r.stop_reason)
    })
    .await
    .unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    let updates = drain(&mut urx);
    assert_eq!(
        kinds(&updates)[..2],
        ["user_message_chunk", "agent_message_chunk"]
    );
    assert_eq!(updates[0]["content"]["text"], "hello");
    assert_eq!(updates[1]["content"]["text"], "ok");

    // Third process: resume attaches without replay; an unknown id is refused.
    let (utx, mut urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    with_agent(&s, utx, ptx, async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        cx.send_request(ResumeSessionRequest::new(id.clone(), work.clone()))
            .block_task()
            .await?;
        let unknown = cx
            .send_request(ResumeSessionRequest::new(
                SessionId::new("nope"),
                work.clone(),
            ))
            .block_task()
            .await;
        assert!(unknown.is_err());
        Ok(())
    })
    .await
    .unwrap();
    assert!(drain(&mut urx).is_empty());
}

#[tokio::test]
async fn modes_config_and_errors() {
    let s = setup(notes_script());
    let (utx, _urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    with_agent(&s, utx, ptx, async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let new = cx
            .send_request(NewSessionRequest::new(work.clone()))
            .block_task()
            .await?;
        let v = serde_json::to_value(&new).unwrap();
        assert_eq!(v["modes"]["currentModeId"], "ask");
        assert_eq!(v["configOptions"][0]["currentValue"], "small");
        let id = new.session_id;

        cx.send_request(SetSessionModeRequest::new(id.clone(), "edits"))
            .block_task()
            .await?;
        assert!(
            cx.send_request(SetSessionModeRequest::new(id.clone(), "nope"))
                .block_task()
                .await
                .is_err()
        );
        let set = cx
            .send_request(SetSessionConfigOptionRequest::new(
                id.clone(),
                "model",
                "large",
            ))
            .block_task()
            .await?;
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["configOptions"][0]["currentValue"], "large");
        assert!(
            cx.send_request(SetSessionConfigOptionRequest::new(
                id.clone(),
                "model",
                "huge"
            ))
            .block_task()
            .await
            .is_err()
        );
        Ok(())
    })
    .await
    .unwrap();

    let s = setup(json!({"authRequired": true}));
    let (utx, _urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    let err = with_agent(&s, utx, ptx, async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        cx.send_request(NewSessionRequest::new(work.clone()))
            .block_task()
            .await
            .map(|_| ())
    })
    .await
    .unwrap_err();
    assert_eq!(err.code, agent_client_protocol::ErrorCode::AuthRequired);
}

#[tokio::test]
async fn exit_step_crashes_the_process() {
    let s = setup(notes_script());
    let (utx, _urx) = mpsc::unbounded_channel();
    let (ptx, _prx) = mpsc::unbounded_channel();
    let work = cwd(&s);
    let result = with_agent(&s, utx, ptx, async |cx| {
        let id = start(&cx, &work).await;
        cx.send_request(PromptRequest::new(id, text("crash now")))
            .block_task()
            .await
            .map(|_| ())
    })
    .await;
    assert!(result.is_err());
}

use super::*;
use actuo_protocol::events::*;
use actuo_protocol::ids::PermissionOptionId;
use actuo_protocol::types::*;

fn new_session(store: &Store) -> SessionSummary {
    let project = store.open_project("/repo").unwrap();
    store
        .create_session(NewSession {
            id: new_session_id(),
            project_id: project.id,
            agent: "fake".into(),
            agent_version: Some("1.0".into()),
            level: Level::Ask,
            clean: false,
            cwd: "/wt".into(),
            worktree_path: Some("/wt".into()),
            branch: Some("actuo/x".into()),
            base: Some("origin/main".into()),
            sandbox: None,
            web_tools: true,
            telemetry: true,
            config: BTreeMap::from([("model".into(), ConfigValue::String("m1".into()))]),
        })
        .unwrap()
}

fn state(s: SessionState) -> NewEvent {
    NewEvent::new(EventBody::SessionState(SessionStateData {
        state: s,
        reason: None,
    }))
}

fn tool_call() -> ToolCall {
    ToolCall {
        tool_kind: ToolKind::Edit,
        title: "Write NOTES.md".into(),
        status: ToolStatus::Pending,
        raw_input: None,
        raw_output: None,
        content: vec![],
        locations: vec![],
    }
}

fn request_opened(id: &str) -> NewEvent {
    NewEvent::new(EventBody::RequestOpened(RequestOpenedData {
        request_id: RequestId::from(id),
        tool_call: tool_call(),
        options: vec![PermissionOption {
            option_id: PermissionOptionId::from("allow-once"),
            name: "Yes".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
    }))
    .turn(1)
    .item(ItemId::from("i1"))
}

#[test]
fn open_project_is_idempotent() {
    let store = Store::open_in_memory().unwrap();
    let a = store.open_project("/repo").unwrap();
    let b = store.open_project("/repo").unwrap();
    assert_eq!(a, b);
    assert_eq!(store.projects().unwrap(), vec![a.clone()]);
    assert_eq!(store.project(&a.id).unwrap(), a);
    assert!(matches!(
        store.project(&ProjectId::from("nope")),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn append_assigns_seq_stores_then_broadcasts_in_order() {
    let store = Store::open_in_memory().unwrap();
    let s = new_session(&store);
    assert_eq!(s.state, SessionState::Starting);
    let mut rx = store.subscribe();

    let raw = serde_json::json!({"jsonrpc": "2.0", "method": "session/update"});
    store.append(&s.id, state(SessionState::Idle)).unwrap();
    store
        .append(
            &s.id,
            NewEvent::new(EventBody::ContentDelta(ContentDeltaData {
                text: "hi".into(),
            }))
            .turn(1)
            .item(ItemId::from("i1"))
            .raw(raw.clone()),
        )
        .unwrap();

    let first = rx.try_recv().unwrap();
    let second = rx.try_recv().unwrap();
    assert_eq!((first.seq, second.seq), (1, 2));

    let stored = store.events_after(&s.id, 0, 100).unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(&stored[0], first.as_ref());
    assert_eq!(&stored[1], second.as_ref());
    assert_eq!(stored[1].raw.as_ref(), Some(&raw));
    assert_eq!(
        store.events_after(&s.id, 1, 100).unwrap(),
        vec![stored[1].clone()]
    );
    assert_eq!(store.events_after(&s.id, 0, 1).unwrap().len(), 1);

    let s = store.session(&s.id).unwrap();
    assert_eq!(s.head_seq, 2);
    assert_eq!(s.state, SessionState::Idle);
}

#[test]
fn append_to_unknown_session_fails_without_broadcast() {
    let store = Store::open_in_memory().unwrap();
    let mut rx = store.subscribe();
    let err = store
        .append(&SessionId::from("nope"), state(SessionState::Idle))
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn projections_follow_events() {
    let store = Store::open_in_memory().unwrap();
    let s = store_with_turn(&store);

    // Checkpoint 1 then turn 1 completes: files land on checkpoint 1.
    store
        .append(
            &s,
            NewEvent::new(EventBody::CheckpointCreated(CheckpointCreatedData {
                n: 1,
                commit: "c1".into(),
            }))
            .turn(1),
        )
        .unwrap();
    let files = vec![FileChange {
        path: "NOTES.md".into(),
        status: FileStatus::Added,
    }];
    store
        .append(
            &s,
            NewEvent::new(EventBody::TurnCompleted(TurnCompletedData {
                stop_reason: StopReason::EndTurn,
                tokens: Some(TokenUsage {
                    total: Some(10),
                    ..Default::default()
                }),
                cost: Some(Cost {
                    amount: 0.3,
                    currency: "USD".into(),
                }),
                files: files.clone(),
            }))
            .turn(1),
        )
        .unwrap();

    let turns = store.turns(&s).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].text, "do it");
    assert_eq!(turns[0].stop_reason, Some(StopReason::EndTurn));
    assert_eq!(turns[0].tokens.as_ref().unwrap().total, Some(10));
    assert_eq!(turns[0].cost.as_ref().unwrap().amount, 0.3);
    assert_eq!(store.running_turn(&s).unwrap(), None);

    let cps = store.checkpoints(&s).unwrap();
    assert_eq!(cps.iter().map(|c| c.n).collect::<Vec<_>>(), vec![0, 1]);
    assert!(cps[0].files.is_empty());
    assert_eq!(cps[1].files, files);

    // Requests.
    store.append(&s, request_opened("r1")).unwrap();
    store.append(&s, request_opened("r2")).unwrap();
    store
        .append(
            &s,
            NewEvent::new(EventBody::RequestResolved(RequestResolvedData {
                request_id: RequestId::from("r1"),
                outcome: RequestOutcome::Selected,
                option_id: Some(PermissionOptionId::from("allow-once")),
                option_kind: Some(PermissionOptionKind::AllowOnce),
                by: ResolvedBy::Client,
                client: Some("actuo-cli".into()),
            })),
        )
        .unwrap();
    let open = store.open_requests(&s).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].request_id, RequestId::from("r2"));
    assert_eq!(open[0].tool_call, tool_call());

    // Agent, commands, title, end.
    store
        .append(
            &s,
            NewEvent::new(EventBody::SessionAgent(SessionAgentData {
                agent_info: None,
                agent_session_id: "acp-1".into(),
                can_load: true,
                can_resume: false,
                modes: vec![],
                applied_mode: Some("default".into()),
                config_options: vec![],
                applied_config: vec![],
            })),
        )
        .unwrap();
    assert_eq!(
        store.agent_session_id(&s).unwrap().as_deref(),
        Some("acp-1")
    );
    assert_eq!(
        store.agent_can_continue(&s).unwrap(),
        (Some(false), Some(true))
    );
    let commands = vec![AgentCommand {
        name: "compact".into(),
        description: None,
        input_hint: None,
    }];
    store
        .append(
            &s,
            NewEvent::new(EventBody::SessionCommands(SessionCommandsData {
                commands: commands.clone(),
            })),
        )
        .unwrap();
    store
        .append(
            &s,
            NewEvent::new(EventBody::AgentUpdate(AgentUpdateData {
                kind: "session_info_update".into(),
                data: serde_json::json!({"title": "NOTES.md summary"}),
            })),
        )
        .unwrap();
    store
        .append(
            &s,
            NewEvent::new(EventBody::SessionState(SessionStateData {
                state: SessionState::Ended,
                reason: Some("stopped".into()),
            })),
        )
        .unwrap();

    let detail = store.session_detail(&s).unwrap();
    assert_eq!(detail.commands, commands);
    assert_eq!(detail.summary.title.as_deref(), Some("NOTES.md summary"));
    assert_eq!(detail.summary.state, SessionState::Ended);
    assert_eq!(detail.summary.end_reason.as_deref(), Some("stopped"));
    assert_eq!(detail.summary.turn_count, 1);
    assert_eq!(detail.turns.len(), 1);
    assert_eq!(detail.open_requests.len(), 1);
    assert_eq!(
        detail.config.get("model"),
        Some(&ConfigValue::String("m1".into()))
    );
}

/// A session with checkpoint 0 and turn 1 started.
fn store_with_turn(store: &Store) -> SessionId {
    let s = new_session(store).id;
    store.append(&s, state(SessionState::Idle)).unwrap();
    store
        .append(
            &s,
            NewEvent::new(EventBody::CheckpointCreated(CheckpointCreatedData {
                n: 0,
                commit: "c0".into(),
            }))
            .turn(1),
        )
        .unwrap();
    store
        .append(
            &s,
            NewEvent::new(EventBody::TurnStarted(TurnStartedData {
                text: "do it".into(),
                client: None,
            }))
            .turn(1),
        )
        .unwrap();
    store.append(&s, state(SessionState::Running)).unwrap();
    s
}

#[test]
fn state_survives_reopen_for_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actuo.db");
    let (running, idle) = {
        let store = Store::open(&path).unwrap();
        let running = store_with_turn(&store);
        store.append(&running, request_opened("r1")).unwrap();
        store
            .set_agent_process(
                &running,
                Some(&AgentProcess {
                    pid: 4242,
                    started_at: "Wed Sep 30 12:00:00 2026".into(),
                }),
            )
            .unwrap();
        let idle = new_session(&store).id;
        store.append(&idle, state(SessionState::Idle)).unwrap();
        (running, idle)
    };

    // Reopening applies no migration twice and keeps everything.
    let store = Store::open(&path).unwrap();
    let left = store
        .sessions_in_states(&[SessionState::Starting, SessionState::Running])
        .unwrap();
    assert_eq!(
        left.iter().map(|s| &s.id).collect::<Vec<_>>(),
        vec![&running]
    );
    assert_eq!(store.running_turn(&running).unwrap(), Some(1));
    assert_eq!(store.open_requests(&running).unwrap().len(), 1);
    assert_eq!(
        store.agent_process(&running).unwrap().map(|p| p.pid),
        Some(4242)
    );
    store.set_agent_process(&running, None).unwrap();
    assert_eq!(store.agent_process(&running).unwrap(), None);
    assert_eq!(store.session(&idle).unwrap().state, SessionState::Idle);
    assert_eq!(store.sessions(None).unwrap().len(), 2);

    // seq continues from the stored head.
    let head = store.session(&running).unwrap().head_seq;
    let e = store
        .append(&running, state(SessionState::Detached))
        .unwrap();
    assert_eq!(e.seq, head + 1);
}

#[tokio::test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a store write ran on an async thread")]
async fn a_write_on_an_async_thread_panics_in_debug() {
    let store = Store::open_in_memory().unwrap();
    let s = new_session(&store);
    let _ = store.append(&s.id, state(SessionState::Idle));
}

#[tokio::test]
async fn a_write_through_spawn_blocking_is_fine() {
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let s = new_session(&store);
    let st = store.clone();
    tokio::task::spawn_blocking(move || off_async(|| st.append(&s.id, state(SessionState::Idle))))
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn unfinished_tool_calls_of_a_turn() {
    let store = Store::open_in_memory().unwrap();
    let s = store_with_turn(&store);
    let item = |id: &str, status: ToolStatus, body: fn(ItemData) -> EventBody| {
        let mut call = tool_call();
        call.status = status;
        NewEvent::new(body(ItemData {
            item: Item::ToolCall(call),
        }))
        .turn(1)
        .item(ItemId::from(id))
    };
    store
        .append(
            &s,
            item("1.1", ToolStatus::InProgress, EventBody::ItemStarted),
        )
        .unwrap();
    store
        .append(&s, item("1.2", ToolStatus::Pending, EventBody::ItemStarted))
        .unwrap();
    store
        .append(
            &s,
            item("1.2", ToolStatus::Pending, EventBody::ItemCompleted),
        )
        .unwrap();
    store
        .append(&s, item("1.3", ToolStatus::Pending, EventBody::ItemStarted))
        .unwrap();
    store
        .append(
            &s,
            item("1.3", ToolStatus::Completed, EventBody::ItemCompleted),
        )
        .unwrap();
    let open: Vec<_> = store
        .unfinished_tool_calls(&s, 1)
        .unwrap()
        .into_iter()
        .map(|(id, c)| (id.0, c.status))
        .collect();
    assert_eq!(
        open,
        vec![
            ("1.1".to_string(), ToolStatus::InProgress),
            ("1.2".to_string(), ToolStatus::Pending)
        ]
    );
    assert!(store.unfinished_tool_calls(&s, 2).unwrap().is_empty());
}

/// A probe's results, replaced by the next probe of the same agent.
#[test]
fn agent_options_are_kept_per_agent() {
    use crate::store::AgentOptions;
    use actuo_protocol::types::{AgentConfigOption, ConfigOptionType};
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.agent_options("claude").unwrap(), None);
    let option = |id: &str| AgentConfigOption {
        id: id.into(),
        name: id.into(),
        description: None,
        category: None,
        option_type: ConfigOptionType::Select,
        current_value: None,
        values: vec![],
    };
    let first = AgentOptions {
        version: Some("1".into()),
        own: vec![option("model")],
        by_model: [("opus".to_string(), vec![option("model"), option("effort")])].into(),
        probed_at: "t1".into(),
    };
    store.set_agent_options("claude", &first).unwrap();
    assert_eq!(store.agent_options("claude").unwrap(), Some(first.clone()));
    let second = AgentOptions {
        version: Some("2".into()),
        probed_at: "t2".into(),
        ..first
    };
    store.set_agent_options("claude", &second).unwrap();
    assert_eq!(store.agent_options("claude").unwrap(), Some(second));
    assert_eq!(store.agent_options("codex").unwrap(), None);
}

#[test]
fn a_baseline_is_the_newest_finished_check_run_at_its_commit() {
    use actuo_protocol::ids::CheckRunId;
    use actuo_protocol::runs::{CheckRun, CheckRunState};
    let store = Store::open_in_memory().unwrap();
    let check = |id: &str, commit: &str, state: CheckRunState, at: &str| CheckRun {
        id: CheckRunId(id.into()),
        repo: "o/r".into(),
        state,
        commit: Some(commit.into()),
        error: None,
        steps: vec![],
        node_modules_kb: None,
        store_kb_before: None,
        store_kb_after: None,
        started_at: at.into(),
        ended_at: None,
    };
    store
        .save_check_run(&check(
            "old",
            "aaa",
            CheckRunState::Failed,
            "2026-10-10T01:00:00Z",
        ))
        .unwrap();
    store
        .save_check_run(&check(
            "new",
            "aaa",
            CheckRunState::Passed,
            "2026-10-10T02:00:00Z",
        ))
        .unwrap();
    store
        .save_check_run(&check(
            "running",
            "aaa",
            CheckRunState::Running,
            "2026-10-10T03:00:00Z",
        ))
        .unwrap();
    store
        .save_check_run(&check(
            "error",
            "aaa",
            CheckRunState::Error,
            "2026-10-10T04:00:00Z",
        ))
        .unwrap();
    store
        .save_check_run(&check(
            "other",
            "bbb",
            CheckRunState::Failed,
            "2026-10-10T05:00:00Z",
        ))
        .unwrap();
    let at = |c: &str| store.check_run_at("o/r", c).unwrap().map(|r| r.id.0);
    assert_eq!(at("aaa").as_deref(), Some("new"));
    assert_eq!(at("bbb").as_deref(), Some("other"));
    assert_eq!(at("ccc"), None);
    assert_eq!(store.check_run_at("o/x", "aaa").unwrap(), None);
}

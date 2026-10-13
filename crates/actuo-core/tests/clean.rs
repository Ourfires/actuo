//! A clean session: clean.env reaches the agent, clean.dirs are created and
//! clean.files are (re)written before the agent starts; a normal session gets none.

mod common;

use actuo_core::agent::acp::AcpBackend;
use actuo_core::config::{AgentsConfig, Paths};
use actuo_core::session::Host;
use actuo_core::store::Store;
use actuo_protocol::events::EventBody;
use actuo_protocol::ids::{ProjectId, SessionId};
use actuo_protocol::methods::SessionStartParams;
use actuo_protocol::types::SessionState;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

async fn wait_idle(
    store: &Store,
    rx: &mut tokio::sync::broadcast::Receiver<Arc<actuo_protocol::events::Event>>,
    sid: &SessionId,
) {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("events")
            .unwrap();
        if &e.session_id == sid
            && let EventBody::SessionState(d) = &e.body
        {
            assert_ne!(
                d.state,
                SessionState::Ended,
                "{:?}",
                store.events_after(sid, 0, 100)
            );
            if d.state == SessionState::Idle {
                return;
            }
        }
    }
}

fn started_env(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["method"] == "started")
        .map(|v| v["params"]["env"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_session_env_dirs_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = common::git_repo(&root);
    let script = root.join("script.json");
    std::fs::write(
        &script,
        json!({"modes": [{"id": "ask", "name": "Ask"}]}).to_string(),
    )
    .unwrap();
    let home = root.join("clean-home");
    let log = root.join("log.jsonl");
    std::fs::write(
        root.join("agents.toml"),
        format!(
            r#"
            [agents.fake]
            command = "{agent}"
            args = ["--script", "{script}", "--log", "{log}"]
            modes = {{ ask = "ask" }}
            clean.dirs = ["FAKE_HOME"]
            [agents.fake.clean.env]
            FAKE_HOME = "{home}"
            FAKE_AGENT_ECHO = "clean"
            [[agents.fake.clean.files]]
            dir = "FAKE_HOME"
            name = "config.toml"
            content = "apps = false\n"
            "#,
            agent = common::fake_agent().display(),
            script = script.display(),
            log = log.display(),
            home = home.display(),
        ),
    )
    .unwrap();
    let agents = AgentsConfig::load(&root.join("agents.toml")).unwrap();
    let paths = Paths::new(root.join("home"));
    paths.create().unwrap();
    let store = Arc::new(Store::open(&paths.db()).unwrap());
    let host = Host::new(
        store.clone(),
        paths,
        agents,
        Default::default(),
        Arc::new(AcpBackend),
    );
    let project = host.open_project(repo.to_str().unwrap()).await.unwrap();
    let mut rx = store.subscribe();
    let start = |clean: bool| SessionStartParams {
        project_id: ProjectId(project.id.0.clone()),
        agent: Some("fake".into()),
        worktree: true,
        base: None,
        branch: None,
        sandbox: None,
        web_tools: true,
        telemetry: true,
        level: Default::default(),
        clean,
        config: Default::default(),
        prompt: None,
    };

    // A normal session: nothing created, no clean env.
    let normal = host.start_session(start(false), None).await.unwrap();
    wait_idle(&store, &mut rx, &normal).await;
    assert!(!home.exists());
    assert_eq!(started_env(&log), vec![Value::Null]);

    // A clean session: the directory and file exist before the agent starts, and
    // the env reaches it. A file the agent (or anyone) changed is rewritten.
    let clean = host.start_session(start(true), None).await.unwrap();
    wait_idle(&store, &mut rx, &clean).await;
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        "apps = false\n"
    );
    assert_eq!(started_env(&log), vec![Value::Null, json!("clean")]);
    std::fs::write(home.join("config.toml"), "apps = true\n").unwrap();
    let again = host.start_session(start(true), None).await.unwrap();
    wait_idle(&store, &mut rx, &again).await;
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        "apps = false\n"
    );

    for s in [normal, clean, again] {
        host.stop(&s).await.unwrap();
    }
}

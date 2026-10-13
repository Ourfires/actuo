//! The protocol over the Unix socket, with the fake agent behind it.

mod common;

use actuo_core::agent::acp::AcpBackend;
use actuo_core::config::Paths;
use actuo_core::server::serve_unix;
use actuo_core::session::Host;
use actuo_core::store::Store;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

struct Client {
    writer: OwnedWriteHalf,
    lines: Lines<BufReader<OwnedReadHalf>>,
    next_id: i64,
    events: VecDeque<Value>,
    /// How long a read waits for the next message.
    patience: Duration,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let (r, w) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            writer: w,
            lines: BufReader::new(r).lines(),
            next_id: 1,
            events: VecDeque::new(),
            patience: Duration::from_secs(15),
        }
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(self.patience, self.lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("a message within {:?}", self.patience))
            .unwrap()
            .expect("connection open");
        serde_json::from_str(&line).unwrap()
    }

    /// Calls a method; `Err` is the JSON-RPC error object.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_vec(&msg).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
        loop {
            let v = self.read().await;
            if v["method"] == "event" {
                self.events.push_back(v["params"].clone());
            } else if v["id"] == id {
                return match v.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(v["result"].clone()),
                };
            }
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        match self.call(method, params).await {
            Ok(v) => v,
            Err(e) => panic!("{method}: {e}"),
        }
    }

    /// The `data.kind` of the error a call fails with.
    async fn fails(&mut self, method: &str, params: Value) -> String {
        match self.call(method, params).await {
            Ok(v) => panic!("{method} succeeded: {v}"),
            Err(e) => e["data"]["kind"].as_str().unwrap().to_string(),
        }
    }

    /// The next event of type `ty` (earlier ones are skipped).
    async fn event(&mut self, ty: &str) -> Value {
        loop {
            let e = match self.events.pop_front() {
                Some(e) => e,
                None => {
                    let v = self.read().await;
                    if v["method"] != "event" {
                        continue;
                    }
                    v["params"].clone()
                }
            };
            if e["type"] == ty {
                return e;
            }
        }
    }

    /// The next `session.state` event with this state.
    async fn state(&mut self, state: &str) -> Value {
        loop {
            let e = self.event("session.state").await;
            if e["data"]["state"] == state {
                return e;
            }
        }
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
                {"message": "Writing NOTES.md"},
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

struct Actuo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    socket: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    host: Arc<Host>,
}

async fn actuo() -> Actuo {
    actuo_with(&script()).await
}

/// An actuo whose agent `fake` follows `script`.
async fn actuo_with(script: &Value) -> Actuo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = common::git_repo(&root);
    let agents = common::fake_agents(&root, script);
    let paths = Paths::new(root.join("home"));
    paths.create().unwrap();
    let store = Arc::new(Store::open(&paths.db()).unwrap());
    let host = Host::new(
        store,
        paths.clone(),
        agents,
        Default::default(),
        Arc::new(AcpBackend),
    );
    let socket = paths.socket();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let path = socket.clone();
    let served = host.clone();
    let server = tokio::spawn(async move {
        serve_unix(served, &path, async {
            let _ = stopped.await;
        })
        .await
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Actuo {
        _dir: dir,
        root,
        repo,
        socket,
        stop: Some(stop),
        server,
        host,
    }
}

async fn initialized(r: &Actuo) -> Client {
    let mut c = Client::connect(&r.socket).await;
    c.ok(
        "initialize",
        json!({"protocolVersion": 1, "client": {"name": "test", "version": "0"}}),
    )
    .await;
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake() {
    let r = actuo().await;
    let mut c = Client::connect(&r.socket).await;
    assert_eq!(c.fails("agent/list", json!({})).await, "not_initialized");
    let e = c
        .call(
            "initialize",
            json!({"protocolVersion": 2, "client": {"name": "t", "version": "0"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], -32002);
    assert_eq!(e["data"]["kind"], "unsupported_protocol_version");
    assert_eq!(e["data"]["supported"], json!([1]));
    // "lane" is the lane's own name, in any case and with spaces around it.
    for name in ["lane", "Lane", " LANE\t"] {
        let e = c
            .call(
                "initialize",
                json!({"protocolVersion": 1, "client": {"name": name, "version": "0"}}),
            )
            .await
            .unwrap_err();
        assert_eq!(e["data"]["kind"], "invalid_params", "{name:?}");
        assert_eq!(c.fails("agent/list", json!({})).await, "not_initialized");
    }
    let mut lanes = Client::connect(&r.socket).await;
    lanes
        .ok(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": "lanes", "version": "0"}}),
        )
        .await;
    let ok = c
        .ok(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": "t", "version": "0"}}),
        )
        .await;
    assert_eq!(ok["protocolVersion"], 1);
    let agents = c.ok("agent/list", Value::Null).await;
    assert!(
        agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "fake")
    );
    assert_eq!(c.fails("no/such", json!({})).await, "method_not_found");
    assert_eq!(
        c.fails("session/get", json!({"nope": 1})).await,
        "invalid_params"
    );

    // The socket is private, and a second actuo refuses to start on it.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&r.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let host2 = Host::new(
        Arc::new(Store::open_in_memory().unwrap()),
        Paths::new(r.root.join("other")),
        Default::default(),
        Default::default(),
        Arc::new(AcpBackend),
    );
    let err = serve_unix(host2, &r.socket, async {}).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
}

/// A default the agent doesn't offer with the session's model is skipped with a
/// note; the same value asked for by a client fails the session. The model is
/// applied first: the effort level exists only with the large model.
#[tokio::test(flavor = "multi_thread")]
async fn defaults_the_agent_doesnt_offer_are_skipped() {
    let mut s = script();
    s["configOptions"] = json!([
        {"id": "model", "name": "Model", "values": ["small", "large"], "current": "small"},
        {"id": "effort", "name": "Effort", "values": ["low"], "current": "low",
         "valuesByModel": {"large": ["low", "max"]}}
    ]);
    let r = actuo_with(&s).await;
    let mut c = initialized(&r).await;
    c.ok(
        "settings/update",
        json!({"patch": {"defaults": {"fake": {"model": "large", "effort": "max", "fast": "on"}}}}),
    )
    .await;
    let pid = c.ok("project/open", json!({"path": r.repo})).await["project"]["id"].clone();
    let sid = c
        .ok("session/start", json!({"projectId": pid, "agent": "fake"}))
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    let note = c.event("error").await;
    assert_eq!(note["data"]["fatal"], false);
    assert_eq!(
        note["data"]["message"],
        "skipped the default fast = \"on\": the agent doesn't offer it here"
    );
    let agent = c.event("session.agent").await;
    let applied: Vec<&str> = agent["data"]["appliedConfig"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(applied, vec!["model", "effort"]);
    c.state("idle").await;

    // Asked for by a client: the session doesn't start.
    let sid = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "config": {"fast": "on"}}),
        )
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    let ended = c.state("ended").await;
    assert!(
        ended["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("no config option fast"),
        "{ended}"
    );
}

/// `agent/probe` asks the agent model by model: here effort exists only with the
/// large model, with a level more; `agent/list` then offers each model's options.
#[tokio::test(flavor = "multi_thread")]
async fn agent_probe() {
    let mut s = script();
    s["configOptions"] = json!([
        {"id": "model", "name": "Model", "values": ["small", "large"], "current": "small"},
        {"id": "effort", "name": "Effort", "values": ["low", "high"], "current": "low",
         "models": ["large"], "valuesByModel": {"large": ["low", "high", "max"]}}
    ]);
    let r = actuo_with(&s).await;
    let mut c = initialized(&r).await;
    assert_eq!(
        c.fails("agent/probe", json!({"agent": "nope"})).await,
        "agent_not_found"
    );
    let probed = c.ok("agent/probe", json!({"agent": "fake"})).await["agent"].clone();
    let ids = |list: &Value| -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect()
    };
    // Its own model (small): no effort.
    assert_eq!(ids(&probed["configOptions"]), vec!["model"]);
    let by_model = &probed["optionsByModel"];
    assert_eq!(ids(&by_model["small"]), vec!["model"]);
    assert_eq!(ids(&by_model["large"]), vec!["model", "effort"]);
    let levels: Vec<&str> = by_model["large"][1]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["value"].as_str().unwrap())
        .collect();
    assert_eq!(levels, vec!["low", "high", "max"]);
    assert!(by_model["large"][1].get("currentValue").is_none());
    assert!(probed["probedAt"].is_string());
    // agent/list says the same; the probe's folders are gone.
    let list = c.ok("agent/list", json!({})).await;
    let fake = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "fake")
        .unwrap()
        .clone();
    assert_eq!(fake["optionsByModel"], *by_model);
    let tmp = r.root.join("home/tmp");
    let left: Vec<_> = std::fs::read_dir(&tmp)
        .map(|d| d.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

/// `initialize` names the build's commit; `actuo/status` reads the machine and,
/// with no lane, has no poll.
#[tokio::test(flavor = "multi_thread")]
async fn actuo_status() {
    let r = actuo().await;
    let mut c = Client::connect(&r.socket).await;
    let init = c
        .ok(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": "test", "version": "0"}}),
        )
        .await;
    assert!(
        init["actuo"]["commit"]
            .as_str()
            .is_some_and(|s| s.len() >= 7)
    );
    // The test binary runs from cargo's target folder, which this user can write.
    assert!(
        init["actuo"]["binary"]
            .as_str()
            .is_some_and(|b| b.starts_with('/'))
    );
    assert_eq!(init["actuo"]["binaryWritable"], true);
    let s = c.ok("actuo/status", json!({})).await;
    let disk = &s["disk"];
    assert!(disk["total"].as_u64().unwrap() > 0);
    assert!(disk["free"].as_u64().unwrap() <= disk["total"].as_u64().unwrap());
    assert!(s["memory"]["total"].as_u64().unwrap() > 0);
    assert!(s.get("lastPoll").is_none());
    assert!(s["problems"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_over_the_socket() {
    let mut r = actuo().await;
    let mut c = initialized(&r).await;

    assert_eq!(
        c.fails("project/open", json!({"path": r.root.join("home")}))
            .await,
        "not_a_git_repo"
    );
    let project = c.ok("project/open", json!({"path": r.repo})).await["project"].clone();
    let pid = project["id"].clone();
    // Before the agent has run here: no options to offer.
    let fake = |list: &Value| {
        list["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "fake")
            .cloned()
            .unwrap()
    };
    assert_eq!(
        fake(&c.ok("agent/list", json!({})).await)["configOptions"],
        json!([])
    );
    assert_eq!(
        c.fails("session/start", json!({"projectId": pid, "agent": "nope"}))
            .await,
        "agent_not_found"
    );
    assert_eq!(
        c.fails(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "config": {"mode": "x"}})
        )
        .await,
        "invalid_config_option"
    );

    let sid = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "config": {"model": "large"}}),
        )
        .await["sessionId"]
        .clone();
    let sub = c.ok("session/subscribe", json!({"sessionId": sid})).await;
    assert!(sub["subscriptionId"].is_string());
    let agent = c.event("session.agent").await;
    assert_eq!(agent["data"]["appliedMode"], "ask");
    assert_eq!(agent["data"]["appliedConfig"][0]["current"], "large");
    c.state("idle").await;
    // Now agent/list offers what it advertised, without this session's value.
    let options = fake(&c.ok("agent/list", json!({})).await)["configOptions"].clone();
    assert_eq!(options[0]["id"], "model");
    assert_eq!(options[0]["values"].as_array().unwrap().len(), 2);
    assert!(options[0].get("currentValue").is_none());

    // Turn 1: a permission request answered from the client; the file lands in the
    // worktree, not in the project checkout.
    assert_eq!(
        c.ok(
            "session/send",
            json!({"sessionId": sid, "text": "write notes"})
        )
        .await["turn"],
        1
    );
    assert_eq!(
        c.fails("session/send", json!({"sessionId": sid, "text": "again"}))
            .await,
        "turn_in_progress"
    );
    let opened = c.event("request.opened").await;
    let rid = opened["data"]["requestId"].clone();
    assert_eq!(opened["turn"], 1);
    assert_eq!(
        c.fails(
            "session/respond",
            json!({"sessionId": sid, "requestId": rid, "optionId": "maybe"})
        )
        .await,
        "invalid_option"
    );
    assert_eq!(
        c.fails(
            "session/respond",
            json!({"sessionId": sid, "requestId": "9.r9", "optionId": "allow-once"})
        )
        .await,
        "request_not_found"
    );
    assert_eq!(
        c.fails(
            "session/respond",
            json!({"sessionId": sid, "requestId": rid})
        )
        .await,
        "invalid_params"
    );
    c.ok(
        "session/respond",
        json!({"sessionId": sid, "requestId": rid, "optionId": "allow-once"}),
    )
    .await;
    let resolved = c.event("request.resolved").await;
    assert_eq!(resolved["data"]["by"], "client");
    assert_eq!(resolved["data"]["optionKind"], "allow_once");
    let done = c.event("turn.completed").await;
    assert_eq!(done["data"]["stopReason"], "end_turn");
    assert_eq!(
        done["data"]["files"],
        json!([{"path": "NOTES.md", "status": "added"}])
    );
    assert_eq!(done["data"]["cost"]["amount"], 0.25);

    let detail = c.ok("session/get", json!({"sessionId": sid})).await["session"].clone();
    let cwd = PathBuf::from(detail["cwd"].as_str().unwrap());
    assert!(cwd.starts_with(r.root.join("home/worktrees")));
    assert_eq!(
        std::fs::read_to_string(cwd.join("NOTES.md")).unwrap(),
        "notes\n"
    );
    assert!(!r.repo.join("NOTES.md").exists());
    assert_eq!(detail["branch"], format!("actuo/{}", sid.as_str().unwrap()));
    assert_eq!(detail["commands"][0]["name"], "compact");

    // Checkpoints and revert.
    let cps = c.ok("checkpoint/list", json!({"sessionId": sid})).await["checkpoints"].clone();
    assert_eq!(cps.as_array().unwrap().len(), 2);
    assert_eq!(cps[1]["files"][0]["path"], "NOTES.md");
    assert_eq!(
        c.fails("checkpoint/revert", json!({"sessionId": sid, "turn": 5}))
            .await,
        "checkpoint_not_found"
    );
    let reverted = c
        .ok("checkpoint/revert", json!({"sessionId": sid, "turn": 1}))
        .await;
    assert_eq!(reverted["deleted"], json!(["NOTES.md"]));
    assert!(!cwd.join("NOTES.md").exists());
    assert_eq!(c.event("checkpoint.reverted").await["data"]["toN"], 0);

    // The next prompt says so.
    c.ok("session/send", json!({"sessionId": sid, "text": "hello"}))
        .await;
    let started = c.event("turn.started").await;
    assert!(
        started["data"]["text"]
            .as_str()
            .unwrap()
            .starts_with("[Actuo restored the files to how they were before turn 1.")
    );
    assert!(started["data"]["text"].as_str().unwrap().ends_with("hello"));
    c.event("turn.completed").await;

    // Interrupt with a request open.
    assert_eq!(
        c.fails("session/interrupt", json!({"sessionId": sid}))
            .await,
        "no_turn_running"
    );
    c.ok(
        "session/send",
        json!({"sessionId": sid, "text": "long task"}),
    )
    .await;
    c.event("request.opened").await;
    c.ok("session/interrupt", json!({"sessionId": sid})).await;
    let resolved = c.event("request.resolved").await;
    assert_eq!(resolved["data"]["by"], "interrupt");
    assert_eq!(resolved["data"]["outcome"], "cancelled");
    let done = c.event("turn.completed").await;
    assert_eq!(done["data"]["stopReason"], "cancelled");
    assert!(!cwd.join("part1.txt").exists());

    let detail = c.ok("session/get", json!({"sessionId": sid})).await["session"].clone();
    assert_eq!(detail["turnCount"], 3);
    assert_eq!(detail["turns"][2]["stopReason"], "cancelled");
    assert_eq!(detail["openRequests"], json!([]));

    // Stop.
    c.ok("session/stop", json!({"sessionId": sid})).await;
    let ended = c.state("ended").await;
    assert_eq!(ended["data"]["reason"], "stopped");
    assert_eq!(
        c.fails("session/send", json!({"sessionId": sid, "text": "x"}))
            .await,
        "session_ended"
    );
    assert_eq!(
        c.fails("session/get", json!({"sessionId": "nope"})).await,
        "session_not_found"
    );

    // A second client replays the whole history in order; raw only on request.
    let mut c2 = initialized(&r).await;
    let head = c2.ok("session/subscribe", json!({"sessionId": sid})).await["headSeq"]
        .as_u64()
        .unwrap();
    let mut seqs = Vec::new();
    while seqs.len() < head as usize {
        let v = c2.read().await;
        assert!(v["params"].get("raw").is_none());
        seqs.push(v["params"]["seq"].as_u64().unwrap());
    }
    assert_eq!(seqs, (1..=head).collect::<Vec<_>>());
    c2.ok(
        "session/subscribe",
        json!({"sessionId": sid, "fromSeq": 0, "includeRaw": true}),
    )
    .await;
    let delta = c2.event("content.delta").await;
    assert_eq!(delta["raw"]["method"], "session/update");

    // A crash: the running turn ends agent_exited, the session is detached because
    // the agent can resume (continuing it comes with reattach).
    let crash = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "prompt": "crash now"}),
        )
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": crash})).await;
    // No prompt error: the exit ends the turn.
    let err = c.event("error").await;
    assert_eq!(err["data"]["kind"], "agent_exited");
    assert_eq!(err["data"]["fatal"], true);
    assert!(err["data"]["message"].as_str().unwrap().contains("code 3"));
    assert_eq!(
        c.event("turn.completed").await["data"]["stopReason"],
        "agent_exited"
    );
    // Straight to detached, no idle in between.
    assert_eq!(c.event("session.state").await["data"]["state"], "detached");

    // Sending to the detached session reattaches it (resume) and runs the turn.
    assert_eq!(
        c.ok("session/send", json!({"sessionId": crash, "text": "hello"}))
            .await["turn"],
        2
    );
    assert_eq!(
        c.state("idle").await["data"]["reason"],
        "reattached with session/resume"
    );
    assert_eq!(
        c.event("turn.completed").await["data"]["stopReason"],
        "end_turn"
    );

    // A reattach that fails (the agent lost its sessions) leaves it detached.
    c.ok("session/stop", json!({"sessionId": crash})).await;
    let crash2 = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "prompt": "crash again"}),
        )
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": crash2}))
        .await;
    c.state("detached").await;
    std::fs::remove_dir_all(r.root.join("agent-state")).unwrap();
    assert_eq!(
        c.fails(
            "session/send",
            json!({"sessionId": crash2, "text": "hello"})
        )
        .await,
        "agent_error"
    );
    let err = c.event("error").await;
    assert_eq!(err["data"]["fatal"], false);
    assert_eq!(c.event("session.state").await["data"]["state"], "detached");

    let sessions = c.ok("session/list", json!({})).await["sessions"].clone();
    assert_eq!(sessions.as_array().unwrap().len(), 3);

    let _ = r.stop.take().unwrap().send(());
    r.server.await.unwrap().unwrap();
    assert!(!r.socket.exists());
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `base` and `branch` on `session/start`, reusing a branch's worktree, and the
/// client name on `turn.started` and `request.resolved`.
#[tokio::test(flavor = "multi_thread")]
async fn branches_and_client_names() {
    let r = actuo().await;
    let mut c = initialized(&r).await;
    let mut other = Client::connect(&r.socket).await;
    other
        .ok(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": "other", "version": "0"}}),
        )
        .await;
    let first = git(&r.repo, &["rev-parse", "HEAD"]);
    std::fs::write(r.repo.join("README.md"), "second\n").unwrap();
    git(&r.repo, &["commit", "-q", "-am", "second"]);
    let project = c
        .ok("project/open", json!({"path": r.repo.to_str().unwrap()}))
        .await["project"]["id"]
        .clone();

    // A branch from a base that isn't HEAD; the first prompt names its client.
    let s1 = c
        .ok(
            "session/start",
            json!({"projectId": project, "agent": "fake", "base": first,
                   "branch": "actuo/issue-1", "prompt": "hello"}),
        )
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": s1})).await;
    let started = c.event("turn.started").await;
    assert_eq!(started["data"]["client"], "test");
    c.event("turn.completed").await;
    let detail = c.ok("session/get", json!({"sessionId": s1})).await["session"].clone();
    assert_eq!(detail["branch"], "actuo/issue-1");
    assert_eq!(detail["base"], first.as_str());
    let cwd = PathBuf::from(detail["cwd"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(cwd.join("README.md")).unwrap(),
        "readme\n"
    );

    // Another client prompts and answers: both carry its name.
    other
        .ok("session/send", json!({"sessionId": s1, "text": "notes"}))
        .await;
    let started = c.event("turn.started").await;
    assert_eq!(started["data"]["client"], "other");
    let opened = c.event("request.opened").await;
    other
        .ok(
            "session/respond",
            json!({"sessionId": s1, "requestId": opened["data"]["requestId"], "optionId": "allow-once"}),
        )
        .await;
    let resolved = c.event("request.resolved").await;
    assert_eq!(resolved["data"]["by"], "client");
    assert_eq!(resolved["data"]["client"], "other");
    c.event("turn.completed").await;

    // The branch's worktree is in use while the session hasn't ended.
    let start = |branch: &str| json!({"projectId": project, "agent": "fake", "branch": branch});
    assert_eq!(
        c.fails("session/start", start("actuo/issue-1")).await,
        "invalid_params"
    );
    c.ok("session/stop", json!({"sessionId": s1})).await;
    c.state("ended").await;

    // After it ended, a new session works in the same worktree, on the same branch.
    let s2 = c.ok("session/start", start("actuo/issue-1")).await["sessionId"].clone();
    let detail = c.ok("session/get", json!({"sessionId": s2})).await["session"].clone();
    assert_eq!(
        PathBuf::from(detail["cwd"].as_str().unwrap()),
        cwd.canonicalize().unwrap()
    );
    assert!(cwd.join("NOTES.md").exists());

    // A branch that exists without a worktree gets one, as it is (base unused).
    git(&r.repo, &["branch", "kept", &first]);
    let s3 = c
        .ok(
            "session/start",
            json!({"projectId": project, "agent": "fake", "branch": "kept", "base": "HEAD"}),
        )
        .await["sessionId"]
        .clone();
    let detail = c.ok("session/get", json!({"sessionId": s3})).await["session"].clone();
    let kept = PathBuf::from(detail["cwd"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(kept.join("README.md")).unwrap(),
        "readme\n"
    );

    assert_eq!(
        c.fails("session/start", start("main")).await,
        "invalid_params"
    );
    assert_eq!(
        c.fails("session/start", start("a..b")).await,
        "invalid_params"
    );
    assert_eq!(
        c.fails(
            "session/start",
            json!({"projectId": project, "agent": "fake", "worktree": false, "branch": "x"})
        )
        .await,
        "invalid_params"
    );
    assert_eq!(
        c.fails(
            "session/start",
            json!({"projectId": project, "agent": "fake", "base": "no-such-ref", "branch": "y"})
        )
        .await,
        "git_error"
    );
}

/// `sessions.idle_agent_timeout`: an idle agent is stopped (the session goes
/// `detached`, reason `idle`) and resumed on the next prompt; an agent with an open
/// request isn't stopped; a new timeout applies at once.
#[tokio::test(flavor = "multi_thread")]
async fn idle_agents_are_stopped() {
    let r = actuo().await;
    let mut c = initialized(&r).await;
    c.ok(
        "settings/update",
        json!({"patch": {"sessions": {"idle_agent_timeout": "1s"}}}),
    )
    .await;
    let project = c
        .ok("project/open", json!({"path": r.repo.to_str().unwrap()}))
        .await["project"]["id"]
        .clone();
    let sid = c
        .ok(
            "session/start",
            json!({"projectId": project, "agent": "fake"}),
        )
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    c.state("idle").await;
    let idle_at = std::time::Instant::now();
    let detached = c.state("detached").await;
    assert_eq!(detached["data"]["reason"], "idle");
    assert!(idle_at.elapsed() >= Duration::from_millis(900));
    let pids = || {
        std::fs::read_to_string(r.root.join("agent-log.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("\"started\""))
            .count()
    };
    let started = pids();

    // The next prompt reattaches (session/resume) and runs.
    c.ok("session/send", json!({"sessionId": sid, "text": "hello"}))
        .await;
    let starting = c.state("starting").await;
    assert_eq!(starting["data"]["reason"], "reattaching");
    c.event("turn.completed").await;
    assert!(pids() > started, "a new agent process");

    // An open request keeps the agent: well past the timeout, still running.
    c.ok("session/send", json!({"sessionId": sid, "text": "notes"}))
        .await;
    let opened = c.event("request.opened").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let detail = c.ok("session/get", json!({"sessionId": sid})).await["session"].clone();
    assert_eq!(detail["state"], "running");
    c.ok(
        "session/respond",
        json!({"sessionId": sid, "requestId": opened["data"]["requestId"], "optionId": "allow-once"}),
    )
    .await;
    c.event("turn.completed").await;
    assert_eq!(c.state("detached").await["data"]["reason"], "idle");

    // `off` applies at once, to a session that is already waiting.
    c.ok("session/send", json!({"sessionId": sid, "text": "hello"}))
        .await;
    c.event("turn.completed").await;
    c.ok(
        "settings/update",
        json!({"patch": {"sessions": {"idle_agent_timeout": "off"}}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let detail = c.ok("session/get", json!({"sessionId": sid})).await["session"].clone();
    assert_eq!(detail["state"], "idle");
}

/// `session/diff`: against the first checkpoint for a session started without a
/// `base`, against the merge base with one; untracked files listed, srt's
/// placeholders left out; allowed while a turn runs with a request open.
#[tokio::test(flavor = "multi_thread")]
async fn session_diff() {
    let r = actuo().await;
    let mut c = initialized(&r).await;
    let pid = c.ok("project/open", json!({"path": r.repo})).await["project"]["id"].clone();
    assert_eq!(
        c.fails("session/diff", json!({"sessionId": "nope"})).await,
        "session_not_found"
    );

    let sid = c
        .ok("session/start", json!({"projectId": pid, "agent": "fake"}))
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    c.state("idle").await;
    let start =
        c.ok("checkpoint/list", json!({"sessionId": sid})).await["checkpoints"][0]["commit"]
            .clone();
    let worktree = PathBuf::from(
        c.ok("session/get", json!({"sessionId": sid})).await["session"]["worktreePath"]
            .as_str()
            .unwrap(),
    );
    let empty = c.ok("session/diff", json!({"sessionId": sid})).await;
    assert_eq!(empty["base"], start);
    assert_eq!(empty["files"], json!([]));
    assert_eq!(empty["patch"], "");
    assert_eq!(empty["truncated"], false);

    std::fs::write(worktree.join("new.txt"), "new\n").unwrap();
    std::fs::write(worktree.join(".bashrc"), "").unwrap();
    c.ok(
        "session/send",
        json!({"sessionId": sid, "text": "write notes"}),
    )
    .await;
    c.event("request.opened").await;
    let d = c.ok("session/diff", json!({"sessionId": sid})).await;
    assert_eq!(
        d["files"],
        json!([{"path": "new.txt", "status": "added", "additions": 1, "deletions": 0, "binary": false}])
    );
    assert!(d["patch"].as_str().unwrap().contains("+++ b/new.txt"));

    // With a base: the merge base of it and the worktree's HEAD.
    let main = git(&r.repo, &["rev-parse", "main"]);
    let based = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "base": "main", "branch": "diff-base"}),
        )
        .await["sessionId"]
        .clone();
    let d = c.ok("session/diff", json!({"sessionId": based})).await;
    assert_eq!(d["base"], main);

    // In the project checkout, with no base: nothing to diff against.
    let checkout = c
        .ok(
            "session/start",
            json!({"projectId": pid, "agent": "fake", "worktree": false}),
        )
        .await["sessionId"]
        .clone();
    assert_eq!(
        c.fails("session/diff", json!({"sessionId": checkout}))
            .await,
        "git_error"
    );
}

/// `session/send` with `interrupt`: a running turn with an open request is
/// interrupted (the request resolved `by: interrupt`), and the prompt starts the
/// next turn once it has ended, in one call. Without a running turn it just sends.
#[tokio::test(flavor = "multi_thread")]
async fn send_with_interrupt() {
    let r = actuo().await;
    let mut c = initialized(&r).await;
    let pid = c.ok("project/open", json!({"path": r.repo})).await["project"]["id"].clone();
    let sid = c
        .ok("session/start", json!({"projectId": pid, "agent": "fake"}))
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    c.state("idle").await;

    c.ok(
        "session/send",
        json!({"sessionId": sid, "text": "write notes"}),
    )
    .await;
    let opened = c.event("request.opened").await;
    let turn = c
        .ok(
            "session/send",
            json!({"sessionId": sid, "text": "do it differently", "interrupt": true}),
        )
        .await["turn"]
        .clone();
    assert_eq!(turn, 2);
    let resolved = c.event("request.resolved").await;
    assert_eq!(resolved["data"]["requestId"], opened["data"]["requestId"]);
    assert_eq!(resolved["data"]["by"], "interrupt");
    let ended = c.event("turn.completed").await;
    assert_eq!(
        (ended["turn"].clone(), ended["data"]["stopReason"].clone()),
        (json!(1), json!("cancelled"))
    );
    let started = c.event("turn.started").await;
    assert_eq!(started["turn"], 2);
    assert_eq!(started["data"]["text"], "do it differently");
    assert_eq!(started["data"]["client"], "test");
    assert_eq!(c.event("turn.completed").await["turn"], 2);

    // Nothing running: the flag changes nothing.
    assert_eq!(
        c.ok(
            "session/send",
            json!({"sessionId": sid, "text": "hello", "interrupt": true})
        )
        .await["turn"],
        3
    );
}

/// A script whose agent ends a cancelled turn `delay_ms` after the cancel.
fn slow_to_cancel(delay_ms: u64) -> Value {
    let mut s = script();
    s["cancelDelayMs"] = json!(delay_ms);
    s
}

/// A started session on the slow agent, idle, subscribed, with turn 1 running and
/// its request open.
async fn waiting_on_a_request(r: &Actuo, c: &mut Client) -> Value {
    let pid = c.ok("project/open", json!({"path": r.repo})).await["project"]["id"].clone();
    let sid = c
        .ok("session/start", json!({"projectId": pid, "agent": "fake"}))
        .await["sessionId"]
        .clone();
    c.ok("session/subscribe", json!({"sessionId": sid})).await;
    c.state("idle").await;
    c.ok(
        "session/send",
        json!({"sessionId": sid, "text": "write notes"}),
    )
    .await;
    c.event("request.opened").await;
    sid
}

/// `session/send` with `interrupt` when the interrupted turn doesn't end within
/// the grace (30 s; 2 s here): `turn_in_progress`, and nothing is sent, not even
/// once the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn send_with_interrupt_gives_up_after_the_grace() {
    let r = actuo_with(&slow_to_cancel(3_000)).await;
    r.host.set_interrupt_grace(Duration::from_secs(2));
    let mut c = initialized(&r).await;
    let sid = waiting_on_a_request(&r, &mut c).await;

    c.patience = Duration::from_secs(10);
    let asked = std::time::Instant::now();
    assert_eq!(
        c.fails(
            "session/send",
            json!({"sessionId": sid, "text": "never sent", "interrupt": true})
        )
        .await,
        "turn_in_progress"
    );
    let waited = asked.elapsed();
    assert!(
        waited >= Duration::from_millis(1_900) && waited < Duration::from_millis(2_900),
        "{waited:?}"
    );

    // The turn ends later (cancelled); the refused prompt never starts a turn.
    let ended = c.event("turn.completed").await;
    assert_eq!(ended["turn"], 1);
    assert_eq!(ended["data"]["stopReason"], "cancelled");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let detail = c.ok("session/get", json!({"sessionId": sid})).await["session"].clone();
    assert_eq!(detail["turns"].as_array().unwrap().len(), 1, "{detail}");
    let prompts: Vec<Value> = std::fs::read_to_string(r.root.join("agent-log.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|m| m["method"] == "session/prompt")
        .collect();
    assert_eq!(prompts.len(), 1, "{prompts:#?}");
}

/// A second `session/send` with `interrupt` while one waits for the turn to end:
/// `turn_in_progress` at once; the first one is sent when the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_interrupt_send_while_one_waits() {
    let r = actuo_with(&slow_to_cancel(2_000)).await;
    let mut c = initialized(&r).await;
    let sid = waiting_on_a_request(&r, &mut c).await;

    let mut first = initialized(&r).await;
    let first_sid = sid.clone();
    let first_send = tokio::spawn(async move {
        first
            .ok(
                "session/send",
                json!({"sessionId": first_sid, "text": "the first", "interrupt": true}),
            )
            .await
    });
    // The first is waiting once its interrupt has cancelled the request.
    assert_eq!(c.event("request.resolved").await["data"]["by"], "interrupt");
    let asked = std::time::Instant::now();
    assert_eq!(
        c.fails(
            "session/send",
            json!({"sessionId": sid, "text": "the second", "interrupt": true})
        )
        .await,
        "turn_in_progress"
    );
    assert!(asked.elapsed() < Duration::from_secs(1));

    assert_eq!(first_send.await.unwrap()["turn"], 2);
    let started = c.event("turn.started").await;
    assert_eq!(started["turn"], 2);
    assert_eq!(started["data"]["text"], "the first");
}

/// A client that half-closes its side (as `actuo stdio` does when its stdin ends)
/// still gets the answers to every call it sent, then the connection closes.
#[tokio::test(flavor = "multi_thread")]
async fn answers_after_the_client_half_closes() {
    let r = actuo_with(&slow_to_cancel(2_000)).await;
    let mut c = initialized(&r).await;
    let sid = waiting_on_a_request(&r, &mut c).await;

    let stream = UnixStream::connect(&r.socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let calls = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": 1, "client": {"name": "t", "version": "0"}}}),
        // Answered only once the interrupted turn ends, about 2 s later.
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/send",
               "params": {"sessionId": sid, "text": "after the close", "interrupt": true}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/subscribe",
               "params": {"sessionId": sid}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "agent/list", "params": {}}),
    ];
    for call in &calls {
        write
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
    }
    write.shutdown().await.unwrap();

    let mut lines = BufReader::new(read).lines();
    let mut answered = std::collections::BTreeMap::new();
    let until_closed = async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let m: Value = serde_json::from_str(&line).unwrap();
            if let Some(id) = m["id"].as_i64() {
                answered.insert(id, m);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), until_closed)
        .await
        .expect("actuo closes the connection after answering");
    assert_eq!(answered.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert_eq!(answered[&2]["result"]["turn"], 2, "{}", answered[&2]);
    assert!(answered[&3]["result"]["subscriptionId"].is_string());
    assert!(answered[&4]["result"]["agents"].is_array());
}

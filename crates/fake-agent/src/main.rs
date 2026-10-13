//! A scripted ACP v1 agent for actuo's tests. It follows a JSON script (see
//! `script.rs`): message and thought chunks, tool calls with diffs, permission
//! requests, files written straight to disk, waiting to be cancelled, ending a
//! cancelled turn late (`cancelDelayMs`), crashing, and load or resume after a
//! restart (with `--state`).
//!
//! Started from the ACP Rust SDK's `examples/simple_agent.rs` (Apache-2.0); see
//! NOTICE. Messages are built from JSON so they match the wire format.
//!
//! Usage: `fake-agent --script <file> [--state <dir>] [--log <file>]`

mod script;

use agent_client_protocol::schema::v1::{
    CancelNotification, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, ResumeSessionRequest,
    ResumeSessionResponse, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Error, Stdio};
use clap::Parser;
use script::{Script, Step};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Parser)]
struct Args {
    /// The JSON script to follow.
    #[arg(long)]
    script: PathBuf,
    /// Directory where sessions are kept, so load and resume work after a restart.
    #[arg(long)]
    state: Option<PathBuf>,
    /// Appends every message received, one JSON object per line.
    #[arg(long)]
    log: Option<PathBuf>,
}

/// A session as kept in `--state`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    cwd: PathBuf,
    /// `(role, text)`: `user` prompts and `agent` messages, for `session/load` replay.
    history: Vec<(String, String)>,
}

struct Live {
    cwd: PathBuf,
    mode: Option<String>,
    config: BTreeMap<String, String>,
    cancelled: Arc<AtomicBool>,
    cancel: Arc<Notify>,
}

struct FakeAgent {
    script: Script,
    state: Option<PathBuf>,
    log: Option<Mutex<std::fs::File>>,
    live: Mutex<HashMap<String, Live>>,
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let text = std::fs::read_to_string(&args.script).map_err(Error::into_internal_error)?;
    let script: Script = serde_json::from_str(&text).map_err(Error::into_internal_error)?;
    let log = match &args.log {
        Some(p) => Some(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(Error::into_internal_error)?,
        )),
        None => None,
    };
    let agent = Arc::new(FakeAgent {
        script,
        state: args.state,
        log,
        live: Mutex::new(HashMap::new()),
    });

    agent.record(
        "started",
        &json!({"pid": std::process::id(), "env": std::env::var("FAKE_AGENT_ECHO").ok()}),
    );
    let a = agent.clone();
    let b = agent.clone();
    let c = agent.clone();
    let d = agent.clone();
    let e = agent.clone();
    let f = agent.clone();
    let g = agent.clone();
    let h = agent.clone();
    Agent
        .builder()
        .name("fake-agent")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                a.record("initialize", &req);
                responder.respond(a.initialize(&req)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx| {
                b.record("session/new", &req);
                if b.script.auth_required {
                    return responder.respond_with_error(Error::auth_required());
                }
                let id = format!("fake-{}", nanos());
                b.save(
                    &id,
                    &Stored {
                        cwd: req.cwd.clone(),
                        history: vec![],
                    },
                )?;
                b.go_live(&id, req.cwd.clone());
                responder.respond(b.session_setup::<NewSessionResponse>(Some(&id))?)?;
                b.send_commands(&cx, &id, &b.script.commands)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx| {
                c.record("session/load", &req);
                let id = req.session_id.0.to_string();
                let Some(stored) = c.stored(&id).filter(|_| c.script.load_session) else {
                    return responder
                        .respond_with_error(Error::resource_not_found(Some(id.clone())));
                };
                // Replay the history before answering, as ACP's session/load does.
                for (role, text) in &stored.history {
                    let kind = if role == "user" {
                        "user_message_chunk"
                    } else {
                        "agent_message_chunk"
                    };
                    send_update(
                        &cx,
                        &id,
                        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
                    )?;
                }
                c.go_live(&id, req.cwd.clone());
                responder.respond(c.session_setup::<LoadSessionResponse>(None)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest, responder, _cx| {
                d.record("session/resume", &req);
                let id = req.session_id.0.to_string();
                if !d.script.resume || d.stored(&id).is_none() {
                    return responder.respond_with_error(Error::resource_not_found(Some(id)));
                }
                d.go_live(&id, req.cwd.clone());
                responder.respond(d.session_setup::<ResumeSessionResponse>(None)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionModeRequest, responder, _cx| {
                e.record("session/set_mode", &req);
                let mode = req.mode_id.0.to_string();
                if !e.script.modes.iter().any(|m| m.id == mode) {
                    return responder.respond_with_error(invalid(format!("unknown mode {mode}")));
                }
                match e.with_live(&req.session_id.0, |s| s.mode = Some(mode)) {
                    Ok(()) => responder.respond(SetSessionModeResponse::new()),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                f.record("session/set_config_option", &req);
                let raw = serde_json::to_value(&req).map_err(Error::into_internal_error)?;
                let id = raw["configId"].as_str().unwrap_or_default().to_string();
                let value = raw["value"].as_str().unwrap_or_default().to_string();
                let session = req.session_id.0.to_string();
                let known = f.config_options(&session).iter().any(|o| {
                    o["id"] == id.as_str()
                        && o["options"]
                            .as_array()
                            .is_some_and(|c| c.iter().any(|x| x["value"] == value.as_str()))
                });
                if !known {
                    return responder
                        .respond_with_error(invalid(format!("unknown option {id}={value}")));
                }
                if let Err(err) = f.with_live(&session, |s| {
                    s.config.insert(id, value);
                }) {
                    return responder.respond_with_error(err);
                }
                let options = f.config_options(&session);
                responder.respond(from_json::<SetSessionConfigOptionResponse>(
                    json!({"configOptions": options}),
                )?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                g.record("session/prompt", &req);
                let agent = g.clone();
                cx.clone().spawn(async move {
                    let reply = agent.run_prompt(&cx, &req).await;
                    match reply {
                        Ok(stop) => responder
                            .respond(from_json::<PromptResponse>(json!({"stopReason": stop}))?),
                        Err(err) => responder.respond_with_error(err),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                h.record("session/cancel", &n);
                if let Some(s) = h.live.lock().unwrap().get(&*n.session_id.0) {
                    s.cancelled.store(true, Ordering::SeqCst);
                    s.cancel.notify_waiters();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

enum Flow {
    Continue,
    Cancelled,
}

impl FakeAgent {
    fn record(&self, method: &str, params: &impl Serialize) {
        if let Some(log) = &self.log {
            // One write per line: agents of parallel sessions share the file.
            let line = format!("{}\n", json!({"method": method, "params": params}));
            let mut f = log.lock().unwrap();
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    fn initialize(&self, req: &InitializeRequest) -> Result<InitializeResponse> {
        let mut session_caps = json!({});
        if self.script.resume {
            session_caps["resume"] = json!({});
        }
        from_json(json!({
            "protocolVersion": req.protocol_version,
            "agentCapabilities": {
                "loadSession": self.script.load_session,
                "sessionCapabilities": session_caps,
            },
            "agentInfo": {"name": "fake-agent", "version": env!("CARGO_PKG_VERSION")},
            "authMethods": [],
        }))
    }

    /// The modes and config options part of a new, load or resume response.
    fn session_setup<T: DeserializeOwned>(&self, id: Option<&str>) -> Result<T> {
        let mut v = json!({});
        if let Some(id) = id {
            v["sessionId"] = json!(id);
        }
        if let Some(first) = self.script.modes.first() {
            v["modes"] = json!({
                "currentModeId": first.id,
                "availableModes": self.script.modes.iter()
                    .map(|m| json!({"id": m.id, "name": m.name}))
                    .collect::<Vec<_>>(),
            });
        }
        if !self.script.config_options.is_empty() {
            v["configOptions"] = json!(self.config_options(""));
        }
        from_json(v)
    }

    fn config_options(&self, session: &str) -> Vec<Value> {
        let live = self.live.lock().unwrap();
        let chosen = live.get(session).map(|s| &s.config);
        let current_of = |o: &script::ConfigOption| {
            chosen
                .and_then(|c| c.get(&o.id))
                .unwrap_or(&o.current)
                .clone()
        };
        let model = self
            .script
            .config_options
            .iter()
            .find(|o| o.id == "model")
            .map(current_of);
        self.script
            .config_options
            .iter()
            .filter(|o| match (&o.models, &model) {
                (Some(models), Some(m)) => models.contains(m),
                _ => true,
            })
            .map(|o| {
                let values = model
                    .as_ref()
                    .and_then(|m| o.values_by_model.get(m))
                    .unwrap_or(&o.values);
                let mut v = json!({
                    "id": o.id, "name": o.name, "type": "select", "currentValue": current_of(o),
                    "options": values.iter().map(|x| json!({"value": x, "name": x})).collect::<Vec<_>>(),
                });
                if let Some(c) = &o.category {
                    v["category"] = json!(c);
                }
                v
            })
            .collect()
    }

    fn send_commands(
        &self,
        cx: &ConnectionTo<Client>,
        id: &str,
        commands: &[script::Command],
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        send_update(
            cx,
            id,
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": commands.iter()
                    .map(|c| json!({"name": c.name, "description": c.description}))
                    .collect::<Vec<_>>(),
            }),
        )
    }

    fn go_live(&self, id: &str, cwd: PathBuf) {
        self.live.lock().unwrap().insert(
            id.to_string(),
            Live {
                cwd,
                mode: None,
                config: BTreeMap::new(),
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(Notify::new()),
            },
        );
    }

    fn with_live<R>(&self, id: &str, f: impl FnOnce(&mut Live) -> R) -> Result<R> {
        let mut live = self.live.lock().unwrap();
        let s = live
            .get_mut(id)
            .ok_or_else(|| Error::resource_not_found(Some(id.to_string())))?;
        Ok(f(s))
    }

    fn stored(&self, id: &str) -> Option<Stored> {
        let dir = self.state.as_ref()?;
        let all: HashMap<String, Stored> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("sessions.json")).ok()?).ok()?;
        all.get(id).cloned()
    }

    fn save(&self, id: &str, stored: &Stored) -> Result<()> {
        let Some(dir) = &self.state else {
            return Ok(());
        };
        std::fs::create_dir_all(dir).map_err(Error::into_internal_error)?;
        let path = dir.join("sessions.json");
        let mut all: HashMap<String, Stored> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        all.insert(id.to_string(), stored.clone());
        std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap())
            .map_err(Error::into_internal_error)
    }

    async fn run_prompt(&self, cx: &ConnectionTo<Client>, req: &PromptRequest) -> Result<String> {
        let id = req.session_id.0.to_string();
        let (cwd, cancelled, cancel) = self.with_live(&id, |s| {
            s.cancelled.store(false, Ordering::SeqCst);
            (s.cwd.clone(), s.cancelled.clone(), s.cancel.clone())
        })?;
        let prompt: String = serde_json::to_value(&req.prompt)
            .map_err(Error::into_internal_error)?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["text"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
        let turn = self.script.turn_for(&prompt, &cwd);
        let ctx = TurnCtx {
            cx,
            id: &id,
            cwd: &cwd,
            cancelled: &cancelled,
            cancel: &cancel,
            said: Mutex::new(String::new()),
        };
        let flow = self.run_steps(&ctx, &turn.steps).await?;

        let mut stored = self.stored(&id).unwrap_or(Stored {
            cwd: cwd.clone(),
            history: vec![],
        });
        stored.history.push(("user".into(), prompt));
        let said = ctx.said.into_inner().unwrap();
        if !said.is_empty() {
            stored.history.push(("agent".into(), said));
        }
        self.save(&id, &stored)?;

        let was_cancelled = matches!(flow, Flow::Cancelled) || cancelled.load(Ordering::SeqCst);
        if was_cancelled && let Some(ms) = self.script.cancel_delay_ms {
            self.record("cancel_delay", &json!({"ms": ms}));
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        Ok(if was_cancelled {
            "cancelled".into()
        } else {
            turn.stop_reason.unwrap_or_else(|| "end_turn".into())
        })
    }

    fn run_steps<'a>(
        &'a self,
        ctx: &'a TurnCtx<'a>,
        steps: &'a [Step],
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Flow>> + Send + 'a>> {
        Box::pin(async move {
            for step in steps {
                if ctx.cancelled.load(Ordering::SeqCst) {
                    return Ok(Flow::Cancelled);
                }
                if let Flow::Cancelled = self.run_step(ctx, step).await? {
                    return Ok(Flow::Cancelled);
                }
            }
            Ok(Flow::Continue)
        })
    }

    async fn run_step(&self, ctx: &TurnCtx<'_>, step: &Step) -> Result<Flow> {
        let (cx, id) = (ctx.cx, ctx.id);
        match step {
            Step::Message(text) => {
                ctx.said.lock().unwrap().push_str(text);
                send_update(cx, id, chunk("agent_message_chunk", text))?;
            }
            Step::Thought(text) => send_update(cx, id, chunk("agent_thought_chunk", text))?,
            Step::Plan(entries) => send_update(
                cx,
                id,
                json!({"sessionUpdate": "plan", "entries": entries.iter()
                    .map(|e| json!({"content": e.content, "status": e.status, "priority": "medium"}))
                    .collect::<Vec<_>>()}),
            )?,
            Step::ToolCall(t) => {
                let mut v = json!({
                    "sessionUpdate": "tool_call", "toolCallId": t.id, "title": t.title,
                    "kind": t.kind, "status": t.status.as_deref().unwrap_or("pending"),
                });
                if let Some(input) = &t.raw_input {
                    v["rawInput"] = input.clone();
                }
                if let Some(d) = &t.diff {
                    let path = ctx.cwd.join(&d.path);
                    v["content"] = json!([{"type": "diff", "path": path,
                        "oldText": d.old_text, "newText": d.new_text}]);
                    v["locations"] = json!([{"path": path}]);
                }
                send_update(cx, id, v)?;
            }
            Step::ToolUpdate(t) => {
                let mut v = json!({"sessionUpdate": "tool_call_update", "toolCallId": t.id});
                if let Some(s) = &t.status {
                    v["status"] = json!(s);
                }
                if let Some(title) = &t.title {
                    v["title"] = json!(title);
                }
                send_update(cx, id, v)?;
            }
            Step::Permission(p) => {
                let request: RequestPermissionRequest = from_json(json!({
                    "sessionId": id,
                    "toolCall": {"toolCallId": p.tool_call_id},
                    "options": p.options.iter()
                        .map(|o| json!({"optionId": o.id, "name": o.name, "kind": o.kind}))
                        .collect::<Vec<_>>(),
                }))?;
                let response = cx.send_request(request).block_task().await?;
                let chosen = match response.outcome {
                    RequestPermissionOutcome::Selected(s) => s.option_id.0.to_string(),
                    _ => return Ok(Flow::Cancelled),
                };
                let allowed = p
                    .options
                    .iter()
                    .find(|o| o.id == chosen)
                    .is_some_and(|o| o.kind.starts_with("allow"));
                let branch = if allowed { &p.allowed } else { &p.rejected };
                return self.run_steps(ctx, branch).await;
            }
            Step::WriteFile {
                path,
                content,
                executable,
            } => {
                let path = ctx.cwd.join(path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(Error::into_internal_error)?;
                }
                std::fs::write(&path, content).map_err(Error::into_internal_error)?;
                if *executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .map_err(Error::into_internal_error)?;
                }
            }
            Step::Usage { used, size, cost } => {
                let mut v = json!({"sessionUpdate": "usage_update", "used": used, "size": size});
                if let Some(c) = cost {
                    v["cost"] = json!({"amount": c, "currency": "USD"});
                }
                send_update(cx, id, v)?;
            }
            Step::Title(title) => send_update(
                cx,
                id,
                json!({"sessionUpdate": "session_info_update", "title": title}),
            )?,
            Step::Commands(commands) => self.send_commands(cx, id, commands)?,
            Step::SleepMs(ms) => {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(*ms)) => {}
                    _ = wait_cancel(ctx) => return Ok(Flow::Cancelled),
                }
            }
            Step::Spawn(argv) => {
                let child = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(ctx.cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(Error::into_internal_error)?;
                self.record("spawned", &json!({"pid": child.id()}));
            }
            Step::Command(argv) => {
                let out = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(ctx.cwd)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .map_err(Error::into_internal_error)?;
                self.record("command", &json!({"argv": argv, "code": out.status.code()}));
                if !out.status.success() {
                    return Err(Error::internal_error().data(format!(
                        "{argv:?}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )));
                }
            }
            Step::WaitForCancel => {
                wait_cancel(ctx).await;
                return Ok(Flow::Cancelled);
            }
            Step::Exit(code) => std::process::exit(*code),
            Step::Error { code, message } => {
                return Err(Error::new(*code, message.clone()));
            }
        }
        Ok(Flow::Continue)
    }
}

struct TurnCtx<'a> {
    cx: &'a ConnectionTo<Client>,
    id: &'a str,
    cwd: &'a Path,
    cancelled: &'a AtomicBool,
    cancel: &'a Notify,
    /// Agent message text of this turn, for the history.
    said: Mutex<String>,
}

async fn wait_cancel(ctx: &TurnCtx<'_>) {
    loop {
        let notified = ctx.cancel.notified();
        if ctx.cancelled.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

fn chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
}

fn send_update(cx: &ConnectionTo<Client>, id: &str, update: Value) -> Result<()> {
    cx.send_notification(from_json::<SessionNotification>(
        json!({"sessionId": id, "update": update}),
    )?)
}

fn from_json<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(Error::into_internal_error)
}

fn invalid(message: String) -> Error {
    Error::invalid_params().data(Value::String(message))
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

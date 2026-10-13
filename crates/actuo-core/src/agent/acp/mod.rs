//! The ACP v1 backend: agents through their ACP adapters.
//!
//! One agent process per session, spawned as the leader of its own process group.
//! `session/update`, `session/request_permission` and the `session/prompt` result
//! are handled as raw JSON (the SDK's `UntypedMessage`), so actuo keeps the
//! agent's exact message as `raw`, reads the prompt's `usage` (typed only behind an
//! unstable SDK feature), and survives update kinds the SDK doesn't know yet.
//!
//! Ordering: notification and request handlers run in the connection's dispatch
//! loop, one message at a time, and the prompt result is handled there too
//! (`on_receiving_result`), so events leave in the order the agent sent them.

pub mod mapper;

use super::{
    AgentBackend, AgentError, AgentEvent, AgentHandle, AgentInfo, AgentSpec, Command, ExitInfo,
    Result, SessionSetup,
};
use crate::fs_guard;
use actuo_protocol::ids::RequestId;
use actuo_protocol::types::{
    AgentConfigOption, AgentImplementation, AgentMode, ConfigChoice, ConfigOptionType, ConfigValue,
};
use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, Error as AcpError, Responder, UntypedMessage,
};
use mapper::Mapper;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// ACP's `auth_required` error code.
const AUTH_REQUIRED: i32 = -32000;
/// How much of the agent's stderr is kept for error messages.
const STDERR_TAIL: usize = 4096;

pub struct AcpBackend;

/// State shared by the handlers, the driver and the process watcher.
struct Shared {
    events: mpsc::UnboundedSender<AgentEvent>,
    mapper: Mutex<Mapper>,
    /// While `session/load` runs, its history replay is dropped.
    replaying: AtomicBool,
    permissions: Mutex<HashMap<RequestId, Responder<Value>>>,
    session_id: Mutex<Option<String>>,
    root: PathBuf,
    stderr_tail: Mutex<VecDeque<u8>>,
}

impl Shared {
    fn emit(&self, events: impl IntoIterator<Item = super::Mapped>) {
        for e in events {
            let _ = self.events.send(AgentEvent::Event(e));
        }
    }

    fn session_id(&self) -> Result<String> {
        self.session_id
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AgentError::Protocol("no session yet".into()))
    }
}

impl AgentBackend for AcpBackend {
    fn spawn(&self, spec: AgentSpec) -> Result<(AgentHandle, mpsc::UnboundedReceiver<AgentEvent>)> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        if spec.clear_env {
            cmd.env_clear();
        }
        cmd.args(&spec.args)
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let records = match &spec.srt_records {
            Some(_) => Some(
                crate::sandbox::records(&mut cmd)
                    .map_err(|e| AgentError::Spawn(format!("srt records pipe: {e}")))?,
            ),
            None => None,
        };
        let (spawned, records) = match records {
            // The pipe's write end lives only in srt from here.
            Some((reader, writer)) => (
                crate::process::spawn_passing(&mut cmd, writer),
                Some(reader),
            ),
            None => (crate::process::spawn(&mut cmd), None),
        };
        let mut child = spawned.map_err(|e| AgentError::Spawn(format!("{}: {e}", spec.command)))?;
        let pid = child
            .id()
            .ok_or_else(|| AgentError::Spawn("the process exited at once".into()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            events: events_tx,
            mapper: Mutex::new(Mapper::new()),
            replaying: AtomicBool::new(false),
            permissions: Mutex::new(HashMap::new()),
            session_id: Mutex::new(None),
            root: spec.cwd.canonicalize().unwrap_or(spec.cwd.clone()),
            stderr_tail: Mutex::new(VecDeque::new()),
        });

        tokio::spawn(pump_stderr(stderr, spec.stderr_log.clone(), shared.clone()));
        if let (Some(reader), Some(label)) = (records, spec.srt_records.clone()) {
            // srt's own messages go where its stderr went: the agent log and the tail
            // an exit message quotes.
            let (log, shared) = (spec.stderr_log.clone(), shared.clone());
            crate::sandbox::read_records(
                reader,
                spec.stderr_log.clone(),
                label,
                None,
                move |text| {
                    if let Some(path) = &log
                        && let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                    {
                        use std::io::Write;
                        let _ = writeln!(f, "{text}");
                    }
                    let mut tail = shared.stderr_tail.lock().unwrap();
                    tail.extend(text.as_bytes());
                    tail.push_back(b'\n');
                    while tail.len() > STDERR_TAIL {
                        tail.pop_front();
                    }
                },
            );
        }
        let connection = tokio::spawn(run_connection(
            ByteStreams::new(stdin.compat_write(), SmallReads(stdout).compat()),
            cmd_rx,
            shared.clone(),
        ));
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            // Anything the agent started is killed with it.
            crate::process::kill_group(pid, crate::process::TERM_GRACE).await;
            connection.abort();
            let events: Vec<_> = shared.mapper.lock().unwrap().close_all();
            shared.emit(events);
            use std::os::unix::process::ExitStatusExt;
            let tail = shared
                .stderr_tail
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            let _ = shared.events.send(AgentEvent::Exited(ExitInfo {
                code: status.and_then(|s| s.code()),
                signal: status.and_then(|s| s.signal()),
                stderr_tail: String::from_utf8_lossy(&tail).into_owned(),
            }));
        });

        Ok((AgentHandle::new(cmd_tx, pid), events_rx))
    }
}

async fn pump_stderr(
    mut stderr: tokio::process::ChildStderr,
    log: Option<PathBuf>,
    shared: Arc<Shared>,
) {
    let mut file = match log {
        Some(p) => {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        }
        None => None,
    };
    let mut buf = [0u8; 8192];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        if let Some(f) = file.as_mut() {
            use std::io::Write;
            let _ = f.write_all(&buf[..n]);
        }
        let mut tail = shared.stderr_tail.lock().unwrap();
        tail.extend(&buf[..n]);
        while tail.len() > STDERR_TAIL {
            tail.pop_front();
        }
    }
}

async fn run_connection<T>(
    transport: T,
    commands: mpsc::UnboundedReceiver<Command>,
    shared: Arc<Shared>,
) -> std::result::Result<(), AcpError>
where
    T: agent_client_protocol::ConnectTo<Client> + 'static,
{
    let read = shared.clone();
    let write = shared.clone();
    let requests = shared.clone();
    let notifications = shared.clone();
    Client
        .builder()
        .name("actuo")
        .on_receive_request(
            async move |req: ReadTextFileRequest, responder, _cx| match read_file(&read.root, &req)
            {
                Ok(content) => responder.respond(ReadTextFileResponse::new(content)),
                Err(msg) => responder
                    .respond_with_error(AcpError::invalid_params().data(Value::String(msg))),
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: WriteTextFileRequest, responder, _cx| {
                let written = fs_guard::resolve_in_root(&write.root, &req.path).and_then(|p| {
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&p, &req.content).map_err(|e| e.to_string())
                });
                match written {
                    Ok(()) => responder.respond(WriteTextFileResponse::new()),
                    Err(msg) => responder
                        .respond_with_error(AcpError::invalid_params().data(Value::String(msg))),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: UntypedMessage, responder: Responder<Value>, _cx| {
                if req.method != "session/request_permission" {
                    // No terminals, elicitation or anything else.
                    return responder.respond_with_error(AcpError::method_not_found());
                }
                let (id, events) = requests.mapper.lock().unwrap().permission(&req.params);
                requests.permissions.lock().unwrap().insert(id, responder);
                requests.emit(events);
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: UntypedMessage, _cx| {
                // Other notifications (e.g. Claude's `_auth/status_update`) are ignored.
                if n.method == "session/update" && !notifications.replaying.load(Ordering::SeqCst) {
                    let events = notifications.mapper.lock().unwrap().update(&n.params);
                    notifications.emit(events);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            drive(cx, commands, shared).await;
            Ok(())
        })
        .await
}

/// Runs commands from the session until the handle is dropped.
async fn drive(
    cx: ConnectionTo<Agent>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    shared: Arc<Shared>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Initialize(reply) => {
                let params = json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": false,
                    },
                    "clientInfo": {"name": "actuo", "version": env!("CARGO_PKG_VERSION")},
                });
                let result = request(&cx, "initialize", params)
                    .await
                    .map(|v| parse_info(&v));
                let _ = reply.send(result);
            }
            Command::NewSession { meta, reply } => {
                let mut params = json!({"cwd": shared.root, "mcpServers": []});
                add_meta(&mut params, meta);
                let result = request(&cx, "session/new", params).await.and_then(|v| {
                    let id = v["sessionId"]
                        .as_str()
                        .ok_or_else(|| {
                            AgentError::Protocol("session/new without sessionId".into())
                        })?
                        .to_string();
                    *shared.session_id.lock().unwrap() = Some(id.clone());
                    Ok(parse_setup(id, &v))
                });
                let _ = reply.send(result);
            }
            Command::ResumeSession { id, meta, reply } => {
                let mut params = json!({"sessionId": id, "cwd": shared.root, "mcpServers": []});
                add_meta(&mut params, meta);
                let result = request(&cx, "session/resume", params).await.map(|v| {
                    *shared.session_id.lock().unwrap() = Some(id.clone());
                    parse_setup(id, &v)
                });
                let _ = reply.send(result);
            }
            Command::LoadSession { id, meta, reply } => {
                let mut params = json!({"sessionId": id, "cwd": shared.root, "mcpServers": []});
                add_meta(&mut params, meta);
                // Drop the replay until the response, which is handled in dispatch
                // order, so no live update after it is dropped.
                shared.replaying.store(true, Ordering::SeqCst);
                let (tx, rx) = oneshot::channel();
                let flag = shared.clone();
                let sent = message("session/load", params).and_then(|m| {
                    cx.send_request(m)
                        .on_receiving_result(async move |r| {
                            flag.replaying.store(false, Ordering::SeqCst);
                            let _ = tx.send(r);
                            Ok(())
                        })
                        .map_err(agent_error)
                });
                let result = match sent {
                    Err(e) => Err(e),
                    Ok(()) => match rx.await {
                        Ok(r) => r.map_err(agent_error),
                        Err(_) => Err(AgentError::Gone),
                    },
                };
                shared.replaying.store(false, Ordering::SeqCst);
                let result = result.map(|v| {
                    *shared.session_id.lock().unwrap() = Some(id.clone());
                    parse_setup(id, &v)
                });
                let _ = reply.send(result);
            }
            Command::SetMode { mode, reply } => {
                let result = match shared.session_id() {
                    Ok(sid) => request(
                        &cx,
                        "session/set_mode",
                        json!({"sessionId": sid, "modeId": mode}),
                    )
                    .await
                    .map(|_| ()),
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
            }
            Command::SetConfig { id, value, reply } => {
                let result = match shared.session_id() {
                    Ok(sid) => {
                        let params = match &value {
                            ConfigValue::String(s) => {
                                json!({"sessionId": sid, "configId": id, "value": s})
                            }
                            ConfigValue::Bool(b) => {
                                json!({"sessionId": sid, "configId": id, "type": "boolean", "value": b})
                            }
                        };
                        request(&cx, "session/set_config_option", params)
                            .await
                            .map(|v| parse_config_options(&v["configOptions"]))
                    }
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
            }
            Command::Prompt { turn, text, reply } => {
                let sent = shared.session_id().and_then(|sid| {
                    shared.mapper.lock().unwrap().begin_turn(turn);
                    let params = json!({
                        "sessionId": sid,
                        "prompt": [{"type": "text", "text": text}],
                    });
                    let done = shared.clone();
                    message("session/prompt", params).and_then(|m| {
                        cx.send_request(m)
                            .on_receiving_result(
                                async move |r: std::result::Result<Value, AcpError>| {
                                    let event = {
                                        let mut mapper = done.mapper.lock().unwrap();
                                        match r {
                                            Ok(v) => {
                                                let (events, end) = mapper.end_turn(&v);
                                                done.emit(events);
                                                AgentEvent::TurnEnded(end)
                                            }
                                            // The process died: its exit ends the
                                            // turn (agent_exited), not a failed prompt.
                                            Err(e) if agent_client_protocol::is_incoming_transport_closed(&e) => {
                                                return Ok(());
                                            }
                                            Err(e) => {
                                                done.emit(mapper.close_all());
                                                AgentEvent::TurnFailed(agent_error(e))
                                            }
                                        }
                                    };
                                    let _ = done.events.send(event);
                                    Ok(())
                                },
                            )
                            .map_err(agent_error)
                    })
                });
                let _ = reply.send(sent);
            }
            Command::Cancel => {
                if let Ok(sid) = shared.session_id()
                    && let Ok(m) = message("session/cancel", json!({"sessionId": sid}))
                {
                    let _ = cx.send_notification(m);
                }
            }
            Command::Respond { request, choice } => {
                let responder = shared.permissions.lock().unwrap().remove(&request);
                if let Some(responder) = responder {
                    let outcome = match choice {
                        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
                        None => json!({"outcome": {"outcome": "cancelled"}}),
                    };
                    let _ = responder.respond(outcome);
                }
            }
        }
    }
}

/// Reads at most 1 KiB at a time. tokio charges its cooperative budget per read, and
/// the SDK parses every line of a read before reading again, so with 8 KiB reads one
/// task poll could parse a backlog of thousands of messages (100+ ms in a debug
/// build) before other tasks on the thread got a turn.
struct SmallReads<R>(R);

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for SmallReads<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut chunk = [0u8; 1024];
        let n = buf.remaining().min(chunk.len());
        let mut small = tokio::io::ReadBuf::new(&mut chunk[..n]);
        let poll = std::pin::Pin::new(&mut self.0).poll_read(cx, &mut small);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            buf.put_slice(small.filled());
        }
        poll
    }
}

fn message(method: &str, params: Value) -> Result<UntypedMessage> {
    UntypedMessage::new(method, params).map_err(agent_error)
}

async fn request(cx: &ConnectionTo<Agent>, method: &str, params: Value) -> Result<Value> {
    cx.send_request(message(method, params)?)
        .block_task()
        .await
        .map_err(agent_error)
}

fn agent_error(e: AcpError) -> AgentError {
    let code: i32 = e.code.into();
    if code == AUTH_REQUIRED {
        return AgentError::AuthRequired(e.message);
    }
    AgentError::Agent {
        code,
        message: e.message,
        data: e.data,
    }
}

fn add_meta(params: &mut Value, meta: Option<Value>) {
    if let Some(meta) = meta.filter(|m| !m.is_null()) {
        params["_meta"] = meta;
    }
}

fn parse_info(v: &Value) -> AgentInfo {
    let caps = &v["agentCapabilities"];
    let info = &v["agentInfo"];
    AgentInfo {
        implementation: info["name"].as_str().map(|name| AgentImplementation {
            name: name.to_string(),
            title: info["title"].as_str().map(str::to_string),
            version: info["version"].as_str().map(str::to_string),
        }),
        can_load: caps["loadSession"].as_bool().unwrap_or(false),
        can_resume: caps["sessionCapabilities"]["resume"].is_object(),
    }
}

fn parse_setup(agent_session_id: String, v: &Value) -> SessionSetup {
    let modes = &v["modes"];
    SessionSetup {
        agent_session_id,
        modes: modes["availableModes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(AgentMode {
                    id: m["id"].as_str()?.to_string(),
                    name: m["name"].as_str().unwrap_or_default().to_string(),
                    description: m["description"].as_str().map(str::to_string),
                })
            })
            .collect(),
        current_mode: modes["currentModeId"].as_str().map(str::to_string),
        config_options: parse_config_options(&v["configOptions"]),
    }
}

fn parse_config_options(v: &Value) -> Vec<AgentConfigOption> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let option_type = match o["type"].as_str() {
                Some("boolean") => ConfigOptionType::Boolean,
                _ => ConfigOptionType::Select,
            };
            let current_value = match &o["currentValue"] {
                Value::Bool(b) => Some(ConfigValue::Bool(*b)),
                Value::String(s) => Some(ConfigValue::String(s.clone())),
                _ => None,
            };
            // Select options come as a flat list or as groups of lists.
            let mut values = Vec::new();
            for entry in o["options"].as_array().into_iter().flatten() {
                let group = entry["options"].as_array();
                for choice in group.map_or_else(|| vec![entry], |g| g.iter().collect()) {
                    if let Some(value) = choice["value"].as_str() {
                        values.push(ConfigChoice {
                            value: value.to_string(),
                            name: choice["name"].as_str().unwrap_or(value).to_string(),
                            description: choice["description"].as_str().map(str::to_string),
                        });
                    }
                }
            }
            Some(AgentConfigOption {
                id: o["id"].as_str()?.to_string(),
                name: o["name"].as_str().unwrap_or_default().to_string(),
                description: o["description"].as_str().map(str::to_string),
                category: o["category"].as_str().map(str::to_string),
                option_type,
                current_value,
                values,
            })
        })
        .collect()
}

fn read_file(
    root: &std::path::Path,
    req: &ReadTextFileRequest,
) -> std::result::Result<String, String> {
    let path = fs_guard::resolve_in_root(root, &req.path)?;
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if req.line.is_none() && req.limit.is_none() {
        return Ok(content);
    }
    // `line` is 1-based.
    let start = req.line.unwrap_or(1).saturating_sub(1) as usize;
    let limit = req.limit.map_or(usize::MAX, |l| l as usize);
    Ok(content
        .split_inclusive('\n')
        .skip(start)
        .take(limit)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_options_flat_grouped_and_boolean() {
        let options = parse_config_options(&json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select",
             "currentValue": "opus",
             "options": [{"value": "opus", "name": "Opus"}, {"value": "sonnet", "name": "Sonnet"}]},
            {"id": "effort", "name": "Effort", "type": "select", "currentValue": "high",
             "options": [{"group": "g", "name": "G", "options": [{"value": "high", "name": "High"}]}]},
            {"id": "fast", "name": "Fast", "type": "boolean", "currentValue": false},
            {"name": "no id"}
        ]));
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].values.len(), 2);
        assert_eq!(options[1].values[0].value, "high");
        assert_eq!(options[2].option_type, ConfigOptionType::Boolean);
        assert_eq!(options[2].current_value, Some(ConfigValue::Bool(false)));
    }

    #[test]
    fn capabilities_from_initialize() {
        let info = parse_info(&json!({
            "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}},
            "agentInfo": {"name": "@agentclientprotocol/codex-acp", "title": "Codex", "version": "2.0.1"}
        }));
        assert!(info.can_load && info.can_resume);
        assert_eq!(
            info.implementation.unwrap().version.as_deref(),
            Some("2.0.1")
        );
        let info = parse_info(&json!({"agentCapabilities": {}}));
        assert!(!info.can_load && !info.can_resume && info.implementation.is_none());
    }
}

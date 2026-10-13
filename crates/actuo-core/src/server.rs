//! Actuo's protocol server: JSON-RPC 2.0, one JSON object per line.
//!
//! [`serve_connection`] works on any byte stream (the transport); [`serve_unix`]
//! accepts connections on actuo's Unix socket; `actuo stdio` joins a byte stream
//! to that socket.

use crate::session::{Host, HostResult};
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::events::Event;
use actuo_protocol::ids::{SessionId, SubscriptionId};
use actuo_protocol::jsonrpc::{Id, Message};
use actuo_protocol::methods::*;
use actuo_protocol::types::{ActuoCapabilities, ActuoInfo};
use actuo_protocol::{
    CHECK_STATE_NOTIFICATION, EVENT_NOTIFICATION, PROTOCOL_VERSION, RUN_STATE_NOTIFICATION,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Events read from the store per page when a subscription catches up.
const REPLAY_PAGE: usize = 500;

/// Listens on `path` until `shutdown` resolves. Refuses to start if another actuo
/// answers on the socket; removes a stale socket file otherwise.
pub async fn serve_unix(
    host: Arc<Host>,
    path: &Path,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if path.as_os_str().len() > crate::config::MAX_SOCKET_PATH {
        return Err(std::io::Error::other(format!(
            "socket path {} is longer than {} bytes",
            path.display(),
            crate::config::MAX_SOCKET_PATH
        )));
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("an actuo is already listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let host = host.clone();
                tokio::spawn(async move {
                    let (r, w) = stream.into_split();
                    serve_connection(host, r, w).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

struct Connection {
    host: Arc<Host>,
    out: mpsc::UnboundedSender<Message>,
    initialized: AtomicBool,
    /// The `client.name` from `initialize`, recorded on turns and answers it causes.
    client: Mutex<Option<String>>,
    subscriptions: Mutex<HashMap<SubscriptionId, JoinHandle<()>>>,
    /// Held for a restart (`actuo/drain`) until the connection ends.
    hold: Mutex<Option<crate::drain::Hold>>,
    /// Turns true when the client is gone.
    closed: tokio::sync::watch::Sender<bool>,
    next_subscription: AtomicU64,
}

/// Serves one client until it disconnects. When the client closes its side (or
/// half-closes it, as `actuo stdio` does when its stdin ends), the calls already
/// read are finished and answered before the connection closes.
pub async fn serve_connection<R, W>(host: Arc<Host>, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let mut line = match serde_json::to_vec(&msg) {
                Ok(l) => l,
                Err(_) => continue,
            };
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let conn = Arc::new(Connection {
        host,
        out,
        initialized: AtomicBool::new(false),
        client: Mutex::new(None),
        subscriptions: Mutex::new(HashMap::new()),
        hold: Mutex::new(None),
        closed: tokio::sync::watch::Sender::new(false),
        next_subscription: AtomicU64::new(1),
    });

    let mut lines = BufReader::new(reader).lines();
    // Calls being answered; reaped as they finish, awaited when the client is done.
    let mut calls = tokio::task::JoinSet::new();
    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                _ => break,
            },
            Some(_) = calls.join_next(), if !calls.is_empty() => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                conn.send_error(None, RpcError::new(ErrorKind::ParseError, e.to_string()));
                continue;
            }
        };
        let (Some(id), Some(method)) = (msg.id.clone(), msg.method.clone()) else {
            // Actuo expects no notifications or responses from clients.
            continue;
        };
        let params = msg.params.unwrap_or(Value::Null);
        // initialize is handled in line so later requests see its result.
        if method == Initialize::NAME {
            let r = conn.initialize(params);
            conn.reply(id, r);
            continue;
        }
        if !conn.initialized.load(Ordering::SeqCst) {
            conn.send_error(
                Some(id),
                RpcError::new(ErrorKind::NotInitialized, "call initialize first"),
            );
            continue;
        }
        let conn = conn.clone();
        calls.spawn(async move {
            if method == SessionSubscribe::NAME {
                conn.subscribe(id, params).await;
            } else {
                let r = conn.call(&method, params).await;
                conn.reply(id, r);
            }
        });
    }

    // Answer what was asked (a drain stops waiting), then stop the subscriptions (a
    // subscribe still being answered would otherwise start one after they were
    // stopped). The hold for a restart goes with the connection.
    conn.closed.send_replace(true);
    while calls.join_next().await.is_some() {}
    for (_, task) in conn.subscriptions.lock().unwrap().drain() {
        task.abort();
    }
    drop(conn);
    let _ = writer_task.await;
}

impl Connection {
    fn reply(&self, id: Id, result: HostResult<Value>) {
        let _ = self.out.send(Message::response(id, result));
    }

    fn send_error(&self, id: Option<Id>, e: RpcError) {
        let mut m = Message::response(Id::Number(0), Err(e));
        m.id = id;
        let _ = self.out.send(m);
    }

    fn client_name(&self) -> Option<String> {
        self.client.lock().unwrap().clone()
    }

    fn initialize(&self, params: Value) -> HostResult<Value> {
        let p: InitializeParams = parse(params)?;
        if p.protocol_version != PROTOCOL_VERSION {
            return Err(RpcError::new(
                ErrorKind::UnsupportedProtocolVersion,
                format!(
                    "this actuo speaks protocol {PROTOCOL_VERSION}, this connection {}",
                    p.protocol_version
                ),
            )
            .with_detail("supported", json!([PROTOCOL_VERSION])));
        }
        // The lane's own turns and answers carry this name, and the take-over watch
        // tells them apart by it: no client may use it.
        if p.client
            .name
            .trim()
            .eq_ignore_ascii_case(crate::lane::CLIENT)
        {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                format!(
                    "\"{}\" is actuo's own name for the lane; pick another client name",
                    crate::lane::CLIENT
                ),
            ));
        }
        *self.client.lock().unwrap() = Some(p.client.name);
        self.initialized.store(true, Ordering::SeqCst);
        let binary = std::env::current_exe().ok();
        to_value(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            actuo: ActuoInfo {
                name: "actuo".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                commit: crate::COMMIT.map(Into::into),
                binary: binary.as_ref().map(|b| b.display().to_string()),
                binary_writable: binary
                    .as_deref()
                    .and_then(|b| b.parent())
                    .map(|dir| rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok()),
            },
            capabilities: ActuoCapabilities::default(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> HostResult<Value> {
        let host = &self.host;
        match method {
            AgentList::NAME => to_value(AgentListResult {
                agents: host.agent_list().await?,
            }),
            AgentProbe::NAME => {
                let p: AgentProbeParams = parse(params)?;
                crate::probe::probe(host, &p.agent).await?;
                let agent = host
                    .agent_list()
                    .await?
                    .into_iter()
                    .find(|a| a.name == p.agent)
                    .expect("probed an agent in the config");
                to_value(AgentProbeResult { agent })
            }
            ProjectOpen::NAME => {
                let p: ProjectOpenParams = parse(params)?;
                let project = host.open_project(&p.path).await?;
                to_value(ProjectOpenResult {
                    project: host.with_repo(project),
                })
            }
            ProjectList::NAME => {
                let projects: Vec<_> = host
                    .db(|s| s.projects())
                    .await?
                    .into_iter()
                    .map(|p| host.with_repo(p))
                    .collect();
                // Each project's last check run.
                let projects = host
                    .db(move |s| {
                        projects
                            .into_iter()
                            .map(|mut p| {
                                if let Some(repo) = &p.repo {
                                    p.last_check_run = s.last_check_run(repo)?;
                                }
                                Ok(p)
                            })
                            .collect::<crate::store::Result<Vec<_>>>()
                    })
                    .await?;
                to_value(ProjectListResult { projects })
            }
            ProjectCheck::NAME => {
                let p: ProjectRepoParams = parse(params)?;
                to_value(ProjectCheckResult {
                    check_run: host.lane()?.check_project(&p.repo).await?,
                })
            }
            ProjectAllowHost::NAME => {
                let p: ProjectAllowHostParams = parse(params)?;
                to_value(SettingsResult {
                    settings: host.allow_project_host(p.repo, p.host).await?,
                })
            }
            ProjectInspect::NAME => {
                let p: ProjectRepoParams = parse(params)?;
                to_value(host.lane()?.inspect(&p.repo).await?)
            }
            ProjectAdd::NAME => {
                let p: ProjectAddParams = parse(params)?;
                let (project, accepted_invitation) =
                    host.lane()?.add_project(&p.repo, &p.settings).await?;
                to_value(ProjectAddResult {
                    project,
                    accepted_invitation,
                })
            }
            ProjectRemove::NAME => {
                let p: ProjectRemoveParams = parse(params)?;
                host.lane()?.remove_project(&p.repo, p.delete_clone).await?;
                to_value(Empty {})
            }
            SessionStart::NAME => {
                let p: SessionStartParams = parse(params)?;
                to_value(SessionStartResult {
                    session_id: host.start_session(p, self.client_name()).await?,
                })
            }
            SessionSend::NAME => {
                let p: SessionSendParams = parse(params)?;
                to_value(SessionSendResult {
                    turn: host
                        .send(&p.session_id, p.text, self.client_name(), p.interrupt)
                        .await?,
                })
            }
            SessionInterrupt::NAME => {
                let p: SessionParams = parse(params)?;
                host.interrupt(&p.session_id).await?;
                to_value(Empty {})
            }
            SessionRespond::NAME => {
                let p: SessionRespondParams = parse(params)?;
                host.respond(
                    &p.session_id,
                    p.request_id,
                    p.option_id,
                    p.cancel,
                    self.client_name(),
                )
                .await?;
                to_value(Empty {})
            }
            SessionUnsubscribe::NAME => {
                let p: SessionUnsubscribeParams = parse(params)?;
                if let Some(task) = self
                    .subscriptions
                    .lock()
                    .unwrap()
                    .remove(&p.subscription_id)
                {
                    task.abort();
                }
                to_value(Empty {})
            }
            SessionList::NAME => {
                let p: SessionListParams = parse_or_default(params)?;
                to_value(SessionListResult {
                    sessions: host.db(move |s| s.sessions(p.project_id.as_ref())).await?,
                })
            }
            SessionGet::NAME => {
                let p: SessionParams = parse(params)?;
                to_value(SessionGetResult {
                    session: host.db(move |s| s.session_detail(&p.session_id)).await?,
                })
            }
            SessionStop::NAME => {
                let p: SessionParams = parse(params)?;
                host.stop(&p.session_id).await?;
                to_value(Empty {})
            }
            CheckpointList::NAME => {
                let p: SessionParams = parse(params)?;
                let sid = p.session_id.clone();
                host.db(move |s| s.session(&sid)).await?;
                to_value(CheckpointListResult {
                    checkpoints: host.db(move |s| s.checkpoints(&p.session_id)).await?,
                })
            }
            CheckpointRevert::NAME => {
                let p: CheckpointRevertParams = parse(params)?;
                to_value(host.revert(&p.session_id, p.turn, p.force).await?)
            }
            SessionDiff::NAME => {
                let p: SessionParams = parse(params)?;
                to_value(host.diff(&p.session_id).await?)
            }
            RunStart::NAME => {
                let p: RunStartParams = parse(params)?;
                to_value(RunStartResult {
                    run_id: host
                        .lane()?
                        .start_run(&p.issue_url, p.agent, p.level, self.client_name())
                        .await?,
                })
            }
            RunList::NAME => {
                let p: RunListParams = parse_or_default(params)?;
                let runs = host.db(move |s| s.runs(p.repo.as_deref(), p.state)).await?;
                to_value(RunListResult {
                    runs: runs.iter().map(|r| r.summary()).collect(),
                })
            }
            RunGet::NAME => {
                let p: RunParams = parse(params)?;
                to_value(RunGetResult {
                    run: host.db(move |s| s.run_detail(&p.run_id)).await?,
                })
            }
            RunResume::NAME => {
                let p: RunParams = parse(params)?;
                host.lane()?
                    .resume_run(&p.run_id, self.client_name())
                    .await?;
                to_value(Empty {})
            }
            RunCancel::NAME => {
                let p: RunParams = parse(params)?;
                host.lane()?
                    .cancel_run(&p.run_id, self.client_name())
                    .await?;
                to_value(Empty {})
            }
            RunSubscribe::NAME => {
                let p: RunSubscribeParams = parse_or_default(params)?;
                let lane = host.lane()?;
                let mut runs = lane.subscribe();
                let mut checks = lane.check_subscribe();
                // Counted while the subscription lives: dropped with the task, on
                // unsubscribe or when the connection ends.
                let hold = p.notifies.then(|| lane.notifiers().hold());
                let n = self.next_subscription.fetch_add(1, Ordering::SeqCst);
                let subscription = SubscriptionId(format!("sub-{n}"));
                let out = self.out.clone();
                let task = tokio::spawn(async move {
                    let _hold = hold;
                    loop {
                        // Runs and check runs; a later notification carries the
                        // latest state, so a lagged receiver skips ahead.
                        let (method, value) = tokio::select! {
                            r = runs.recv() => match r {
                                Ok(run) => (RUN_STATE_NOTIFICATION, serde_json::to_value(&run)),
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => return,
                            },
                            c = checks.recv() => match c {
                                Ok(check) => (CHECK_STATE_NOTIFICATION, serde_json::to_value(&check)),
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => return,
                            },
                        };
                        let Ok(value) = value else { continue };
                        if out.send(Message::notification(method, value)).is_err() {
                            return;
                        }
                    }
                });
                self.subscriptions
                    .lock()
                    .unwrap()
                    .insert(subscription.clone(), task);
                to_value(RunSubscribeResult {
                    subscription_id: subscription,
                })
            }
            RunUnsubscribe::NAME => {
                let p: RunUnsubscribeParams = parse(params)?;
                if let Some(task) = self
                    .subscriptions
                    .lock()
                    .unwrap()
                    .remove(&p.subscription_id)
                {
                    task.abort();
                }
                to_value(Empty {})
            }
            ActuoStatus::NAME => to_value(crate::status::status(host).await),
            ActuoDrain::NAME => {
                let p: ActuoDrainParams = parse(params)?;
                self.hold
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| host.hold_for_restart());
                to_value(
                    crate::drain::drain(
                        host,
                        std::time::Duration::from_secs(p.limit_s),
                        self.closed.subscribe(),
                    )
                    .await,
                )
            }
            SettingsGet::NAME => to_value(SettingsResult {
                settings: host.settings(),
            }),
            SettingsUpdate::NAME => {
                let p: SettingsUpdateParams = parse(params)?;
                to_value(SettingsResult {
                    settings: host.update_settings(p.patch).await?,
                })
            }
            _ => Err(RpcError::new(
                ErrorKind::MethodNotFound,
                format!("no method {method}"),
            )),
        }
    }

    /// Answers first, then replays stored events after `fromSeq` up to the head read
    /// at subscribe time, then streams live ones. Live events are received from
    /// before the head is read, so nothing falls between; duplicates are dropped by
    /// `seq`. A subscriber that falls too far behind catches up from the store.
    async fn subscribe(self: &Arc<Self>, id: Id, params: Value) {
        let p: SessionSubscribeParams = match parse(params) {
            Ok(p) => p,
            Err(e) => return self.reply(id, Err(e)),
        };
        let mut live = self.host.store().subscribe();
        let sid = p.session_id.clone();
        let head = match self.host.db(move |s| s.session(&sid)).await {
            Ok(s) => s.head_seq,
            Err(e) => return self.reply(id, Err(e)),
        };
        let n = self.next_subscription.fetch_add(1, Ordering::SeqCst);
        let subscription = SubscriptionId(format!("sub-{n}"));
        self.reply(
            id,
            to_value(SessionSubscribeResult {
                subscription_id: subscription.clone(),
                head_seq: head,
            }),
        );

        let conn = self.clone();
        let task = tokio::spawn(async move {
            let session = p.session_id;
            let mut last = p.from_seq;
            if conn
                .catch_up(&session, &mut last, Some(head), p.include_raw)
                .await
                .is_err()
            {
                return;
            }
            loop {
                match live.recv().await {
                    Ok(event) => {
                        if event.session_id != session || event.seq <= last {
                            continue;
                        }
                        last = event.seq;
                        if !conn.send_event(&event, p.include_raw) {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if conn
                            .catch_up(&session, &mut last, None, p.include_raw)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        self.subscriptions
            .lock()
            .unwrap()
            .insert(subscription, task);
    }

    /// Sends stored events after `last` (up to `upto`), updating `last`.
    async fn catch_up(
        &self,
        session: &SessionId,
        last: &mut u64,
        upto: Option<u64>,
        include_raw: bool,
    ) -> Result<(), ()> {
        loop {
            let (sid, after) = (session.clone(), *last);
            let page = self
                .host
                .db(move |s| s.events_after(&sid, after, REPLAY_PAGE))
                .await
                .map_err(|_| ())?;
            let full = page.len() == REPLAY_PAGE;
            for event in page {
                if upto.is_some_and(|u| event.seq > u) {
                    return Ok(());
                }
                *last = event.seq;
                if !self.send_event(&event, include_raw) {
                    return Err(());
                }
            }
            if !full {
                return Ok(());
            }
        }
    }

    /// Returns false when the connection is gone.
    fn send_event(&self, event: &Event, include_raw: bool) -> bool {
        let mut value = match serde_json::to_value(event) {
            Ok(v) => v,
            Err(_) => return true,
        };
        if !include_raw && let Some(obj) = value.as_object_mut() {
            obj.remove("raw");
        }
        self.out
            .send(Message::notification(EVENT_NOTIFICATION, value))
            .is_ok()
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> HostResult<T> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(ErrorKind::InvalidParams, e.to_string()))
}

fn parse_or_default<T: DeserializeOwned + Default>(params: Value) -> HostResult<T> {
    if params.is_null() {
        return Ok(T::default());
    }
    parse(params)
}

fn to_value(v: impl Serialize) -> HostResult<Value> {
    serde_json::to_value(v).map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))
}

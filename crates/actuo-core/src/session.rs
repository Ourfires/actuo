//! Sessions: the [`Host`] owns the store, the agents config and one task per live
//! session. A session task owns its agent process, turns, open permission requests
//! and checkpoints, and writes every event to the store before it is broadcast.
//!
//! Store writes and git run on blocking threads (`spawn_blocking`), never on the
//! async threads: a busy turn writes one row per text chunk.

use crate::agent::{AgentBackend, AgentError, AgentEvent, AgentHandle, AgentSpec, Mapped};
use crate::config::{AgentConfig, AgentsConfig, Paths, SessionOptions};
use crate::git;
use crate::settings;
use crate::store::{self, NewEvent, NewSession, Store, StoreError, checkpoint_ref};
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::events::*;
use actuo_protocol::ids::{PermissionOptionId, RequestId, SessionId};
use actuo_protocol::methods::{CheckpointRevertResult, SessionDiffResult, SessionStartParams};
use actuo_protocol::settings::Settings;
use actuo_protocol::types::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

pub type HostResult<T> = Result<T, RpcError>;

/// How long a stopping session waits for the agent to end a cancelled turn.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long `session/send` with `interrupt` waits for the interrupted turn to end.
const INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Host {
    store: Arc<Store>,
    paths: Paths,
    pub(crate) agents: AgentsConfig,
    /// The settings in use; session tasks watch it (the idle-agent timeout applies
    /// at once).
    settings: watch::Sender<Settings>,
    /// Held while `settings.toml` is read, patched and written.
    settings_write: Mutex<()>,
    pub(crate) backend: Arc<dyn AgentBackend>,
    live: Mutex<HashMap<SessionId, mpsc::UnboundedSender<Cmd>>>,
    /// Agents being probed (`crate::probe`), one probe per agent at a time.
    pub(crate) probing: Mutex<std::collections::HashSet<String>>,
    /// The issue-to-PR lane, when `actuo serve` started one.
    lane: std::sync::OnceLock<Arc<crate::lane::Lane>>,
    /// Whether the sandbox runs here, asked once.
    sandbox_probe: tokio::sync::OnceCell<Result<(), String>>,
    /// Clients holding actuo for a restart (`actuo/drain`).
    holders: Arc<std::sync::atomic::AtomicUsize>,
    /// A newer release's commit (`crate::release::check`).
    update: Mutex<Option<String>>,
    /// [`INTERRUPT_GRACE`]; tests shorten it.
    interrupt_grace: Mutex<std::time::Duration>,
}

enum Cmd {
    Send {
        text: String,
        client: Option<String>,
        /// Interrupt a running turn first and send when it has ended.
        interrupt: bool,
        reply: oneshot::Sender<HostResult<u32>>,
    },
    Interrupt {
        reply: oneshot::Sender<HostResult<()>>,
    },
    Respond {
        request: RequestId,
        option: Option<PermissionOptionId>,
        client: Option<String>,
        reply: oneshot::Sender<HostResult<()>>,
    },
    Revert {
        turn: u32,
        force: bool,
        reply: oneshot::Sender<HostResult<CheckpointRevertResult>>,
    },
    /// End the session (`session/stop`).
    Stop { reply: oneshot::Sender<()> },
    /// Let go of the agent but keep the session (actuo is stopping).
    Detach { reply: oneshot::Sender<()> },
}

impl Host {
    pub fn new(
        store: Arc<Store>,
        paths: Paths,
        agents: AgentsConfig,
        settings: Settings,
        backend: Arc<dyn AgentBackend>,
    ) -> Arc<Self> {
        trust_worktrees(&store, &paths);
        Arc::new(Self {
            store,
            paths,
            agents,
            settings: watch::Sender::new(settings),
            settings_write: Mutex::new(()),
            backend,
            live: Mutex::new(HashMap::new()),
            probing: Mutex::new(Default::default()),
            lane: std::sync::OnceLock::new(),
            sandbox_probe: tokio::sync::OnceCell::new(),
            holders: Default::default(),
            update: Default::default(),
            interrupt_grace: Mutex::new(INTERRUPT_GRACE),
        })
    }

    /// How long `session/send` with `interrupt` waits for the interrupted turn to
    /// end: 30 s, unless a test set it shorter.
    pub fn set_interrupt_grace(&self, grace: std::time::Duration) {
        *self.interrupt_grace.lock().unwrap() = grace;
    }

    /// A newer release's commit, as last asked.
    pub fn update(&self) -> Option<String> {
        self.update.lock().unwrap().clone()
    }

    pub fn set_update(&self, commit: Option<String>) {
        *self.update.lock().unwrap() = commit;
    }

    /// Holds actuo for a restart until the hold is dropped (`crate::drain`).
    pub fn hold_for_restart(&self) -> crate::drain::Hold {
        crate::drain::Hold::new(&self.holders)
    }

    /// Whether a client holds actuo for a restart: the lane starts nothing new.
    pub fn held(&self) -> bool {
        self.holders.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Whether `srt` runs here (installed and probed once); `sandbox_unavailable`
    /// with the reason if not.
    pub async fn sandbox_ready(&self) -> HostResult<()> {
        let paths = self.paths.clone();
        self.sandbox_probe
            .get_or_init(|| async move { crate::sandbox::probe(&paths).await })
            .await
            .clone()
            .map_err(|e| RpcError::new(ErrorKind::SandboxUnavailable, e))
    }

    pub fn set_lane(&self, lane: Arc<crate::lane::Lane>) {
        let _ = self.lane.set(lane);
    }

    pub fn lane(&self) -> HostResult<&Arc<crate::lane::Lane>> {
        self.lane
            .get()
            .ok_or_else(|| RpcError::new(ErrorKind::Internal, "this actuo has no lane"))
    }

    /// Appends an event to a session's log from outside its task (a lane run's step).
    pub async fn append_event(&self, id: &SessionId, event: NewEvent) -> HostResult<()> {
        let sid = id.clone();
        self.db(move |s| s.append(&sid, event)).await.map(|_| ())
    }

    pub fn agents(&self) -> Vec<AgentEntry> {
        let settings = self.settings();
        self.agents
            .agents
            .iter()
            .map(|(name, a)| {
                a.entry(
                    name,
                    settings.defaults.get(name).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    /// `agent/list`: each agent with what it offers, from its last probe, else from
    /// its last session here.
    pub async fn agent_list(&self) -> HostResult<Vec<AgentEntry>> {
        let mut agents = self.agents();
        let names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
        let found = self
            .db(move |s| {
                names
                    .iter()
                    .map(|n| Ok((s.agent_options(n)?, s.config_options(n)?)))
                    .collect::<store::Result<Vec<_>>>()
            })
            .await?;
        for (a, (probed, seen)) in agents.iter_mut().zip(found) {
            match probed {
                Some(p) => {
                    a.config_options = p.own;
                    a.options_by_model = p.by_model;
                    a.probed_at = Some(p.probed_at);
                }
                None => a.config_options = seen,
            }
        }
        Ok(agents)
    }

    /// Re-reads `settings.toml`, applies the merge patch, validates and writes it,
    /// then uses the result. Updates run one at a time.
    pub async fn update_settings(self: &Arc<Self>, patch: Value) -> HostResult<Settings> {
        let host = self.clone();
        tokio::task::spawn_blocking(move || {
            let _writing = host.settings_write.lock().unwrap();
            let new = settings::update(&host.paths.settings_toml(), &patch, &host.agents)?;
            host.settings.send_replace(new.clone());
            // An agent newly in use gets probed.
            let probing = host.clone();
            tokio::runtime::Handle::current().spawn(crate::probe::in_use(probing));
            Ok(new)
        })
        .await
        .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?
    }

    /// Adds one host to a project's `network`, under the settings lock, so hosts
    /// allowed at the same time all stay and a removed project stays removed.
    pub async fn allow_project_host(
        self: &Arc<Self>,
        repo: String,
        name: String,
    ) -> HostResult<Settings> {
        let name = crate::detect::download_host(&name).ok_or_else(|| {
            RpcError::new(
                ErrorKind::InvalidParams,
                "expected an exact public DNS hostname, without a URL, port or wildcard",
            )
        })?;
        let host = self.clone();
        tokio::task::spawn_blocking(move || {
            let _writing = host.settings_write.lock().unwrap();
            let current = settings::load(&host.paths.settings_toml(), &host.agents)?;
            let project = current.projects.get(&repo).ok_or_else(|| {
                RpcError::new(
                    ErrorKind::ProjectNotFound,
                    format!("{repo} isn't a project here"),
                )
            })?;
            let mut network = project.network.clone().unwrap_or_else(|| {
                let files = crate::detect::GitFiles::new(&host.paths.lane_clone(&repo), "HEAD");
                crate::detect::default_network(&crate::detect::detect(&files), &files)
            });
            if !network.iter().any(|h| h.eq_ignore_ascii_case(&name)) {
                network.push(name);
            }
            let new = settings::update(
                &host.paths.settings_toml(),
                &serde_json::json!({"projects": {repo: {"network": network}}}),
                &host.agents,
            )?;
            host.settings.send_replace(new.clone());
            Ok(new)
        })
        .await
        .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?
    }

    /// Runs a store call on a blocking thread.
    pub async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> store::Result<T> + Send + 'static,
    ) -> HostResult<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store::off_async(|| f(&store)))
            .await
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?
            .map_err(store_error)
    }

    /// `project` with its `repo` when its root is a project's clone.
    pub fn with_repo(&self, mut project: Project) -> Project {
        let real =
            |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let root = real(std::path::Path::new(&project.root));
        project.repo = self
            .settings()
            .projects
            .keys()
            .find(|repo| real(&self.paths.lane_clone(repo)) == root)
            .cloned();
        project
    }

    pub async fn open_project(&self, path: &str) -> HostResult<Project> {
        let path = PathBuf::from(path);
        let root = blocking(move || {
            let root = git::toplevel(&path)?;
            if !git::has_commit(&root)? {
                return Err(git::GitError::NotARepo(root));
            }
            Ok(root)
        })
        .await
        .map_err(|e| match e {
            git::GitError::NotARepo(p) => RpcError::new(
                ErrorKind::NotAGitRepo,
                format!("{} is not a git repository with a commit", p.display()),
            ),
            e => git_error(e),
        })?;
        let root = root.display().to_string();
        self.db(move |s| s.open_project(&root)).await
    }

    /// Creates the session (and its worktree) and returns at once; the agent is
    /// started by the session's task. `client` is the caller's `client.name`, recorded
    /// on the first prompt's `turn.started`.
    pub async fn start_session(
        self: &Arc<Self>,
        p: SessionStartParams,
        client: Option<String>,
    ) -> HostResult<SessionId> {
        let project_id = p.project_id.clone();
        let project = self.db(move |s| s.project(&project_id)).await?;
        let settings = self.settings();
        let name = p.agent.clone().unwrap_or(settings.sessions.agent);
        let agent = self.agents.agents.get(&name).cloned().ok_or_else(|| {
            RpcError::new(ErrorKind::AgentNotFound, format!("no agent named {name}"))
        })?;
        if !agent.modes.is_empty() && !agent.modes.contains_key(&p.level) {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                format!("agent {name} has no mode for this level"),
            ));
        }
        if p.config.contains_key("mode") {
            return Err(RpcError::new(
                ErrorKind::InvalidConfigOption,
                "`mode` isn't set through config; the level sets it",
            ));
        }
        if let Some(sb) = &p.sandbox {
            for entry in &sb.read {
                crate::config::check_read_entry(entry)
                    .map_err(|e| RpcError::new(ErrorKind::InvalidParams, e))?;
            }
            self.sandbox_ready().await?;
        }

        let id = store::new_session_id();
        let root = PathBuf::from(&project.root);
        let (cwd, worktree, branch) = if p.worktree {
            let branch = p.branch.clone().unwrap_or_else(|| format!("actuo/{id}"));
            let path = self
                .worktree_for(&project, &id, &branch, p.base.clone())
                .await?;
            (path, true, Some(branch))
        } else {
            if p.base.is_some() || p.branch.is_some() {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    "base and branch need a worktree",
                ));
            }
            (root, false, None)
        };

        let mut config = settings.defaults.get(&name).cloned().unwrap_or_default();
        config.extend(p.config.clone());
        let new = NewSession {
            id: id.clone(),
            project_id: project.id.clone(),
            agent: name.clone(),
            agent_version: agent.version.clone(),
            level: p.level,
            clean: p.clean,
            sandbox: p.sandbox.clone(),
            web_tools: p.web_tools,
            telemetry: p.telemetry,
            cwd: cwd.display().to_string(),
            worktree_path: worktree.then(|| cwd.display().to_string()),
            branch,
            base: p.base.clone(),
            config: config.clone(),
        };
        self.db(move |s| s.create_session(new)).await?;
        let sid = id.clone();
        self.db(move |s| s.append(&sid, state_event(SessionState::Starting, None)))
            .await?;

        let task = SessionTask::new(
            self.clone(),
            id.clone(),
            cwd,
            worktree,
            agent,
            name.clone(),
            p.level,
            SessionOptions {
                clean: p.clean,
                sandboxed: p.sandbox.is_some(),
                web_tools: p.web_tools,
                telemetry: p.telemetry,
            },
            p.sandbox.clone(),
            config,
            // A value a client asked for must apply; actuo's defaults and the
            // lane's (a repo's config) are skipped when the agent doesn't offer them.
            if client.as_deref() == Some(crate::lane::CLIENT) {
                Default::default()
            } else {
                p.config.keys().cloned().collect()
            },
        );
        tokio::spawn(task.run(Attach::New, p.prompt.map(|t| (t, client)), None));
        Ok(id)
    }

    /// The worktree for a new session on `branch`: the project's existing worktree on
    /// it (unless a session that hasn't ended works there), else a new one, for the
    /// branch if it exists, else for a new branch from `base` (default `HEAD`).
    async fn worktree_for(
        &self,
        project: &Project,
        id: &SessionId,
        branch: &str,
        base: Option<String>,
    ) -> HostResult<PathBuf> {
        let root = PathBuf::from(&project.root);
        let (r, b) = (root.clone(), branch.to_string());
        let (valid, existing, exists) = blocking(move || {
            Ok::<_, git::GitError>((
                git::valid_branch_name(&r, &b),
                git::worktree_for_branch(&r, &b)?,
                git::branch_exists(&r, &b)?,
            ))
        })
        .await
        .map_err(git_error)?;
        if !valid {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                format!("{branch:?} isn't a valid branch name"),
            ));
        }
        if let Some(existing) = existing {
            if root.canonicalize().ok().as_ref() == Some(&existing) {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    format!("{branch} is the project's current branch"),
                ));
            }
            let pid = project.id.clone();
            let sessions = self.db(move |s| s.sessions(Some(&pid))).await?;
            let busy = sessions.iter().find(|s| {
                s.state != SessionState::Ended
                    && s.worktree_path
                        .as_ref()
                        .and_then(|w| PathBuf::from(w).canonicalize().ok())
                        .as_ref()
                        == Some(&existing)
            });
            if let Some(busy) = busy {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    format!(
                        "the worktree on {branch} is in use by session {} ({})",
                        busy.id,
                        state_name(busy.state)
                    ),
                ));
            }
            return Ok(existing);
        }
        let path = self.paths.worktree(&project.id, id);
        let (r, wt, b) = (root, path.clone(), branch.to_string());
        let base = base.unwrap_or_else(|| "HEAD".into());
        blocking(move || {
            if exists {
                git::worktree_add_existing(&r, &wt, &b)
            } else {
                git::worktree_add(&r, &wt, &b, &base)
            }
        })
        .await
        .map_err(git_error)?;
        Ok(path)
    }

    fn live(&self, id: &SessionId) -> Option<mpsc::UnboundedSender<Cmd>> {
        self.live.lock().unwrap().get(id).cloned()
    }

    /// Sends a command to a live session, or explains why there is none.
    async fn command<T>(
        &self,
        id: &SessionId,
        make: impl FnOnce(oneshot::Sender<HostResult<T>>) -> Cmd,
    ) -> HostResult<T> {
        let Some(tx) = self.live(id) else {
            return Err(self.not_live(id).await);
        };
        let (reply, rx) = oneshot::channel();
        if tx.send(make(reply)).is_err() {
            return Err(self.not_live(id).await);
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(self.not_live(id).await),
        }
    }

    async fn not_live(&self, id: &SessionId) -> RpcError {
        let sid = id.clone();
        match self.db(move |s| s.session(&sid)).await {
            Err(e) => e,
            Ok(s) => RpcError::new(
                ErrorKind::SessionEnded,
                format!("session {id} has no running agent ({:?})", s.state),
            ),
        }
    }

    /// Sends a prompt for the client named `client`. A detached session is
    /// reattached first (resume, else load).
    pub async fn send(
        self: &Arc<Self>,
        id: &SessionId,
        text: String,
        client: Option<String>,
        interrupt: bool,
    ) -> HostResult<u32> {
        // Twice at most: an idle agent may be stopped between the check and the
        // command, leaving the session detached.
        for attempt in 0..2 {
            if self.live(id).is_none() {
                self.reattach(id).await?;
            }
            let r = self
                .command(id, |reply| Cmd::Send {
                    text: text.clone(),
                    client: client.clone(),
                    interrupt,
                    reply,
                })
                .await;
            match r {
                Err(_) if attempt == 0 && self.live(id).is_none() && self.is_detached(id).await => {
                }
                r => return r,
            }
        }
        unreachable!("the second attempt returns")
    }

    async fn is_detached(&self, id: &SessionId) -> bool {
        let sid = id.clone();
        matches!(
            self.db(move |s| s.session(&sid)).await,
            Ok(s) if s.state == SessionState::Detached
        )
    }

    /// Starts a new agent process for a detached session and reattaches it to the
    /// agent's session: `session/resume` if the agent can, else `session/load` (whose
    /// replay is dropped; history comes from the event log). On failure the session
    /// stays detached.
    async fn reattach(self: &Arc<Self>, id: &SessionId) -> HostResult<()> {
        let sid = id.clone();
        let (detail, agent_session_id, checkpoints) = self
            .db(move |s| {
                Ok((
                    s.session_detail(&sid)?,
                    s.agent_session_id(&sid)?,
                    s.checkpoints(&sid)?,
                ))
            })
            .await?;
        let summary = &detail.summary;
        if summary.state != SessionState::Detached {
            return Err(RpcError::new(
                ErrorKind::SessionEnded,
                format!("session {id} is {}", state_name(summary.state)),
            ));
        }
        let Some(agent_session_id) = agent_session_id else {
            return Err(RpcError::new(
                ErrorKind::SessionEnded,
                "the agent never created its session",
            ));
        };
        let agent = self
            .agents
            .agents
            .get(&summary.agent)
            .cloned()
            .ok_or_else(|| {
                RpcError::new(
                    ErrorKind::AgentNotFound,
                    format!("no agent named {} in the agents config", summary.agent),
                )
            })?;
        let sid = id.clone();
        self.db(move |s| {
            s.append(
                &sid,
                state_event(SessionState::Starting, Some("reattaching")),
            )
        })
        .await?;
        let mut task = SessionTask::new(
            self.clone(),
            id.clone(),
            PathBuf::from(&summary.cwd),
            summary.worktree_path.is_some(),
            agent,
            summary.agent.clone(),
            summary.level,
            SessionOptions {
                clean: summary.clean,
                sandboxed: summary.sandbox.is_some(),
                web_tools: summary.web_tools,
                telemetry: summary.telemetry,
            },
            summary.sandbox.clone(),
            detail.config.clone(),
            Default::default(),
        );
        task.last_commit = checkpoints.last().map(|c| c.commit.clone());
        let (ready, rx) = oneshot::channel();
        tokio::spawn(task.run(Attach::Existing(agent_session_id), None, Some(ready)));
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(RpcError::new(ErrorKind::Internal, "reattaching stopped")),
        }
    }

    /// After a restart: sessions left starting, idle or running lose their
    /// agent. Leftover agent processes are killed first (and waited for), so nothing
    /// writes while the end checkpoint is taken. Then open requests are resolved
    /// `actuo_restart`, a running turn ends `actuo_stopped` with its checkpoint and
    /// files, and the session is detached if the agent can resume or load, else ended.
    pub async fn recover(&self) {
        let left = match self
            .db(|s| {
                s.sessions_in_states(&[
                    SessionState::Starting,
                    SessionState::Idle,
                    SessionState::Running,
                ])
            })
            .await
        {
            Ok(l) => l,
            Err(e) => {
                crate::log!("recovery: listing sessions failed: {e}");
                return;
            }
        };
        for s in left {
            if let Err(e) = self.recover_session(&s).await {
                crate::log!("recovery of session {}: {e}", s.id);
            }
        }
    }

    async fn recover_session(&self, s: &SessionSummary) -> HostResult<()> {
        let id = s.id.clone();
        let sid = id.clone();
        if let Some(p) = self.db(move |st| st.agent_process(&sid)).await? {
            crate::process::kill_leftover(p.pid, &p.started_at).await;
            let sid = id.clone();
            self.db(move |st| st.set_agent_process(&sid, None)).await?;
        }
        let sid = id.clone();
        let (open, running, checkpoints, agent_session, can) = self
            .db(move |st| {
                Ok((
                    st.open_requests(&sid)?,
                    st.running_turn(&sid)?,
                    st.checkpoints(&sid)?,
                    st.agent_session_id(&sid)?,
                    st.agent_can_continue(&sid)?,
                ))
            })
            .await?;
        for r in open {
            let sid = id.clone();
            let turn = r.turn;
            self.db(move |st| {
                st.append(
                    &sid,
                    NewEvent::new(EventBody::RequestResolved(RequestResolvedData {
                        request_id: r.request_id,
                        outcome: RequestOutcome::Cancelled,
                        option_id: None,
                        option_kind: None,
                        by: ResolvedBy::ActuoRestart,
                        client: None,
                    }))
                    .turn(turn),
                )
            })
            .await?;
        }
        if let Some(n) = running {
            close_unfinished_tools(self, &id, n).await;
            let before = checkpoints
                .iter()
                .find(|c| c.n + 1 == n)
                .map(|c| c.commit.clone());
            let files = match capture(self, &id, PathBuf::from(&s.cwd), n).await {
                Ok(commit) => match before {
                    Some(before) => {
                        let cwd = PathBuf::from(&s.cwd);
                        blocking(move || git::diff_name_status(&cwd, &before, &commit))
                            .await
                            .unwrap_or_default()
                    }
                    None => Vec::new(),
                },
                Err(_) => Vec::new(),
            };
            let sid = id.clone();
            self.db(move |st| {
                st.append(
                    &sid,
                    NewEvent::new(EventBody::TurnCompleted(TurnCompletedData {
                        stop_reason: StopReason::ActuoStopped,
                        tokens: None,
                        cost: None,
                        files,
                    }))
                    .turn(n),
                )
            })
            .await?;
        }
        let continuable = agent_session.is_some() && (can.0 == Some(true) || can.1 == Some(true));
        let (state, reason) = if continuable {
            (SessionState::Detached, "actuo restarted")
        } else {
            (
                SessionState::Ended,
                "actuo restarted; the agent can't resume or load",
            )
        };
        let sid = id.clone();
        self.db(move |st| st.append(&sid, state_event(state, Some(reason))))
            .await?;
        Ok(())
    }

    pub async fn interrupt(&self, id: &SessionId) -> HostResult<()> {
        self.command(id, |reply| Cmd::Interrupt { reply }).await
    }

    /// Answers (or cancels) a request for the client named `client`.
    pub async fn respond(
        &self,
        id: &SessionId,
        request: RequestId,
        option: Option<PermissionOptionId>,
        cancel: bool,
        client: Option<String>,
    ) -> HostResult<()> {
        if option.is_some() == cancel {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                "give exactly one of optionId and cancel: true",
            ));
        }
        self.command(id, |reply| Cmd::Respond {
            request,
            option,
            client,
            reply,
        })
        .await
    }

    pub async fn stop(&self, id: &SessionId) -> HostResult<()> {
        let Some(tx) = self.live(id) else {
            let sid = id.clone();
            let s = self.db(move |s| s.session(&sid)).await?;
            if s.state != SessionState::Ended {
                let sid = id.clone();
                self.db(move |s| s.append(&sid, state_event(SessionState::Ended, Some("stopped"))))
                    .await?;
            }
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        if tx.send(Cmd::Stop { reply }).is_ok() {
            let _ = rx.await;
        }
        Ok(())
    }

    pub async fn revert(
        &self,
        id: &SessionId,
        turn: u32,
        force: bool,
    ) -> HostResult<CheckpointRevertResult> {
        if self.live(id).is_some() {
            return self
                .command(id, |reply| Cmd::Revert { turn, force, reply })
                .await;
        }
        // Without an agent (detached or ended), files can still be reverted.
        let sid = id.clone();
        let s = self.db(move |s| s.session(&sid)).await?;
        revert_files(
            self,
            id,
            PathBuf::from(&s.cwd),
            s.worktree_path.is_some(),
            turn,
            force,
        )
        .await
    }

    /// What the session's working tree changes against its base (`session/diff`).
    /// Reads only, so it runs outside the session's task, also while a turn runs.
    pub async fn diff(&self, id: &SessionId) -> HostResult<SessionDiffResult> {
        let sid = id.clone();
        let s = self.db(move |s| s.session(&sid)).await?;
        let cwd = PathBuf::from(&s.cwd);
        let base = match (&s.base, &s.worktree_path) {
            (Some(base), _) => {
                let (cwd, base) = (cwd.clone(), base.clone());
                blocking(move || git::merge_base(&cwd, &base, "HEAD"))
                    .await
                    .map_err(git_error)?
            }
            (None, Some(_)) => {
                let sid = id.clone();
                self.db(move |s| s.checkpoints(&sid))
                    .await?
                    .into_iter()
                    .find(|c| c.n == 0)
                    .map(|c| c.commit)
                    .ok_or_else(|| {
                        RpcError::new(
                            ErrorKind::GitError,
                            "the session has no checkpoint from its start to diff against",
                        )
                    })?
            }
            (None, None) => {
                return Err(RpcError::new(
                    ErrorKind::GitError,
                    "the session runs in the project checkout and has no base to diff against",
                ));
            }
        };
        let against = base.clone();
        let diff = blocking(move || git::diff(&cwd, &against))
            .await
            .map_err(git_error)?;
        Ok(SessionDiffResult {
            base,
            files: diff
                .files
                .into_iter()
                .map(|(c, status)| DiffFile {
                    path: c.path,
                    status,
                    additions: c.added,
                    deletions: c.deleted,
                    binary: c.binary,
                })
                .collect(),
            patch: diff.patch,
            truncated: diff.truncated,
        })
    }

    /// Lets go of every agent (actuo is stopping): running turns end with
    /// `actuo_stopped` and sessions are left detached or ended.
    pub async fn shutdown(&self) {
        let live: Vec<_> = self.live.lock().unwrap().values().cloned().collect();
        let mut waits = Vec::new();
        for tx in live {
            let (reply, rx) = oneshot::channel();
            if tx.send(Cmd::Detach { reply }).is_ok() {
                waits.push(rx);
            }
        }
        for rx in waits {
            let _ = rx.await;
        }
    }
}

struct SessionTask {
    host: Arc<Host>,
    id: SessionId,
    cwd: PathBuf,
    worktree: bool,
    agent_config: AgentConfig,
    /// The agent's name in actuo's agents config.
    agent_name: String,
    level: Level,
    opts: SessionOptions,
    sandbox: Option<SandboxOptions>,
    config: BTreeMap<String, ConfigValue>,
    /// Config ids a client asked for in `session/start`: refused if the agent
    /// doesn't offer the value. The rest (defaults) are skipped with a note.
    explicit: std::collections::BTreeSet<String>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    agent: Option<(AgentHandle, mpsc::UnboundedReceiver<AgentEvent>)>,
    /// The running turn.
    turn: Option<u32>,
    /// The commit of the last checkpoint (the one before the next turn).
    last_commit: Option<String>,
    /// Open permission requests and their options.
    open: HashMap<RequestId, Vec<PermissionOption>>,
    /// The agent can resume or load, so the session can be continued later.
    can_continue: bool,
    /// Set while letting go of the agent: why, and who to tell when it's gone.
    stopping: Option<(LetGo, Option<oneshot::Sender<()>>)>,
    /// A prompt sent with `interrupt`, waiting for the interrupted turn to end.
    pending: Option<PendingSend>,
}

/// A `session/send` with `interrupt`: sent when the running turn has ended, or
/// refused at `deadline`.
struct PendingSend {
    text: String,
    client: Option<String>,
    reply: oneshot::Sender<HostResult<u32>>,
    deadline: tokio::time::Instant,
}

/// Why a session lets go of its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LetGo {
    /// `session/stop`: the session ends.
    End,
    /// Actuo is stopping: the session is detached.
    Detach,
    /// Idle for `sessions.idle_agent_timeout`: detached, reattached on the next prompt.
    Idle,
}

/// What woke the session task. Lives only for one loop turn, so its size doesn't
/// matter.
#[allow(clippy::large_enum_variant)]
enum Wake {
    Cmd(Cmd),
    Agent(Option<AgentEvent>),
    Idle,
    /// A prompt sent with `interrupt` waited too long for its turn to end.
    PendingExpired,
    SettingsChanged,
}

/// How to start an agent process, for a session or an options probe
/// (`crate::probe`).
pub(crate) struct Launch<'a> {
    pub host: &'a Host,
    pub agent_config: &'a AgentConfig,
    pub opts: SessionOptions,
    /// Inside `srt` with these, or not sandboxed.
    pub sandbox: Option<&'a SandboxOptions>,
    /// Names the sandbox's settings file and temp dir.
    pub name: &'a str,
    pub cwd: &'a std::path::Path,
    pub stderr_log: Option<PathBuf>,
    /// How `srt`'s refusals are labeled in the log.
    pub records: String,
}

/// The agent's process: its environment (a token or API key from actuo's
/// environment, to this agent only, under everything the agent's config sets),
/// its directories and files, its program, inside `srt` when sandboxed.
pub(crate) async fn agent_spec(l: Launch<'_>) -> Result<AgentSpec, SetupError> {
    let a = l.agent_config;
    let mut env = crate::pass_env::env_for(&a.pass_env);
    env.extend(a.env_for(l.opts));
    // Directories a session needs (e.g. CODEX_HOME), listed in the agents config
    // as `clean.dirs` and `sandbox.dirs`. Nothing is copied into them.
    for dir in a.dirs_for(l.opts) {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    // Files a clean session needs (e.g. Codex's config.toml), rewritten each time.
    for (path, content) in a.files_for(l.opts) {
        tokio::fs::write(&path, content).await.map_err(|e| {
            SetupError(
                ErrorKind::Internal,
                format!("writing {}: {e}", path.display()),
            )
        })?;
    }
    let (mut command, mut args) = a.program(&l.host.paths);
    let mut clear_env = false;
    let mut srt_records = None;
    if let Some(sb) = l.sandbox {
        let (c, x, e) = sandboxed(&l, sb, &command, &args, env)
            .await
            .map_err(|e| SetupError(ErrorKind::SandboxUnavailable, e))?;
        (command, args, env, clear_env) = (c, x, e, true);
        srt_records = Some(l.records.clone());
    }
    Ok(AgentSpec {
        command,
        args,
        env,
        clear_env,
        cwd: l.cwd.to_path_buf(),
        stderr_log: l.stderr_log,
        srt_records,
    })
}

/// The agent inside `srt`: its settings (read: the working folder, its git dir,
/// the installed adapters, the agent's state dirs, the temp dir, the agent's and
/// the launch's `read` lists; write: the working folder, the state dirs, the temp
/// dir; hosts: the agent's and the launch's), and the environment it starts with
/// (the allowlist, the temp dir, the generated git config, the agent's env).
async fn sandboxed(
    l: &Launch<'_>,
    sb: &SandboxOptions,
    program: &str,
    args: &[String],
    agent_env: BTreeMap<String, String>,
) -> Result<(String, Vec<String>, BTreeMap<String, String>), String> {
    l.host.sandbox_ready().await.map_err(|e| e.message)?;
    let paths = l.host.paths.clone();
    let (name, cwd) = (l.name.to_string(), l.cwd.to_path_buf());
    let a = l.agent_config;
    let state_dirs = a.sandbox_dirs(l.opts);
    let read_lists: Vec<PathBuf> = a
        .sandbox
        .read
        .iter()
        .chain(&sb.read)
        .map(|r| PathBuf::from(crate::config::expand_home(r)))
        .collect();
    let tmp_env = a.sandbox.tmp_env.clone();
    let hosts: Vec<String> = a.sandbox.hosts.iter().chain(&sb.hosts).cloned().collect();
    let (program, args) = (program.to_string(), args.to_vec());
    blocking(move || {
        let tmp = crate::sandbox::temp_dir(&paths, &name).map_err(|e| e.to_string())?;
        let git = crate::sandbox::gitconfig(&paths).map_err(|e| e.to_string())?;
        let common = git::common_dir(&cwd).map_err(|e| e.to_string())?;
        // A real git dir is never inside the working tree; a `.git` file the agent
        // rewrote to point inside it would be.
        if common.starts_with(&cwd) {
            return Err(format!(
                "{}'s .git points inside the worktree",
                cwd.display()
            ));
        }
        let mut read = vec![
            cwd.clone(),
            common,
            paths.home.join("agents"),
            paths.home.join("node"),
            git.clone(),
        ];
        read.extend(read_lists);
        let mut write = vec![cwd.clone(), tmp.clone()];
        write.extend(state_dirs);
        let policy = crate::sandbox::Policy {
            read,
            write,
            hosts,
            local_binding: false,
            unix_sockets: false,
        };
        let file =
            crate::sandbox::write_settings(&paths, &name, &policy).map_err(|e| e.to_string())?;
        let mut extra = agent_env;
        // The agent's own pnpm runs (checks, `pnpm exec`) as setup's and the checks' do.
        for (k, v) in crate::sandbox::PACKAGE_MANAGER_ENV {
            extra.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
        for k in tmp_env {
            extra.insert(k, tmp.display().to_string());
        }
        let mut env = crate::sandbox::env(&tmp, &git, &extra);
        env.insert(
            "NODE_OPTIONS".into(),
            crate::sandbox::records_node_options(&paths).map_err(|e| e.to_string())?,
        );
        let (c, a) = crate::sandbox::wrap(&paths, &file, true, &program, &args);
        Ok((c, a, env))
    })
    .await
}

pub(crate) struct SetupError(pub ErrorKind, pub String);

/// A new agent session, or the agent's existing one to reattach to.
enum Attach {
    New,
    Existing(String),
}

impl SessionTask {
    #[allow(clippy::too_many_arguments)]
    fn new(
        host: Arc<Host>,
        id: SessionId,
        cwd: PathBuf,
        worktree: bool,
        agent_config: AgentConfig,
        agent_name: String,
        level: Level,
        opts: SessionOptions,
        sandbox: Option<SandboxOptions>,
        config: BTreeMap<String, ConfigValue>,
        explicit: std::collections::BTreeSet<String>,
    ) -> Self {
        let (tx, cmds) = mpsc::unbounded_channel();
        host.live.lock().unwrap().insert(id.clone(), tx);
        Self {
            host,
            id,
            cwd,
            worktree,
            agent_config,
            agent_name,
            level,
            opts,
            sandbox,
            config,
            explicit,
            cmds,
            agent: None,
            turn: None,
            last_commit: None,
            open: HashMap::new(),
            can_continue: false,
            stopping: None,
            pending: None,
        }
    }

    /// Sets the agent up, then serves commands and agent events until the agent is
    /// gone. `ready` hears how setup went (reattach waits for it).
    async fn run(
        mut self,
        attach: Attach,
        prompt: Option<(String, Option<String>)>,
        ready: Option<oneshot::Sender<HostResult<()>>>,
    ) {
        let reattaching = matches!(attach, Attach::Existing(_));
        if let Err(SetupError(kind, mut message)) = self.setup(attach).await {
            if kind == ErrorKind::AuthRequired {
                message = format!(
                    "{message}: {}",
                    login_hint(&self.agent_name, self.sandbox.is_some())
                );
            }
            // A failed reattach leaves the session detached, to try again later.
            self.error(kind, &message, !reattaching).await;
            if let Some((agent, _)) = &self.agent {
                agent.shutdown().await;
            }
            self.clear_process().await;
            let state = if reattaching {
                SessionState::Detached
            } else {
                SessionState::Ended
            };
            self.set_state(state, Some(&message)).await;
            self.host.live.lock().unwrap().remove(&self.id);
            if let Some(ready) = ready {
                let _ = ready.send(Err(RpcError::new(kind, message)));
            }
            return;
        }
        if let Some(ready) = ready {
            let _ = ready.send(Ok(()));
        }
        if let Some((text, client)) = prompt {
            let _ = self.start_turn(text, client).await;
        }
        self.run_loop().await;
        if let Some(p) = self.pending.take() {
            let _ = p.reply.send(Err(RpcError::new(
                ErrorKind::SessionEnded,
                "the agent stopped before the interrupted turn ended; nothing was sent",
            )));
        }
        self.host.live.lock().unwrap().remove(&self.id);
    }

    async fn setup(&mut self, attach: Attach) -> Result<(), SetupError> {
        // An adapter with a pinned package runs from its own install, on actuo's
        // pinned Node.
        if self.agent_config.package.is_some() && !crate::node::bin(&self.host.paths).exists() {
            self.set_state(
                SessionState::Starting,
                Some(&format!("installing node {}", crate::node::VERSION)),
            )
            .await;
            crate::node::ensure(&self.host.paths)
                .await
                .map_err(|e| SetupError(ErrorKind::AgentError, e))?;
        }
        if let (Some(dir), Some(package), Some(version), Some(bin)) = (
            self.agent_config.install_dir(&self.host.paths),
            self.agent_config.package.clone(),
            self.agent_config.version.clone(),
            self.agent_config.bin.clone(),
        ) && !dir.join("node_modules/.bin").join(&bin).exists()
        {
            self.set_state(
                SessionState::Starting,
                Some(&format!("installing {package}@{version}")),
            )
            .await;
            crate::sandbox::install(&self.host.paths, &dir, &package, &version, &bin)
                .await
                .map_err(|e| SetupError(ErrorKind::AgentError, e))?;
        }
        let spec = agent_spec(Launch {
            host: &self.host,
            agent_config: &self.agent_config,
            opts: self.opts,
            sandbox: self.sandbox.as_ref(),
            name: &self.id.0,
            cwd: &self.cwd,
            stderr_log: Some(self.host.paths.agent_log(&self.id)),
            records: format!("session {}", self.id),
        })
        .await?;
        let (agent, events) = self
            .host
            .backend
            .spawn(spec)
            .map_err(|e| SetupError(ErrorKind::AgentError, e.to_string()))?;
        let pid = agent.pid();
        let started_at = blocking(move || Ok::<_, ()>(crate::process::start_time(pid)))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let process = crate::store::AgentProcess { pid, started_at };
        let sid = self.id.clone();
        let _ = self
            .host
            .db(move |s| s.set_agent_process(&sid, Some(&process)))
            .await;
        self.agent = Some((agent.clone(), events));

        let info = agent.initialize().await.map_err(setup_error)?;
        self.can_continue = info.can_resume || info.can_load;
        let meta = self.agent_config.meta_for(self.opts);
        let (setup, how) = match &attach {
            Attach::New => (agent.new_session(meta).await.map_err(setup_error)?, None),
            Attach::Existing(sid) if info.can_resume => {
                match agent.resume_session(sid, meta.clone()).await {
                    Ok(s) => (s, Some("reattached with session/resume")),
                    Err(_) if info.can_load => (
                        agent.load_session(sid, meta).await.map_err(setup_error)?,
                        Some("reattached with session/load after session/resume failed"),
                    ),
                    Err(e) => return Err(setup_error(e)),
                }
            }
            Attach::Existing(sid) if info.can_load => (
                agent.load_session(sid, meta).await.map_err(setup_error)?,
                Some("reattached with session/load"),
            ),
            Attach::Existing(_) => {
                return Err(SetupError(
                    ErrorKind::SessionEnded,
                    "the agent no longer advertises resume or load".into(),
                ));
            }
        };

        let applied_mode = match self.agent_config.modes.get(&self.level) {
            Some(mode) => {
                if !setup.modes.is_empty() && !setup.modes.iter().any(|m| &m.id == mode) {
                    return Err(SetupError(
                        ErrorKind::InvalidConfigOption,
                        format!("the agent has no mode {mode}"),
                    ));
                }
                agent.set_mode(mode).await.map_err(setup_error)?;
                Some(mode.clone())
            }
            None => None,
        };

        let mut options = setup.config_options.clone();
        let mut applied = Vec::new();
        let mut skipped = Vec::new();
        // The model first: the other options and their choices depend on it.
        let ordered = self
            .config
            .iter()
            .filter(|(id, _)| id.as_str() == "model")
            .chain(self.config.iter().filter(|(id, _)| id.as_str() != "model"));
        for (id, value) in ordered {
            let refusal = match options.iter().find(|o| &o.id == id) {
                None => Some(format!("the agent has no config option {id}")),
                Some(option) => {
                    let valid = match (option.option_type, value) {
                        (ConfigOptionType::Select, ConfigValue::String(v)) => {
                            option.values.iter().any(|c| &c.value == v)
                        }
                        (ConfigOptionType::Boolean, ConfigValue::Bool(_)) => true,
                        _ => false,
                    };
                    (!valid).then(|| format!("{id} can't be {value:?}"))
                }
            };
            if let Some(refusal) = refusal {
                if self.explicit.contains(id) {
                    return Err(SetupError(ErrorKind::InvalidConfigOption, refusal));
                }
                let shown = match value {
                    ConfigValue::String(s) => format!("\"{s}\""),
                    ConfigValue::Bool(b) => b.to_string(),
                };
                skipped.push(format!(
                    "skipped the default {id} = {shown}: the agent doesn't offer it here"
                ));
                continue;
            }
            let after = agent
                .set_config(id, value.clone())
                .await
                .map_err(setup_error)?;
            if !after.is_empty() {
                options = after;
            }
            applied.push(AppliedConfig {
                id: id.clone(),
                requested: value.clone(),
                current: options
                    .iter()
                    .find(|o| &o.id == id)
                    .and_then(|o| o.current_value.clone()),
            });
        }

        // What the agent offers here, against what the last probe found.
        tokio::spawn(crate::probe::check(
            self.host.clone(),
            self.id.clone(),
            self.opts.sandboxed,
            options.clone(),
        ));
        for message in skipped {
            self.append(NewEvent::new(EventBody::Error(ErrorEventData {
                kind: ErrorKind::InvalidConfigOption,
                message,
                fatal: false,
            })))
            .await;
        }
        self.append(NewEvent::new(EventBody::SessionAgent(SessionAgentData {
            agent_info: info.implementation,
            agent_session_id: setup.agent_session_id,
            can_load: info.can_load,
            can_resume: info.can_resume,
            modes: setup.modes,
            applied_mode,
            config_options: options,
            applied_config: applied,
        })))
        .await;

        if matches!(attach, Attach::New) {
            let commit = self
                .capture(0)
                .await
                .map_err(|e| SetupError(ErrorKind::GitError, e.message))?;
            self.last_commit = Some(commit);
        }
        self.set_state(SessionState::Idle, how).await;
        Ok(())
    }

    async fn run_loop(&mut self) {
        let mut settings = self.host.settings.subscribe();
        let mut active = tokio::time::Instant::now();
        loop {
            let idle_at = self.idle_deadline(active);
            let pending_at = self.pending.as_ref().map(|p| p.deadline);
            let wake = {
                let Some((_, events)) = self.agent.as_mut() else {
                    return;
                };
                tokio::select! {
                    cmd = self.cmds.recv() => match cmd {
                        Some(cmd) => Wake::Cmd(cmd),
                        // The host is gone.
                        None => return,
                    },
                    event = events.recv() => Wake::Agent(event),
                    _ = sleep_until(idle_at) => Wake::Idle,
                    _ = sleep_until(pending_at) => Wake::PendingExpired,
                    Ok(()) = settings.changed() => Wake::SettingsChanged,
                }
            };
            match wake {
                Wake::Cmd(cmd) => {
                    self.command(cmd).await;
                    active = tokio::time::Instant::now();
                }
                Wake::Agent(Some(AgentEvent::Exited(info))) => {
                    self.exited(Some(info)).await;
                    return;
                }
                Wake::Agent(None) => {
                    self.exited(None).await;
                    return;
                }
                Wake::Agent(Some(event)) => {
                    self.agent_event(event).await;
                    // The interrupted turn has ended: send the waiting prompt before
                    // anything else can start a turn.
                    if self.turn.is_none()
                        && let Some(p) = self.pending.take()
                    {
                        let _ = p.reply.send(self.start_turn(p.text, p.client).await);
                    }
                    active = tokio::time::Instant::now();
                }
                Wake::PendingExpired => {
                    if let Some(p) = self.pending.take() {
                        let _ = p.reply.send(Err(RpcError::new(
                            ErrorKind::TurnInProgress,
                            "the interrupted turn didn't end in time; nothing was sent",
                        )));
                    }
                }
                Wake::Idle => {
                    self.stop_idle().await;
                    return;
                }
                // The deadline is worked out again with the new timeout.
                Wake::SettingsChanged => {}
            }
        }
    }

    /// When the agent counts as idle: no turn running, no open request, an agent
    /// that can be reattached, and a timeout that isn't `off`.
    fn idle_deadline(&self, active: tokio::time::Instant) -> Option<tokio::time::Instant> {
        if self.turn.is_some()
            || !self.open.is_empty()
            || !self.can_continue
            || self.stopping.is_some()
        {
            return None;
        }
        settings::idle_agent_timeout(&self.host.settings.borrow()).map(|t| active + t)
    }

    /// Stops an idle agent and waits until it is gone; the session goes `detached`
    /// (reason `idle`) and the next prompt reattaches it.
    async fn stop_idle(&mut self) {
        self.stopping = Some((LetGo::Idle, None));
        let Some((agent, _)) = &self.agent else {
            return;
        };
        agent.shutdown().await;
        loop {
            let Some((_, events)) = self.agent.as_mut() else {
                return;
            };
            match events.recv().await {
                Some(AgentEvent::Exited(info)) => return self.exited(Some(info)).await,
                None => return self.exited(None).await,
                Some(event) => self.agent_event(event).await,
            }
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Send {
                text,
                client,
                interrupt,
                reply,
            } => {
                if interrupt && self.turn.is_some() {
                    if self.pending.is_some() {
                        let _ = reply.send(Err(RpcError::new(
                            ErrorKind::TurnInProgress,
                            "another prompt is already waiting for this turn to end",
                        )));
                        return;
                    }
                    self.interrupt().await;
                    self.pending = Some(PendingSend {
                        text,
                        client,
                        reply,
                        deadline: tokio::time::Instant::now()
                            + *self.host.interrupt_grace.lock().unwrap(),
                    });
                } else {
                    let _ = reply.send(self.start_turn(text, client).await);
                }
            }
            Cmd::Interrupt { reply } => {
                let r = match self.turn {
                    None => Err(RpcError::new(
                        ErrorKind::NoTurnRunning,
                        "no turn is running",
                    )),
                    Some(_) => {
                        self.interrupt().await;
                        Ok(())
                    }
                };
                let _ = reply.send(r);
            }
            Cmd::Respond {
                request,
                option,
                client,
                reply,
            } => {
                let _ = reply.send(self.respond(request, option, client).await);
            }
            Cmd::Revert { turn, force, reply } => {
                let r = if self.turn.is_some() {
                    Err(RpcError::new(
                        ErrorKind::TurnInProgress,
                        "can't revert while a turn is running",
                    ))
                } else {
                    revert_files(
                        &self.host,
                        &self.id,
                        self.cwd.clone(),
                        self.worktree,
                        turn,
                        force,
                    )
                    .await
                };
                let _ = reply.send(r);
            }
            Cmd::Stop { reply } => self.let_go(LetGo::End, reply).await,
            Cmd::Detach { reply } => self.let_go(LetGo::Detach, reply).await,
        }
    }

    async fn start_turn(&mut self, text: String, client: Option<String>) -> HostResult<u32> {
        if self.turn.is_some() {
            return Err(RpcError::new(
                ErrorKind::TurnInProgress,
                "a turn is already running",
            ));
        }
        let Some((agent, _)) = &self.agent else {
            return Err(RpcError::new(ErrorKind::SessionEnded, "no agent"));
        };
        let agent = agent.clone();
        let sid = self.id.clone();
        let (turns, reverted) = self
            .host
            .db(move |s| Ok((s.session(&sid)?.turn_count, s.revert_since_last_turn(&sid)?)))
            .await?;
        let n = turns + 1;
        if let Some(t) = reverted {
            // The files are as they were before turn t: this turn's changes are
            // measured from there, not from the last checkpoint.
            let sid = self.id.clone();
            let restored = self
                .host
                .db(move |s| s.checkpoints(&sid))
                .await?
                .into_iter()
                .find(|c| c.n + 1 == t)
                .map(|c| c.commit);
            if restored.is_some() {
                self.last_commit = restored;
            }
        }
        let text = match reverted {
            Some(t) => format!(
                "[Actuo restored the files to how they were before turn {t}. Changes made since are gone.]\n\n{text}"
            ),
            None => text,
        };
        self.turn = Some(n);
        self.append(
            NewEvent::new(EventBody::TurnStarted(TurnStartedData {
                text: text.clone(),
                client,
            }))
            .turn(n),
        )
        .await;
        self.set_state(SessionState::Running, None).await;
        if let Err(e) = agent.prompt(n, &text).await {
            self.error(agent_error_kind(&e), &e.to_string(), false)
                .await;
            self.finish_turn(StopReason::Failed, None, None, None, true)
                .await;
        }
        Ok(n)
    }

    async fn agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Event(Mapped { item_id, body, raw }) => {
                if let EventBody::RequestOpened(d) = &body {
                    self.open.insert(d.request_id.clone(), d.options.clone());
                }
                let mut e = NewEvent::new(body);
                e.turn = self.turn;
                e.item_id = item_id;
                e.raw = raw;
                self.append(e).await;
            }
            AgentEvent::TurnEnded(end) => {
                // Requests still open when the agent ends its turn were cut off by it;
                // agents normally wait for the answer, so this is only a safety net.
                self.resolve_open(ResolvedBy::TurnEnded).await;
                self.finish_turn(end.stop_reason, end.tokens, end.cost, end.raw, true)
                    .await;
            }
            AgentEvent::TurnFailed(e) => {
                self.resolve_open(ResolvedBy::TurnEnded).await;
                self.error(agent_error_kind(&e), &e.to_string(), false)
                    .await;
                self.finish_turn(StopReason::Failed, None, None, None, true)
                    .await;
            }
            AgentEvent::Exited(_) => unreachable!("handled by the loop"),
        }
    }

    /// Cancels the turn: ACP `session/cancel`, then every open request is answered
    /// `cancelled`, as ACP requires. The turn ends when the agent answers the prompt.
    async fn interrupt(&mut self) {
        if let Some((agent, _)) = &self.agent {
            let _ = agent.cancel();
        }
        self.resolve_open(ResolvedBy::Interrupt).await;
    }

    async fn respond(
        &mut self,
        request: RequestId,
        option: Option<PermissionOptionId>,
        client: Option<String>,
    ) -> HostResult<()> {
        let Some(options) = self.open.get(&request) else {
            return Err(RpcError::new(
                ErrorKind::RequestNotFound,
                format!("no open request {request}"),
            ));
        };
        let kind = match &option {
            Some(id) => Some(
                options
                    .iter()
                    .find(|o| &o.option_id == id)
                    .map(|o| o.kind)
                    .ok_or_else(|| {
                        RpcError::new(
                            ErrorKind::InvalidOption,
                            format!("{id} isn't an option of {request}"),
                        )
                    })?,
            ),
            None => None,
        };
        self.open.remove(&request);
        if let Some((agent, _)) = &self.agent {
            let _ = agent.respond(request.clone(), option.clone());
        }
        self.append(
            NewEvent::new(EventBody::RequestResolved(RequestResolvedData {
                request_id: request,
                outcome: if option.is_some() {
                    RequestOutcome::Selected
                } else {
                    RequestOutcome::Cancelled
                },
                option_id: option,
                option_kind: kind,
                by: ResolvedBy::Client,
                client,
            }))
            .turn_opt(self.turn),
        )
        .await;
        Ok(())
    }

    /// Answers every open request `cancelled` and records who ended it.
    async fn resolve_open(&mut self, by: ResolvedBy) {
        let mut open: Vec<_> = self.open.drain().map(|(id, _)| id).collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        for request in open {
            if let Some((agent, _)) = &self.agent {
                let _ = agent.respond(request.clone(), None);
            }
            self.append(
                NewEvent::new(EventBody::RequestResolved(RequestResolvedData {
                    request_id: request,
                    outcome: RequestOutcome::Cancelled,
                    option_id: None,
                    option_kind: None,
                    by,
                    client: None,
                }))
                .turn_opt(self.turn),
            )
            .await;
        }
    }

    /// Takes the checkpoint after the turn, then records how it ended with the files
    /// it changed. `checkpoint.created` comes first (the store needs that order).
    async fn finish_turn(
        &mut self,
        stop_reason: StopReason,
        tokens: Option<TokenUsage>,
        cost: Option<Cost>,
        raw: Option<Value>,
        idle: bool,
    ) {
        let Some(n) = self.turn.take() else {
            return;
        };
        close_unfinished_tools(&self.host, &self.id, n).await;
        let mut files = Vec::new();
        match self.capture(n).await {
            Ok(commit) => {
                if let Some(before) = self.last_commit.replace(commit.clone()) {
                    let cwd = self.cwd.clone();
                    match blocking(move || git::diff_name_status(&cwd, &before, &commit)).await {
                        Ok(f) => files = f,
                        Err(e) => self.error(ErrorKind::GitError, &e.to_string(), false).await,
                    }
                }
            }
            Err(e) => self.error(ErrorKind::GitError, &e.message, false).await,
        }
        let mut e = NewEvent::new(EventBody::TurnCompleted(TurnCompletedData {
            stop_reason,
            tokens,
            cost,
            files,
        }))
        .turn(n);
        e.raw = raw;
        self.append(e).await;
        if idle && self.stopping.is_none() {
            self.set_state(SessionState::Idle, None).await;
        }
    }

    /// Stop or detach: cancel a running turn (giving the agent a moment to end it),
    /// then kill the agent. The rest happens when its exit arrives.
    async fn let_go(&mut self, why: LetGo, reply: oneshot::Sender<()>) {
        self.stopping = Some((why, Some(reply)));
        if self.turn.is_some() {
            self.interrupt().await;
            let deadline = tokio::time::Instant::now() + STOP_GRACE;
            while self.turn.is_some() {
                let Some((_, events)) = self.agent.as_mut() else {
                    break;
                };
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Some(AgentEvent::Exited(info))) => {
                        self.exited(Some(info)).await;
                        return;
                    }
                    Ok(Some(event)) => self.agent_event(event).await,
                    Ok(None) | Err(_) => break,
                }
            }
        }
        if let Some((agent, _)) = &self.agent {
            agent.shutdown().await;
        }
    }

    /// The agent process is gone.
    async fn exited(&mut self, info: Option<crate::agent::ExitInfo>) {
        let stopping = self.stopping.take();
        if stopping.is_none() {
            let message = match &info {
                Some(i) => format!(
                    "the agent exited ({}){}",
                    match (i.code, i.signal) {
                        (Some(c), _) => format!("code {c}"),
                        (None, Some(s)) => format!("signal {s}"),
                        _ => "unknown status".into(),
                    },
                    last_lines(&i.stderr_tail, 5)
                ),
                None => "the agent's connection closed".into(),
            };
            self.error(ErrorKind::AgentExited, &message, true).await;
        }
        let by = if stopping.is_some() {
            ResolvedBy::Interrupt
        } else {
            ResolvedBy::AgentExited
        };
        self.resolve_open(by).await;
        if self.turn.is_some() {
            let reason = if stopping.is_some() {
                StopReason::ActuoStopped
            } else {
                StopReason::AgentExited
            };
            self.finish_turn(reason, None, None, None, false).await;
        }
        self.clear_process().await;
        self.agent = None;
        let (state, reason) = match &stopping {
            Some((LetGo::End, _)) => (SessionState::Ended, "stopped"),
            Some((LetGo::Detach, _)) if self.can_continue => {
                (SessionState::Detached, "actuo stopped")
            }
            Some((LetGo::Idle, _)) if self.can_continue => (SessionState::Detached, "idle"),
            _ if self.can_continue => (SessionState::Detached, "agent not running"),
            _ => (SessionState::Ended, "the agent can't resume or load"),
        };
        self.set_state(state, Some(reason)).await;
        if let Some((_, Some(reply))) = stopping {
            let _ = reply.send(());
        }
    }

    async fn capture(&self, n: u32) -> HostResult<String> {
        capture(&self.host, &self.id, self.cwd.clone(), n).await
    }

    async fn clear_process(&self) {
        let sid = self.id.clone();
        let _ = self.host.db(move |s| s.set_agent_process(&sid, None)).await;
    }

    async fn set_state(&self, state: SessionState, reason: Option<&str>) {
        self.append(state_event(state, reason)).await;
    }

    async fn error(&self, kind: ErrorKind, message: &str, fatal: bool) {
        self.append(
            NewEvent::new(EventBody::Error(ErrorEventData {
                kind,
                message: message.to_string(),
                fatal,
            }))
            .turn_opt(self.turn),
        )
        .await;
    }

    /// Appends an event. A store failure can't be reported anywhere better than the
    /// log; the session goes on.
    async fn append(&self, event: NewEvent) {
        let sid = self.id.clone();
        if let Err(e) = self.host.db(move |s| s.append(&sid, event)).await {
            crate::log!("session {}: storing an event failed: {e}", self.id);
        }
    }
}

/// Ends the turn's tool calls that never finished: `item.completed` with status
/// `cancelled`, so no item stays open after its turn.
async fn close_unfinished_tools(host: &Host, id: &SessionId, turn: u32) {
    let sid = id.clone();
    let Ok(open) = host.db(move |s| s.unfinished_tool_calls(&sid, turn)).await else {
        return;
    };
    for (item_id, mut call) in open {
        call.status = ToolStatus::Cancelled;
        let sid = id.clone();
        let _ = host
            .db(move |s| {
                s.append(
                    &sid,
                    NewEvent::new(EventBody::ItemCompleted(ItemData {
                        item: Item::ToolCall(call),
                    }))
                    .turn(turn)
                    .item(item_id),
                )
            })
            .await;
    }
}

/// Takes checkpoint `n` and records it.
async fn capture(host: &Host, id: &SessionId, cwd: PathBuf, n: u32) -> HostResult<String> {
    let reference = checkpoint_ref(id, n);
    let commit = blocking(move || git::capture_checkpoint(&cwd, &reference))
        .await
        .map_err(git_error)?;
    let sid = id.clone();
    let data = CheckpointCreatedData {
        n,
        commit: commit.clone(),
    };
    // Checkpoint n (n > 0) is taken at the end of turn n.
    let mut event = NewEvent::new(EventBody::CheckpointCreated(data));
    if n > 0 {
        event = event.turn(n);
    }
    host.db(move |s| s.append(&sid, event)).await?;
    Ok(commit)
}

fn state_name(state: SessionState) -> String {
    match serde_json::to_value(state) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

async fn revert_files(
    host: &Host,
    id: &SessionId,
    cwd: PathBuf,
    worktree: bool,
    turn: u32,
    force: bool,
) -> HostResult<CheckpointRevertResult> {
    if !worktree && !force {
        return Err(RpcError::new(
            ErrorKind::ForceRequired,
            "this session runs in the project checkout; reverting also undoes anything else changed there since, so it needs force",
        ));
    }
    if turn == 0 {
        return Err(RpcError::new(
            ErrorKind::CheckpointNotFound,
            "turns start at 1",
        ));
    }
    let to_n = turn - 1;
    let sid = id.clone();
    let checkpoint = host
        .db(move |s| s.checkpoints(&sid))
        .await?
        .into_iter()
        .find(|c| c.n == to_n)
        .ok_or_else(|| {
            RpcError::new(
                ErrorKind::CheckpointNotFound,
                format!("no checkpoint before turn {turn}"),
            )
        })?;
    let restored = blocking(move || git::restore(&cwd, &checkpoint.commit))
        .await
        .map_err(git_error)?;
    let result = CheckpointRevertResult {
        to_n,
        restored: restored.restored,
        deleted: restored.deleted,
    };
    let sid = id.clone();
    let data = CheckpointRevertedData {
        to_n,
        restored: result.restored.clone(),
        deleted: result.deleted.clone(),
    };
    host.db(move |s| {
        s.append(
            &sid,
            NewEvent::new(EventBody::CheckpointReverted(data)).turn(turn),
        )
    })
    .await?;
    Ok(result)
}

trait TurnOpt {
    fn turn_opt(self, turn: Option<u32>) -> Self;
}

impl TurnOpt for NewEvent {
    fn turn_opt(mut self, turn: Option<u32>) -> Self {
        self.turn = turn;
        self
    }
}

fn state_event(state: SessionState, reason: Option<&str>) -> NewEvent {
    NewEvent::new(EventBody::SessionState(SessionStateData {
        state,
        reason: reason.map(str::to_string),
    }))
}

async fn blocking<T: Send + 'static, E: Send + 'static>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E> {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}

pub(crate) fn setup_error(e: AgentError) -> SetupError {
    let kind = agent_error_kind(&e);
    let message = match &e {
        AgentError::AuthRequired(m) => format!("the agent needs a login ({m})"),
        e => e.to_string(),
    };
    SetupError(kind, message)
}

/// How to log `agent` in: sandboxed (lane) sessions use actuo's own login for it,
/// others the agent's own CLI on this machine.
pub fn login_hint(agent: &str, sandboxed: bool) -> String {
    if sandboxed {
        return format!("log it in for sandboxed runs with `actuo login {agent}`");
    }
    match agent {
        "claude" => "log in with `claude auth login`".into(),
        "codex" => "log in with `codex login`".into(),
        _ => format!("log {agent} in with its own CLI"),
    }
}

fn agent_error_kind(e: &AgentError) -> ErrorKind {
    match e {
        AgentError::AuthRequired(_) => ErrorKind::AuthRequired,
        AgentError::Gone => ErrorKind::AgentExited,
        _ => ErrorKind::AgentError,
    }
}

fn git_error(e: git::GitError) -> RpcError {
    RpcError::new(ErrorKind::GitError, e.to_string())
}

fn store_error(e: StoreError) -> RpcError {
    match e {
        StoreError::NotFound(what) if what.starts_with("session") => {
            RpcError::new(ErrorKind::SessionNotFound, format!("{what} not found"))
        }
        StoreError::NotFound(what) if what.starts_with("project") => {
            RpcError::new(ErrorKind::ProjectNotFound, format!("{what} not found"))
        }
        StoreError::NotFound(what) if what.starts_with("run ") => {
            RpcError::new(ErrorKind::RunNotFound, format!("{what} not found"))
        }
        StoreError::Conflict(what) => RpcError::new(ErrorKind::InvalidParams, what),
        e => RpcError::new(ErrorKind::Internal, e.to_string()),
    }
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<_> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(": {}", lines[lines.len().saturating_sub(n)..].join(" | "))
}

/// Sleeps until `at`, or forever without one.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Marks actuo's worktrees folder and resolves the git dir of every stored
/// session's worktree from its project's repo ([`git::trust_worktree`]), so git
/// actuo runs in a worktree after a restart never reads the worktree's own `.git`.
/// A worktree that's gone is skipped.
fn trust_worktrees(store: &Store, paths: &Paths) {
    git::add_worktrees_root(&paths.home.join("worktrees"));
    let Ok(sessions) = store.sessions(None) else {
        return;
    };
    for s in sessions {
        let Some(wt) = s.worktree_path.as_deref() else {
            continue;
        };
        let wt = std::path::Path::new(wt);
        if !wt.exists() {
            continue;
        }
        if let Ok(project) = store.project(&s.project_id)
            && let Err(e) = git::trust_worktree(std::path::Path::new(&project.root), wt)
        {
            crate::log!("{}: {e}", s.id);
        }
    }
}

//! The issue-to-PR lane: a client of sessions that turns labeled GitHub issues into
//! draft PRs.
//!
//! [`Lane`] polls the watched repos with `gh`, queues a run per issue a trusted user
//! labeled, and starts queued runs while fewer than `lane.max_runs` are running or
//! checking. Each run is driven by one task (`run.rs`), which talks to its session
//! through the same host calls a client makes, as client `lane`.

mod checks;
pub mod diagnose;
mod failure;
pub mod gh;
pub mod git;
mod mac;
mod projects;
pub mod prompt;
pub mod publish;
pub mod rules;
mod run;
pub mod steps;
pub mod watch;

use crate::session::{Host, HostResult};
use crate::settings::parse_duration;
use crate::store::{ChangedBy, NewRun};
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::events::EventBody;
use actuo_protocol::ids::RunId;
use actuo_protocol::methods::LanePoll;
use actuo_protocol::runs::{Run, RunState, StartedVia};
use actuo_protocol::settings::{LaneLabels, Rule, Settings};
use actuo_protocol::types::Level;
use gh::{Gh, GhError};
use run::{Control, Entry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, mpsc};

/// The client name the lane uses on the turns it sends and the requests it answers.
pub const CLIENT: &str = "lane";

/// What a run keeps from the settings for its whole life (`runs.limits_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub turn_time_limit_s: u64,
    pub run_time_limit_s: u64,
    pub max_check_retries: u32,
    pub max_changed_files: u32,
    pub max_changed_lines: u32,
    pub max_reject_turns: u32,
    pub max_restart_turns: u32,
    pub output_tail_lines: u32,
    pub output_tail_bytes: u32,
    pub max_prompt_bytes: u32,
    pub protected_paths: Vec<String>,
    pub web_tools: bool,
    pub sign_commits: bool,
    /// `None`: detected from the run's worktree (`crate::detect::resolve`).
    pub setup: Option<Vec<String>>,
    pub setup_time_limit_s: u64,
    pub checks: Option<Vec<String>>,
    pub check_time_limit_s: u64,
    pub network: Option<Vec<String>>,
    pub agent_network: bool,
    /// The project's `checks_unix_sockets`.
    #[serde(default)]
    pub checks_unix_sockets: bool,
    /// The project's `setup_reads_npmrc`.
    #[serde(default)]
    pub setup_reads_npmrc: bool,
    #[serde(default)]
    pub checks_local_ports: Vec<u16>,
    #[serde(default)]
    pub checks_local_listen_ports: Vec<u16>,
    /// The repo's `read` list (its toolchain under your home folder).
    #[serde(default)]
    pub read: Vec<String>,
    /// The agents' own telemetry (`lane.telemetry`).
    #[serde(default)]
    pub telemetry: bool,
    /// The repo's commit rules: `commit_check`, `commit_subject.prompt` and
    /// `co_authored_by`.
    #[serde(default)]
    pub commit_check: Option<String>,
    #[serde(default)]
    pub commit_subject_prompt: Option<String>,
    #[serde(default = "yes")]
    pub co_authored_by: bool,
    /// The agent's approval rules (the settings', else the built-in ones).
    pub rules: Vec<Rule>,
    pub labels: LaneLabels,
    /// Whose comments go into prompts: the gh login and `lane.trusted_users`.
    pub trusted: Vec<String>,
}

fn yes() -> bool {
    true
}

impl RunConfig {
    /// The lane's settings with the project's overrides, or `None` if the repo isn't
    /// a project.
    pub fn new(s: &Settings, repo: &str, agent: &str, login: &str) -> Option<Self> {
        let lane = &s.lane;
        let r = s.projects.get(repo)?;
        let secs = |d: &str| parse_duration(d).map(|d| d.as_secs()).unwrap_or(0);
        let mut trusted = vec![login.to_string()];
        trusted.extend(trusted_users(s, repo));
        let mut labels = lane.labels.clone();
        labels.trigger = trigger_label(s, repo);
        Some(Self {
            turn_time_limit_s: secs(r.turn_time_limit.as_ref().unwrap_or(&lane.turn_time_limit)),
            run_time_limit_s: secs(r.run_time_limit.as_ref().unwrap_or(&lane.run_time_limit)),
            max_check_retries: r.max_check_retries.unwrap_or(lane.max_check_retries),
            max_changed_files: r.max_changed_files.unwrap_or(lane.max_changed_files),
            max_changed_lines: r.max_changed_lines.unwrap_or(lane.max_changed_lines),
            max_reject_turns: r.max_reject_turns.unwrap_or(lane.max_reject_turns),
            max_restart_turns: r.max_restart_turns.unwrap_or(lane.max_restart_turns),
            output_tail_lines: r.output_tail_lines.unwrap_or(lane.output_tail_lines),
            output_tail_bytes: r.output_tail_bytes.unwrap_or(lane.output_tail_bytes),
            max_prompt_bytes: r.max_prompt_bytes.unwrap_or(lane.max_prompt_bytes),
            protected_paths: r
                .protected_paths
                .clone()
                .unwrap_or_else(|| lane.protected_paths.clone()),
            web_tools: r.web_tools.unwrap_or(lane.web_tools),
            sign_commits: r.sign_commits.unwrap_or(lane.sign_commits),
            setup: r.setup.clone(),
            setup_time_limit_s: secs(&r.setup_time_limit),
            checks: r.checks.clone(),
            check_time_limit_s: secs(&r.check_time_limit),
            network: r.network.clone(),
            agent_network: r.agent_network,
            checks_unix_sockets: r.checks_unix_sockets,
            setup_reads_npmrc: r.setup_reads_npmrc,
            checks_local_ports: r.checks_local_ports.clone(),
            checks_local_listen_ports: r.checks_local_listen_ports.clone(),
            read: r.read.clone(),
            telemetry: lane.telemetry,
            commit_check: r.commit_check.clone(),
            commit_subject_prompt: r.commit_subject.as_ref().and_then(|c| c.prompt.clone()),
            co_authored_by: r.co_authored_by,
            rules: r
                .rules
                .get(agent)
                .or_else(|| lane.rules.get(agent))
                .cloned()
                .unwrap_or_else(|| rules::builtin(agent)),
            labels,
            trusted,
        })
    }

    pub fn turn_time_limit(&self) -> Duration {
        Duration::from_secs(self.turn_time_limit_s)
    }

    pub fn run_time_limit(&self) -> Duration {
        Duration::from_secs(self.run_time_limit_s)
    }
}

pub struct Lane {
    host: Arc<Host>,
    gh: Gh,
    /// The account gh is logged in as (asked once).
    login: tokio::sync::Mutex<Option<String>>,
    /// Repos whose status labels exist (created once per start).
    labels_ready: Mutex<HashSet<String>>,
    /// Issues already logged as skipped, so later polls don't log them again.
    skipped: Mutex<HashSet<(String, u64)>>,
    runs_tx: broadcast::Sender<Run>,
    /// `check.state` notifications: each check run as it changes.
    checks_tx: broadcast::Sender<actuo_protocol::runs::CheckRun>,
    wake: Notify,
    /// Runs with a task driving them, and how to reach it.
    active: Mutex<HashMap<RunId, mpsc::UnboundedSender<Control>>>,
    /// Set when actuo stops: tasks stop without touching their runs, which the
    /// next start picks up.
    stopping: AtomicBool,
    /// One per repo: held while a run clones or fetches the lane's clone and adds its
    /// worktree, so two runs of a repo don't do it at once.
    repo_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    notifiers: Notifiers,
    /// The client that asked a run's task to cancel, hand back or take over (its
    /// `client.name`, if it gave one), until the task records the change.
    causes: Mutex<HashMap<RunId, Option<String>>>,
    /// The last poll since actuo started (`actuo/status`).
    last_poll: Mutex<Option<LanePoll>>,
}

/// The agent a project's lane runs get when no label picks one: the project's, else
/// actuo's default.
pub(crate) fn repo_agent(s: &Settings, repo: &str) -> String {
    s.projects
        .get(repo)
        .and_then(|r| r.agent.clone())
        .unwrap_or_else(|| s.sessions.agent.clone())
}

/// The label that starts a run in a project: its own, else the lane's.
fn trigger_label(s: &Settings, repo: &str) -> String {
    s.projects
        .get(repo)
        .and_then(|p| p.label.clone())
        .unwrap_or_else(|| s.lane.labels.trigger.clone())
}

/// Who may start a project's runs by label (besides gh's own account): its own
/// list, else the lane's.
fn trusted_users(s: &Settings, repo: &str) -> Vec<String> {
    s.projects
        .get(repo)
        .and_then(|p| p.trusted_users.clone())
        .unwrap_or_else(|| s.lane.trusted_users.clone())
}

/// A project's level: its own, else the lane's.
fn repo_level(s: &Settings, repo: &str) -> Level {
    s.projects
        .get(repo)
        .and_then(|p| p.level)
        .unwrap_or(s.lane.level)
}

/// A run's config values over actuo's `[defaults]`: the repo's `config`, when
/// the run's agent is the repo's; none for another agent (a label picked it).
fn repo_config(
    s: &Settings,
    repo: &str,
    agent: &str,
) -> std::collections::BTreeMap<String, actuo_protocol::types::ConfigValue> {
    match s.projects.get(repo) {
        Some(r) if repo_agent(s, repo) == agent => r.config.clone(),
        _ => Default::default(),
    }
}

/// Connections that show run notifications themselves (`run/subscribe` with
/// `notifies`). While any holds one, actuo posts none on macOS.
#[derive(Clone, Default)]
pub struct Notifiers(Arc<AtomicUsize>);

impl Notifiers {
    /// Counts a subscription until the returned value is dropped.
    pub fn hold(&self) -> NotifierHold {
        self.0.fetch_add(1, Ordering::SeqCst);
        NotifierHold(self.0.clone())
    }

    pub fn any(&self) -> bool {
        self.0.load(Ordering::SeqCst) > 0
    }
}

/// One `notifies` subscription, counted while it lives.
pub struct NotifierHold(Arc<AtomicUsize>);

impl Drop for NotifierHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lane {
    /// Starts the lane's loop and hands the lane to the host (for the run methods).
    pub fn start(host: Arc<Host>, gh: Gh) -> Arc<Self> {
        let (runs_tx, _) = broadcast::channel(256);
        let (checks_tx, _) = broadcast::channel(64);
        let lane = Arc::new(Self {
            host: host.clone(),
            gh,
            login: tokio::sync::Mutex::new(None),
            labels_ready: Mutex::new(HashSet::new()),
            skipped: Mutex::new(HashSet::new()),
            runs_tx,
            checks_tx,
            wake: Notify::new(),
            active: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            repo_locks: Mutex::new(HashMap::new()),
            notifiers: Notifiers::default(),
            causes: Mutex::new(HashMap::new()),
            last_poll: Mutex::new(None),
        });
        host.set_lane(lane.clone());
        tokio::spawn(lane.clone().run_loop());
        tokio::spawn(lane.clone().watch_take_overs());
        if cfg!(target_os = "macos") {
            tokio::spawn(mac::watch(lane.subscribe(), lane.notifiers.clone()));
        }
        lane
    }

    /// The clients showing run notifications themselves.
    pub fn notifiers(&self) -> &Notifiers {
        &self.notifiers
    }

    /// Actuo is stopping: runs stay as they are for the next start.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Starts a task for a run that has none; false if it has one.
    fn spawn(self: &Arc<Self>, run: crate::store::RunRow, entry: Entry) -> bool {
        if self.stopping() {
            return false;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&run.id) {
                return false;
            }
            active.insert(run.id.clone(), tx);
        }
        tokio::spawn(run::RunTask::new(self.clone(), run, entry, rx).drive());
        true
    }

    /// Notes which client asked for what a run's task does next.
    fn cause(&self, id: &RunId, client: Option<String>) {
        self.causes.lock().unwrap().insert(id.clone(), client);
    }

    /// The client noted for a run, taken: `None` if none was noted, `Some(None)`
    /// for a client without a name.
    pub(super) fn take_cause(&self, id: &RunId) -> Option<Option<String>> {
        self.causes.lock().unwrap().remove(id)
    }

    /// Tells a run's task something; false if the run has no task.
    fn tell(&self, id: &RunId, c: Control) -> bool {
        self.active
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|tx| tx.send(c).is_ok())
    }

    /// Runs a restart left running, checking, or waiting on a request (which the
    /// restart cancelled) get a task that goes on from their step.
    async fn recover(self: &Arc<Self>) {
        // A check run can't go on after a restart.
        let at = crate::time::now();
        let _ = self.host.db(move |st| st.end_stale_check_runs(&at)).await;
        let runs = match self
            .host
            .db(|st| st.runs_in_states(&[RunState::Running, RunState::Checking, RunState::Waiting]))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                crate::log!("lane: recovery: {}", e.message);
                return;
            }
        };
        for run in runs {
            let waiting_on_request =
                run.state == RunState::Waiting && run.reason.as_deref() == Some("request");
            if run.state != RunState::Waiting || waiting_on_request {
                self.spawn(run, Entry::Recover);
            }
        }
    }

    /// A prompt from another client in a run's session takes the run over.
    async fn watch_take_overs(self: Arc<Self>) {
        let mut events = self.host.store().subscribe();
        loop {
            let e = match events.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            let EventBody::TurnStarted(d) = &e.body else {
                continue;
            };
            if d.client.as_deref() == Some(CLIENT) {
                continue;
            }
            let sid = e.session_id.clone();
            let Ok(Some(run)) = self.host.db(move |st| st.open_run_for_session(&sid)).await else {
                continue;
            };
            if matches!(
                run.state,
                RunState::Running | RunState::Checking | RunState::Waiting
            ) {
                self.cause(&run.id, d.client.clone());
                if !self.tell(&run.id, Control::TakeOver) {
                    self.spawn(run, Entry::TakenOver);
                }
            }
        }
    }

    /// `run/resume`, from the client named `client`.
    pub async fn resume_run(
        self: &Arc<Self>,
        id: &RunId,
        client: Option<String>,
    ) -> HostResult<()> {
        let rid = id.clone();
        let run = self.host.db(move |st| st.run(&rid)).await?;
        let ok = match run.state {
            RunState::TakenOver => true,
            RunState::Waiting => !self.active.lock().unwrap().contains_key(id),
            _ => false,
        };
        if !ok {
            let why = if run.state == RunState::Waiting {
                "it waits for an answer to an open request: answer it (`actuo attach`)"
            } else {
                "only a taken-over or waiting run can be handed back"
            };
            return Err(RpcError::new(
                ErrorKind::InvalidRunState,
                format!("run {id} is {}; {why}", state_name(run.state)),
            ));
        }
        if self.host.held() {
            return Err(RpcError::new(
                ErrorKind::InvalidRunState,
                "actuo is about to restart (an update); resume the run after",
            ));
        }
        self.cause(id, client);
        if !self.spawn(run, Entry::Resume) {
            return Err(RpcError::new(
                ErrorKind::InvalidRunState,
                format!("run {id} is busy"),
            ));
        }
        Ok(())
    }

    /// `run/cancel`, from the client named `client`.
    pub async fn cancel_run(
        self: &Arc<Self>,
        id: &RunId,
        client: Option<String>,
    ) -> HostResult<()> {
        let rid = id.clone();
        let run = self.host.db(move |st| st.run(&rid)).await?;
        if run.state.ended() {
            return Err(RpcError::new(
                ErrorKind::InvalidRunState,
                format!("run {id} has ended ({})", state_name(run.state)),
            ));
        }
        self.cause(id, client);
        if !self.tell(id, Control::Cancel) {
            self.spawn(run, Entry::Cancel);
        }
        Ok(())
    }

    /// Whether a task drives this run now.
    pub fn drives(&self, id: &RunId) -> bool {
        self.active.lock().unwrap().contains_key(id)
    }

    /// Whether a run has a task working on it now.
    pub fn busy(&self) -> bool {
        !self.active.lock().unwrap().is_empty()
    }

    /// The account gh is logged in as, if it answers (asked once).
    pub async fn gh_login(&self) -> Option<String> {
        self.login().await.ok()
    }

    /// The last poll since actuo started; `None` while the lane is off.
    pub fn last_poll(&self) -> Option<LanePoll> {
        if !self.host.settings().lane.enabled {
            return None;
        }
        self.last_poll.lock().unwrap().clone()
    }

    /// `run.state` notifications: every run whose state or step changes.
    pub fn subscribe(&self) -> broadcast::Receiver<Run> {
        self.runs_tx.subscribe()
    }

    async fn run_loop(self: Arc<Self>) {
        self.recover().await;
        let mut next_poll = tokio::time::Instant::now();
        loop {
            let s = self.host.settings();
            if s.lane.enabled {
                if tokio::time::Instant::now() >= next_poll {
                    let result = self.poll(&s).await;
                    if let Err(e) = &result {
                        crate::log!("lane: poll: {e}");
                    }
                    *self.last_poll.lock().unwrap() = Some(LanePoll {
                        at: crate::time::now(),
                        error: result.err().map(|e| e.to_string()),
                    });
                    let every =
                        parse_duration(&s.lane.poll_interval).unwrap_or(Duration::from_secs(60));
                    next_poll = tokio::time::Instant::now() + every;
                }
                if let Err(e) = self.schedule(&s).await {
                    crate::log!("lane: scheduling: {e}");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(next_poll) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn login(&self) -> Result<String, GhError> {
        let mut login = self.login.lock().await;
        if let Some(l) = login.as_ref() {
            return Ok(l.clone());
        }
        let l = self.gh.login().await?;
        *login = Some(l.clone());
        Ok(l)
    }

    /// Queues a run for each open issue with the trigger label that a trusted user
    /// labeled and that has no run on this actuo yet.
    async fn poll(self: &Arc<Self>, s: &Settings) -> Result<(), GhError> {
        let login = self.login().await?;
        // Projects with their lane on.
        for repo in s.projects.iter().filter(|(_, p)| p.lane).map(|(r, _)| r) {
            let mut labels = s.lane.labels.clone();
            labels.trigger = trigger_label(s, repo);
            let labels = &labels;
            let trusted_users = trusted_users(s, repo);
            if let Err(e) = self.ensure_labels(repo, labels).await {
                crate::log!("lane: {repo}: creating labels: {e}");
                continue;
            }
            let issues = match self.gh.issues_with_label(repo, &labels.trigger).await {
                Ok(i) => i,
                Err(e) => {
                    crate::log!("lane: {repo}: listing issues: {e}");
                    continue;
                }
            };
            for issue in issues {
                let (r, n) = (repo.clone(), issue.number);
                let last = self
                    .host
                    .db(move |st| st.last_run(&r, n))
                    .await
                    .ok()
                    .flatten();
                if last.as_ref().is_some_and(|l| l.state == RunState::Queued) {
                    continue;
                }
                let actor = self.gh.label_actor(repo, n, &labels.trigger).await?;
                let trusted = actor
                    .as_ref()
                    .is_some_and(|a| *a == login || trusted_users.contains(a));
                let Some(actor) = actor.filter(|_| trusted) else {
                    if self.skipped.lock().unwrap().insert((repo.clone(), n)) {
                        crate::log!(
                            "lane: {repo}#{n}: the trigger label wasn't added by a trusted user; skipped"
                        );
                    }
                    continue;
                };
                if let Some(last) = last.clone()
                    && !last.state.ended()
                {
                    self.label_added_again(last, &issue).await;
                    continue;
                }
                let agent = agent_from_labels(&issue.labels, &labels.agent_prefix)
                    .unwrap_or_else(|| repo_agent(s, repo));
                let mut new = self.new_run(
                    s,
                    repo,
                    &issue,
                    agent,
                    repo_level(s, repo),
                    (&actor, StartedVia::Label),
                    &login,
                );
                new.previous_run_id = last.as_ref().map(|l| l.id.clone());
                let mut run = match self.host.db(move |st| st.create_run(new)).await {
                    Ok(run) => run,
                    Err(e) => {
                        crate::log!("lane: {repo}#{n}: queueing: {}", e.message);
                        continue;
                    }
                };
                // A re-run: the same session, PR and base, and comments from where the
                // earlier run's prompts ended.
                if let Some(last) = last
                    && last.session_id.is_some()
                {
                    run.session_id = last.session_id;
                    run.base = last.base;
                    run.pr_number = last.pr_number;
                    run.pr_url = last.pr_url;
                    run.last_prompt_at = last.last_prompt_at;
                    run = match self
                        .host
                        .db(move |st| st.save_run(&run, &ChangedBy::Lane))
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            crate::log!("lane: {repo}#{n}: re-run: {}", e.message);
                            continue;
                        }
                    };
                }
                self.notify(&run.summary());
            }
            self.check_prs(repo).await;
        }
        Ok(())
    }

    /// Ends the runs whose PR was merged or closed: the latest run of each of the
    /// repo's issues that has a PR and is `pr_opened` or waiting. Runs working, taken
    /// over, failed or cancelled are left alone.
    async fn check_prs(self: &Arc<Self>, repo: &str) {
        let r = repo.to_string();
        let runs = match self.host.db(move |st| st.runs(Some(&r), None)).await {
            Ok(runs) => runs,
            Err(e) => {
                crate::log!("lane: {repo}: listing runs: {}", e.message);
                return;
            }
        };
        let mut seen = HashSet::new();
        // Newest first: the first run of an issue is its latest.
        for run in runs {
            if !seen.insert(run.issue) {
                continue;
            }
            let Some(pr) = run.pr_number else {
                continue;
            };
            if !matches!(run.state, RunState::PrOpened | RunState::Waiting) {
                continue;
            }
            let merged = match self.gh.pr_state(repo, pr).await.as_deref() {
                Ok("MERGED") => true,
                Ok("CLOSED") => false,
                Ok(_) => continue,
                Err(e) => {
                    crate::log!("lane: {repo}#{}: PR #{pr}'s state: {e}", run.issue);
                    continue;
                }
            };
            self.end_for_pr(run, merged);
        }
    }

    /// The run's PR was merged or closed: its task ends it, or a task is started to.
    fn end_for_pr(self: &Arc<Self>, run: crate::store::RunRow, merged: bool) {
        if !self.tell(&run.id, Control::PrEnded { merged }) {
            self.spawn(run, Entry::PrEnded { merged });
        }
    }

    /// The trigger label is back on an issue whose run hasn't ended.
    async fn label_added_again(self: &Arc<Self>, run: crate::store::RunRow, issue: &gh::Issue) {
        let trigger = run_config_labels(&run).trigger;
        match run.state {
            RunState::Running | RunState::Checking => {
                // The new comments go into the run's next turn.
                if self.tell(&run.id, Control::Comments) {
                    let _ = self
                        .gh
                        .edit_labels(&run.repo, issue.number, &[], &[&trigger])
                        .await;
                }
            }
            // Held for a restart: the label stays, for the next start's poll.
            RunState::Waiting if self.host.held() => {}
            RunState::Waiting => {
                let _ = self
                    .gh
                    .edit_labels(&run.repo, issue.number, &[], &[&trigger])
                    .await;
                if !self.tell(&run.id, Control::Comments) {
                    self.spawn(run, Entry::Comments);
                }
            }
            RunState::TakenOver => {
                self.spawn(run, Entry::LabelIgnored);
            }
            _ => {}
        }
    }

    async fn ensure_labels(&self, repo: &str, labels: &LaneLabels) -> Result<(), GhError> {
        if self.labels_ready.lock().unwrap().contains(repo) {
            return Ok(());
        }
        for l in [
            &labels.trigger,
            &labels.running,
            &labels.waiting,
            &labels.pr,
            &labels.failed,
        ] {
            self.gh.ensure_label(repo, l).await?;
        }
        self.labels_ready.lock().unwrap().insert(repo.to_string());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn new_run(
        &self,
        s: &Settings,
        repo: &str,
        issue: &gh::Issue,
        agent: String,
        level: Level,
        (started_by, started_via): (&str, StartedVia),
        login: &str,
    ) -> NewRun {
        let config = RunConfig::new(s, repo, &agent, login).expect("a watched repo");
        NewRun {
            repo: repo.to_string(),
            issue: issue.number,
            issue_url: issue.url.clone(),
            title: Some(issue.title.clone()),
            config: repo_config(s, repo, &agent),
            agent,
            level,
            limits: serde_json::to_value(config).expect("config serializes"),
            branch: format!("actuo/issue-{}", issue.number),
            started_by: started_by.to_string(),
            started_via,
            previous_run_id: None,
        }
    }

    /// Starts queued runs, oldest first, while fewer than `max_runs` are running or
    /// checking (paused runs don't count).
    async fn schedule(self: &Arc<Self>, s: &Settings) -> HostResult<()> {
        // Held for a restart: queued runs wait for the next start.
        if self.host.held() {
            return Ok(());
        }
        let runs = self
            .host
            .db(|st| st.runs_in_states(&[RunState::Queued, RunState::Running, RunState::Checking]))
            .await?;
        let mut busy = runs
            .iter()
            .filter(|r| matches!(r.state, RunState::Running | RunState::Checking))
            .count();
        for run in runs.into_iter().filter(|r| r.state == RunState::Queued) {
            if busy >= s.lane.max_runs as usize {
                break;
            }
            if self.spawn(run, Entry::New) {
                busy += 1;
            }
        }
        Ok(())
    }

    /// `run/start` from the client named `client`: a run of the issue at `url`,
    /// without the label check.
    pub async fn start_run(
        &self,
        url: &str,
        agent: Option<String>,
        level: Option<Level>,
        client: Option<String>,
    ) -> HostResult<RunId> {
        let s = self.host.settings();
        let (repo, n) = gh::parse_issue_url(url).ok_or_else(|| {
            RpcError::new(
                ErrorKind::InvalidParams,
                format!(
                    "{url} isn't a GitHub issue URL (https://github.com/<owner>/<repo>/issues/<n>)"
                ),
            )
        })?;
        if !s.lane.enabled {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                "every project's runs are paused (lane.enabled = false)",
            ));
        }
        match s.projects.get(&repo) {
            None => {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    format!("{repo} isn't a project here; add it with its setup and checks first"),
                ));
            }
            Some(p) if !p.lane => {
                return Err(RpcError::new(
                    ErrorKind::InvalidParams,
                    format!("{repo}'s lane is off (projects.\"{repo}\".lane = false)"),
                ));
            }
            Some(_) => {}
        }
        let login = self.login().await.map_err(gh_error)?;
        let issue = self.gh.issue(&repo, n).await.map_err(|e| match &e {
            GhError::Failed { stderr, .. } if stderr.contains("Could not resolve") => {
                RpcError::new(ErrorKind::IssueNotFound, format!("{repo}#{n}: {stderr}"))
            }
            _ => gh_error(e),
        })?;
        let agent = agent
            .or_else(|| agent_from_labels(&issue.labels, &s.lane.labels.agent_prefix))
            .unwrap_or_else(|| repo_agent(&s, &repo));
        if !self.host.agents.agents.contains_key(&agent) {
            return Err(RpcError::new(
                ErrorKind::AgentNotFound,
                format!("actuo has no agent named {agent}"),
            ));
        }
        // Calls come after `initialize`, so a name is there; shown, never compared.
        let client = client.unwrap_or_default();
        let new = self.new_run(
            &s,
            &repo,
            &issue,
            agent,
            level.unwrap_or_else(|| repo_level(&s, &repo)),
            (&client, StartedVia::Client),
            &login,
        );
        let run = self.host.db(move |st| st.create_run(new)).await?;
        self.notify(&run.summary());
        Ok(run.id)
    }

    /// Tells subscribers about a run and wakes the scheduler (a slot may be free).
    fn notify(&self, run: &Run) {
        let _ = self.runs_tx.send(run.clone());
        self.wake.notify_one();
    }

    fn repo_lock(&self, repo: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.repo_locks
            .lock()
            .unwrap()
            .entry(repo.to_string())
            .or_default()
            .clone()
    }

    fn finished(&self, id: &RunId) {
        self.active.lock().unwrap().remove(id);
        self.causes.lock().unwrap().remove(id);
        self.wake.notify_one();
    }
}

fn state_name(s: RunState) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The labels a run keeps in its config.
fn run_config_labels(run: &crate::store::RunRow) -> LaneLabels {
    serde_json::from_value::<RunConfig>(run.limits.clone())
        .map(|c| c.labels)
        .unwrap_or_default()
}

/// The agent an `agent:<name>` label picks.
fn agent_from_labels(labels: &[String], prefix: &str) -> Option<String> {
    labels
        .iter()
        .find_map(|l| l.strip_prefix(prefix))
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

fn gh_error(e: GhError) -> RpcError {
    RpcError::new(ErrorKind::GhError, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo's agent and values: its own agent, else actuo's; its config only
    /// for that agent (a label that picks another gets none).
    #[test]
    fn a_repos_agent_and_config() {
        let mut s = Settings::default();
        s.sessions.agent = "claude".into();
        s.projects.insert(
            "o/own".into(),
            toml::from_str("agent = \"codex\"\nconfig = { model = \"m\" }").unwrap(),
        );
        s.projects.insert(
            "o/plain".into(),
            toml::from_str("config = { effort = \"low\" }").unwrap(),
        );

        assert_eq!(repo_agent(&s, "o/own"), "codex");
        assert_eq!(repo_agent(&s, "o/plain"), "claude");
        assert_eq!(repo_config(&s, "o/own", "codex").len(), 1);
        assert!(repo_config(&s, "o/own", "claude").is_empty());
        assert_eq!(repo_config(&s, "o/plain", "claude").len(), 1);
        assert!(repo_config(&s, "o/missing", "claude").is_empty());
    }

    /// Limits as runs of each release saved them (`runs.limits_json`), oldest
    /// first. A run saved by one release is resumed by the next: each must still
    /// load. When `RunConfig` gains a field, add a file (the next test says so); never
    /// edit or remove one.
    const SAVED_LIMITS: &[(&str, &str)] = &[(
        "0.1.0",
        include_str!("../../tests/fixtures/run-limits/0.1.0.json"),
    )];

    #[test]
    fn limits_saved_by_each_release_still_load() {
        for (name, text) in SAVED_LIMITS {
            let saved: serde_json::Value = serde_json::from_str(text).unwrap();
            if let Err(e) = serde_json::from_value::<RunConfig>(saved) {
                panic!("limits saved by {name} don't load: {e}");
            }
        }
    }

    #[test]
    fn the_newest_saved_limits_have_every_field() {
        let mut s = Settings::default();
        s.projects.insert("o/r".into(), Default::default());
        let now = serde_json::to_value(RunConfig::new(&s, "o/r", "claude", "me").unwrap()).unwrap();
        let (name, text) = SAVED_LIMITS.last().unwrap();
        let newest: serde_json::Value = serde_json::from_str(text).unwrap();
        let missing: Vec<_> = now
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| newest.get(k.as_str()).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "RunConfig has fields the saved limits of {name} don't: {missing:?}. Save today's \
             limits as a new file in tests/fixtures/run-limits/ and add it to SAVED_LIMITS"
        );
    }

    /// A project's own trusted users, rules, signing and label win over the lane's.
    #[test]
    fn config_takes_project_overrides() {
        let mut s = Settings::default();
        s.lane.trusted_users = vec!["lane-user".into()];
        s.projects.insert(
            "o/r".into(),
            toml::from_str(
                "label = \"go\"\ntrusted_users = [\"mine\"]\nsign_commits = true\n[[rules.claude]]\nkind = \"read\"\nanswer = [\"allow_once\"]",
            )
            .unwrap(),
        );
        let c = RunConfig::new(&s, "o/r", "claude", "me").unwrap();
        assert_eq!(c.trusted, vec!["me", "mine"]);
        assert!(c.sign_commits);
        assert_eq!(c.labels.trigger, "go");
        assert_eq!(c.rules.len(), 1);
        // Codex has no project rules: the lane's (here the built-in ones).
        let c = RunConfig::new(&s, "o/r", "codex", "me").unwrap();
        assert_eq!(c.rules, rules::builtin("codex"));
    }

    #[test]
    fn config_takes_repo_overrides() {
        let mut s = Settings::default();
        s.projects.insert(
            "o/r".into(),
            toml::from_str(
                "checks = [\"pnpm test\"]\nmax_check_retries = 0\nturn_time_limit = \"5m\"",
            )
            .unwrap(),
        );
        s.lane.trusted_users = vec!["you".into()];
        let c = RunConfig::new(&s, "o/r", "claude", "me").unwrap();
        assert_eq!(c.max_check_retries, 0);
        assert_eq!(c.turn_time_limit_s, 300);
        assert_eq!(c.run_time_limit_s, 7200);
        assert_eq!(c.checks, Some(vec!["pnpm test".to_string()]));
        assert_eq!(c.trusted, vec!["me", "you"]);
        assert_eq!(c.rules, rules::builtin("claude"));
        // `lane.rules.<agent>` replaces the built-in list whole; `[]` pauses on all.
        s.lane.rules.insert("claude".into(), Vec::new());
        assert!(
            RunConfig::new(&s, "o/r", "claude", "me")
                .unwrap()
                .rules
                .is_empty()
        );
        assert!(RunConfig::new(&s, "o/other", "claude", "me").is_none());
        assert_eq!(
            agent_from_labels(&["bug".into(), "agent:codex".into()], "agent:"),
            Some("codex".into())
        );
        assert_eq!(agent_from_labels(&["agent:".into()], "agent:"), None);
    }
}

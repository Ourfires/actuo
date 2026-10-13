//! One run, from claim to draft PR (the session starts before setup). The task
//! writes the run's step before the step starts, and ends when the run opens its PR,
//! fails, or pauses for a person (a pause on an open request keeps the task waiting
//! for the answer).
//!
//! While a run has a task, only the task writes its row; the lane talks to it
//! through [`Control`]. A run without a task gets one for each thing that changes
//! it ([`Entry`]): a queued run starting, `run/resume`, recovery after a restart,
//! the trigger label coming back, a take-over, `run/cancel`.

use super::gh::{Comment, GhError, Issue};
use super::steps::CommandOutcome;
use super::watch::Watch;
use super::{CLIENT, Lane, RunConfig, git as lane_git, prompt, publish, rules};
use crate::store::{ChangedBy, RunRow};
use actuo_protocol::errors::ErrorKind;
use actuo_protocol::events::{EventBody, RunStepData};
use actuo_protocol::ids::{ProjectId, RequestId};
use actuo_protocol::methods::SessionStartParams;
use actuo_protocol::runs::{RunState, RunStep, RunStepKind, StartedVia};
use actuo_protocol::types::{
    Item, PermissionOptionKind, ResolvedBy, SessionState, StopReason, ToolCall, ToolContent, Turn,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Marks actuo's own comments, so they never go into a prompt.
const COMMENT_TAG: &str = "<!-- actuo -->";

/// A comment actuo posted, by its mark.
fn own_comment(body: &str) -> bool {
    body.starts_with(COMMENT_TAG)
}

/// Why a task starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Entry {
    /// A queued run: claim, then a first run, or a re-run when an earlier run of
    /// the issue left a session.
    New,
    /// `run/resume`.
    Resume,
    /// After a restart, from the step the run was at.
    Recover,
    /// The trigger label came back while the run waited: new comments as a turn.
    Comments,
    /// Someone else sent a prompt in the run's session.
    TakenOver,
    /// The trigger label came back while the run is taken over: noted, nothing else.
    LabelIgnored,
    /// `run/cancel`.
    Cancel,
    /// The run's PR was merged (or closed): end the run and clean up.
    PrEnded { merged: bool },
}

/// What the lane tells a run's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    TakeOver,
    Cancel,
    /// The run's PR was merged (or closed).
    PrEnded {
        merged: bool,
    },
    /// The trigger label came back: fetch the new comments.
    Comments,
}

/// Where [`RunTask::finish_from`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum From {
    Turn,
    Checks,
    /// The review turn, then the checks (a commit message was refused).
    Review,
    Guards,
}

/// The run stopped here; its state is saved and the task ends.
struct Halt;

/// A commit message the repo's commit rules refused.
struct Refused {
    /// The `commit_check` that refused it; `None` for a `Co-Authored-By` line.
    command: Option<String>,
    tail: String,
    /// What the pause comment says.
    text: String,
}

type Step<T = ()> = Result<T, Halt>;

/// How a check or setup command went.
#[derive(Debug, Clone)]
struct CommandResult {
    command: String,
    exit_code: Option<i32>,
    tail: String,
    /// Why it failed, when actuo can tell (`diagnose`).
    diagnosis: Option<actuo_protocol::runs::Diagnosis>,
    /// The packages turbo's summary lists as failed, from the whole output.
    failed_packages: Vec<String>,
    /// Its failing lines (`failure::failure_lines`), when the output shows any.
    failure: Option<String>,
    /// A failing check that also fails at the run's starting commit: what it
    /// failed there (the baseline).
    baseline: Option<Baseline>,
}

/// A check's failure at the commit a run started from, in a check run there.
#[derive(Debug, Clone)]
struct Baseline {
    commit: String,
    check_run: String,
    /// The packages that failed there; empty: the check failed as a whole (no
    /// turbo summary).
    packages: Vec<String>,
}

impl CommandResult {
    fn passed(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// What the agent and the PR get of its output: its failing lines, else its
    /// tail; and whether they're the failing lines.
    fn shown(&self) -> (&str, bool) {
        match &self.failure {
            Some(f) => (f, true),
            None => (&self.tail, false),
        }
    }

    /// Failed here and at the run's starting commit, with nothing new: not the
    /// change's. By package when both list turbo's; as a whole when neither does.
    fn already_failing(&self) -> bool {
        match &self.baseline {
            None => false,
            Some(b) if b.packages.is_empty() => self.failed_packages.is_empty(),
            Some(b) => {
                !self.failed_packages.is_empty()
                    && self.failed_packages.iter().all(|p| b.packages.contains(p))
            }
        }
    }

    /// The packages failing here that failed at the starting commit too.
    fn old_packages(&self) -> Vec<String> {
        let Some(b) = &self.baseline else {
            return Vec::new();
        };
        self.failed_packages
            .iter()
            .filter(|p| b.packages.contains(p))
            .cloned()
            .collect()
    }

    /// How it went, with the starting commit's failure when it's not the change's.
    fn outcome_line(&self) -> String {
        match &self.baseline {
            Some(b) if self.already_failing() => {
                let what = if b.packages.is_empty() {
                    String::new()
                } else {
                    format!(": {}", self.failed_packages.join(", "))
                };
                format!(
                    "{}, already failing on the default branch at `{}` (check run `{}`){what}",
                    self.outcome(),
                    short_commit(&b.commit),
                    b.check_run
                )
            }
            _ => self.outcome(),
        }
    }

    fn outcome(&self) -> String {
        match self.exit_code {
            Some(0) => "passed".into(),
            Some(c) => format!("failed (exit {c})"),
            None => "stopped at its time limit".into(),
        }
    }

    /// The diagnosis for a comment: why, and what you can do.
    fn why(&self) -> String {
        let Some(d) = &self.diagnosis else {
            return String::new();
        };
        let fixes: String = d.fixes.iter().map(|f| format!("\n- {f}")).collect();
        format!(
            "\n\nWhy, as far as actuo can tell: {}.\n\nWhat you can do:{fixes}",
            d.detail
        )
    }
}

/// Text actuo didn't publish: where it was, and the credential format in it.
#[derive(Debug, Clone)]
struct Held {
    field: String,
    format: &'static str,
}

/// How an agent turn ended, as the lane sees it.
struct TurnEnd {
    /// The turn's number in its session.
    turn: u32,
    stop_reason: StopReason,
    files_changed: bool,
    /// Which limit made the lane interrupt it.
    timed_out: Option<&'static str>,
    auth_required: bool,
    /// The lane cancelled its open request because new comments came in.
    superseded: bool,
    /// Requests the rules rejected: the tool call and the rule.
    rejected: Vec<(ToolCall, String)>,
    /// The last error the session reported during the turn (the agent's, as it
    /// gave it).
    error: Option<String>,
}

pub(super) struct RunTask {
    lane: Arc<Lane>,
    run: RunRow,
    cfg: RunConfig,
    marker: String,
    issue: Option<Issue>,
    project: Option<ProjectId>,
    worktree: PathBuf,
    default_branch: String,
    /// Agent time left for the whole run (paused time doesn't count).
    run_left: Duration,
    last_message: String,
    pr_title: Option<String>,
    /// A follow-up commit's subject, from the review reply.
    commit_subject: Option<String>,
    pr_description: String,
    setup_results: Vec<CommandResult>,
    check_results: Vec<CommandResult>,
    entry: Entry,
    control: mpsc::UnboundedReceiver<Control>,
    /// Controls read while a setup or check command ran, handled next.
    held: std::collections::VecDeque<Control>,
    /// Failing checks go back to the agent (off when a resume runs them once).
    retries: bool,
    review_enabled: bool,
    /// The run was resumed after the guards paused it: they don't pause it again.
    skip_guards: bool,
    /// The run paused at a time limit: the next turn says so first.
    time_limit_note: bool,
    /// The commit message was refused: the next review turn says why first.
    commit_refusal: Option<String>,
    /// The automatic turn after a refused commit message has run in this task:
    /// another refusal pauses the run.
    commit_corrected: bool,
    /// The agent's view of why the checks still fail (its review reply's
    /// `Check failure:` line), for the `checks_failed` comment and the PR.
    check_failure: Option<String>,
    /// Who the next change of state or reason is recorded as caused by.
    by: ChangedBy,
    /// The state and reason last saved, to tell a change from a save.
    saved: (RunState, Option<String>),
}

impl RunTask {
    pub(super) fn new(
        lane: Arc<Lane>,
        run: RunRow,
        entry: Entry,
        control: mpsc::UnboundedReceiver<Control>,
    ) -> Self {
        let cfg: RunConfig = serde_json::from_value(run.limits.clone()).unwrap_or_else(|e| {
            panic!("run {}: unreadable limits_json: {e}", run.id);
        });
        let run_left = cfg.run_time_limit();
        let saved = (run.state, run.reason.clone());
        Self {
            lane,
            run,
            cfg,
            marker: prompt::new_marker(),
            issue: None,
            project: None,
            worktree: PathBuf::new(),
            default_branch: String::new(),
            run_left,
            last_message: String::new(),
            pr_title: None,
            commit_subject: None,
            pr_description: String::new(),
            setup_results: Vec::new(),
            check_results: Vec::new(),
            entry,
            control,
            held: std::collections::VecDeque::new(),
            retries: true,
            review_enabled: true,
            skip_guards: false,
            time_limit_note: false,
            commit_refusal: None,
            commit_corrected: false,
            check_failure: None,
            by: ChangedBy::Lane,
            saved,
        }
    }

    /// The client that asked for what happens next (a cancel, a hand-back or a
    /// take-over), as the lane noted it; a client not known otherwise.
    fn caused_by_client(&mut self) {
        self.by = ChangedBy::Client(self.lane.take_cause(&self.run.id).flatten());
    }

    pub(super) async fn drive(mut self) {
        let _ = self.go().await;
        self.lane.finished(&self.run.id);
    }

    async fn go(&mut self) -> Step {
        self.time_limit_note = matches!(self.entry, Entry::Resume | Entry::Comments)
            && matches!(
                self.run.reason.as_deref(),
                Some("turn_time_limit" | "run_time_limit")
            );
        match self.entry {
            Entry::New if self.run.session_id.is_some() => self.go_rerun().await,
            Entry::New => self.go_new().await,
            Entry::Resume => {
                self.caused_by_client();
                self.go_resume().await
            }
            Entry::Recover => self.go_recover().await,
            Entry::Comments => {
                self.restore().await?;
                self.set_running().await;
                self.fast_forward().await?;
                self.setup().await?;
                let cancelled = self.run.reason.as_deref() == Some("request");
                let text = self.comments_turn(cancelled).await?;
                self.agent_turn(text).await?;
                self.finish_from(From::Turn).await
            }
            Entry::TakenOver => Err(self.taken_over("you sent a prompt").await),
            Entry::LabelIgnored => {
                let l = self.cfg.labels.clone();
                let _ = self.labels(&[], &[&l.trigger]).await;
                let _ = self
                    .comment(
                        "the comment",
                        "The trigger label was added again while you have the run (taken over); it was removed and nothing else happened. `actuo run resume` hands the run back.",
                    )
                    .await;
                Ok(())
            }
            Entry::Cancel => Err(self.cancelled().await),
            Entry::PrEnded { merged } => Err(self.pr_ended(merged).await),
        }
    }

    /// A first run: clone or fetch, session, setup, the issue as the first prompt.
    async fn go_new(&mut self) -> Step {
        self.claim().await?;
        {
            let lock = self.lane.repo_lock(&self.run.repo);
            let _clone = lock.lock().await;
            self.prepare().await?;
            self.start_session().await?;
        }
        self.wait_for_agent().await?;
        self.setup().await?;
        let first = self.first_prompt().await?;
        self.agent_turn(first).await?;
        self.finish_from(From::Turn).await
    }

    /// A re-run after an earlier run of the issue ended: the same branch, PR and
    /// session (a new session on the same worktree if that one can't go on), the
    /// branch fast-forwarded to what's on `origin`, setup again (the fast-forward may
    /// have changed dependencies), and the comments since the earlier run's last
    /// prompt as the turn.
    async fn go_rerun(&mut self) -> Step {
        self.claim().await?;
        {
            let lock = self.lane.repo_lock(&self.run.repo);
            let _clone = lock.lock().await;
            self.prepare().await?;
            let sid = self.run.session_id.clone().expect("a session");
            let s = sid.clone();
            let ended = self
                .lane
                .host
                .db(move |st| st.session(&s))
                .await
                .map(|s| s.state == SessionState::Ended)
                .unwrap_or(true);
            if ended {
                self.start_session().await?;
                drop(_clone);
                self.wait_for_agent().await?;
                self.fast_forward().await?;
                self.setup().await?;
                let first = self.first_prompt().await?;
                // "Earlier work" only when the branch has some.
                let (wt, base) = (self.worktree.clone(), self.base());
                let earlier = blocking(move || lane_git::ahead(&wt, &base))
                    .await
                    .unwrap_or(0)
                    > 0;
                let text = if earlier {
                    prompt::earlier_work(&first)
                } else {
                    first
                };
                self.agent_turn(text).await?;
                return self.finish_from(From::Turn).await;
            }
            self.set_worktree().await?;
        }
        self.fast_forward().await?;
        self.setup().await?;
        let text = self.comments_turn(false).await?;
        self.agent_turn(text).await?;
        self.finish_from(From::Turn).await
    }

    /// `run/resume`: from where the run stopped, after any turn still running.
    async fn go_resume(&mut self) -> Step {
        self.restore().await?;
        self.run_left = self.cfg.run_time_limit();
        let reason = self.run.reason.clone().unwrap_or_default();
        let state = self.run.state;
        self.set_running().await;
        self.wait_turn_end().await?;
        match (state, reason.as_str()) {
            (RunState::Waiting, "checks_failed") => {
                self.retries = false;
                self.review_enabled = false;
                self.finish_from(From::Checks).await
            }
            (RunState::Waiting, "protected_path" | "diff_too_large") => {
                self.skip_guards = true;
                self.finish_from(From::Guards).await
            }
            // A failed turn whose error was held back: that turn again, like
            // `turn_failed`.
            (RunState::Waiting, "secret_detected") if self.last_turn_failed().await => {
                match self.last_turn_text().await {
                    Some(text) => self.turn_again(text).await,
                    None => self.finish_from(From::Turn).await,
                }
            }
            // The step that produced the held-back text, again.
            (RunState::Waiting, "secret_detected") => match self.run.step {
                RunStep::Prepare | RunStep::Setup => {
                    self.setup().await?;
                    let first = self.first_prompt().await?;
                    self.agent_turn(first).await?;
                    self.finish_from(From::Turn).await
                }
                RunStep::Guards | RunStep::Push => self.finish_from(From::Guards).await,
                RunStep::Checks | RunStep::Review | RunStep::Pr => {
                    self.retries = false;
                    self.review_enabled = true;
                    self.finish_from(From::Checks).await
                }
                RunStep::Turn => self.finish_from(From::Turn).await,
            },
            // The failed turn again, once per hand-back, on top of whatever it changed.
            (RunState::Waiting, "turn_failed") => match self.last_turn_text().await {
                Some(text) => self.turn_again(text).await,
                None => self.finish_from(From::Turn).await,
            },
            (RunState::Waiting, "agent_stopped") => self.turn_again(prompt::STOPPED.into()).await,
            // The review turn again with the refusal, then the checks again.
            (RunState::Waiting, "commit_format") => {
                self.commit_refusal = self.saved_refusal().await;
                self.finish_from(From::Review).await
            }
            (RunState::Waiting, "cannot_fast_forward") => {
                self.fast_forward().await?;
                self.setup().await?;
                let text = self.comments_turn(false).await?;
                self.agent_turn(text).await?;
                self.finish_from(From::Turn).await
            }
            _ => self.finish_from(From::Turn).await,
        }
    }

    /// A handed-back turn, then on from where that turn would have gone: after the
    /// review turn, its checks; after any other, the usual end of a turn.
    async fn turn_again(&mut self, text: String) -> Step {
        let review = self.run.step == RunStep::Review;
        self.agent_turn(text).await?;
        if review {
            self.review_enabled = false;
            return self.finish_from(From::Checks).await;
        }
        self.finish_from(From::Turn).await
    }

    /// The session's last turn: the failed one, for a hand-back.
    async fn last_turn(&self) -> Option<Turn> {
        let sid = self.run.session_id.clone()?;
        let detail = self
            .lane
            .host
            .db(move |st| st.session_detail(&sid))
            .await
            .ok()?;
        detail.turns.last().cloned()
    }

    async fn last_turn_text(&self) -> Option<String> {
        self.last_turn().await.map(|t| t.text)
    }

    async fn last_turn_failed(&self) -> bool {
        self.last_turn()
            .await
            .is_some_and(|t| t.stop_reason == Some(StopReason::Failed))
    }

    /// After a restart: the step the run was at, again. A turn the restart
    /// cut off gets one "actuo restarted" turn, then the run pauses.
    async fn go_recover(&mut self) -> Step {
        if self.run.session_id.is_none() {
            {
                let lock = self.lane.repo_lock(&self.run.repo);
                let _clone = lock.lock().await;
                self.restore_issue().await?;
                self.prepare().await?;
                self.start_session().await?;
            }
            self.wait_for_agent().await?;
            self.setup().await?;
            let first = self.first_prompt().await?;
            self.agent_turn(first).await?;
            return self.finish_from(From::Turn).await;
        }
        self.restore().await?;
        match self.run.step {
            RunStep::Prepare | RunStep::Setup => {
                self.setup().await?;
                let first = self.first_prompt().await?;
                self.agent_turn(first).await?;
                self.finish_from(From::Turn).await
            }
            RunStep::Turn | RunStep::Review => {
                if !self.last_turn_cut_off().await {
                    return self.finish_from(From::Turn).await;
                }
                if self.run.restart_turns >= self.cfg.max_restart_turns {
                    return Err(self
                        .pause(
                            "restarted",
                            "Actuo restarted during the agent's turn, again.",
                        )
                        .await);
                }
                self.run.restart_turns += 1;
                self.agent_turn(prompt::RESTARTED.into()).await?;
                self.finish_from(From::Turn).await
            }
            RunStep::Checks => self.finish_from(From::Checks).await,
            RunStep::Guards | RunStep::Push | RunStep::Pr => {
                // The check results weren't kept: run them once more, without retries.
                self.retries = false;
                self.review_enabled = false;
                self.finish_from(From::Checks).await
            }
        }
    }

    /// From the end of a turn to the PR: no-changes guard, checks with retries, the
    /// review turn, guards, commit and push, PR. Comments that came in meanwhile get
    /// one more turn before the run finishes.
    async fn finish_from(&mut self, mut from: From) -> Step {
        loop {
            if from <= From::Turn {
                self.no_changes_guard().await?;
            }
            let mut failing = None;
            if from <= From::Checks {
                failing = self.check_rounds().await?;
                if failing.is_none() && self.review_enabled && self.review(None).await? {
                    failing = self.check_rounds().await?;
                }
            } else if from == From::Review {
                self.review(None).await?;
                failing = self.check_rounds().await?;
            }
            // A repo with commit rules gets its commit subject from the agent: without
            // a review reply in this task it would be a fallback (the issue title) the
            // rules refuse. After failing checks, the turn gets their output.
            if self.needs_agent_subject().await && self.review(failing.as_ref()).await? {
                failing = self.check_rounds().await?;
            }
            if !self.skip_guards {
                self.guards().await?;
            }
            if !self.commit_and_push().await? {
                // The turn that corrected a refused message changed files: the checks
                // and guards again before the commit.
                self.review_enabled = false;
                from = From::Checks;
                continue;
            }
            self.open_pr(failing.as_ref()).await?;
            self.check_control().await?;
            if self.pending().is_empty() {
                return self.finish(failing).await;
            }
            let text = self.comments_turn(false).await?;
            self.agent_turn(text).await?;
            self.review_enabled = false;
            from = From::Turn;
        }
    }

    // Steps

    /// Re-reads the issue: if the trigger label is gone (for a labeled run), or a
    /// status label is there that no run of this actuo set, the run is cancelled
    /// without touching the issue. Otherwise the trigger label is swapped for the
    /// running label and the status comment is posted.
    async fn claim(&mut self) -> Step {
        let issue = match self.lane.gh.issue(&self.run.repo, self.run.issue).await {
            Ok(i) => i,
            Err(e) => return Err(self.cancel("not_claimed", &e.to_string()).await),
        };
        let l = self.cfg.labels.clone();
        if self.run.started_via == StartedVia::Label && !issue.labels.contains(&l.trigger) {
            return Err(self
                .cancel(
                    "not_claimed",
                    "the trigger label was removed before the run started",
                )
                .await);
        }
        let ours = [&l.running, &l.waiting, &l.pr, &l.failed];
        let status_set = issue.labels.iter().any(|x| ours.contains(&x));
        let (repo, n, id) = (self.run.repo.clone(), self.run.issue, self.run.id.clone());
        let earlier = self
            .lane
            .host
            .db(move |s| {
                Ok(s.runs(Some(&repo), None)?
                    .into_iter()
                    .any(|r| r.issue == n && r.id != id))
            })
            .await
            .unwrap_or(false);
        if status_set && !earlier {
            return Err(self
                .cancel(
                    "not_claimed",
                    "a status label is set that no run of this actuo set (another actuo instance may be handling this issue)",
                )
                .await);
        }
        self.run.title = Some(issue.title.clone());
        self.issue = Some(issue);
        self.run.state = RunState::Running;
        self.run.step = RunStep::Prepare;
        self.save().await;
        let remove: Vec<&str> = [&l.trigger, &l.waiting, &l.pr, &l.failed]
            .iter()
            .map(|s| s.as_str())
            .collect();
        if let Err(e) = self.labels(&[&l.running], &remove).await {
            return Err(self.fail("gh_error", &e.to_string()).await);
        }
        self.status().await;
        if let Err(e) = self.lane.host.sandbox_ready().await {
            return Err(self.fail("sandbox_unavailable", &e.message).await);
        }
        // Any agent actuo has (an `agent:` label may name one it hasn't).
        let agents = &self.lane.host.agents.agents;
        if !agents.contains_key(&self.run.agent) {
            let message = format!(
                "Actuo has no agent `{}` (it has: {}).",
                self.run.agent,
                agents.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            return Err(self.fail("agent_not_found", &message).await);
        }
        Ok(())
    }

    /// The lane's clone (cloned once, fetched each run), the default branch, and the
    /// project for it.
    async fn prepare(&mut self) -> Step {
        let repo = self.run.repo.clone();
        let clone = self.lane.host.paths().lane_clone(&repo);
        if clone.join(".git").exists() {
            let started = Instant::now();
            let n = self
                .step_start(RunStepKind::Fetch, Some("git fetch origin"))
                .await;
            let c = clone.clone();
            let r = blocking(move || lane_git::fetch(&c)).await;
            let (code, tail) = match &r {
                Ok(()) => (Some(0), None),
                Err(e) => (None, Some(e.to_string())),
            };
            self.step_end(
                n,
                RunStepKind::Fetch,
                Some("git fetch origin"),
                code,
                tail.as_deref(),
                None,
                started,
            )
            .await;
            if let Err(e) = r {
                return Err(self.fail("git_error", &e.to_string()).await);
            }
        } else {
            let command = format!("gh repo clone {repo}");
            let started = Instant::now();
            let n = self.step_start(RunStepKind::Fetch, Some(&command)).await;
            let r = self.lane.gh.clone_repo(&repo, &clone).await;
            let (code, tail) = match &r {
                Ok(()) => (Some(0), None),
                Err(e) => (None, Some(e.to_string())),
            };
            self.step_end(
                n,
                RunStepKind::Fetch,
                Some(&command),
                code,
                tail.as_deref(),
                None,
                started,
            )
            .await;
            if let Err(e) = r {
                return Err(self.fail("gh_error", &e.to_string()).await);
            }
        }
        self.default_branch = match self.lane.gh.default_branch(&repo).await {
            Ok(b) => b,
            Err(e) => return Err(self.fail("gh_error", &e.to_string()).await),
        };
        self.run.base = Some(format!("origin/{}", self.default_branch));
        match self.lane.host.open_project(&clone.to_string_lossy()).await {
            Ok(p) => self.project = Some(p.id),
            Err(e) => return Err(self.fail("git_error", &e.message).await),
        }
        self.save().await;
        Ok(())
    }

    /// A clean session on `actuo/issue-<n>` from `origin/<default>`, without a
    /// prompt (its worktree exists when this returns).
    async fn start_session(&mut self) -> Step {
        let params = SessionStartParams {
            project_id: self.project.clone().expect("prepared"),
            agent: Some(self.run.agent.clone()),
            worktree: true,
            base: self.run.base.clone(),
            branch: Some(self.run.branch.clone()),
            level: self.run.level,
            clean: true,
            config: self.run.config.clone(),
            prompt: None,
            // Always inside the sandbox; the package registries in the project's
            // network, all of it with `agent_network`.
            sandbox: Some(actuo_protocol::types::SandboxOptions {
                hosts: self.agent_hosts(),
                read: self.cfg.read.clone(),
            }),
            web_tools: self.cfg.web_tools,
            telemetry: self.cfg.telemetry,
        };
        let sid = match self
            .lane
            .host
            .start_session(params, Some(CLIENT.into()))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let reason = if e.kind() == Some(ErrorKind::GitError) {
                    "git_error"
                } else {
                    "agent_error"
                };
                return Err(self.fail(reason, &e.message).await);
            }
        };
        self.run.session_id = Some(sid);
        self.status().await;
        self.save().await;
        Ok(())
    }

    /// Waits until the session's agent is up.
    async fn wait_for_agent(&mut self) -> Step {
        let sid = self.run.session_id.clone().expect("a session");
        let mut watch = Watch::from_seq(&self.lane.host, &sid, 0);
        let mut auth = false;
        let mut last_error = String::new();
        let waited = tokio::time::timeout(self.cfg.turn_time_limit(), async {
            while let Some(e) = watch.next().await {
                match &e.body {
                    EventBody::Error(d) => {
                        auth |= d.kind == ErrorKind::AuthRequired;
                        last_error = d.message.clone();
                    }
                    EventBody::SessionState(d) if d.state == SessionState::Idle => return true,
                    EventBody::SessionState(d) if d.state == SessionState::Ended => return false,
                    _ => {}
                }
            }
            false
        })
        .await;
        match waited {
            Ok(true) => {}
            Ok(false) if auth => {
                return Err(self
                    .fail(
                        "auth_required",
                        "The agent needs a login: log in with the agent's own CLI.",
                    )
                    .await);
            }
            Ok(false) => {
                let message = format!("The agent didn't start: {last_error}");
                return Err(self.fail("agent_error", &message).await);
            }
            Err(_) => {
                let _ = self.lane.host.stop(&sid).await;
                return Err(self
                    .fail("agent_error", "The agent didn't start in time.")
                    .await);
            }
        }
        let s = sid.clone();
        match self.lane.host.db(move |st| st.session(&s)).await {
            Ok(s) => self.worktree = PathBuf::from(s.cwd),
            Err(e) => return Err(self.fail("agent_error", &e.message).await),
        }
        Ok(())
    }

    /// The project's setup, checks and network as this run uses them now: what it
    /// sets, else what the run's worktree says (`detect::resolve`).
    fn resolved(&self) -> crate::detect::Resolved {
        crate::detect::resolve(
            self.cfg.setup.as_ref(),
            self.cfg.checks.as_ref(),
            self.cfg.network.as_ref(),
            &crate::detect::DirFiles {
                root: self.worktree.clone(),
            },
        )
    }

    /// The hosts the agent's session reaches (besides its own), decided before its
    /// worktree exists: from the clone at the run's base.
    fn agent_hosts(&self) -> Vec<String> {
        let clone = self.lane.host.paths().lane_clone(&self.run.repo);
        let base = self
            .run
            .base
            .clone()
            .unwrap_or_else(|| format!("origin/{}", self.default_branch));
        let r = crate::detect::resolve(
            self.cfg.setup.as_ref(),
            self.cfg.checks.as_ref(),
            self.cfg.network.as_ref(),
            &crate::detect::GitFiles::new(&clone, &base),
        );
        crate::detect::agent_hosts(&r.network, self.cfg.agent_network)
    }

    /// The repo's setup commands in the worktree. One that fails fails the run and
    /// stops the session.
    async fn setup(&mut self) -> Step {
        self.run.step = RunStep::Setup;
        self.save().await;
        let limit = Duration::from_secs(self.cfg.setup_time_limit_s);
        for command in self.resolved().setup {
            let result = self
                .command(RunStepKind::Setup, &command, limit, None)
                .await?;
            self.setup_results.push(result.clone());
            self.status().await;
            if !result.passed() {
                if let Some(sid) = self.run.session_id.clone() {
                    let _ = self.lane.host.stop(&sid).await;
                }
                let message = format!(
                    "Setup `{}` {}:\n\n{}{}",
                    result.command,
                    result.outcome(),
                    publish::fenced(&result.tail),
                    result.why()
                );
                return Err(self.fail("setup_failed", &message).await);
            }
        }
        Ok(())
    }

    async fn first_prompt(&mut self) -> Step<String> {
        let comments = match self.lane.gh.comments(&self.run.repo, self.run.issue).await {
            Ok(c) => c,
            Err(e) => return Err(self.fail("gh_error", &e.to_string()).await),
        };
        let comments: Vec<Comment> = comments
            .into_iter()
            .filter(|c| self.cfg.trusted.contains(&c.author) && !own_comment(&c.body))
            .collect();
        self.run.last_prompt_at = comments.iter().map(|c| c.created_at.clone()).max();
        let issue = self.issue.clone().expect("claimed");
        let resolved = self.resolved();
        Ok(prompt::first_prompt(&prompt::FirstPrompt {
            repo: &self.run.repo,
            issue: &issue,
            comments: &comments,
            checks: &resolved.checks,
            agent_network: !crate::detect::agent_hosts(&resolved.network, self.cfg.agent_network)
                .is_empty(),
            installed: !resolved.setup.is_empty(),
            marker: &self.marker,
            max_bytes: self.cfg.max_prompt_bytes as usize,
        }))
    }

    /// Sends a turn and follows it to its end, answering requests by the rules and
    /// pausing on the ones they don't cover. Returns whether the turn changed files.
    async fn agent_turn(&mut self, mut text: String) -> Step<bool> {
        if std::mem::take(&mut self.time_limit_note) {
            text = format!("{}\n{text}", prompt::TIME_LIMIT);
        }
        loop {
            self.check_control().await?;
            self.run.state = RunState::Running;
            if self.run.step != RunStep::Review {
                self.run.step = RunStep::Turn;
            }
            self.save().await;
            let end = self.one_turn(text).await?;
            if end.superseded {
                text = self.comments_turn(true).await?;
                continue;
            }
            // Rules rejected something: say what and why, a limited number of times.
            // A reject can end the agent's turn (Codex ends it whole).
            let ended = end.stop_reason == StopReason::EndTurn
                || (end.stop_reason == StopReason::Cancelled && end.timed_out.is_none());
            if ended && !end.rejected.is_empty() {
                if self.run.reject_turns >= self.cfg.max_reject_turns {
                    let list: Vec<String> = end
                        .rejected
                        .iter()
                        .map(|(call, rule)| {
                            format!("{}Rule: `{rule}`.", asks(call, &self.worktree))
                        })
                        .collect();
                    let message = format!(
                        "The rules rejected what the agent asked for, after {} follow-up turns:\n\n{}",
                        self.cfg.max_reject_turns,
                        list.join("\n\n")
                    );
                    return Err(self.pause("rejects", &message).await);
                }
                self.run.reject_turns += 1;
                let titles: Vec<(String, String)> = end
                    .rejected
                    .iter()
                    .map(|(call, rule)| (call.title.clone(), rule.clone()))
                    .collect();
                text = prompt::rejected(&titles);
                continue;
            }
            match end.stop_reason {
                StopReason::EndTurn => return Ok(end.files_changed),
                StopReason::Cancelled => {
                    if let Some(limit) = end.timed_out {
                        let message = format!(
                            "The agent's turn was stopped at the {limit} limit ({}).",
                            match limit {
                                "turn time" => format!("{}s", self.cfg.turn_time_limit_s),
                                _ => format!("{}s", self.cfg.run_time_limit_s),
                            }
                        );
                        let reason = if limit == "turn time" {
                            "turn_time_limit"
                        } else {
                            "run_time_limit"
                        };
                        return Err(self.pause(reason, &message).await);
                    }
                    return Err(self.taken_over("the turn was interrupted").await);
                }
                StopReason::MaxTokens => {
                    return Err(self
                        .pause(
                            "max_tokens",
                            "The agent stopped: it ran out of output tokens.",
                        )
                        .await);
                }
                StopReason::Refusal => {
                    return Err(self.pause("refusal", "The agent refused to go on.").await);
                }
                _ if end.auth_required => {
                    return Err(self
                        .fail(
                            "auth_required",
                            "The agent needs a login: log in with the agent's own CLI.",
                        )
                        .await);
                }
                // The agent answered the turn with an error: no automatic turn, since
                // the same prompt would most likely fail the same way. Handing back
                // sends it again.
                StopReason::Failed => return Err(self.turn_failed(&end).await),
                StopReason::MaxTurnRequests
                | StopReason::AgentExited
                | StopReason::ActuoStopped => {
                    if self.run.restart_turns >= self.cfg.max_restart_turns {
                        return Err(self.stopped(&end).await);
                    }
                    self.run.restart_turns += 1;
                    text = prompt::STOPPED.into();
                }
            }
        }
    }

    async fn one_turn(&mut self, text: String) -> Step<TurnEnd> {
        let sid = self.run.session_id.clone().expect("a session");
        let mut watch = match Watch::from_head(&self.lane.host, &sid).await {
            Ok(w) => w,
            Err(e) => return Err(self.fail("agent_error", &e.message).await),
        };
        self.last_message.clear();
        let n = match self
            .lane
            .host
            .send(&sid, text, Some(CLIENT.into()), false)
            .await
        {
            Ok(n) => n,
            Err(e) if e.kind() == Some(ErrorKind::AuthRequired) => {
                return Err(self
                    .fail(
                        "auth_required",
                        "The agent needs a login: log in with the agent's own CLI.",
                    )
                    .await);
            }
            // Someone sent a prompt between the lane's turns.
            Err(e) if e.kind() == Some(ErrorKind::TurnInProgress) => {
                return Err(self.taken_over("you sent a prompt").await);
            }
            Err(e) if self.lane.stopping() => {
                let _ = e;
                return Err(Halt);
            }
            Err(e) => return Err(self.fail("agent_error", &e.message).await),
        };
        let mut turn_left = self.cfg.turn_time_limit();
        let mut superseded = false;
        let mut active_since = Some(tokio::time::Instant::now());
        let mut paused_on: Option<RequestId> = None;
        // A client's interrupt cancelled the open request: the turn ending
        // cancelled takes the run over from waiting, with no "running" between.
        let mut interrupted = false;
        let mut timed_out = None;
        let mut auth_required = false;
        let mut error = None;
        let mut rejected = Vec::new();
        loop {
            let deadline = match (active_since, timed_out) {
                (Some(since), None) => Some(since + turn_left.min(self.run_left)),
                _ => None,
            };
            let woke = tokio::select! {
                e = watch.next() => Ok(e),
                c = self.control.recv() => Err(c),
                _ = sleep_until(deadline) => {
                    timed_out = Some(if turn_left <= self.run_left { "turn time" } else { "run time" });
                    let _ = self.lane.host.interrupt(&sid).await;
                    continue;
                }
            };
            let event = match woke {
                Ok(e) => e,
                Err(Some(Control::Cancel)) => return Err(self.cancelled().await),
                Err(Some(Control::PrEnded { merged })) => return Err(self.pr_ended(merged).await),
                Err(Some(Control::Comments)) if paused_on.is_some() => {
                    // New comments replace the open request: cancel it, end the turn,
                    // and send the comments as the next turn.
                    let request = paused_on.take().expect("paused");
                    superseded = true;
                    let _ = self
                        .lane
                        .host
                        .respond(&sid, request, None, true, Some(CLIENT.into()))
                        .await;
                    let _ = self.lane.host.interrupt(&sid).await;
                    continue;
                }
                Err(Some(Control::Comments)) => {
                    self.collect_comments().await?;
                    continue;
                }
                // A turn is running: nobody else can have sent one.
                Err(Some(Control::TakeOver)) => continue,
                Err(None) => continue,
            };
            let Some(event) = event else {
                return Err(self
                    .fail("agent_error", "the session's events stopped")
                    .await);
            };
            match &event.body {
                EventBody::RequestOpened(d) if event.turn == Some(n) => {
                    let decision = rules::decide(
                        &self.cfg.rules,
                        &d.tool_call,
                        &d.options,
                        &self.worktree,
                        true,
                    );
                    match decision {
                        rules::Decision::Answer(option, kind, rule) => {
                            let _ = self
                                .lane
                                .host
                                .respond(
                                    &sid,
                                    d.request_id.clone(),
                                    Some(option),
                                    false,
                                    Some(CLIENT.into()),
                                )
                                .await;
                            if matches!(
                                kind,
                                PermissionOptionKind::RejectOnce
                                    | PermissionOptionKind::RejectAlways
                            ) {
                                rejected.push((
                                    d.tool_call.clone(),
                                    rules::describe(&self.cfg.rules[rule]),
                                ));
                            }
                        }
                        rules::Decision::Pause if paused_on.is_none() => {
                            if let Some(since) = active_since.take() {
                                let used = since.elapsed();
                                turn_left = turn_left.saturating_sub(used);
                                self.run_left = self.run_left.saturating_sub(used);
                            }
                            paused_on = Some(d.request_id.clone());
                            self.pause_for_request(&d.tool_call).await;
                        }
                        rules::Decision::Pause => {}
                    }
                }
                EventBody::RequestResolved(d) if paused_on.as_ref() == Some(&d.request_id) => {
                    paused_on = None;
                    active_since = Some(tokio::time::Instant::now());
                    if d.by == ResolvedBy::Interrupt {
                        interrupted = true;
                    } else {
                        self.resume_after_request().await;
                    }
                }
                EventBody::ItemCompleted(d) if event.turn == Some(n) => {
                    if let Item::Message(m) = &d.item {
                        self.last_message = m.text.clone();
                    }
                }
                EventBody::Error(d) => {
                    auth_required |= d.kind == ErrorKind::AuthRequired;
                    error = Some(d.message.clone());
                }
                EventBody::TurnCompleted(d) if event.turn == Some(n) => {
                    if d.stop_reason == StopReason::ActuoStopped && self.lane.stopping() {
                        // Actuo is stopping: the run picks up from here at start.
                        return Err(Halt);
                    }
                    if let Some(since) = active_since.take() {
                        self.run_left = self.run_left.saturating_sub(since.elapsed());
                    }
                    // As `agent_turn` decides: rejects get a follow-up turn first.
                    let taken_over = interrupted
                        && d.stop_reason == StopReason::Cancelled
                        && timed_out.is_none()
                        && rejected.is_empty();
                    if paused_on.is_some() || superseded || (interrupted && !taken_over) {
                        self.resume_after_request().await;
                    }
                    return Ok(TurnEnd {
                        turn: n,
                        stop_reason: d.stop_reason,
                        files_changed: !d.files.is_empty(),
                        timed_out,
                        auth_required,
                        superseded,
                        rejected,
                        error,
                    });
                }
                _ => {}
            }
        }
    }

    /// After the first turn: a worktree with no changes pauses, with the agent's
    /// last message (usually a question or a reason).
    async fn no_changes_guard(&mut self) -> Step {
        let (wt, base) = (self.worktree.clone(), self.base());
        match blocking(move || lane_git::changes(&wt, &base)).await {
            Ok(c) if c.is_empty() => {
                let message = format!(
                    "The agent made no changes. Its last message:\n\n{}",
                    quote(&self.last_message)
                );
                Err(self.pause("no_changes", &message).await)
            }
            Ok(_) => Ok(()),
            Err(e) => Err(self.fail("git_error", &e.to_string()).await),
        }
    }

    /// Runs the checks; each failing round goes back to the agent while retries are
    /// left. Returns the failing check when they still fail after the retries.
    async fn check_rounds(&mut self) -> Step<Option<CommandResult>> {
        loop {
            self.check_control().await?;
            self.run.state = RunState::Checking;
            self.run.step = RunStep::Checks;
            self.save().await;
            self.check_results.clear();
            let limit = Duration::from_secs(self.cfg.check_time_limit_s);
            let mut failed = None;
            // After each turn: what the worktree says now (the agent may have added
            // the project's scripts).
            for command in self.resolved().checks {
                self.check_control().await?;
                let mut r = self
                    .command(RunStepKind::Check, &command, limit, None)
                    .await?;
                if !r.passed() {
                    r.baseline = self.baseline(&r.command).await?;
                }
                self.check_results.push(r.clone());
                // Failing at the run's starting commit too, nothing new: not the
                // change's; the PR says so and the next check runs.
                if !r.passed() && !r.already_failing() {
                    failed = Some(r);
                    break;
                }
            }
            self.status().await;
            // Comments that came in during the checks go into the next turn.
            self.check_control().await?;
            let Some(failed) = failed else {
                return Ok(None);
            };
            if !self.retries || self.run.check_rounds >= self.cfg.max_check_retries {
                return Ok(Some(failed));
            }
            self.run.check_rounds += 1;
            let (output, failing_lines) = failed.shown();
            let mut text = prompt::check_failed(
                &failed.command,
                failed.exit_code,
                output,
                failing_lines,
                &self.marker,
            );
            let old = failed.old_packages();
            if !old.is_empty() {
                let new: Vec<String> = failed
                    .failed_packages
                    .iter()
                    .filter(|p| !old.contains(p))
                    .cloned()
                    .collect();
                text.push_str(&prompt::already_failing(&old, &new));
            }
            let text = self.with_pending(text);
            self.agent_turn(text).await?;
        }
    }

    /// Whether the next commit's subject would be a fallback in a repo with commit
    /// rules: no PR title (first commit) or `Commit:` subject (follow-up) from a
    /// review reply in this task.
    async fn needs_agent_subject(&self) -> bool {
        if self.cfg.commit_check.is_none() && self.cfg.commit_subject_prompt.is_none() {
            return false;
        }
        if self.follow_up().await {
            self.commit_subject.is_none()
        } else {
            self.pr_title.is_none()
        }
    }

    /// One more turn: review against the issue, reply with a PR title and description.
    /// `failing`: the checks still fail; the turn gets their output and is asked
    /// whether the failure is its change's or the machine's. Returns whether it
    /// changed files (then the checks run again).
    async fn review(&mut self, failing: Option<&CommandResult>) -> Step<bool> {
        self.run.step = RunStep::Review;
        self.check_control().await?;
        let follow_up = self.follow_up().await;
        let mut review = prompt::review(
            &self.default_branch,
            follow_up,
            self.cfg.commit_subject_prompt.as_deref(),
        );
        if let Some(check) = &self.cfg.commit_check {
            review.push_str(&prompt::check_your_subject(
                check,
                follow_up,
                self.run.issue,
            ));
        }
        let text = match self.commit_refusal.take() {
            Some(refusal) => format!("{refusal}{review}"),
            None => review,
        };
        let text = match failing {
            Some(f) => format!(
                "{}{text}",
                prompt::checks_still_failing(
                    &f.command,
                    &f.outcome(),
                    f.shown().0,
                    f.shown().1,
                    &self.marker,
                )
            ),
            None => text,
        };
        let text = self.with_pending(text);
        let changed = self.agent_turn(text).await?;
        let (note, reply) = prompt::split_check_failure(&self.last_message);
        if failing.is_some() {
            self.check_failure = note;
        }
        let reply = if follow_up {
            let (subject, rest) = prompt::split_commit_subject(&reply);
            self.commit_subject = subject;
            rest
        } else {
            reply.as_str()
        };
        if let Some((title, description)) = prompt::parse_review_reply(reply) {
            self.pr_title = Some(title);
            self.pr_description = prompt::trim_description(&description, self.run.issue);
        }
        Ok(changed)
    }

    /// Pauses when the change touches a protected path or is over the size limits.
    async fn guards(&mut self) -> Step {
        self.check_control().await?;
        self.run.state = RunState::Running;
        self.run.step = RunStep::Guards;
        self.save().await;
        let (wt, base) = (self.worktree.clone(), self.base());
        let changes = match blocking(move || lane_git::changes(&wt, &base)).await {
            Ok(c) => c,
            Err(e) => return Err(self.fail("git_error", &e.to_string()).await),
        };
        let protected: Vec<&str> = changes
            .iter()
            .filter(|c| lane_git::is_protected(&c.path, &self.cfg.protected_paths))
            .map(|c| c.path.as_str())
            .collect();
        if !protected.is_empty() {
            let message = format!(
                "The change touches protected paths, which need you:\n\n{}",
                publish::fenced(&protected.join("\n"))
            );
            return Err(self.pause("protected_path", &message).await);
        }
        // Lockfiles are generated: a scaffold or a dependency change makes
        // thousands of lines there; they don't count toward the limits.
        let (locks, counted): (Vec<_>, Vec<_>) = changes.iter().partition(|c| is_lockfile(&c.path));
        let lines: u64 = counted.iter().map(|c| c.added + c.deleted).sum();
        // 0: no limit.
        let over = |n: u64, limit: u32| limit != 0 && n > u64::from(limit);
        if over(counted.len() as u64, self.cfg.max_changed_files)
            || over(lines, self.cfg.max_changed_lines)
        {
            let lock_lines: u64 = locks.iter().map(|c| c.added + c.deleted).sum();
            let not_counted = if locks.is_empty() {
                String::new()
            } else {
                format!(
                    " Not counted: {lock_lines} lines in {}.",
                    locks
                        .iter()
                        .map(|c| c.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let limit = |l: u32| {
                if l == 0 {
                    "none".to_string()
                } else {
                    l.to_string()
                }
            };
            let message = format!(
                "The change is over the limits: {} {} (limit {}), {lines} lines (limit {}).{not_counted}",
                counted.len(),
                if counted.len() == 1 { "file" } else { "files" },
                limit(self.cfg.max_changed_files),
                limit(self.cfg.max_changed_lines)
            );
            return Err(self.pause("diff_too_large", &message).await);
        }
        Ok(())
    }

    /// Before actuo's commit: the credential check over what the push would add
    /// (the branch's head on `origin`, or the base when it isn't there yet, to the
    /// staged change): added lines and new file names, the message of each commit
    /// already made that isn't on `origin` (only from outside the sandbox: the agent
    /// can't write the clone's `.git`), and actuo's own message.
    async fn commit_gate(&mut self, message: &str) -> Step<Option<Held>> {
        let (wt, branch, base) = (self.worktree.clone(), self.run.branch.clone(), self.base());
        let message = message.to_string();
        let r = blocking(move || -> Result<Option<Held>, crate::git::GitError> {
            lane_git::stage(&wt)?;
            let against = lane_git::remote_head(&wt, &branch)?.unwrap_or(base);
            let diff = lane_git::staged_diff(&wt, &against)?;
            if let Some(h) = publish::scan_diff(&diff) {
                let field = match h.line {
                    Some(n) => format!("the change, {} line {n}", publish::code_span(&h.path)),
                    None => "the change, a file name".to_string(),
                };
                return Ok(Some(Held {
                    field,
                    format: h.format,
                }));
            }
            for (id, text) in lane_git::messages(&wt, &against)? {
                if let Some(h) = publish::scan(&text) {
                    return Ok(Some(Held {
                        field: format!("the commit `{}`'s message", &id[..id.len().min(12)]),
                        format: h.format,
                    }));
                }
            }
            Ok(publish::scan(&message).map(|h| Held {
                field: "the commit message (from the PR title or commit subject)".to_string(),
                format: h.format,
            }))
        })
        .await;
        match r {
            Ok(h) => Ok(h),
            Err(e) => Err(self.fail("git_error", &e.to_string()).await),
        }
    }

    /// The repo's commit rules on actuo's message: no `Co-Authored-By` line
    /// when `co_authored_by` is off, then `commit_check` on a file holding the
    /// message, sandboxed like the checks and recorded as one. A refusal, with
    /// nothing committed.
    async fn commit_rules(&mut self, message: &str) -> Step<Option<Refused>> {
        if !self.cfg.co_authored_by && has_co_authored_by(message) {
            let text = format!(
                "The commit message has a `Co-Authored-By` line, which this repo doesn't allow \
                 (`co_authored_by = false`), so nothing was committed or pushed. The message:\n\n{}",
                publish::fenced(message.trim_end())
            );
            return Ok(Some(Refused {
                command: None,
                tail: String::new(),
                text,
            }));
        }
        let Some(check) = self.cfg.commit_check.clone() else {
            return Ok(None);
        };
        let file = crate::sandbox::temp_path(self.lane.host.paths(), &self.run.id.0)
            .join("commit-message");
        let command = format!("{check} {}", sh_quote(&file.to_string_lossy()));
        let limit = Duration::from_secs(self.cfg.check_time_limit_s);
        let r = self
            .command(RunStepKind::Check, &command, limit, Some((&file, message)))
            .await?;
        if r.passed() {
            return Ok(None);
        }
        let how = match r.exit_code {
            Some(c) => format!("exit code {c}"),
            None => "stopped at its time limit".to_string(),
        };
        let text = format!(
            "The commit message was refused by `{check}` ({how}), so nothing was committed or \
             pushed. The message:\n\n{}\n\nThe end of the command's output:\n\n{}",
            publish::fenced(message.trim_end()),
            publish::fenced(&r.tail)
        );
        Ok(Some(Refused {
            command: Some(check),
            tail: r.tail,
            text,
        }))
    }

    /// Pauses `commit_format`, keeping the refusal for the review turn a hand-back
    /// runs (in the run's log folder: the run's temp dir is made fresh for each step).
    async fn commit_refused(&mut self, command: Option<&str>, tail: &str, text: &str) -> Halt {
        let file = self.refusal_file();
        let saved = serde_json::to_vec(&serde_json::json!({"command": command, "tail": tail}))
            .map_err(std::io::Error::other)
            .and_then(|v| {
                std::fs::create_dir_all(file.parent().expect("a folder"))?;
                std::fs::write(&file, v)
            });
        if let Err(e) = saved {
            crate::log!(
                "lane: {}: couldn't keep the commit refusal: {e}",
                self.run.id
            );
        }
        let text = format!(
            "{text}\n\nThe agent had one turn to correct it, and this is the message after it. \
             Handing the run back runs the review turn again, with the refusal."
        );
        self.pause("commit_format", &text).await
    }

    /// The refusal a `commit_format` pause kept, as the review turn's opening.
    async fn saved_refusal(&self) -> Option<String> {
        let file = self.refusal_file();
        let bytes = std::fs::read(&file).ok()?;
        let _ = std::fs::remove_file(&file);
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        Some(prompt::commit_refused(
            v["command"].as_str(),
            v["tail"].as_str().unwrap_or_default(),
            &self.marker,
        ))
    }

    fn refusal_file(&self) -> PathBuf {
        self.lane
            .host
            .paths()
            .run_logs(&self.run.id)
            .join("commit_format.json")
    }

    /// Actuo's commit and the push to `actuo/issue-<n>`. A message the
    /// repo's commit rules refuse gets one automatic turn with the refusal; false
    /// when that turn changed files (the checks run again first). A second refusal
    /// pauses the run (`commit_format`).
    async fn commit_and_push(&mut self) -> Step<bool> {
        self.check_control().await?;
        self.run.step = RunStep::Push;
        self.save().await;
        let message = loop {
            let message = self.commit_message().await;
            let Some(refused) = self.commit_rules(&message).await? else {
                break message;
            };
            if self.commit_corrected {
                return Err(self
                    .commit_refused(refused.command.as_deref(), &refused.tail, &refused.text)
                    .await);
            }
            self.commit_corrected = true;
            self.commit_refusal = Some(prompt::commit_refused(
                refused.command.as_deref(),
                &refused.tail,
                &self.marker,
            ));
            if self.review(None).await? {
                return Ok(false);
            }
            self.run.step = RunStep::Push;
            self.save().await;
        };
        self.commit(message).await?;
        Ok(true)
    }

    /// The message of actuo's commit: the first commit takes the PR title; a
    /// follow-up one (the branch has the earlier runs' commits) its own subject.
    async fn commit_message(&self) -> String {
        let title = if self.follow_up().await {
            self.commit_subject
                .clone()
                .unwrap_or_else(|| prompt::follow_up_subject(self.run.issue))
        } else {
            self.title()
        };
        format!(
            "{title}\n\nCloses #{}\n\nActuo-Session: {} ({})\n",
            self.run.issue,
            self.run
                .session_id
                .as_ref()
                .map(|s| s.0.as_str())
                .unwrap_or_default(),
            self.run.agent
        )
    }

    /// Commits `message` (after the commit gate) and pushes.
    async fn commit(&mut self, message: String) -> Step {
        if let Some(h) = self.commit_gate(&message).await? {
            return Err(self.hold(h).await);
        }
        let started = Instant::now();
        let n = self.step_start(RunStepKind::Commit, None).await;
        let (wt, sign) = (self.worktree.clone(), self.cfg.sign_commits);
        let r = blocking(move || lane_git::commit(&wt, &message, sign)).await;
        let (code, tail) = match &r {
            Ok(Some(sha)) => (Some(0), Some(sha.clone())),
            Ok(None) => (Some(0), Some("nothing to commit".to_string())),
            Err(e) => (None, Some(e.to_string())),
        };
        self.step_end(
            n,
            RunStepKind::Commit,
            None,
            code,
            tail.as_deref(),
            None,
            started,
        )
        .await;
        if let Err(e) = r {
            return Err(self.fail("git_error", &e.to_string()).await);
        }
        // Pushed already (a re-run without new changes, or a restart after the push).
        let (wt, branch) = (self.worktree.clone(), self.run.branch.clone());
        let pushed = blocking(move || {
            Ok::<_, crate::git::GitError>(
                lane_git::remote_head(&wt, &branch)? == Some(lane_git::head(&wt)?),
            )
        })
        .await;
        match pushed {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(self.fail("git_error", &e.to_string()).await),
        }
        let command = format!("git push origin HEAD:refs/heads/{}", self.run.branch);
        let started = Instant::now();
        let n = self.step_start(RunStepKind::Push, Some(&command)).await;
        let (wt, branch, issue) = (
            self.worktree.clone(),
            self.run.branch.clone(),
            self.run.issue,
        );
        let r = blocking(move || lane_git::push_run_branch(&wt, &branch, issue)).await;
        let (code, tail) = match &r {
            Ok(()) => (Some(0), None),
            Err(e) => (None, Some(e.to_string())),
        };
        self.step_end(
            n,
            RunStepKind::Push,
            Some(&command),
            code,
            tail.as_deref(),
            None,
            started,
        )
        .await;
        if let Err(e) = r {
            return Err(self.fail("git_error", &e.to_string()).await);
        }
        Ok(())
    }

    /// The draft PR (the open one from this branch, if there is one already).
    async fn open_pr(&mut self, failing: Option<&CommandResult>) -> Step {
        self.check_control().await?;
        self.run.step = RunStep::Pr;
        self.save().await;
        let body = self.pr_body(failing).await;
        let (title, body) = match (
            outgoing("the PR title", &self.title()),
            outgoing("the PR description", &body),
        ) {
            (Ok(t), Ok(b)) => (t, b),
            (Err(h), _) | (_, Err(h)) => return Err(self.hold(h).await),
        };
        let started = Instant::now();
        let n = self.step_start(RunStepKind::Pr, None).await;
        let gh = self.lane.gh.clone();
        let (repo, branch) = (self.run.repo.clone(), self.run.branch.clone());
        let r = match gh.open_pr(&repo, &branch).await {
            Ok(Some(pr)) => gh.edit_pr_body(&repo, pr.number, &body).await.map(|_| pr),
            Ok(None) => match gh
                .create_draft_pr(&repo, &self.default_branch, &branch, &title, &body)
                .await
            {
                // Created meanwhile (a `gh` call that outlived a killed actuo).
                Err(GhError::Failed { stderr, .. }) if stderr.contains("already exists") => {
                    match gh.open_pr(&repo, &branch).await {
                        Ok(Some(pr)) => gh.edit_pr_body(&repo, pr.number, &body).await.map(|_| pr),
                        Ok(None) => Err(GhError::Output {
                            command: "pr list".into(),
                            reason: format!("no open PR from {branch}"),
                        }),
                        Err(e) => Err(e),
                    }
                }
                r => r,
            },
            Err(e) => Err(e),
        };
        let (code, tail) = match &r {
            Ok(pr) => (Some(0), Some(pr.url.clone())),
            Err(e) => (None, Some(e.to_string())),
        };
        self.step_end(
            n,
            RunStepKind::Pr,
            None,
            code,
            tail.as_deref(),
            None,
            started,
        )
        .await;
        match r {
            Ok(pr) => {
                self.run.pr_number = Some(pr.number);
                self.run.pr_url = Some(pr.url);
                self.save().await;
                Ok(())
            }
            Err(e) => Err(self.fail("gh_error", &e.to_string()).await),
        }
    }

    async fn finish(&mut self, failing: Option<CommandResult>) -> Step {
        let url = self.run.pr_url.clone().unwrap_or_default();
        let l = self.cfg.labels.clone();
        match failing {
            None => {
                self.run.state = RunState::PrOpened;
                self.run.reason = None;
                self.run.reason_text = None;
                let _ = self.labels(&[&l.pr], &[&l.running, &l.waiting]).await;
                if let Err(h) = self
                    .comment("the comment", &format!("Draft PR: {url}"))
                    .await
                {
                    return Err(self.hold(h).await);
                }
            }
            Some(f) => {
                self.run.state = RunState::Waiting;
                self.run.reason = Some("checks_failed".into());
                self.run.reason_text = None;
                let _ = self.labels(&[&l.waiting], &[&l.running]).await;
                let mut why = format!(
                    "The check `{}` still {} after {} {}.{}",
                    f.command,
                    f.outcome(),
                    self.run.check_rounds,
                    if self.run.check_rounds == 1 {
                        "retry"
                    } else {
                        "retries"
                    },
                    f.why(),
                );
                let sep = match &self.check_failure {
                    Some(note) => {
                        why.push_str(&format!("\n\nThe agent's view of the failure: {note}"));
                        "\n\n"
                    }
                    None => " ",
                };
                let text = format!(
                    "Draft PR: {url}\n\n{why}{sep}The session is waiting for you: `actuo attach {}`.",
                    self.session_label()
                );
                if let Err(h) = self.comment("the comment", &text).await {
                    return Err(self.hold(h).await);
                }
                self.run.reason_text = Some(why);
            }
        }
        // GitHub first, then the state: `pr_opened` means GitHub shows it.
        self.status().await;
        self.save().await;
        Ok(())
    }

    // Context for a run that already has a session

    /// The issue (for prompts and titles).
    async fn restore_issue(&mut self) -> Step {
        match self.lane.gh.issue(&self.run.repo, self.run.issue).await {
            Ok(i) => {
                self.issue = Some(i);
                Ok(())
            }
            Err(e) => Err(self.fail("gh_error", &e.to_string()).await),
        }
    }

    /// The issue, the default branch, the project and the worktree of a run that
    /// already got that far.
    async fn restore(&mut self) -> Step {
        self.restore_issue().await?;
        self.default_branch = self
            .base()
            .strip_prefix("origin/")
            .unwrap_or("main")
            .to_string();
        let clone = self.lane.host.paths().lane_clone(&self.run.repo);
        match self.lane.host.open_project(&clone.to_string_lossy()).await {
            Ok(p) => self.project = Some(p.id),
            Err(e) => return Err(self.fail("git_error", &e.message).await),
        }
        self.set_worktree().await
    }

    async fn set_worktree(&mut self) -> Step {
        let sid = self.run.session_id.clone().expect("a session");
        match self.lane.host.db(move |st| st.session(&sid)).await {
            Ok(s) => {
                self.worktree = PathBuf::from(s.cwd);
                Ok(())
            }
            Err(e) => Err(self.fail("agent_error", &e.message).await),
        }
    }

    /// Back to running (a resume): the running label, no comment.
    async fn set_running(&mut self) {
        self.run.state = RunState::Running;
        self.run.reason = None;
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.running], &[&l.waiting, &l.trigger]).await;
        self.status().await;
        self.save().await;
    }

    /// Someone may have pushed to the PR branch: fast-forward to it. Never a rebase;
    /// a branch that went another way pauses the run.
    async fn fast_forward(&mut self) -> Step {
        let (wt, branch) = (self.worktree.clone(), self.run.branch.clone());
        match blocking(move || lane_git::fast_forward(&wt, &branch)).await {
            Ok(lane_git::FastForward::Diverged) => Err(self
                .pause(
                    "cannot_fast_forward",
                    &format!(
                        "`{}` on GitHub has commits the run's worktree doesn't, and the worktree has commits it doesn't. Actuo doesn't rebase (no force push).",
                        self.run.branch
                    ),
                )
                .await),
            Ok(_) => Ok(()),
            Err(e) => Err(self.fail("git_error", &e.to_string()).await),
        }
    }

    /// Waits until no turn runs in the session (after a take-over).
    async fn wait_turn_end(&mut self) -> Step {
        let sid = self.run.session_id.clone().expect("a session");
        let mut watch = match Watch::from_head(&self.lane.host, &sid).await {
            Ok(w) => w,
            Err(e) => return Err(self.fail("agent_error", &e.message).await),
        };
        let s = sid.clone();
        let running = self
            .lane
            .host
            .db(move |st| st.running_turn(&s))
            .await
            .unwrap_or(None);
        let Some(n) = running else {
            return Ok(());
        };
        while let Some(e) = watch.next().await {
            if matches!(e.body, EventBody::TurnCompleted(_)) && e.turn == Some(n) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Whether the session's last turn was cut off (actuo or the agent stopped).
    async fn last_turn_cut_off(&self) -> bool {
        let Some(sid) = self.run.session_id.clone() else {
            return false;
        };
        let Ok(detail) = self.lane.host.db(move |st| st.session_detail(&sid)).await else {
            return true;
        };
        !matches!(
            detail.turns.last().and_then(|t| t.stop_reason),
            Some(StopReason::EndTurn)
        )
    }

    // Comments that came in after the run's last prompt

    fn pending(&self) -> Vec<Comment> {
        self.run
            .pending_comments
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// Fetches trusted comments newer than the last prompt (issue, PR conversation,
    /// PR review comments) into the run's pending comments.
    async fn collect_comments(&mut self) -> Step {
        let gh = self.lane.gh.clone();
        let mut all = match gh.comments(&self.run.repo, self.run.issue).await {
            Ok(c) => c,
            Err(e) => return Err(self.fail("gh_error", &e.to_string()).await),
        };
        if let Some(pr) = self.run.pr_number {
            let more = match gh.comments(&self.run.repo, pr).await {
                Ok(c) => c,
                Err(e) => return Err(self.fail("gh_error", &e.to_string()).await),
            };
            let review = match gh.review_comments(&self.run.repo, pr).await {
                Ok(c) => c,
                Err(e) => return Err(self.fail("gh_error", &e.to_string()).await),
            };
            all.extend(more);
            all.extend(review);
        }
        let since = self.run.last_prompt_at.clone().unwrap_or_default();
        let mut pending = self.pending();
        for c in all {
            let new = c.created_at > since
                && self.cfg.trusted.contains(&c.author)
                && !own_comment(&c.body)
                && !pending.iter().any(|p| p.id == c.id && p.path == c.path);
            if new {
                pending.push(c);
            }
        }
        pending.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        self.run.pending_comments = Some(serde_json::to_value(&pending).unwrap_or_default());
        self.save().await;
        Ok(())
    }

    /// Takes the pending comments for a prompt; the last prompt moves past them.
    fn take_pending(&mut self) -> Vec<Comment> {
        let pending = self.pending();
        if let Some(last) = pending.iter().map(|c| c.created_at.clone()).max() {
            self.run.last_prompt_at = Some(last);
        }
        self.run.pending_comments = None;
        pending
    }

    /// The comments since the last prompt as a turn of their own.
    async fn comments_turn(&mut self, cancelled_request: bool) -> Step<String> {
        self.collect_comments().await?;
        let comments = self.take_pending();
        Ok(prompt::new_comments(
            self.run.issue,
            &self.run.repo,
            &comments,
            cancelled_request,
            &self.marker,
            self.cfg.max_prompt_bytes as usize,
        ))
    }

    /// `text` with any pending comments before it.
    fn with_pending(&mut self, text: String) -> String {
        let comments = self.take_pending();
        prompt::with_comments(
            &text,
            &comments,
            &self.marker,
            self.cfg.max_prompt_bytes as usize,
        )
    }

    /// How the check `command` went at the commit this run started from (where its
    /// branch leaves the default branch), in a check run there: `None` when it
    /// passed there, or there's no check run with it to compare with. With none at
    /// that commit, or one from before the check was a check (the project's checks
    /// changed), starts one with today's checks and waits for it (a `baseline`
    /// step); with another one running for the repo, waits for that first. If the
    /// one it started doesn't have the check either (the change made it), `None`.
    async fn baseline(&mut self, command: &str) -> Step<Option<Baseline>> {
        let (wt, base) = (self.worktree.clone(), self.base());
        let Ok(commit) = blocking(move || lane_git::merge_base(&wt, &base)).await else {
            return Ok(None);
        };
        let repo = self.run.repo.clone();
        // The check run this lookup started: its id, and its open step.
        let mut ours: Option<String> = None;
        let mut step: Option<(u32, Instant, String)> = None;
        loop {
            let (r, c) = (repo.clone(), commit.clone());
            let found = self
                .lane
                .host
                .db(move |s| s.check_run_at(&r, &c))
                .await
                .ok()
                .flatten()
                // While ours runs, an older one doesn't count.
                .filter(|c| ours.as_ref().is_none_or(|id| &c.id.0 == id));
            if let Some(check) = found {
                if let Some((n, at, label)) = step.take() {
                    self.step_end(
                        n,
                        RunStepKind::Baseline,
                        Some(&label),
                        Some(0),
                        None,
                        None,
                        at,
                    )
                    .await;
                }
                let there = check
                    .steps
                    .iter()
                    .find(|s| s.kind == RunStepKind::Check && s.command == command);
                match there {
                    Some(s) if s.exit_code != Some(0) => {
                        return Ok(Some(Baseline {
                            commit,
                            check_run: check.id.0.clone(),
                            packages: s.failed_packages.clone().unwrap_or_default(),
                        }));
                    }
                    Some(_) => return Ok(None),
                    None if ours.is_some() => return Ok(None),
                    // From before the check was one: a new check run, below.
                    None => {}
                }
            }
            let r = repo.clone();
            let last = self
                .lane
                .host
                .db(move |s| s.last_check_run(&r))
                .await
                .ok()
                .flatten();
            let running = last
                .as_ref()
                .is_some_and(|l| l.state == actuo_protocol::runs::CheckRunState::Running);
            if !running {
                if ours.is_some() {
                    // Ours ended without a result (it couldn't run): no baseline.
                    if let Some((n, at, label)) = step.take() {
                        self.step_end(
                            n,
                            RunStepKind::Baseline,
                            Some(&label),
                            Some(1),
                            None,
                            None,
                            at,
                        )
                        .await;
                    }
                    return Ok(None);
                }
                match self.lane.check_commit(&repo, &commit).await {
                    Ok(check) => {
                        let label =
                            format!("check run {} at {}", check.id.0, short_commit(&commit));
                        let n = self.step_start(RunStepKind::Baseline, Some(&label)).await;
                        step = Some((n, Instant::now(), label));
                        ours = Some(check.id.0);
                    }
                    // Another one just started: wait for it.
                    Err(e)
                        if e.kind() == Some(ErrorKind::ProjectBusy) && !self.lane.host.held() => {}
                    Err(e) => {
                        crate::log!("run {}: baseline: {}", self.run.id, e.message);
                        return Ok(None);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.check_control().await?;
        }
    }

    /// Handles what the lane sent while the task was busy. A take-over or a cancel
    /// ends the task here; a stopping actuo ends it without touching the run.
    async fn check_control(&mut self) -> Step {
        if self.lane.stopping() {
            return Err(Halt);
        }
        while let Some(c) = self
            .held
            .pop_front()
            .or_else(|| self.control.try_recv().ok())
        {
            match c {
                Control::TakeOver => return Err(self.taken_over("you sent a prompt").await),
                Control::Cancel => return Err(self.cancelled().await),
                Control::PrEnded { merged } => return Err(self.pr_ended(merged).await),
                Control::Comments => self.collect_comments().await?,
            }
        }
        Ok(())
    }

    /// `run/cancel`: the run ends; the session and the branch stay.
    async fn cancelled(&mut self) -> Halt {
        self.caused_by_client();
        self.interrupt_turn().await;
        self.run.state = RunState::Cancelled;
        self.run.reason = Some("cancelled".into());
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self
            .labels(&[&l.failed], &[&l.running, &l.waiting, &l.trigger])
            .await;
        let text = format!(
            "The run was cancelled. The session stays: `actuo attach {}`.",
            self.session_label()
        );
        let _ = self.comment("the comment", &text).await;
        self.run.reason_text = Some("The run was cancelled.".into());
        self.status().await;
        self.save().await;
        Halt
    }

    /// The run's PR was merged or closed: the run ends (`merged` or `closed`), the
    /// lane's labels leave the issue, and what the run left on actuo's machine is
    /// removed. The run is saved first, so a failed cleanup leaves it ended.
    async fn pr_ended(&mut self, merged: bool) -> Halt {
        self.interrupt_turn().await;
        self.run.state = if merged {
            RunState::Merged
        } else {
            RunState::Closed
        };
        self.run.reason = None;
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self
            .labels(&[], &[&l.pr, &l.waiting, &l.running, &l.failed, &l.trigger])
            .await;
        self.status().await;
        self.save().await;
        self.clean_up().await;
        Halt
    }

    /// Removes what a run whose PR ended left on actuo, best effort (each
    /// failure is logged): the session (stopped first), its worktree, the local
    /// branch, the session's checkpoint refs, and the run's and session's temp dirs
    /// and sandbox settings. The run's records and logs, the clone and the repo's
    /// store stay.
    async fn clean_up(&mut self) {
        let tag = format!("lane: {}#{}: cleanup", self.run.repo, self.run.issue);
        let sid = self.run.session_id.clone();
        if let Some(sid) = &sid {
            if let Err(e) = self.lane.host.stop(sid).await {
                crate::log!("{tag}: stopping the session: {}", e.message);
            }
            // Its agent goes before its folder does.
            for _ in 0..50 {
                let s = sid.clone();
                match self.lane.host.db(move |st| st.session(&s)).await {
                    Ok(s) if s.state != SessionState::Ended => {
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                    _ => break,
                }
            }
        }
        let lock = self.lane.repo_lock(&self.run.repo);
        let _clone = lock.lock().await;
        let paths = self.lane.host.paths().clone();
        let clone = paths.lane_clone(&self.run.repo);
        let (branch, run) = (self.run.branch.clone(), self.run.id.0.clone());
        let session = sid.map(|s| s.0);
        let problems = blocking(move || {
            clean_up_files(&paths.home, &clone, &branch, &run, session.as_deref())
        })
        .await;
        for p in problems {
            crate::log!("{tag}: {p}");
        }
    }

    /// A turn still running (the lane's or yours) is interrupted, and this returns
    /// once the turn has ended (or after 10 s, if the agent doesn't stop).
    async fn interrupt_turn(&self) {
        if let Some(sid) = self.run.session_id.clone() {
            let s = sid.clone();
            let watch = Watch::from_head(&self.lane.host, &sid).await;
            if let (Ok(mut watch), Ok(Some(n))) = (
                watch,
                self.lane.host.db(move |st| st.running_turn(&s)).await,
            ) {
                let _ = self.lane.host.interrupt(&sid).await;
                let _ = tokio::time::timeout(Duration::from_secs(10), async {
                    while let Some(e) = watch.next().await {
                        if matches!(e.body, EventBody::TurnCompleted(_)) && e.turn == Some(n) {
                            return;
                        }
                    }
                })
                .await;
            }
        }
    }

    // Pauses, failures and status

    /// The agent answered a turn with an error (for example, a login it couldn't refresh).
    async fn turn_failed(&mut self, end: &TurnEnd) -> Halt {
        let mut message = format!(
            "The agent's turn failed with this error:\n\n{}",
            agent_error(end.error.as_deref())
        );
        if end.files_changed {
            message.push_str(&format!(
                "\n\nThe turn changed files before it failed. They're kept: handing back runs \
                 the turn again on top of them. To run it from before the turn instead, first \
                 restore the files with `actuo revert {} {}`.",
                self.session_label(),
                end.turn
            ));
        }
        message.push_str("\n\nHanding the run back sends the same turn again, once.");
        self.pause("turn_failed", &message).await
    }

    /// The agent stopped mid-turn again after its one "continue" turn. A turn
    /// actuo itself stopped (a restart, the session stopped) stays `restarted`.
    async fn stopped(&mut self, end: &TurnEnd) -> Halt {
        let what = match end.stop_reason {
            StopReason::AgentExited => "The agent's process exited before it finished its turn",
            StopReason::MaxTurnRequests => {
                "The agent hit its own limit on requests in one turn before it finished"
            }
            _ => {
                return self
                    .pause("restarted", "Actuo stopped the agent's turn, again.")
                    .await;
            }
        };
        let mut message = format!("{what}, again.");
        if end.stop_reason == StopReason::AgentExited {
            message.push_str(&format!(
                "\n\nWhat actuo saw:\n\n{}",
                agent_error(end.error.as_deref())
            ));
        }
        message.push_str("\n\nHanding the run back sends one \"continue\" turn.");
        self.pause("agent_stopped", &message).await
    }

    async fn pause(&mut self, reason: &str, message: &str) -> Halt {
        self.run.state = RunState::Waiting;
        self.run.reason = Some(reason.into());
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.waiting], &[&l.running]).await;
        let text = format!(
            "{message}\n\nThe run is waiting for you. {} From a terminal: `actuo attach {}`.",
            how_to_go_on(reason, &l.trigger, &self.run.id.0),
            self.session_label()
        );
        if let Err(h) = self.comment(field_for(reason), &text).await {
            return self.hold(h).await;
        }
        // Only once the comment passed the credential scan.
        self.run.reason_text = Some(message.into());
        self.status().await;
        self.save().await;
        Halt
    }

    /// Text that had a credential format in it wasn't published: the run waits
    /// (`secret_detected`), and the comment and notification say where, never what.
    async fn hold(&mut self, held: Held) -> Halt {
        self.hold_notice(held).await;
        Halt
    }

    async fn hold_notice(&mut self, held: Held) {
        self.run.state = RunState::Waiting;
        self.run.reason = Some("secret_detected".into());
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.waiting], &[&l.running]).await;
        // A value passed from actuo's environment has no format of its own: it's
        // that value.
        let what = if held.format == publish::PASSED {
            format!("it contains {}", held.format)
        } else {
            format!("it has text in the format of {}", held.format)
        };
        let text = format!(
            "Actuo held back {}: {what}. Nothing from this step was published.\n\nLook at it with `actuo attach {}`; fix it, then `actuo run resume {}`.",
            held.field,
            self.session_label(),
            self.run.id
        );
        // Replaces any earlier text, which may be what was held back.
        self.run.reason_text = Some(format!(
            "Actuo held back {}: {what}. Nothing from this step was published.",
            held.field
        ));
        if let Err(h) = self.comment("the comment", &text).await {
            crate::log!(
                "lane: {}#{}: the held-back notice was held back too ({})",
                self.run.repo,
                self.run.issue,
                h.format
            );
        }
        self.status().await;
        self.save().await;
        if cfg!(target_os = "macos") && !self.lane.notifiers().any() {
            super::mac::notify_held(&self.run.summary(), &held.field);
        }
    }

    async fn taken_over(&mut self, why: &str) -> Halt {
        self.caused_by_client();
        self.run.state = RunState::TakenOver;
        self.run.reason = Some(why.into());
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.waiting], &[&l.running]).await;
        self.status().await;
        self.save().await;
        Halt
    }

    async fn pause_for_request(&mut self, call: &ToolCall) {
        self.run.state = RunState::Waiting;
        self.run.reason = Some("request".into());
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.waiting], &[&l.running]).await;
        let ask = asks(call, &self.worktree);
        let text = format!(
            "{ask}No rule covers it, so the run waits for you: answer it with `actuo attach {}`, which shows the request and its options.",
            self.session_label()
        );
        if let Err(h) = self.comment("the agent's request", &text).await {
            // The request stays open; answering it still lets the run go on.
            self.hold_notice(h).await;
            return;
        }
        self.run.reason_text = Some(format!("{ask}No rule covers it, so the run waits for you."));
        self.status().await;
        self.save().await;
    }

    async fn resume_after_request(&mut self) {
        self.run.state = RunState::Running;
        self.run.reason = None;
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self.labels(&[&l.running], &[&l.waiting]).await;
        self.status().await;
        self.save().await;
    }

    async fn fail(&mut self, reason: &str, message: &str) -> Halt {
        self.run.state = RunState::Failed;
        self.run.reason = Some(reason.into());
        self.run.reason_text = None;
        let l = self.cfg.labels.clone();
        let _ = self
            .labels(&[&l.failed], &[&l.running, &l.waiting, &l.trigger])
            .await;
        let session = match &self.run.session_id {
            Some(s) => format!("\n\nSession: `{s}`."),
            None => String::new(),
        };
        let text = format!("The run failed ({reason}).\n\n{message}{session}");
        if let Err(h) = self.comment(field_for(reason), &text).await {
            return self.hold(h).await;
        }
        self.run.reason_text = Some(message.into());
        self.status().await;
        self.save().await;
        Halt
    }

    /// Never claimed: the issue isn't touched.
    async fn cancel(&mut self, reason: &str, message: &str) -> Halt {
        crate::log!(
            "lane: {}#{}: not claimed: {message}",
            self.run.repo,
            self.run.issue
        );
        self.run.state = RunState::Cancelled;
        self.run.reason = Some(reason.into());
        self.run.reason_text = None;
        self.save().await;
        Halt
    }

    async fn save(&mut self) {
        if self.lane.stopping() {
            // Left as it is for the next start to pick up.
            return;
        }
        let row = self.run.clone();
        let changed = (row.state, &row.reason) != (self.saved.0, &self.saved.1);
        let by = if changed {
            std::mem::replace(&mut self.by, ChangedBy::Lane)
        } else {
            ChangedBy::Lane
        };
        match self.lane.host.db(move |s| s.save_run(&row, &by)).await {
            Ok(r) => {
                self.saved = (r.state, r.reason.clone());
                self.run = r;
            }
            Err(e) => crate::log!("lane: run {}: saving: {}", self.run.id, e.message),
        }
        self.lane.notify(&self.run.summary());
    }

    async fn labels(&self, add: &[&str], remove: &[&str]) -> Result<(), GhError> {
        let r = self
            .lane
            .gh
            .edit_labels(&self.run.repo, self.run.issue, add, remove)
            .await;
        if let Err(e) = &r {
            crate::log!("lane: {}#{}: labels: {e}", self.run.repo, self.run.issue);
        }
        r
    }

    /// Posts a comment on the issue, unless `body` (`field`, for the pause comment
    /// when it's held back) has a credential format in it.
    async fn comment(&self, field: &str, body: &str) -> Result<(), Held> {
        let body = format!("{COMMENT_TAG}\n{}", outgoing(field, body)?);
        if let Err(e) = self
            .lane
            .gh
            .add_comment(&self.run.repo, self.run.issue, &body)
            .await
        {
            crate::log!("lane: {}#{}: comment: {e}", self.run.repo, self.run.issue);
        }
        Ok(())
    }

    /// Creates or edits the run's status comment; one with a credential format in it
    /// (only actuo text and settings go there) isn't posted.
    async fn status(&mut self) {
        let body = match outgoing("the status comment", &self.status_text()) {
            Ok(text) => format!("{COMMENT_TAG}\n{text}"),
            Err(h) => {
                crate::log!(
                    "lane: {}#{}: status comment held back ({})",
                    self.run.repo,
                    self.run.issue,
                    h.format
                );
                return;
            }
        };
        let gh = &self.lane.gh;
        let r = match self.run.status_comment_id {
            Some(id) => gh.edit_comment(&self.run.repo, id, &body).await.map(|_| id),
            None => gh.add_comment(&self.run.repo, self.run.issue, &body).await,
        };
        match r {
            Ok(id) if self.run.status_comment_id != Some(id) => {
                self.run.status_comment_id = Some(id);
                self.save().await;
            }
            Ok(_) => {}
            Err(e) => crate::log!(
                "lane: {}#{}: status comment: {e}",
                self.run.repo,
                self.run.issue
            ),
        }
    }

    fn status_text(&self) -> String {
        let state = serde_json::to_value(self.run.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut text = format!("**Actuo:** {state}");
        if let Some(r) = &self.run.reason {
            text.push_str(&format!(" ({r})"));
        }
        let level = serde_json::to_value(self.run.level)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        text.push_str(&format!(
            "\n\n- Agent: `{}`, level `{level}`, model `{}`",
            self.run.agent,
            self.model()
        ));
        if let Some(s) = &self.run.session_id {
            text.push_str(&format!("\n- Session: `{s}` (`actuo attach {s}`)"));
        }
        for r in &self.setup_results {
            text.push_str(&format!("\n- Setup `{}`: {}", r.command, r.outcome()));
        }
        for r in &self.check_results {
            text.push_str(&format!("\n- Check `{}`: {}", r.command, r.outcome_line()));
        }
        if let Some(url) = &self.run.pr_url {
            text.push_str(&format!("\n- PR: {url}"));
        }
        text.push_str(&format!("\n- Run: `{}`", self.run.id));
        text
    }

    async fn pr_body(&self, failing: Option<&CommandResult>) -> String {
        let mut body = String::new();
        if let Some(f) = failing {
            body.push_str(&format!(
                "**The check `{}` still {}.**\n\n{}\n\n",
                f.command,
                f.outcome(),
                publish::fenced(f.shown().0)
            ));
            if let Some(note) = &self.check_failure {
                body.push_str(&format!("The agent's view of the failure: {note}\n\n"));
            }
        }
        if !self.pr_description.is_empty() {
            body.push_str(&self.pr_description);
            body.push_str("\n\n");
        }
        body.push_str("---\n\n");
        body.push_str(&format!(
            "Opened by actuo. Agent `{}`, model `{}`, session `{}`.\n",
            self.run.agent,
            self.model(),
            self.session_label()
        ));
        for r in &self.setup_results {
            body.push_str(&format!("\n- Setup `{}`: {}", r.command, r.outcome()));
        }
        for r in &self.check_results {
            body.push_str(&format!("\n- Check `{}`: {}", r.command, r.outcome_line()));
        }
        if let Some(sid) = self.run.session_id.clone()
            && let Ok(detail) = self.lane.host.db(move |s| s.session_detail(&sid)).await
        {
            body.push_str(&format!(
                "\n- Turns: {}, tokens: {}{}",
                detail.turns.len(),
                token_counts(&detail.turns),
                cost_text(&detail.turns)
            ));
        }
        body.push_str(&format!("\n\nCloses #{}\n", self.run.issue));
        body
    }

    // Helpers

    fn base(&self) -> String {
        self.run.base.clone().unwrap_or_else(|| "HEAD".into())
    }

    /// Whether the branch already has commits (an earlier run's), so this run's
    /// commit is a follow-up.
    async fn follow_up(&self) -> bool {
        let (wt, base) = (self.worktree.clone(), self.base());
        blocking(move || lane_git::ahead(&wt, &base))
            .await
            .unwrap_or(0)
            > 0
    }

    fn title(&self) -> String {
        self.pr_title
            .clone()
            .or_else(|| self.run.title.clone())
            .unwrap_or_else(|| format!("Issue #{}", self.run.issue))
    }

    fn model(&self) -> String {
        match self.run.config.get("model") {
            Some(actuo_protocol::types::ConfigValue::String(m)) => m.clone(),
            _ => "the agent's default".into(),
        }
    }

    fn session_label(&self) -> String {
        self.run
            .session_id
            .as_ref()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| "-".into())
    }

    /// Runs a setup or check command in its sandbox and records it as a step. `input`: a file to write (in the
    /// run's temp dir, made fresh for the step) and its contents.
    async fn command(
        &mut self,
        kind: RunStepKind,
        command: &str,
        limit: Duration,
        input: Option<(&Path, &str)>,
    ) -> Step<CommandResult> {
        let started = Instant::now();
        let n = self.step_start(kind, Some(command)).await;
        let kind_name = serde_json::to_value(kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let log = self
            .lane
            .host
            .paths()
            .run_logs(&self.run.id)
            .join(format!("{n}-{kind_name}.log"));
        // Checks (and `commit_check`) compare against where the branch left the base.
        let scm_base = if kind == RunStepKind::Check {
            let (wt, base) = (self.worktree.clone(), self.base());
            match blocking(move || lane_git::merge_base(&wt, &base)).await {
                Ok(c) => Some(c),
                Err(e) => return Err(self.fail("git_error", &e.to_string()).await),
            }
        } else {
            None
        };
        let (settings, env) = match self.step_sandbox(kind, scm_base).await {
            Ok(x) => x,
            Err(e) => return Err(self.fail("sandbox_unavailable", &e).await),
        };
        if let Some((file, text)) = input
            && let Err(e) = std::fs::write(file, text)
        {
            return Err(self.fail("sandbox_unavailable", &e.to_string()).await);
        }
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let srt = crate::sandbox::srt(self.lane.host.paths());
        let node_options = match crate::sandbox::records_node_options(self.lane.host.paths()) {
            Ok(x) => x,
            Err(e) => return Err(self.fail("sandbox_unavailable", &e.to_string()).await),
        };
        // Cancel, take over and the PR ending stop the command; comments wait for
        // its end.
        let (control, held) = (&mut self.control, &mut self.held);
        let stop = async move {
            loop {
                let Some(c) = control.recv().await else {
                    return std::future::pending().await;
                };
                held.push_back(c);
                match c {
                    Control::Cancel => return "the run was cancelled".to_string(),
                    Control::TakeOver => return "you took the run over".to_string(),
                    Control::PrEnded { merged: true } => {
                        return "the pull request was merged".to_string();
                    }
                    Control::PrEnded { merged: false } => {
                        return "the pull request was closed".to_string();
                    }
                    Control::Comments => {}
                }
            }
        };
        let outcome = super::steps::run(
            command,
            &self.worktree,
            &env,
            Some(super::steps::Srt {
                program: &srt,
                settings: &settings,
                node_options: &node_options,
                refusal_log: self
                    .run
                    .session_id
                    .as_ref()
                    .map(|s| self.lane.host.paths().agent_log(s)),
                label: format!("run {} {kind_name} `{command}`", self.run.id),
                local_ports: if kind == RunStepKind::Setup {
                    &[]
                } else {
                    &self.cfg.checks_local_ports
                },
                local_listen_ports: if kind == RunStepKind::Setup {
                    &[]
                } else {
                    &self.cfg.checks_local_listen_ports
                },
            }),
            limit,
            &log,
            self.cfg.output_tail_lines as usize,
            self.cfg.output_tail_bytes as usize,
            stop,
        )
        .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => CommandOutcome {
                exit_code: None,
                timed_out: false,
                stopped: false,
                tail: format!("couldn't run it: {e}"),
                duration: started.elapsed(),
                peak_kb: None,
                peak_process_kb: None,
                refusals: Vec::new(),
            },
        };
        if outcome.stopped {
            let log = log.to_string_lossy().into_owned();
            self.step_end(
                n,
                kind,
                Some(command),
                None,
                Some(&outcome.tail),
                Some(&log),
                started,
            )
            .await;
            // The control that stopped it is held: the run ends or pauses there.
            self.check_control().await?;
        }
        // Why it failed, from srt's refusals and the whole output (no second try
        // in a run: the worktree has the agent's change).
        let output = if outcome.passed() {
            String::new()
        } else {
            tokio::fs::read_to_string(&log).await.unwrap_or_default()
        };
        let failed_packages = super::diagnose::turbo_failed(&output);
        let failure = if outcome.passed() {
            None
        } else {
            super::failure::failure_lines(&output, self.cfg.output_tail_bytes as usize)
        };
        let diagnosis = if outcome.passed() {
            None
        } else {
            let tmp = crate::sandbox::temp_path(self.lane.host.paths(), &self.run.id.0);
            super::diagnose::diagnose(&super::diagnose::Seen {
                repo: &self.run.repo,
                setup: kind == RunStepKind::Setup,
                output: &output,
                refusals: &outcome.refusals,
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                limit_s: limit.as_secs(),
                peak_kb: outcome.peak_kb,
                worktree: &self.worktree,
                tmp: &tmp,
            })
        };
        let log = log.to_string_lossy().into_owned();
        self.step_end(
            n,
            kind,
            Some(command),
            outcome.exit_code,
            Some(&outcome.tail),
            Some(&log),
            started,
        )
        .await;
        if let Some(d) = diagnosis.clone() {
            let id = self.run.id.clone();
            let _ = self
                .lane
                .host
                .db(move |s| s.set_step_diagnosis(&id, n, &d))
                .await;
        }
        Ok(CommandResult {
            command: command.to_string(),
            exit_code: outcome.exit_code,
            tail: outcome.tail,
            diagnosis,
            failed_packages,
            failure,
            baseline: None,
        })
    }

    /// The `srt` settings and environment for a setup or check command of this run
    /// (`steps::sandbox_for`).
    async fn step_sandbox(
        &self,
        kind: RunStepKind,
        scm_base: Option<String>,
    ) -> Result<(PathBuf, std::collections::BTreeMap<String, String>), String> {
        let paths = self.lane.host.paths().clone();
        let (id, wt, repo) = (
            self.run.id.clone(),
            self.worktree.clone(),
            self.run.repo.clone(),
        );
        let (network, read) = (self.resolved().network, self.cfg.read.clone());
        let setup = kind == RunStepKind::Setup;
        let unix_sockets = self.cfg.checks_unix_sockets;
        let local_ports = self.cfg.checks_local_ports.clone();
        let local_listen_ports = self.cfg.checks_local_listen_ports.clone();
        let npmrc = self.cfg.setup_reads_npmrc;
        blocking(move || {
            super::steps::sandbox_for(
                &paths,
                &id.0,
                &wt,
                &repo,
                &network,
                &read,
                setup,
                scm_base,
                unix_sockets,
                &local_ports,
                &local_listen_ports,
                npmrc,
            )
        })
        .await
    }

    async fn step_start(&self, kind: RunStepKind, command: Option<&str>) -> u32 {
        let (id, command) = (self.run.id.clone(), command.map(str::to_string));
        self.lane
            .host
            .db(move |s| s.add_step(&id, kind, command.as_deref()))
            .await
            .unwrap_or(0)
    }

    /// Ends a step: its row, and a `run.step` event in the session's log.
    #[allow(clippy::too_many_arguments)]
    async fn step_end(
        &self,
        n: u32,
        kind: RunStepKind,
        command: Option<&str>,
        exit_code: Option<i32>,
        tail: Option<&str>,
        log: Option<&str>,
        started: Instant,
    ) {
        let (id, t, l) = (
            self.run.id.clone(),
            tail.map(str::to_string),
            log.map(str::to_string),
        );
        let _ = self
            .lane
            .host
            .db(move |s| s.finish_step(&id, n, exit_code, t.as_deref(), l.as_deref()))
            .await;
        if let Some(sid) = &self.run.session_id {
            let event = crate::store::NewEvent::new(EventBody::RunStep(RunStepData {
                run_id: self.run.id.clone(),
                n,
                kind,
                command: command.map(str::to_string),
                exit_code,
                output_tail: tail.map(str::to_string),
                duration_ms: started.elapsed().as_millis() as u64,
            }));
            let _ = self.lane.host.append_event(sid, event).await;
        }
    }
}

/// A commit's first 7 characters, as GitHub shows it.
fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// What the agent asked for, as actuo posts it: a fixed phrase by the request's
/// kind, then the command, or the paths one per line, in a code block. The tool
/// call's title, agent text, isn't posted.
fn asks(call: &ToolCall, worktree: &Path) -> String {
    use actuo_protocol::types::ToolKind;
    let command = call.raw_input.as_ref().and_then(|i| match &i["command"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    });
    let url = call
        .raw_input
        .as_ref()
        .and_then(|i| i["url"].as_str().map(str::to_string));
    let mut paths: Vec<String> = call
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Diff { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    if paths.is_empty() {
        paths = call.locations.iter().map(|l| l.path.clone()).collect();
    }
    let paths = (!paths.is_empty()).then(|| paths.join("\n"));
    let (phrase, detail) = match call.tool_kind {
        ToolKind::Execute => ("The agent asks to run a command:".to_string(), command),
        ToolKind::Edit if !rules::inside_worktree(call, worktree) => (
            "The agent asks to edit files outside the worktree:".to_string(),
            paths,
        ),
        ToolKind::Edit => ("The agent asks to edit files:".to_string(), paths),
        ToolKind::Delete => ("The agent asks to delete files:".to_string(), paths),
        ToolKind::Move => ("The agent asks to move files:".to_string(), paths),
        ToolKind::Read => ("The agent asks to read files:".to_string(), paths),
        ToolKind::Fetch => (
            "The agent asks to fetch from the network:".to_string(),
            url.or(paths),
        ),
        kind => {
            let name = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            (
                format!("The agent asks to use a tool (`{name}`):"),
                command.or(paths),
            )
        }
    };
    match detail {
        Some(d) => format!("{phrase}\n\n{}\n", publish::fenced(&d)),
        None => format!("{phrase}\n\n"),
    }
}

/// Text actuo is about to post: neutralized, or held back when it has a
/// credential format in it.
fn outgoing(field: &str, text: &str) -> Result<String, Held> {
    match publish::scan(text) {
        Some(hit) => Err(Held {
            field: field.to_string(),
            format: hit.format,
        }),
        None => Ok(publish::neutralize(text)),
    }
}

/// What [`RunTask::clean_up`] removes on disk, under actuo's `home`: the
/// worktree on `branch` in the lane's `clone` (only one under `home/worktrees`),
/// the local branch, `session`'s checkpoint refs, `tmp/<run>`, `tmp/<session>` and
/// their sandbox settings files (only under `home/tmp` and `home/sandbox`).
/// Returns what couldn't be removed.
fn clean_up_files(
    home: &Path,
    clone: &Path,
    branch: &str,
    run: &str,
    session: Option<&str>,
) -> Vec<String> {
    use crate::git;
    let mut problems = Vec::new();
    let mut note = |what: &str, r: Result<(), String>| {
        if let Err(e) = r {
            problems.push(format!("{what}: {e}"));
        }
    };
    match git::worktree_for_branch(clone, branch) {
        Ok(Some(wt)) if under(&home.join("worktrees"), &wt) => note(
            "removing the worktree",
            git::worktree_remove(clone, &wt).map_err(|e| e.to_string()),
        ),
        Ok(Some(wt)) => note(
            "the worktree",
            Err(format!(
                "{} isn't under actuo's worktrees; left",
                wt.display()
            )),
        ),
        Ok(None) => {}
        Err(e) => note("finding the worktree", Err(e.to_string())),
    }
    note(
        "pruning worktrees",
        git::worktree_prune(clone).map_err(|e| e.to_string()),
    );
    if git::branch_exists(clone, branch).unwrap_or(false) {
        note(
            "deleting the local branch",
            git::branch_delete(clone, branch).map_err(|e| e.to_string()),
        );
    }
    let (tmp, sandbox) = (home.join("tmp"), home.join("sandbox"));
    let mut dirs = vec![tmp.join(run)];
    let mut files = vec![
        sandbox.join(format!("{run}-setup.json")),
        sandbox.join(format!("{run}-checks.json")),
    ];
    if let Some(s) = session {
        note(
            "deleting the checkpoint refs",
            git::delete_refs_under(clone, &format!("refs/actuo/checkpoints/{s}/"))
                .map_err(|e| e.to_string()),
        );
        dirs.push(tmp.join(s));
        files.push(sandbox.join(format!("{s}.json")));
    }
    for d in dirs.iter().filter(|d| under(&tmp, d)) {
        note(
            &format!("removing {}", d.display()),
            std::fs::remove_dir_all(d).map_err(|e| e.to_string()),
        );
    }
    for f in files.iter().filter(|f| under(&sandbox, f)) {
        note(
            &format!("removing {}", f.display()),
            std::fs::remove_file(f).map_err(|e| e.to_string()),
        );
    }
    problems
}

/// Whether `path` exists and resolves (`..` and symlinks followed) to somewhere
/// inside `root`, not `root` itself.
fn under(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(r), Ok(p)) => p != r && p.starts_with(&r),
        _ => false,
    }
}

/// A `Co-Authored-By:` line, in any case.
fn has_co_authored_by(message: &str) -> bool {
    message.lines().any(|l| {
        l.trim_start()
            .get(..15)
            .is_some_and(|p| p.eq_ignore_ascii_case("co-authored-by:"))
    })
}

/// One shell word: `text` in single quotes.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Where a pause or failure comment's text comes from, for a held-back notice.
fn field_for(reason: &str) -> &'static str {
    match reason {
        "no_changes" | "refusal" | "max_tokens" => "the agent's last message",
        "rejects" => "the rejected requests",
        "protected_path" => "the protected paths",
        "setup_failed" => "the setup's output",
        "commit_format" => "the commit message or the check's output",
        "git_error"
        | "gh_error"
        | "agent_error"
        | "sandbox_unavailable"
        | "turn_failed"
        | "agent_stopped" => "the error message",
        _ => "the comment",
    }
}

/// A cancelled or failed turn: the usage its agent reports leaves out the call that
/// was cut off, so it's a lower bound.
fn cut_short(t: &Turn) -> bool {
    matches!(
        t.stop_reason,
        Some(StopReason::Cancelled | StopReason::Failed)
    )
}

/// Input (counting what came from or went into the cache) and output tokens over a
/// session's turns, with how many turns are only partly counted (`cut_short`) or
/// not at all (the agent reported nothing).
fn token_counts(turns: &[Turn]) -> String {
    let (mut input, mut cached, mut output) = (0u64, 0u64, 0u64);
    let (mut partial, mut unknown) = (0usize, 0usize);
    for t in turns {
        let Some(u) = &t.tokens else {
            unknown += 1;
            continue;
        };
        partial += usize::from(cut_short(t));
        let c = u.cached_read.unwrap_or(0) + u.cached_write.unwrap_or(0);
        input += u.input.unwrap_or(0) + c;
        cached += c;
        output += u.output.unwrap_or(0);
    }
    if unknown == turns.len() {
        return "unknown".into();
    }
    let mut text = if cached > 0 {
        format!("{input} in ({cached} cached), {output} out")
    } else {
        format!("{input} in, {output} out")
    };
    let turns_word = |n: usize| if n == 1 { "turn" } else { "turns" };
    let mut notes = Vec::new();
    if partial > 0 {
        notes.push(format!("{partial} {} partial", turns_word(partial)));
    }
    if unknown > 0 {
        notes.push(if partial > 0 {
            format!("{unknown} unknown")
        } else {
            format!("{unknown} {} unknown", turns_word(unknown))
        });
    }
    if !notes.is_empty() {
        text.push_str(&format!(" ({})", notes.join(", ")));
    }
    text
}

/// ", cost $0.123" over the turns that have one; "at least" when any turn's cost
/// is missing or cut short. Nothing when no turn has a cost (Codex reports none).
fn cost_text(turns: &[Turn]) -> String {
    let known: Vec<f64> = turns
        .iter()
        .filter_map(|t| t.cost.as_ref().map(|c| c.amount))
        .collect();
    if known.is_empty() {
        return String::new();
    }
    let total: f64 = known.iter().sum();
    let lower = turns.iter().any(|t| t.cost.is_none() || cut_short(t));
    format!(", cost {}${total:.3}", if lower { "at least " } else { "" })
}

/// Longest agent error a comment shows (agent text is cut, fenced and scanned;
/// `outgoing` scans the whole comment).
const AGENT_ERROR_BYTES: usize = 4000;

/// An error the agent or its process gave, as a comment shows it: cut, in a code
/// block.
fn agent_error(error: Option<&str>) -> String {
    match error.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => publish::fenced(&prompt::cap(e, AGENT_ERROR_BYTES)),
        None => "(no error message)".into(),
    }
}

fn quote(text: &str) -> String {
    if text.trim().is_empty() {
        return "> (no message)".into();
    }
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Lockfiles, by file name: generated, so left out of the change-size limits.
fn is_lockfile(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "pnpm-lock.yaml"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
    )
}

/// What a waiting run's comment says to do, on GitHub or with actuo's CLI.
fn how_to_go_on(reason: &str, label: &str, run: &str) -> String {
    let reply = format!(
        "reply here and add the `{label}` label again: the agent gets your reply as its next turn"
    );
    let hand_back = format!("hand the run back with `actuo run resume {run}`");
    match reason {
        "protected_path" | "diff_too_large" => format!(
            "To go on past this for this run: {hand_back}. To have the agent change its work instead: {reply}."
        ),
        "commit_format" => format!("To give the agent the refusal again: {hand_back}. Or {reply}."),
        _ => format!("To go on: {reply}. Or {hand_back}, to go on without new instructions."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use actuo_protocol::types::TokenUsage;

    fn usage(input: u64, read: Option<u64>, write: Option<u64>, output: u64) -> TokenUsage {
        TokenUsage {
            input: Some(input),
            output: Some(output),
            cached_read: read,
            cached_write: write,
            thought: None,
            total: None,
        }
    }

    fn turn(stop: StopReason, tokens: Option<TokenUsage>, cost: Option<f64>) -> Turn {
        Turn {
            n: 1,
            text: String::new(),
            started_at: String::new(),
            completed_at: None,
            stop_reason: Some(stop),
            tokens,
            cost: cost.map(|amount| actuo_protocol::types::Cost {
                amount,
                currency: "USD".into(),
            }),
        }
    }

    #[test]
    fn token_counts_include_the_cache() {
        // "14 in" would be only the uncached input.
        let a = turn(
            StopReason::EndTurn,
            Some(usage(14, Some(40_000), Some(2_000), 1381)),
            None,
        );
        let b = turn(StopReason::EndTurn, Some(usage(6, None, None, 20)), None);
        assert_eq!(
            token_counts(&[a, b.clone()]),
            "42020 in (42000 cached), 1401 out"
        );
        assert_eq!(token_counts(&[b]), "6 in, 20 out");
    }

    /// A cancelled or failed turn's numbers are a lower bound; a turn without
    /// numbers isn't counted; finished turns are as they were.
    #[test]
    fn partial_and_unknown_turns_are_said() {
        let done = turn(
            StopReason::EndTurn,
            Some(usage(10, None, None, 5)),
            Some(0.1),
        );
        let cut = turn(
            StopReason::Cancelled,
            Some(usage(3, None, None, 1)),
            Some(0.02),
        );
        let none = turn(StopReason::Cancelled, None, None);
        let failed = turn(StopReason::Failed, None, None);
        assert_eq!(
            token_counts(&[done.clone(), cut.clone(), none.clone(), failed.clone()]),
            "13 in, 6 out (1 turn partial, 2 unknown)"
        );
        assert_eq!(
            token_counts(&[done.clone(), none.clone(), failed.clone()]),
            "10 in, 5 out (2 turns unknown)"
        );
        assert_eq!(
            token_counts(&[done.clone(), cut.clone(), cut.clone()]),
            "16 in, 7 out (2 turns partial)"
        );
        assert_eq!(token_counts(&[none.clone(), failed]), "unknown");
        assert_eq!(cost_text(std::slice::from_ref(&done)), ", cost $0.100");
        assert_eq!(cost_text(&[done.clone(), cut]), ", cost at least $0.120");
        assert_eq!(cost_text(&[done, none.clone()]), ", cost at least $0.100");
        assert_eq!(cost_text(&[none]), "");
    }

    #[test]
    fn agent_errors_are_cut_and_fenced() {
        assert_eq!(agent_error(Some(" boom \n")), "```\nboom\n```");
        assert_eq!(agent_error(Some("a ``` b")), "````\na ``` b\n````");
        assert_eq!(agent_error(None), "(no error message)");
        assert_eq!(agent_error(Some("  ")), "(no error message)");
        let long = agent_error(Some(&"é".repeat(3000)));
        assert!(long.contains("[cut at 4000 bytes]"), "{long}");
        assert!(long.len() < 4100);
    }

    #[test]
    fn quotes() {
        assert_eq!(quote("a\nb"), "> a\n> b");
        assert_eq!(quote(" "), "> (no message)");
    }

    fn call(
        kind: actuo_protocol::types::ToolKind,
        input: serde_json::Value,
        paths: &[&str],
    ) -> ToolCall {
        ToolCall {
            tool_kind: kind,
            title: "Run ![x](https://e.test/?d=1)\nand **this**".into(),
            status: actuo_protocol::types::ToolStatus::Pending,
            raw_input: Some(input),
            raw_output: None,
            content: Vec::new(),
            locations: paths
                .iter()
                .map(|p| actuo_protocol::types::Location {
                    path: p.to_string(),
                    line: None,
                })
                .collect(),
        }
    }

    /// The pause comment names what the agent asked for by kind, with agent text
    /// only in a code block; the tool call's title isn't posted.
    #[test]
    fn asks_by_kind() {
        use actuo_protocol::types::ToolKind;
        use serde_json::json;
        let wt = Path::new("/w/t");
        let cases = [
            (
                call(ToolKind::Execute, json!({"command": "pnpm test"}), &[]),
                "The agent asks to run a command:\n\n```\npnpm test\n```\n",
            ),
            (
                call(ToolKind::Edit, json!({}), &["/tmp/notes.txt", "/w/t/a.ts"]),
                "The agent asks to edit files outside the worktree:\n\n```\n/tmp/notes.txt\n/w/t/a.ts\n```\n",
            ),
            (
                call(ToolKind::Edit, json!({}), &["/w/t/a.ts"]),
                "The agent asks to edit files:\n\n```\n/w/t/a.ts\n```\n",
            ),
            (
                call(ToolKind::Read, json!({}), &["/etc/hosts"]),
                "The agent asks to read files:\n\n```\n/etc/hosts\n```\n",
            ),
            (
                call(ToolKind::Fetch, json!({"url": "https://e.test/x"}), &[]),
                "The agent asks to fetch from the network:\n\n```\nhttps://e.test/x\n```\n",
            ),
            (
                call(ToolKind::Other, json!({}), &[]),
                "The agent asks to use a tool (`other`):\n\n",
            ),
        ];
        for (c, want) in cases {
            let got = asks(&c, wt);
            assert_eq!(got, want);
            assert!(!got.contains("![x]") && !got.contains("**this**"), "{got}");
        }
        // A path with backticks stays inside its block.
        let got = asks(&call(ToolKind::Delete, json!({}), &["/w/t/a```b"]), wt);
        assert_eq!(
            got,
            "The agent asks to delete files:\n\n````\n/w/t/a```b\n````\n"
        );
    }

    #[test]
    fn held_text_names_the_field() {
        let token = format!("ghp_{}", "aZ3kQ9xR2mT7vB4nL8pW6cY1aZ3kQ9xR2mT7");
        let h = outgoing("the PR description", &format!("Fixes it.\n\n{token}")).unwrap_err();
        assert_eq!(h.field, "the PR description");
        assert_eq!(h.format, "a GitHub token");
        assert_eq!(
            outgoing("x", "Thanks @someone").unwrap(),
            "Thanks `@someone`"
        );
        assert_eq!(field_for("no_changes"), "the agent's last message");
        assert_eq!(field_for("setup_failed"), "the setup's output");
        assert_eq!(
            field_for("commit_format"),
            "the commit message or the check's output"
        );
    }

    #[test]
    fn cleanup_stays_under_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let (root, out) = (dir.path().join("tmp"), dir.path().join("elsewhere"));
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&out, root.join("link")).unwrap();
        assert!(under(&root, &root.join("run")));
        assert!(!under(&root, &root), "the root itself");
        assert!(!under(&root, &root.join("link")), "a symlink out");
        assert!(!under(&root, &root.join("run/../../elsewhere")), "..");
        assert!(!under(&root, &root.join("missing")), "missing");
    }

    #[test]
    fn co_authored_by_lines() {
        assert!(has_co_authored_by("x\n\nCo-Authored-By: a <a@a>\n"));
        assert!(has_co_authored_by("co-authored-by: a"));
        assert!(has_co_authored_by("x\n  CO-AUTHORED-BY:a"));
        assert!(!has_co_authored_by("Mention Co-Authored-By: in the docs"));
        assert!(!has_co_authored_by("Co-Authored: a"));
    }

    #[test]
    fn shell_words() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}

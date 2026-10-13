//! Runs of the issue-to-PR lane.

use crate::ids::{CheckRunId, RunId, SessionId};
use crate::types::Level;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a run is. `reason` says why it waits, failed or was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Picked up, waiting for a free slot (`lane.max_runs`).
    Queued,
    /// The agent works, or actuo prepares, commits or pushes.
    Running,
    /// The repo's checks run.
    Checking,
    /// Paused for you: an open request, a guard, a time limit, failing checks.
    Waiting,
    /// You sent a prompt yourself; the lane stays out until handed back.
    TakenOver,
    /// Ended with a draft PR whose checks pass; the PR is open.
    PrOpened,
    /// The run's PR was merged.
    Merged,
    /// The run's PR was closed without being merged.
    Closed,
    Failed,
    Cancelled,
}

impl RunState {
    /// The run is over; a new label starts a new run.
    pub fn ended(self) -> bool {
        matches!(
            self,
            Self::PrOpened | Self::Merged | Self::Closed | Self::Failed | Self::Cancelled
        )
    }
}

/// How a run was started. Set by actuo from the way the run came in, never
/// from what a client says; the label check depends on it, not on `started_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartedVia {
    /// The trigger label on GitHub, added by a trusted user.
    Label,
    /// `run/start` from a client.
    Client,
}

/// What caused a change of a run's state, set by actuo like `StartedVia`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeVia {
    /// The lane itself.
    Lane,
    /// The trigger label (the first change of a labeled run).
    Label,
    /// A client: `run/start`, a cancel, a hand-back or a take-over.
    Client,
}

/// The step a run is at (written before the step starts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStep {
    Prepare,
    Setup,
    Turn,
    Checks,
    Review,
    Guards,
    Push,
    Pr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: RunId,
    /// `owner/repo`.
    pub repo: String,
    pub issue: u64,
    pub issue_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub agent: String,
    pub level: Level,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why the run waits, failed or was cancelled, in the words of the issue's
    /// comment (without what to do next). It may quote the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_text: Option<String>,
    pub step: RunStep,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub branch: String,
    /// What the branch started from, e.g. `origin/main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// Who started the run, by `started_via`: the GitHub login that added the
    /// label, or the client's `client.name` (self-reported; for display only).
    pub started_by: String,
    /// How the run was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_via: Option<StartedVia>,
    /// Check rounds that failed and were sent back to the agent.
    pub check_rounds: u32,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

/// A run with its steps and state changes (`run/get`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunDetail {
    #[serde(flatten)]
    pub run: Run,
    pub steps: Vec<RunStepRecord>,
    /// Every change of the run's state or reason, oldest first.
    #[serde(default)]
    pub states: Vec<RunStateChange>,
}

/// One change of a run's state or reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStateChange {
    /// From 1 within the run.
    pub n: u32,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Who caused it, for display: `lane`; the `client.name` of the client that
    /// cancelled, handed back or took over the run (self-reported); for the first
    /// change, who started the run (`started_by`). Absent when a client caused it
    /// but isn't known (a turn interrupted from some client).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// What kind of cause `by` names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<ChangeVia>,
    pub at: String,
}

/// One command or action of a run, with its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStepRecord {
    /// From 1 within the run.
    pub n: u32,
    pub kind: RunStepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The end of the output, as sent to the agent or a comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    /// Why it failed, when actuo can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<Diagnosis>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStepKind {
    /// Cloning or fetching the lane's clone (before the session exists).
    Fetch,
    Setup,
    Check,
    Commit,
    Push,
    Pr,
    /// Waiting for a check run at the run's starting commit, to tell a failing
    /// check's failures already on the default branch from the change's. Its
    /// command names the check run and the commit.
    Baseline,
}

/// A check run (`project/check`): a project's setup, then each check, once, in the
/// sandbox, on a fresh worktree of its default branch, with no agent; measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    pub id: CheckRunId,
    /// `owner/repo`.
    pub repo: String,
    pub state: CheckRunState,
    /// The commit checked (the default branch's head, or a lane run's starting
    /// commit for its baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Why it couldn't run (`error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<CheckStep>,
    /// The worktree's `node_modules` after setup, in KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_modules_kb: Option<u64>,
    /// The project's package store before and after setup, in KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_kb_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_kb_after: Option<u64>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunState {
    Running,
    /// Setup and every check passed.
    Passed,
    /// A step failed or timed out (after a failed setup, the checks don't run).
    Failed,
    /// It couldn't run: the fetch, the worktree or the sandbox failed (`error`).
    Error,
}

/// One step of a check run, as it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckStep {
    /// From 1.
    pub n: u32,
    /// `setup` or `check`.
    pub kind: RunStepKind,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Stopped at its time limit (a check gets 3× `check_time_limit`).
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    pub duration_ms: u64,
    /// The largest total resident memory of its processes, sampled each second, in
    /// KB; absent when the step ended before the first sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_kb: Option<u64>,
    /// The largest single process's, in KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_process_kb: Option<u64>,
    /// Why it failed, when actuo can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<Diagnosis>,
    /// The packages turbo's summary lists as failed (`@a/x`), from the whole output;
    /// absent when it lists none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_packages: Option<Vec<String>>,
}

/// Why a setup or check step failed, as far as actuo can tell, and what you
/// can do about it. Nothing is changed by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Diagnosis {
    pub kind: DiagnosisKind,
    /// What was seen, in plain words (the hosts, the path, the packages).
    pub detail: String,
    /// What you can do, one sentence each.
    pub fixes: Vec<String>,
    /// The hosts setup was refused, which `project allow-host` can add. Absent
    /// for other failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisKind {
    /// The sandbox refused a Unix socket.
    UnixSocket,
    /// A host not in the project's `network`.
    Network,
    /// A write outside the worktree and the temp dir.
    File,
    /// A local database or server the checks expect isn't there.
    Service,
    /// The step's time limit.
    Timeout,
    /// Killed for memory.
    Memory,
}

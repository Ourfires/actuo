//! Actuo's settings: `~/.actuo/settings.toml`, read and changed through
//! `settings/get` and `settings/update`.
//!
//! Field names are the file's keys (snake_case), not camelCase like the rest of the
//! protocol: `settings/update` patches the file by these names, so the JSON and the
//! TOML say the same thing. Every field has a default; `settings/get` fills them in.
//! Durations are strings with a unit: `30s`, `15m`, `2h`.

use crate::types::{ConfigValue, Level, PermissionOptionKind, ToolKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Config values by the agent's config option id, per agent name.
pub type AgentDefaults = BTreeMap<String, BTreeMap<String, ConfigValue>>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub sessions: SessionsSettings,
    /// Model, effort and other config values per agent, for every session.
    /// `session/start`'s `config` goes over them.
    #[serde(default)]
    pub defaults: AgentDefaults,
    /// The lane: actuo-wide, and the defaults of every project's runs.
    #[serde(default)]
    pub lane: LaneSettings,
    /// Projects by `owner/repo`: a GitHub repo on this actuo, its lane on or off,
    /// its own settings over the lane's.
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectSettings>,
}

/// Actuo-wide session settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsSettings {
    /// Actuo's default agent: for `session/start` without one, and for the lane
    /// runs of a repo without its own `agent`.
    #[serde(default = "claude")]
    pub agent: String,
    /// An idle agent is stopped after this long (the session goes `detached`);
    /// `off` keeps it.
    #[serde(default = "fifteen_minutes")]
    pub idle_agent_timeout: String,
}

impl Default for SessionsSettings {
    fn default() -> Self {
        Self {
            agent: claude(),
            idle_agent_timeout: fifteen_minutes(),
        }
    }
}

/// The issue-to-PR lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaneSettings {
    /// Poll the watched repos (and accept `run/start`). Lane runs are always
    /// sandboxed.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// What the agent may do in lane runs without asking: `ask` or `edits` (edits
    /// in its worktree).
    #[serde(default = "edits")]
    pub level: Level,
    /// How often the lane looks for labeled issues.
    #[serde(default = "sixty_seconds")]
    pub poll_interval: String,
    /// Runs at once; paused runs don't count.
    #[serde(default = "two")]
    pub max_runs: u32,
    /// The agents' own telemetry switches.
    #[serde(default)]
    pub telemetry: bool,
    /// Who may start a run by label, besides the account gh is logged in as.
    #[serde(default)]
    pub trusted_users: Vec<String>,
    /// The agents' web search and fetch tools.
    #[serde(default)]
    pub web_tools: bool,
    /// Sign actuo's commits; `false` passes `--no-gpg-sign`, `true` follows
    /// the git config.
    #[serde(default)]
    pub sign_commits: bool,
    /// A turn running longer is stopped and the run waits for you
    /// (`turn_time_limit`).
    #[serde(default = "thirty_minutes")]
    pub turn_time_limit: String,
    /// The agent's time across a run's turns; past it the turn is stopped and the
    /// run waits for you (`run_time_limit`).
    #[serde(default = "two_hours")]
    pub run_time_limit: String,
    /// Times failing checks are sent back to the agent, per run.
    #[serde(default = "two")]
    pub max_check_retries: u32,
    /// A change touching more files (lockfiles not counted) pauses the run before
    /// its commit; 0: no limit.
    #[serde(default = "fifty")]
    pub max_changed_files: u32,
    /// A change with more added and removed lines (lockfiles not counted) pauses
    /// the run before its commit; 0: no limit.
    #[serde(default = "fifteen_hundred")]
    pub max_changed_lines: u32,
    /// Follow-up turns after a rejected request, per run.
    #[serde(default = "two")]
    pub max_reject_turns: u32,
    /// "Actuo restarted" turns, per run.
    #[serde(default = "one")]
    pub max_restart_turns: u32,
    /// Lines of a failing command's output sent back to the agent.
    #[serde(default = "two_hundred")]
    pub output_tail_lines: u32,
    /// The same output's cap in bytes.
    #[serde(default = "twenty_kib")]
    pub output_tail_bytes: u32,
    /// Cap on the issue text in a prompt.
    #[serde(default = "thirty_two_kib")]
    pub max_prompt_bytes: u32,
    /// Paths a run may not change without you; a trailing `/` means a folder.
    #[serde(default = "protected_paths")]
    pub protected_paths: Vec<String>,
    #[serde(default)]
    pub labels: LaneLabels,
    /// Approval rules per agent; an agent without an entry gets the built-in rules.
    #[serde(default)]
    pub rules: BTreeMap<String, Vec<Rule>>,
}

impl Default for LaneSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            level: edits(),
            poll_interval: sixty_seconds(),
            max_runs: two(),
            telemetry: false,
            trusted_users: Vec::new(),
            web_tools: false,
            sign_commits: false,
            turn_time_limit: thirty_minutes(),
            run_time_limit: two_hours(),
            max_check_retries: two(),
            max_changed_files: fifty(),
            max_changed_lines: fifteen_hundred(),
            max_reject_turns: two(),
            max_restart_turns: one(),
            output_tail_lines: two_hundred(),
            output_tail_bytes: twenty_kib(),
            max_prompt_bytes: thirty_two_kib(),
            protected_paths: protected_paths(),
            labels: LaneLabels::default(),
            rules: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaneLabels {
    /// The issue label that starts a run.
    #[serde(default = "label_trigger")]
    pub trigger: String,
    /// On the issue while its run works.
    #[serde(default = "label_running")]
    pub running: String,
    /// While its run waits for you.
    #[serde(default = "label_waiting")]
    pub waiting: String,
    /// Once its run's PR is open.
    #[serde(default = "label_pr")]
    pub pr: String,
    /// When its run failed.
    #[serde(default = "label_failed")]
    pub failed: String,
    /// A label `<prefix><agent>` picks the run's agent.
    #[serde(default = "label_agent_prefix")]
    pub agent_prefix: String,
}

impl Default for LaneLabels {
    fn default() -> Self {
        Self {
            trigger: label_trigger(),
            running: label_running(),
            waiting: label_waiting(),
            pr: label_pr(),
            failed: label_failed(),
            agent_prefix: label_agent_prefix(),
        }
    }
}

/// An approval rule. The first rule of an agent's list that matches answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub kind: ToolKind,
    /// `true`: only when every location is inside the worktree (and there is one);
    /// `false`: only when one is outside. Absent: locations aren't looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inside_worktree: Option<bool>,
    /// Option kinds in order of preference; the first one the request offers is chosen.
    pub answer: Vec<PermissionOptionKind>,
}

/// A project: a GitHub repo on this actuo. The optional fields override the
/// lane's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectSettings {
    /// Its lane runs on or off (off by default: a project is set up first, its lane
    /// turned on after). Off, no new run starts for it; its clone stays, for
    /// sessions started by hand. `lane.enabled` off pauses every project.
    #[serde(default)]
    pub lane: bool,
    /// The issue label that starts a run here, over `lane.labels.trigger`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Over `lane.level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Who may start a run here by label, over `lane.trusted_users` (the account gh
    /// is logged in as always may).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_users: Option<Vec<String>>,
    /// Approval rules per agent, each over the lane's for that agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rules: BTreeMap<String, Vec<Rule>>,
    /// Over `lane.sign_commits`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_commits: Option<bool>,
    /// The agent for this project's lane runs (when no `agent:<name>` label picks
    /// one); absent: actuo's default (`sessions.agent`). Must be in its agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Config values (model, effort, …) for this project's agent in its lane runs, over
    /// actuo's `[defaults]` for it. Not `mode`: the level sets it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, ConfigValue>,
    /// Commands run in the worktree before the agent's first turn. Absent: detected
    /// from the run's worktree (its lockfile: the install), at each run; `[]`: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Vec<String>>,
    /// A setup command running longer is stopped.
    #[serde(default = "ten_minutes")]
    pub setup_time_limit: String,
    /// Commands run after each turn; all must pass. Absent: detected from the
    /// worktree after each turn (its `lint`, `typecheck` and `test` scripts); `[]`:
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
    /// A check running longer is stopped (in a project's check run, at three
    /// times this).
    #[serde(default = "ten_minutes")]
    pub check_time_limit: String,
    /// Hosts setup may reach; checks stay offline. Absent: the package registry
    /// and codeload.github.com for a detected package manager (npm only for an
    /// empty repo). `project add` saves the hosts reviewed from the lockfile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
    /// Also let the agent's session reach all of `network`. It always reaches the
    /// package registries in it (npm's, yarn's), to install and scaffold.
    #[serde(default)]
    pub agent_network: bool,
    /// Let this project's setup and checks use Unix sockets (some tools need one,
    /// e.g. `tsx`). Less isolation: a check could also reach other sockets on
    /// actuo's machine.
    #[serde(default)]
    pub checks_unix_sockets: bool,
    /// Let this project's setup read `~/.npmrc`, for a private registry's token.
    /// Its install scripts can then read the file too: keep only a read-only token
    /// there. Off: setup can't read it, like the checks and the agent.
    #[serde(default)]
    pub setup_reads_npmrc: bool,
    /// macOS only: loopback TCP ports checks may connect to (a test database or
    /// server). Not for listening, other hosts, setup or the agent. Refused on
    /// other platforms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks_local_ports: Vec<u16>,
    /// macOS only: TCP ports checks may listen on and connect to over loopback.
    /// macOS's sandbox can't keep them to loopback: a check listening on one can
    /// be reached from the network. Only for trusted tests; not for setup or the
    /// agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks_local_listen_ports: Vec<u16>,
    /// Paths under your home folder setup, checks and the agent need to read (the
    /// repo's toolchain), e.g. `~/.bun`. Credential paths can't be listed.
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_time_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_time_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_check_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changed_files: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changed_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reject_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restart_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_paths: Option<Vec<String>>,
    /// A command that checks each commit message actuo writes: the message is
    /// in a file whose path is appended. Runs in the worktree, sandboxed like the
    /// checks. Any exit but 0 refuses the message: the agent gets one turn with the
    /// refusal; a second refusal pauses the run (`commit_format`) without committing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_subject: Option<CommitSubject>,
    /// Whether a `Co-Authored-By` line may be in actuo's commit messages. With
    /// `false`, one in the agent's title or `Commit:` line is refused like a
    /// `commit_check` failure: one turn to correct it, then the run pauses
    /// (`commit_format`).
    #[serde(default = "yes")]
    pub co_authored_by: bool,
}

impl Default for ProjectSettings {
    /// A project with every default: its lane off, the lane's settings throughout.
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).expect("every field has a default")
    }
}

/// How the agent writes the PR title and `Commit:` subjects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitSubject {
    /// A sentence the review prompt gives the agent, e.g. the repo's title format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

fn yes() -> bool {
    true
}
fn claude() -> String {
    "claude".into()
}
fn edits() -> Level {
    Level::Edits
}
fn fifteen_minutes() -> String {
    "15m".into()
}
fn sixty_seconds() -> String {
    "60s".into()
}
fn thirty_minutes() -> String {
    "30m".into()
}
fn two_hours() -> String {
    "2h".into()
}
fn ten_minutes() -> String {
    "10m".into()
}
fn one() -> u32 {
    1
}
fn two() -> u32 {
    2
}
fn fifty() -> u32 {
    50
}
fn two_hundred() -> u32 {
    200
}
fn fifteen_hundred() -> u32 {
    1500
}
fn twenty_kib() -> u32 {
    20 * 1024
}
fn thirty_two_kib() -> u32 {
    32 * 1024
}
fn protected_paths() -> Vec<String> {
    [
        ".github/",
        ".claude/",
        ".codex/",
        ".mcp.json",
        "AGENTS.md",
        "CLAUDE.md",
    ]
    .map(String::from)
    .to_vec()
}
fn label_trigger() -> String {
    "actuo".into()
}
fn label_running() -> String {
    "actuo:running".into()
}
fn label_waiting() -> String {
    "actuo:waiting".into()
}
fn label_pr() -> String {
    "actuo:pr".into()
}
fn label_failed() -> String {
    "actuo:failed".into()
}
fn label_agent_prefix() -> String {
    "agent:".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        let s: Settings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.lane.labels.trigger, "actuo");
        assert_eq!(s.lane.level, Level::Edits);
        assert!(s.lane.enabled);
        assert!(s.projects.is_empty());
    }

    #[test]
    fn unknown_keys_are_refused() {
        let r: Result<Settings, _> =
            serde_json::from_value(serde_json::json!({"lane": {"max_run": 3}}));
        assert!(r.unwrap_err().to_string().contains("max_run"));
    }
}

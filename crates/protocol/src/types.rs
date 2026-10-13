//! Types shared by methods and events. Timestamps are RFC 3339 strings in UTC.

use crate::ids::{PermissionOptionId, ProjectId, RequestId, SessionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Who is connecting (sent in `initialize`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Actuo's name and version (returned by `initialize`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActuoInfo {
    pub name: String,
    pub version: String,
    /// The last commit of actuo's sources when it was built, e.g. `1a2b3c4`, with
    /// `-dirty` if they had uncommitted changes. Absent when built outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The running binary's path, e.g. `/home/me/.actuo/bin/actuo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Actuo's user can replace `binary` (it can write its folder): `actuo update`
    /// can install a new release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_writable: Option<bool>,
}

/// Optional capabilities: none in this version. A new field doesn't change the
/// protocol version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActuoCapabilities {}

/// Actuo's approval levels, mapped per agent to the agent's own mode ids.
/// They don't mean the same thing for every agent: each agent's `modes` in the
/// agents config maps them.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// The agent asks before changes.
    #[default]
    Ask,
    /// Edits in the working directory are allowed without asking.
    Edits,
}

/// A value of an agent config option: select options take a value id, boolean
/// options take `true` or `false`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ConfigValue {
    Bool(bool),
    String(String),
}

/// An agent from actuo's agents config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    /// The name used in `session/start`, e.g. `claude`.
    pub name: String,
    /// Pinned adapter version, if the config records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    /// Levels this agent has a mode mapping for.
    pub levels: Vec<Level>,
    /// Default config values by the agent's config option id, from the settings'
    /// `[defaults]`.
    #[serde(default)]
    pub config: BTreeMap<String, ConfigValue>,
    /// Variables actuo passes to this agent's processes from its own
    /// environment: named by the agent's `pass_env` and set when actuo started
    /// (a token or API key). Names only, never values.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// The config options (model, effort, …) with their choices for the agent's own
    /// model: from the last `agent/probe`, else as the agent last advertised them in
    /// a session here. No current values, no `mode`. Empty until either has happened.
    #[serde(default)]
    pub config_options: Vec<AgentConfigOption>,
    /// The whole option list per `model` value (effort levels and fast mode depend
    /// on the model), from the last `agent/probe`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options_by_model: BTreeMap<String, Vec<AgentConfigOption>>,
    /// When the agent was last probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_at: Option<String>,
    /// The lane's approval rules for this agent when the settings have none for it.
    #[serde(default)]
    pub builtin_rules: Vec<crate::settings::Rule>,
}

/// A session that runs inside the sandbox (`srt`): it reads only its worktree and
/// what it needs, writes only its worktree and its own state, and reaches only the
/// agent's API hosts and `hosts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxOptions {
    /// Hosts the agent may reach besides its own API hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Paths under your home folder the agent may read besides what actuo
    /// allows (a repo's toolchain). Credential paths are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    /// Absolute path of the repository's top level.
    pub root: String,
    pub created_at: String,
    /// `owner/repo` when this is a project's clone (`[projects."owner/repo"]`); absent
    /// for a repo opened by path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A project's last check run, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_run: Option<crate::runs::CheckRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The agent is being spawned and set up.
    Starting,
    /// Ready for a prompt.
    Idle,
    /// A turn is running.
    Running,
    /// No agent process (after a restart or an agent crash); it can be
    /// continued with `session/send` because the agent supports resume or load.
    Detached,
    /// Stopped, or can't be continued.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: SessionId,
    pub project_id: ProjectId,
    /// Agent name from the agents config.
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    pub level: Level,
    pub clean: bool,
    /// Where the agent works: the worktree, or the project checkout.
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// What the branch was asked to start from (`session/start`'s `base`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Set when the agent runs inside the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxOptions>,
    #[serde(default = "yes")]
    pub web_tools: bool,
    #[serde(default = "yes")]
    pub telemetry: bool,
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Number of turns so far.
    pub turn_count: u32,
    /// `seq` of the last stored event.
    pub head_seq: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// Requested config values, by the agent's config option id.
    pub config: BTreeMap<String, ConfigValue>,
    pub turns: Vec<Turn>,
    pub open_requests: Vec<PermissionRequest>,
    /// The agent's latest slash-command list.
    pub commands: Vec<AgentCommand>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub n: u32,
    /// Exactly what was sent to the agent.
    pub text: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

/// Why a turn ended: ACP's stop reasons plus two of actuo's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// Actuo stopped while the turn was running.
    ActuoStopped,
    /// The agent process exited while the turn was running.
    AgentExited,
    /// The agent answered the prompt with an error (see the `error` event before).
    Failed,
}

/// Token counts for one turn, as far as the agent reports them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// The cost of one turn: the difference of the agent's running total within one
/// agent process. Absent when the agent reports no cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

/// A file a turn changed, from `git diff --name-status --no-renames` between the
/// checkpoints before and after the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Relative to the session's working directory.
    pub path: String,
    pub status: FileStatus,
}

/// A file in a session's diff (`session/diff`), with its line counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    /// Relative to the repository's top level.
    pub path: String,
    pub status: FileStatus,
    pub additions: u64,
    pub deletions: u64,
    /// A binary file: no line counts and no text in the patch.
    pub binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    /// `0` is before the first turn; `n` is after turn `n`.
    pub n: u32,
    pub commit: String,
    /// Files changed from checkpoint `n - 1` (empty for `0`).
    pub files: Vec<FileChange>,
    pub created_at: String,
}

/// An item of a turn. Events carry the whole item each time; message and thought
/// text also streams as `content.delta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Message(TextItem),
    Thought(TextItem),
    ToolCall(ToolCall),
    Plan(Plan),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextItem {
    /// The text so far.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_kind: ToolKind,
    pub title: String,
    pub status: ToolStatus,
    /// The tool's input as the agent sent it (for example Codex's command and cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    pub content: Vec<ToolContent>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    /// The turn ended before the tool call finished (actuo's, not ACP's).
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
    /// Content actuo doesn't model, as the agent sent it.
    Other {
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// An option of a permission request, passed through as the agent sent it.
/// An answer names its `optionId`; rules choose by `kind`, never by id or name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: PermissionOptionId,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// An open permission request, as returned by `session/get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub request_id: RequestId,
    pub turn: u32,
    pub tool_call: ToolCall,
    pub options: Vec<PermissionOption>,
    pub opened_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    Selected,
    Cancelled,
}

/// Who resolved a permission request. Agents can't tell a cancel from a reject
/// afterwards; actuo records it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Client,
    Interrupt,
    /// Actuo restarted while the request was open.
    ActuoRestart,
    /// The agent process exited while the request was open.
    AgentExited,
    /// The agent ended its turn while the request was open.
    TurnEnded,
}

/// The agent's own name and version, from its `initialize` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentImplementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A mode the agent advertised (for example Claude's `acceptEdits`, Codex's `read-only`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentMode {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A config option the agent advertised (model, effort, fast mode, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// ACP's category, e.g. `mode`, `model`, `thought_level`, `model_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub option_type: ConfigOptionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_value: Option<ConfigValue>,
    /// The choices of a select option; empty for a boolean.
    pub values: Vec<ConfigChoice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionType {
    Select,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A config value actuo applied, and what the agent reported afterwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppliedConfig {
    pub id: String,
    pub requested: ConfigValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ConfigValue>,
}

/// A slash command the agent offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hint for the command's input, if it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

fn yes() -> bool {
    true
}

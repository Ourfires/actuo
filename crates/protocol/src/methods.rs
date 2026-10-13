//! Methods actuo answers on its socket. Each is a type implementing [`Method`].

use crate::ids::{PermissionOptionId, ProjectId, RequestId, RunId, SessionId, SubscriptionId};
use crate::runs::{CheckRun, Run, RunDetail, RunState};
use crate::settings::{ProjectSettings, Settings};
use crate::types::{
    ActuoCapabilities, ActuoInfo, AgentEntry, Checkpoint, ClientInfo, ConfigValue, DiffFile, Level,
    Project, SandboxOptions, SessionDetail, SessionSummary,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A method's name, doc and the schemas of its params and result.
#[doc(hidden)]
pub struct MethodSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub params: schemars::Schema,
    pub result: schemars::Schema,
}

/// A method: its name on the wire and its params and result types.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Result: Serialize + DeserializeOwned + JsonSchema;
}

macro_rules! methods {
    ($($(#[doc = $doc:literal])* $ty:ident = $name:literal ($params:ty) -> $result:ty;)*) => {
        $(
            $(#[doc = $doc])*
            pub enum $ty {}

            impl Method for $ty {
                const NAME: &'static str = $name;
                type Params = $params;
                type Result = $result;
            }
        )*

        /// Every method name, in declaration order.
        pub const ALL: &[&str] = &[$($name,)*];

        /// Every method's name, doc and schemas, in declaration order.
        #[doc(hidden)]
        pub fn describe(generator: &mut schemars::SchemaGenerator) -> Vec<MethodSchema> {
            vec![$(MethodSchema {
                name: $name,
                description: concat!($($doc,)*).trim(),
                params: generator.subschema_for::<$params>(),
                result: generator.subschema_for::<$result>(),
            },)*]
        }
    };
}

methods! {
    /// The first call. Answers with actuo's version, or `unsupported_protocol_version`.
    Initialize = "initialize" (InitializeParams) -> InitializeResult;
    /// The agents in actuo's config. Never spawns an agent.
    AgentList = "agent/list" (Empty) -> AgentListResult;
    /// Opens (or finds) the project for a git repository.
    ProjectOpen = "project/open" (ProjectOpenParams) -> ProjectOpenResult;
    ProjectList = "project/list" (Empty) -> ProjectListResult;
    /// Starts a session. Returns before the agent is up; progress comes as `session.state` events.
    SessionStart = "session/start" (SessionStartParams) -> SessionStartResult;
    /// Sends a prompt, starting a turn. Reattaches first if the session is detached.
    SessionSend = "session/send" (SessionSendParams) -> SessionSendResult;
    /// Cancels the running turn and resolves its open permission requests as cancelled.
    SessionInterrupt = "session/interrupt" (SessionParams) -> Empty;
    /// Answers a permission request with one of its options, or cancels it.
    SessionRespond = "session/respond" (SessionRespondParams) -> Empty;
    /// Replays stored events after `fromSeq`, then streams live ones as `event` notifications.
    SessionSubscribe = "session/subscribe" (SessionSubscribeParams) -> SessionSubscribeResult;
    SessionUnsubscribe = "session/unsubscribe" (SessionUnsubscribeParams) -> Empty;
    SessionList = "session/list" (SessionListParams) -> SessionListResult;
    SessionGet = "session/get" (SessionParams) -> SessionGetResult;
    /// Ends the session and its agent process. Keeps the worktree.
    SessionStop = "session/stop" (SessionParams) -> Empty;
    CheckpointList = "checkpoint/list" (SessionParams) -> CheckpointListResult;
    /// Restores the files to the checkpoint before `turn`. Not the conversation.
    CheckpointRevert = "checkpoint/revert" (CheckpointRevertParams) -> CheckpointRevertResult;
    /// What the session's working tree (committed, uncommitted and untracked files)
    /// changes against its base. Allowed while a turn runs.
    SessionDiff = "session/diff" (SessionParams) -> SessionDiffResult;
    /// Actuo's settings, with every default filled in.
    SettingsGet = "settings/get" (Empty) -> SettingsResult;
    /// Re-reads `settings.toml`, applies the patch, validates and writes it. Fails with
    /// `invalid_settings` (nothing written) if the result doesn't validate.
    SettingsUpdate = "settings/update" (SettingsUpdateParams) -> SettingsResult;
    /// Queues a lane run of an issue by URL, without the label check. The repo must be
    /// a project with its lane on (`projects."owner/repo".lane`), and the lane not
    /// paused (`lane.enabled`).
    RunStart = "run/start" (RunStartParams) -> RunStartResult;
    /// Runs, newest first.
    RunList = "run/list" (RunListParams) -> RunListResult;
    /// One run with its steps.
    RunGet = "run/get" (RunParams) -> RunGetResult;
    /// Streams `run.state` notifications for every run whose state or step changes,
    /// and `check.state` for check runs.
    RunSubscribe = "run/subscribe" (RunSubscribeParams) -> RunSubscribeResult;
    RunUnsubscribe = "run/unsubscribe" (RunUnsubscribeParams) -> Empty;
    /// Hands a run back to the lane: a taken-over run (after its turn ends), or a
    /// waiting one that has no open request. Failing checks run again.
    RunResume = "run/resume" (RunParams) -> Empty;
    /// Ends a run that hasn't ended; the session and branch stay.
    RunCancel = "run/cancel" (RunParams) -> Empty;
    /// Starts the agent without a prompt, sets each of its models in turn and records
    /// the options it offers with each, then stops it (no tokens). Fails with
    /// `agent_error` if the agent can't start or answer within 60 s; the last
    /// probe's results stay.
    AgentProbe = "agent/probe" (AgentProbeParams) -> AgentProbeResult;
    /// Checks actuo's gh login can push to the repo, clones it (or fetches its
    /// clone), reads it and proposes its setup and checks. Saves nothing, except
    /// accepting a pending invitation of actuo's login to this repo (only this
    /// one) when it can't see or push to it otherwise.
    ProjectInspect = "project/inspect" (ProjectRepoParams) -> ProjectInspectResult;
    /// Adds a project: the access check and the clone again, then
    /// `[projects."owner/repo"]` with these settings. Its lane is off unless they
    /// turn it on.
    ProjectAdd = "project/add" (ProjectAddParams) -> ProjectAddResult;
    /// Removes a project from the settings; no run or session of it is touched.
    /// Its clone stays unless `deleteClone`. Refused while one of its runs works, or,
    /// with `deleteClone`, while a run or session in the clone hasn't ended.
    ProjectRemove = "project/remove" (ProjectRemoveParams) -> Empty;
    /// Starts a check run of a project: its setup and checks once, measured, with no
    /// agent. Progress as `check.state` notifications to `run/subscribe` subscribers;
    /// the last one is the project's `lastCheckRun` in `project/list`. Refused while
    /// one of the project's check runs is running.
    ProjectCheck = "project/check" (ProjectRepoParams) -> ProjectCheckResult;
    /// Adds one host to a project's `network` (the hosts setup may reach), keeping
    /// the others. Runs nothing.
    ProjectAllowHost = "project/allowHost" (ProjectAllowHostParams) -> SettingsResult;
    /// The machine actuo runs on (disk, memory), the lane's last poll, and what
    /// isn't right with them.
    ActuoStatus = "actuo/status" (Empty) -> ActuoStatusResult;
    /// Gets actuo ready for a restart (an update): from now until this
    /// connection ends, it starts no lane run, check run or resumed run (they wait
    /// for the next start). Answers once no lane run, check run or turn is in
    /// progress, or at `limitS` with what still is. A run waiting for an answer
    /// isn't waited for.
    ActuoDrain = "actuo/drain" (ActuoDrainParams) -> ActuoDrainResult;
}

/// No params, or an empty result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    /// `client.name` is shown on what this client does (turns, answers, run
    /// changes). "lane" is refused, in any case and with surrounding spaces: it's
    /// actuo's own name for the lane.
    pub client: ClientInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub actuo: ActuoInfo,
    pub capabilities: ActuoCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentListResult {
    pub agents: Vec<AgentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOpenParams {
    /// Any path inside the repository.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOpenResult {
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartParams {
    pub project_id: ProjectId,
    /// Agent name from the agents config, e.g. `claude`; default the settings'
    /// `sessions.agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Run in a new worktree (default) or in the project checkout.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub worktree: bool,
    /// What a new worktree's branch starts from; default `HEAD`. Used only when the
    /// branch is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Run the agent inside the sandbox (`srt`). The lane always sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxOptions>,
    /// `false` turns the agent's web search and fetch off (its `web_tools_off` config).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub web_tools: bool,
    /// `false` turns the agent's own telemetry off (its `telemetry_off` config).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub telemetry: bool,
    /// The worktree's branch; default `actuo/<sessionId>`. If the project already has
    /// a worktree on it, the session uses that worktree (refused while a session that
    /// hasn't ended works there); if the branch exists without one, a worktree is
    /// added for it. Never the project's current branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub level: Level,
    /// Start the agent without the user's global agent config.
    #[serde(default)]
    pub clean: bool,
    /// Values by the agent's config option id (model, effort, …), over the settings'
    /// `[defaults]` for the agent.
    /// `mode` isn't accepted here; the level sets it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, ConfigValue>,
    /// A first prompt, sent once the agent is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartResult {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionParams {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendParams {
    pub session_id: SessionId,
    pub text: String,
    /// Interrupt a running turn first (its open requests resolved `by: interrupt`),
    /// then send once it has ended. Without a running turn it changes nothing. Fails
    /// with `turn_in_progress`, nothing sent, if the turn hasn't ended after 30 s.
    #[serde(default, skip_serializing_if = "is_false")]
    pub interrupt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendResult {
    pub turn: u32,
}

/// Exactly one of `optionId` and `cancel: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRespondParams {
    pub session_id: SessionId,
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<PermissionOptionId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSubscribeParams {
    pub session_id: SessionId,
    /// Replay events with `seq` greater than this; `0` replays everything.
    #[serde(default)]
    pub from_seq: u64,
    /// Include each event's `raw` agent message.
    #[serde(default)]
    pub include_raw: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSubscribeResult {
    pub subscription_id: SubscriptionId,
    /// The last stored `seq` when the subscription started; replay goes up to here,
    /// live events follow. Drop duplicates by `seq`.
    pub head_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnsubscribeParams {
    pub subscription_id: SubscriptionId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResult {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGetResult {
    pub session: SessionDetail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointListResult {
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRevertParams {
    pub session_id: SessionId,
    /// Restore the files to how they were before this turn.
    pub turn: u32,
    /// Required when the session runs in the project checkout, not a worktree.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRevertResult {
    pub to_n: u32,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDiffResult {
    /// The commit the diff is against: the merge base of the session's `base` and
    /// its `HEAD`, or, for a session started without a `base`, its first checkpoint
    /// (the working tree when it started).
    pub base: String,
    /// Every changed file, even when `patch` is cut short. srt's placeholder files
    /// are left out, as in commits.
    pub files: Vec<DiffFile>,
    /// The unified diff (`a/` and `b/` prefixes, no renames).
    pub patch: String,
    /// `patch` was cut at 2 MB, at the end of a line.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsResult {
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdateParams {
    /// A JSON merge patch (RFC 7396) over the settings, by the file's keys: objects
    /// merge, `null` removes a key (its default applies again), anything else
    /// replaces, arrays whole.
    pub patch: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStartParams {
    /// `https://github.com/<owner>/<repo>/issues/<n>`.
    pub issue_url: String,
    /// Default: an `agent:<name>` label, else the repo's `agent`, else actuo's
    /// (`sessions.agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Default `lane.level`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStartResult {
    pub run_id: RunId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RunState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunListResult {
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunParams {
    pub run_id: RunId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunGetResult {
    pub run: RunDetail,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSubscribeParams {
    /// This connection shows run notifications itself: while any connection holds
    /// such a subscription, actuo posts none of its own on macOS.
    #[serde(default, skip_serializing_if = "is_false")]
    pub notifies: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSubscribeResult {
    pub subscription_id: SubscriptionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunUnsubscribeParams {
    pub subscription_id: SubscriptionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActuoDrainParams {
    /// How long to wait for what's in progress, in seconds.
    pub limit_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActuoDrainResult {
    /// What was still in progress at the limit, in plain words (`run 01a… of o/r`);
    /// empty when everything finished.
    pub in_progress: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActuoStatusResult {
    /// The disk that holds actuo's data (`~/.actuo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<Space>,
    /// `free` is memory available without swapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Space>,
    /// Absent until the lane has polled since actuo started, and when the lane
    /// is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll: Option<LanePoll>,
    /// The GitHub account actuo works as (its gh login): whom to give access
    /// to a repo. Absent when gh isn't logged in or actuo has no lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_login: Option<String>,
    /// A newer release of actuo: its commit (`actuo update` installs it).
    /// Asked from actuo's releases at start and once a day. Absent when this
    /// actuo is the latest release, isn't a release (a local build), or the
    /// question failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    /// What isn't right, decided by actuo: free disk under 10 GB (under 5 GB
    /// `serious`), a failed last poll.
    pub problems: Vec<Problem>,
}

/// Bytes free and in total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub free: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LanePoll {
    pub at: String,
    /// Set when the poll failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    /// In plain words, for display.
    pub message: String,
    pub serious: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentProbeParams {
    pub agent: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentProbeResult {
    /// The agent as `agent/list` now shows it.
    pub agent: AgentEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRepoParams {
    /// `owner/repo` on GitHub.
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAllowHostParams {
    pub repo: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInspectResult {
    pub access: RepoAccess,
    pub detected: Detected,
    /// Setup, checks and network from what was detected; edit before adding.
    pub proposal: ProjectSettings,
    /// The project's settings now, when it's a project already.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<ProjectSettings>,
    /// Actuo's gh login had a pending invitation to the repo, and accepted it
    /// (it couldn't see or push to the repo before).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted_invitation: bool,
}

/// What GitHub says about the repo, for actuo's gh login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoAccess {
    /// `ADMIN`, `MAINTAIN`, `WRITE` (enough to add it), `TRIAGE` or `READ`.
    pub permission: String,
    /// `PUBLIC`, `PRIVATE` or `INTERNAL`.
    pub visibility: String,
    pub default_branch: String,
    /// Whether the default branch is protected; absent when GitHub didn't say
    /// (a private repo on GitHub Free has no branch protection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected: Option<bool>,
}

/// What the repo's files say, read from its default branch (nothing installed or
/// run).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    /// `pnpm`, `npm`, `yarn` or `bun`, from the lockfile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
    /// From `packageManager` in `package.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager_version: Option<String>,
    /// `turbo` or `nx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_tool: Option<String>,
    /// The root and each workspace with its `lint`, `typecheck` and `test` scripts.
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// From `engines.node`, else `.nvmrc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    /// Its folder in the repo; empty for the root.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Which of `lint`, `typecheck` and `test` it has.
    pub scripts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAddParams {
    pub repo: String,
    pub settings: ProjectSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAddResult {
    /// Its clone as a project (`repo` set), for sessions started in it.
    pub project: Project,
    /// As in `project/inspect`: a pending invitation to the repo was accepted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted_invitation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRemoveParams {
    pub repo: String,
    /// Also delete actuo's clone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub delete_clone: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCheckResult {
    /// As it starts (`running`, no steps yet).
    pub check_run: CheckRun,
}

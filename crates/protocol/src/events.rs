//! Events: notifications a subscription receives, stored in order per session.
//!
//! Every event carries `seq` (per session, from 1), `sessionId`, `ts`, `type`,
//! optional `turn` and `itemId`, `data`, and `raw` (the agent's JSON-RPC message,
//! only when the subscription asked for it).

use crate::errors::ErrorKind;
use crate::ids::{ItemId, PermissionOptionId, RequestId, RunId, SessionId};
use crate::runs::RunStepKind;
use crate::types::{
    AgentCommand, AgentConfigOption, AgentImplementation, AgentMode, AppliedConfig, Cost,
    FileChange, Item, PermissionOption, PermissionOptionKind, RequestOutcome, ResolvedBy,
    SessionState, StopReason, TokenUsage, ToolCall,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    pub session_id: SessionId,
    pub ts: String,
    /// The turn this event belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// The item this event belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<ItemId>,
    #[serde(flatten)]
    pub body: EventBody,
    /// The agent's message this event came from, when the subscription set `includeRaw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// `type` and `data` of an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data")]
pub enum EventBody {
    #[serde(rename = "session.state")]
    SessionState(SessionStateData),
    #[serde(rename = "session.agent")]
    SessionAgent(SessionAgentData),
    #[serde(rename = "session.commands")]
    SessionCommands(SessionCommandsData),
    #[serde(rename = "turn.started")]
    TurnStarted(TurnStartedData),
    #[serde(rename = "turn.completed")]
    TurnCompleted(TurnCompletedData),
    #[serde(rename = "item.started")]
    ItemStarted(ItemData),
    #[serde(rename = "item.updated")]
    ItemUpdated(ItemData),
    #[serde(rename = "item.completed")]
    ItemCompleted(ItemData),
    #[serde(rename = "content.delta")]
    ContentDelta(ContentDeltaData),
    #[serde(rename = "request.opened")]
    RequestOpened(RequestOpenedData),
    #[serde(rename = "request.resolved")]
    RequestResolved(RequestResolvedData),
    #[serde(rename = "usage.updated")]
    UsageUpdated(UsageUpdatedData),
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated(CheckpointCreatedData),
    #[serde(rename = "checkpoint.reverted")]
    CheckpointReverted(CheckpointRevertedData),
    #[serde(rename = "agent.update")]
    AgentUpdate(AgentUpdateData),
    /// A step of a lane run in this session: setup, a check, commit, push, PR.
    #[serde(rename = "run.step")]
    RunStep(RunStepData),
    #[serde(rename = "error")]
    Error(ErrorEventData),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateData {
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What the agent advertised when the session started or was reattached, and the
/// mode and config actuo applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionAgentData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<AgentImplementation>,
    /// The agent's own session id (ACP's `sessionId`).
    pub agent_session_id: String,
    pub can_load: bool,
    pub can_resume: bool,
    pub modes: Vec<AgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_mode: Option<String>,
    pub config_options: Vec<AgentConfigOption>,
    pub applied_config: Vec<AppliedConfig>,
}

/// The agent's latest slash-command list; sent only when it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandsData {
    pub commands: Vec<AgentCommand>,
}

/// The turn number is the event's `turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedData {
    /// Exactly what was sent to the agent, including any note actuo added
    /// (for example after a revert).
    pub text: String,
    /// The `client.name` (from `initialize`) of the client that sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedData {
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Files changed between the checkpoints before and after the turn.
    pub files: Vec<FileChange>,
}

/// The whole item as it is now. The item id is the event's `itemId`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemData {
    pub item: Item,
}

/// Text appended to a message or thought item (the event's `itemId`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContentDeltaData {
    pub text: String,
}

/// A permission request. The event's `itemId` is the tool call it is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestOpenedData {
    pub request_id: RequestId,
    pub tool_call: ToolCall,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestResolvedData {
    pub request_id: RequestId,
    pub outcome: RequestOutcome,
    /// Set when `outcome` is `selected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<PermissionOptionId>,
    /// Set when `outcome` is `selected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_kind: Option<PermissionOptionKind>,
    pub by: ResolvedBy,
    /// The `client.name` of the client that answered, when `by` is `client`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageUpdatedData {
    pub context_used: u64,
    pub context_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCreatedData {
    pub n: u32,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRevertedData {
    /// The checkpoint restored: `turn - 1` for a revert to before `turn`.
    pub to_n: u32,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
}

/// An agent update actuo doesn't map (for example a session title).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateData {
    /// The agent's name for the update, e.g. ACP's `session_info_update`.
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEventData {
    pub kind: ErrorKind,
    pub message: String,
    /// The session can't go on as it was (the agent exited, for example).
    pub fatal: bool,
}

/// A lane run's step, recorded in its session's log when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStepData {
    pub run_id: RunId,
    /// The step's number within the run (`run/get`'s `steps`).
    pub n: u32,
    pub kind: RunStepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `None` when the command was stopped at its time limit or couldn't start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    pub duration_ms: u64,
}

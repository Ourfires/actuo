//! The agent host, in actuo terms. A backend spawns one agent process per session
//! and gives back an [`AgentHandle`] (commands) and a stream of [`AgentEvent`]s
//! (already mapped to actuo events). Nothing here is ACP-shaped, so other backends
//! can sit next to [`acp::AcpBackend`].

pub mod acp;

use actuo_protocol::events::EventBody;
use actuo_protocol::ids::{ItemId, PermissionOptionId, RequestId};
use actuo_protocol::types::{
    AgentConfigOption, AgentImplementation, AgentMode, ConfigValue, Cost, StopReason, TokenUsage,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

/// How to start an agent process.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub command: String,
    pub args: Vec<String>,
    /// Added to actuo's environment (or the whole environment, with `clear_env`).
    pub env: BTreeMap<String, String>,
    /// Start from an empty environment instead of actuo's (inside the sandbox).
    pub clear_env: bool,
    /// The session's working directory: the process's cwd and the root `fs/*` is
    /// served in.
    pub cwd: PathBuf,
    /// Where the agent's stderr goes (appended).
    pub stderr_log: Option<PathBuf>,
    /// The command is `srt --debug` with the records preload: srt's records come on
    /// a pipe of their own (`sandbox::records`), refusals labeled with this in
    /// actuo's log.
    pub srt_records: Option<String>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    #[error("the agent needs a login: {0}")]
    AuthRequired(String),
    #[error("agent error {code}: {message}")]
    Agent {
        code: i32,
        message: String,
        data: Option<Value>,
    },
    #[error("the agent process is gone")]
    Gone,
    #[error("couldn't start the agent: {0}")]
    Spawn(String),
    #[error("unexpected answer from the agent: {0}")]
    Protocol(String),
}

pub type Result<T, E = AgentError> = std::result::Result<T, E>;

/// What the agent's `initialize` answer says.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub implementation: Option<AgentImplementation>,
    pub can_load: bool,
    pub can_resume: bool,
}

/// What a new, loaded or resumed session offers.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSetup {
    pub agent_session_id: String,
    pub modes: Vec<AgentMode>,
    pub current_mode: Option<String>,
    pub config_options: Vec<AgentConfigOption>,
}

/// An actuo event produced from the agent's messages. The session adds the turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub item_id: Option<ItemId>,
    pub body: EventBody,
    /// The agent's JSON-RPC message it came from.
    pub raw: Option<Value>,
}

/// How a turn ended, as the agent reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub stop_reason: StopReason,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<Cost>,
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// The last few KB of the agent's stderr.
    pub stderr_tail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A mapped event, in the order the agent sent it. `request.opened` events
    /// need an answer through [`AgentHandle::respond`].
    Event(Mapped),
    /// The prompt finished. Open items were completed just before.
    TurnEnded(TurnEnd),
    /// The prompt failed. Open items were completed just before.
    TurnFailed(AgentError),
    /// The process exited (on its own or killed).
    Exited(ExitInfo),
}

pub(crate) enum Command {
    Initialize(oneshot::Sender<Result<AgentInfo>>),
    NewSession {
        meta: Option<Value>,
        reply: oneshot::Sender<Result<SessionSetup>>,
    },
    ResumeSession {
        id: String,
        meta: Option<Value>,
        reply: oneshot::Sender<Result<SessionSetup>>,
    },
    LoadSession {
        id: String,
        meta: Option<Value>,
        reply: oneshot::Sender<Result<SessionSetup>>,
    },
    SetMode {
        mode: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SetConfig {
        id: String,
        value: ConfigValue,
        reply: oneshot::Sender<Result<Vec<AgentConfigOption>>>,
    },
    Prompt {
        turn: u32,
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Cancel,
    Respond {
        request: RequestId,
        choice: Option<PermissionOptionId>,
    },
}

/// Commands to one agent process, which hosts one session.
#[derive(Clone)]
pub struct AgentHandle {
    tx: mpsc::UnboundedSender<Command>,
    pid: u32,
}

impl AgentHandle {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Command>, pid: u32) -> Self {
        Self { tx, pid }
    }

    /// The process group leader's pid (also the group id), to record for leftovers
    /// after a restart. Its start time is looked up with
    /// [`crate::process::start_time`], which runs `ps`: call it off the async threads.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).map_err(|_| AgentError::Gone)?;
        rx.await.map_err(|_| AgentError::Gone)?
    }

    pub async fn initialize(&self) -> Result<AgentInfo> {
        self.call(Command::Initialize).await
    }

    /// `meta` goes into the request's `_meta` (e.g. Claude's `claudeCode.options`).
    pub async fn new_session(&self, meta: Option<Value>) -> Result<SessionSetup> {
        self.call(|reply| Command::NewSession { meta, reply }).await
    }

    pub async fn resume_session(&self, id: &str, meta: Option<Value>) -> Result<SessionSetup> {
        let id = id.to_string();
        self.call(|reply| Command::ResumeSession { id, meta, reply })
            .await
    }

    /// Loads a session. The agent's replay of the history is dropped: history comes
    /// from actuo's event log.
    pub async fn load_session(&self, id: &str, meta: Option<Value>) -> Result<SessionSetup> {
        let id = id.to_string();
        self.call(|reply| Command::LoadSession { id, meta, reply })
            .await
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        let mode = mode.to_string();
        self.call(|reply| Command::SetMode { mode, reply }).await
    }

    /// Returns the config options as the agent reports them afterwards.
    pub async fn set_config(&self, id: &str, value: ConfigValue) -> Result<Vec<AgentConfigOption>> {
        let id = id.to_string();
        self.call(|reply| Command::SetConfig { id, value, reply })
            .await
    }

    /// Sends a prompt. Returns once it is sent; the turn's events follow, then
    /// [`AgentEvent::TurnEnded`] or [`AgentEvent::TurnFailed`].
    pub async fn prompt(&self, turn: u32, text: &str) -> Result<()> {
        let text = text.to_string();
        self.call(|reply| Command::Prompt { turn, text, reply })
            .await
    }

    /// Asks the agent to cancel the running turn. The caller answers open
    /// permission requests with [`respond`](Self::respond)`(_, None)`.
    pub fn cancel(&self) -> Result<()> {
        self.tx.send(Command::Cancel).map_err(|_| AgentError::Gone)
    }

    /// Answers a permission request with one of its options, or `None` for cancelled.
    pub fn respond(&self, request: RequestId, choice: Option<PermissionOptionId>) -> Result<()> {
        self.tx
            .send(Command::Respond { request, choice })
            .map_err(|_| AgentError::Gone)
    }

    /// Kills the agent's process group and waits until it is gone.
    pub async fn shutdown(&self) {
        crate::process::kill_group(self.pid, crate::process::TERM_GRACE).await;
    }
}

/// Something that can start agents ([`acp::AcpBackend`]).
pub trait AgentBackend: Send + Sync {
    fn spawn(&self, spec: AgentSpec) -> Result<(AgentHandle, mpsc::UnboundedReceiver<AgentEvent>)>;
}

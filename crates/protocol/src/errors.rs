//! Errors. A JSON-RPC error's `data` is an [`ErrorData`]: `kind` plus details.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

macro_rules! error_kinds {
    ($($(#[$doc:meta])* $variant:ident = $code:literal, $name:literal;)*) => {
        /// What went wrong. Each kind has one JSON-RPC error code.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        pub enum ErrorKind {
            $($(#[$doc])* #[serde(rename = $name)] $variant,)*
        }

        impl ErrorKind {
            pub const ALL: &[ErrorKind] = &[$(ErrorKind::$variant,)*];

            pub fn code(self) -> i32 {
                match self {
                    $(ErrorKind::$variant => $code,)*
                }
            }

            /// The snake_case name used in `data.kind`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(ErrorKind::$variant => $name,)*
                }
            }

            pub fn from_code(code: i32) -> Option<Self> {
                match code {
                    $($code => Some(ErrorKind::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

error_kinds! {
    // JSON-RPC's own.
    ParseError = -32700, "parse_error";
    InvalidRequest = -32600, "invalid_request";
    MethodNotFound = -32601, "method_not_found";
    InvalidParams = -32602, "invalid_params";
    Internal = -32603, "internal";
    // Actuo's.
    /// A call other than `initialize` came first.
    NotInitialized = -32001, "not_initialized";
    /// `data.supported` lists the versions actuo speaks.
    UnsupportedProtocolVersion = -32002, "unsupported_protocol_version";
    ProjectNotFound = -32003, "project_not_found";
    /// Not a git repository, or one without a commit.
    NotAGitRepo = -32004, "not_a_git_repo";
    AgentNotFound = -32005, "agent_not_found";
    SessionNotFound = -32006, "session_not_found";
    SessionEnded = -32007, "session_ended";
    TurnInProgress = -32008, "turn_in_progress";
    NoTurnRunning = -32009, "no_turn_running";
    /// Unknown or already resolved permission request.
    RequestNotFound = -32010, "request_not_found";
    /// The option isn't one of the request's options.
    InvalidOption = -32011, "invalid_option";
    /// A config id or value the agent didn't advertise, or `mode` in `config`.
    InvalidConfigOption = -32012, "invalid_config_option";
    CheckpointNotFound = -32013, "checkpoint_not_found";
    /// Revert without a worktree needs `force`.
    ForceRequired = -32014, "force_required";
    /// The agent needs a login: log in with the agent's own CLI.
    AuthRequired = -32015, "auth_required";
    /// The agent answered with an error; the message has it.
    AgentError = -32016, "agent_error";
    /// The agent process exited.
    AgentExited = -32017, "agent_exited";
    GitError = -32018, "git_error";
    /// A settings change (or the file) doesn't validate; `data.field` and
    /// `data.reason` say where and why.
    InvalidSettings = -32019, "invalid_settings";
    RunNotFound = -32020, "run_not_found";
    IssueNotFound = -32021, "issue_not_found";
    /// A `gh` call failed; the message has its error.
    GhError = -32022, "gh_error";
    /// `run/resume` or `run/cancel` on a run in a state that doesn't allow it.
    InvalidRunState = -32023, "invalid_run_state";
    /// The sandbox (`srt`) can't start here; the message has why.
    SandboxUnavailable = -32024, "sandbox_unavailable";
    /// GitHub has no such repo, or actuo's gh login can't see it.
    RepoNotFound = -32025, "repo_not_found";
    /// Actuo's gh login can't push to the repo, or the repo is archived.
    NoPushAccess = -32026, "no_push_access";
    /// `project/add` for a repo that's a project already.
    ProjectExists = -32027, "project_exists";
    /// `project/remove` while a run of the project works, or `project/check` while
    /// a check run of it runs or actuo is about to restart.
    ProjectBusy = -32028, "project_busy";
}

/// The `data` of every JSON-RPC error actuo sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    pub kind: ErrorKind,
    /// Details depending on the kind, e.g. `supported` for
    /// `unsupported_protocol_version`.
    #[serde(flatten)]
    pub details: Map<String, Value>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

impl RpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            message: message.into(),
            data: Some(ErrorData {
                kind,
                details: Map::new(),
            }),
        }
    }

    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        if let Some(data) = self.data.as_mut() {
            data.details.insert(key.to_string(), value.into());
        }
        self
    }

    /// The kind, from `data` or else from the code.
    pub fn kind(&self) -> Option<ErrorKind> {
        self.data
            .as_ref()
            .map(|d| d.kind)
            .or_else(|| ErrorKind::from_code(self.code))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            Some(kind) => write!(f, "{} ({}): {}", kind.as_str(), self.code, self.message),
            None => write!(f, "error {}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for RpcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique_and_round_trip() {
        let mut seen = HashSet::new();
        for kind in ErrorKind::ALL {
            assert!(seen.insert(kind.code()), "duplicate code {}", kind.code());
            assert_eq!(ErrorKind::from_code(kind.code()), Some(*kind));
        }
    }

    #[test]
    fn names_match_serde() {
        for kind in ErrorKind::ALL {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                Value::String(kind.as_str().into())
            );
        }
    }

    #[test]
    fn error_shape() {
        let e = RpcError::new(ErrorKind::UnsupportedProtocolVersion, "speaks 1")
            .with_detail("supported", vec![1]);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "code": -32002,
                "message": "speaks 1",
                "data": {"kind": "unsupported_protocol_version", "supported": [1]}
            })
        );
        assert_eq!(serde_json::from_value::<RpcError>(v).unwrap(), e);
    }
}

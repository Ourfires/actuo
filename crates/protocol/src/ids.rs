//! Identifiers. Project and session ids are UUIDv7 strings; request, item and
//! subscription ids are actuo-made strings, unique within their session or connection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

string_id!(
    /// A project: one git repository actuo has opened.
    ProjectId
);
string_id!(
    /// An actuo session. Not the agent's own session id.
    SessionId
);
string_id!(
    /// A permission request, unique within its session.
    RequestId
);
string_id!(
    /// An item (message, thought, tool call, plan), unique within its session.
    ItemId
);
string_id!(
    /// A subscription to a session's events, unique within its connection.
    SubscriptionId
);
string_id!(
    /// An option of a permission request, as the agent sent it.
    PermissionOptionId
);
string_id!(
    /// A run of the issue-to-PR lane: one attempt at one issue.
    RunId
);
string_id!(
    /// A check run: a project's setup and checks once, with no agent.
    CheckRunId
);

//! The script a fake agent follows: what it advertises, and what each turn does.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Script {
    /// Advertise `loadSession` (needs `--state` to find sessions after a restart).
    #[serde(default)]
    pub load_session: bool,
    /// Advertise `sessionCapabilities.resume` (needs `--state` too).
    #[serde(default)]
    pub resume: bool,
    /// Answer `session/new` with `auth_required`.
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default)]
    pub modes: Vec<Mode>,
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    /// Sent as `available_commands_update` right after `session/new`.
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Tried in order; the first whose `match` is in the prompt text runs.
    #[serde(default)]
    pub turns: Vec<Turn>,
    /// Runs when no turn matches. Without one, the agent answers "ok".
    #[serde(default)]
    pub default_turn: Option<Turn>,
    /// A cancelled turn ends only this many milliseconds after it was cancelled: an
    /// agent slow to stop (or, with a long delay, one that ignores the cancel).
    #[serde(default)]
    pub cancel_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mode {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    pub values: Vec<String>,
    pub current: String,
    /// Shown only while the `model` option is one of these (as a real agent's
    /// effort or fast mode depends on the model).
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Choices per `model` value, instead of `values`.
    #[serde(default)]
    pub values_by_model: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Turn {
    /// Substring of the prompt text that selects this turn.
    #[serde(default, rename = "match")]
    pub matches: String,
    pub steps: Vec<Step>,
    /// ACP stop reason when the steps finish; `end_turn` by default.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Skipped once this file exists (relative to the session's cwd): a turn that
    /// writes it runs only the first time.
    #[serde(default)]
    pub unless_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Step {
    /// One `agent_message_chunk`.
    Message(String),
    /// One `agent_thought_chunk`.
    Thought(String),
    /// A `plan` update.
    Plan(Vec<PlanEntry>),
    /// A `tool_call` update.
    ToolCall(ToolCall),
    /// A `tool_call_update`.
    ToolUpdate(ToolUpdate),
    /// Asks `session/request_permission` about a tool call and waits for the answer.
    /// A cancelled answer ends the turn with `cancelled`.
    Permission(Permission),
    /// Writes a file straight to disk under the session's cwd, as real agents do.
    WriteFile {
        path: String,
        content: String,
        /// Mode 0755, like a script or a git hook.
        #[serde(default)]
        executable: bool,
    },
    /// A `usage_update`; `cost` is the running total for this process, in USD.
    Usage {
        used: u64,
        size: u64,
        #[serde(default)]
        cost: Option<f64>,
    },
    /// A `session_info_update` with a title.
    Title(String),
    /// An `available_commands_update`.
    Commands(Vec<Command>),
    SleepMs(u64),
    /// Starts a process in the agent's process group and doesn't wait for it, like
    /// a tool command still running (logged as `spawned` with its pid).
    Spawn(Vec<String>),
    /// Runs a command in the session's cwd and waits for it, like a tool command the
    /// agent ran (logged as `command` with its exit code). A failure fails the turn.
    Command(Vec<String>),
    /// Waits for `session/cancel`, then ends the turn with `cancelled`.
    WaitForCancel,
    /// Exits the process at once with this code (a crash).
    Exit(i32),
    /// Answers the prompt with a JSON-RPC error.
    Error {
        code: i32,
        message: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    /// A diff for a file under the session's cwd.
    #[serde(default)]
    pub diff: Option<Diff>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diff {
    pub path: String,
    #[serde(default)]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolUpdate {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Permission {
    pub tool_call_id: String,
    pub options: Vec<PermissionOption>,
    /// Steps when an `allow_*` option is chosen.
    #[serde(default)]
    pub allowed: Vec<Step>,
    /// Steps when a `reject_*` option is chosen.
    #[serde(default)]
    pub rejected: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// ACP kind: `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

impl Script {
    pub fn turn_for(&self, prompt: &str, cwd: &std::path::Path) -> Turn {
        self.turns
            .iter()
            .filter(|t| t.unless_file.as_ref().is_none_or(|f| !cwd.join(f).exists()))
            .find(|t| prompt.contains(&t.matches))
            .or(self.default_turn.as_ref())
            .cloned()
            .unwrap_or_else(|| Turn {
                matches: String::new(),
                steps: vec![Step::Message("ok".into())],
                stop_reason: None,
                unless_file: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_step() {
        let script: Script = serde_json::from_value(serde_json::json!({
            "loadSession": true,
            "resume": true,
            "modes": [{"id": "ask", "name": "Ask"}],
            "configOptions": [{"id": "model", "name": "Model", "category": "model",
                               "values": ["small", "large"], "current": "small"}],
            "commands": [{"name": "compact", "description": "Compact"}],
            "turns": [{
                "match": "notes",
                "steps": [
                    {"thought": "thinking"},
                    {"message": "Hello"},
                    {"plan": [{"content": "write", "status": "pending"}]},
                    {"toolCall": {"id": "t1", "title": "Write NOTES.md", "kind": "edit",
                                  "diff": {"path": "NOTES.md", "newText": "x\n"}}},
                    {"permission": {"toolCallId": "t1",
                        "options": [{"id": "allow-once", "name": "Yes", "kind": "allow_once"}],
                        "allowed": [{"writeFile": {"path": "NOTES.md", "content": "x\n"}},
                                    {"toolUpdate": {"id": "t1", "status": "completed"}}],
                        "rejected": [{"toolUpdate": {"id": "t1", "status": "failed"}}]}},
                    {"usage": {"used": 10, "size": 100, "cost": 0.1}},
                    {"title": "Notes"},
                    {"commands": [{"name": "review", "description": "Review"}]},
                    {"sleepMs": 5},
                    {"spawn": ["sleep", "1"]},
                    "waitForCancel",
                    {"exit": 3},
                    {"error": {"code": -32603, "message": "boom"}}
                ]
            }]
        }))
        .unwrap();
        assert_eq!(script.turns[0].steps.len(), 13);
        let cwd = std::path::Path::new("/nonexistent");
        assert_eq!(script.turn_for("write notes please", cwd).steps.len(), 13);
        assert!(
            matches!(script.turn_for("other", cwd).steps[0], Step::Message(ref m) if m == "ok")
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(serde_json::from_value::<Script>(serde_json::json!({"loadSesion": true})).is_err());
    }
}

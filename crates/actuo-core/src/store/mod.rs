//! The session store: SQLite with an append-only event log.
//!
//! Events are the source of truth. The other tables are projections, updated in the
//! same transaction as the event that changes them. An event is committed before it
//! is broadcast, and broadcasts happen under the same lock as the write, so every
//! subscriber sees a session's events in `seq` order.

mod migrations;
mod projection;
mod runs;
#[cfg(test)]
mod tests;

use crate::time::now;
use actuo_protocol::events::{Event, EventBody};
use actuo_protocol::ids::{ItemId, ProjectId, RequestId, SessionId};
use actuo_protocol::runs::{CheckRun, CheckRunState};
use actuo_protocol::types::{
    AgentCommand, AgentConfigOption, Checkpoint, ConfigValue, Cost, FileChange, Level,
    PermissionOption, PermissionRequest, Project, SessionDetail, SessionState, SessionSummary,
    StopReason, TokenUsage, ToolCall, Turn,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// How many events a slow subscriber may fall behind before it gets `Lagged` and
/// has to catch up from [`Store::events_after`].
const BROADCAST_CAPACITY: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} not found")]
    NotFound(String),
    /// A unique constraint, e.g. a second open run of one issue.
    #[error("{0}")]
    Conflict(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

pub use runs::{ChangedBy, NewRun, RunRow};

/// An event to append; the store assigns `seq` and `ts`.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub turn: Option<u32>,
    pub item_id: Option<ItemId>,
    pub body: EventBody,
    pub raw: Option<Value>,
}

impl NewEvent {
    pub fn new(body: EventBody) -> Self {
        Self {
            turn: None,
            item_id: None,
            body,
            raw: None,
        }
    }

    pub fn turn(mut self, turn: u32) -> Self {
        self.turn = Some(turn);
        self
    }

    pub fn item(mut self, item_id: ItemId) -> Self {
        self.item_id = Some(item_id);
        self
    }

    pub fn raw(mut self, raw: Value) -> Self {
        self.raw = Some(raw);
        self
    }
}

/// A session to create. The caller makes the id (see [`new_session_id`]) because the
/// worktree path contains it.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub agent: String,
    pub agent_version: Option<String>,
    pub level: Level,
    pub clean: bool,
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub sandbox: Option<actuo_protocol::types::SandboxOptions>,
    pub web_tools: bool,
    pub telemetry: bool,
    pub config: BTreeMap<String, ConfigValue>,
}

/// The agent process a session last ran, to find leftovers after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcess {
    /// The leader's pid, which is also the process group id.
    pub pid: u32,
    /// The leader's start time as the OS reports it, to tell a reused pid apart.
    pub started_at: String,
}

pub struct Store {
    conn: Mutex<Connection>,
    events_tx: broadcast::Sender<Arc<Event>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            conn: Mutex::new(conn),
            events_tx,
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store lock poisoned")
    }

    /// Live events of every session, after they are committed.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events_tx.subscribe()
    }

    // Projects

    /// Finds the project for `root`, or creates it.
    pub fn open_project(&self, root: &str) -> Result<Project> {
        let conn = self.conn();
        if let Some(p) = conn
            .query_row(
                "SELECT id, root, created_at FROM projects WHERE root = ?1",
                [root],
                project_from_row,
            )
            .optional()?
        {
            return Ok(p);
        }
        let project = Project {
            id: ProjectId(new_id()),
            root: root.to_string(),
            created_at: now(),
            repo: None,
            last_check_run: None,
        };
        conn.execute(
            "INSERT INTO projects (id, root, created_at) VALUES (?1, ?2, ?3)",
            params![project.id.0, project.root, project.created_at],
        )?;
        Ok(project)
    }

    pub fn project(&self, id: &ProjectId) -> Result<Project> {
        self.conn()
            .query_row(
                "SELECT id, root, created_at FROM projects WHERE id = ?1",
                [&id.0],
                project_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("project {id}")))
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, root, created_at FROM projects ORDER BY created_at")?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // Sessions

    /// Creates a session in state `starting`. The caller appends the first
    /// `session.state` event.
    pub fn create_session(&self, new: NewSession) -> Result<SessionSummary> {
        let id = new.id.clone();
        let ts = now();
        self.conn().execute(
            "INSERT INTO sessions (id, project_id, agent, agent_version, level, clean, cwd,
                worktree_path, branch, base, config_json, state, created_at, updated_at,
                sandbox_json, web_tools, telemetry)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, ?14, ?15, ?16)",
            params![
                id.0,
                new.project_id.0,
                new.agent,
                new.agent_version,
                enum_str(&new.level),
                new.clean,
                new.cwd,
                new.worktree_path,
                new.branch,
                new.base,
                serde_json::to_string(&new.config)?,
                enum_str(&SessionState::Starting),
                ts,
                new.sandbox
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                new.web_tools,
                new.telemetry,
            ],
        )?;
        self.session(&id)
    }

    pub fn session(&self, id: &SessionId) -> Result<SessionSummary> {
        let row = self
            .conn()
            .query_row(
                &format!("{SESSION_SELECT} WHERE s.id = ?1"),
                [&id.0],
                session_from_row,
            )
            .optional()?;
        row.unwrap_or_else(|| Err(StoreError::NotFound(format!("session {id}"))))
    }

    /// Sessions, newest first, optionally of one project.
    pub fn sessions(&self, project: Option<&ProjectId>) -> Result<Vec<SessionSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "{SESSION_SELECT} WHERE ?1 IS NULL OR s.project_id = ?1 ORDER BY s.created_at DESC"
        ))?;
        let rows = stmt.query_map([project.map(|p| &p.0)], session_from_row)?;
        rows.map(|r| r?).collect()
    }

    /// Sessions in any of `states`, oldest first (for recovery after a restart).
    pub fn sessions_in_states(&self, states: &[SessionState]) -> Result<Vec<SessionSummary>> {
        let wanted: Vec<String> = states.iter().map(enum_str).collect();
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("{SESSION_SELECT} ORDER BY s.created_at"))?;
        let rows = stmt.query_map([], session_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            let s = row??;
            if wanted.contains(&enum_str(&s.state)) {
                out.push(s);
            }
        }
        Ok(out)
    }

    pub fn session_detail(&self, id: &SessionId) -> Result<SessionDetail> {
        let summary = self.session(id)?;
        let (config_json, commands_json): (String, Option<String>) = self.conn().query_row(
            "SELECT config_json, commands_json FROM sessions WHERE id = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let commands: Vec<AgentCommand> = match commands_json {
            Some(j) => serde_json::from_str(&j)?,
            None => Vec::new(),
        };
        Ok(SessionDetail {
            summary,
            config: serde_json::from_str(&config_json)?,
            turns: self.turns(id)?,
            open_requests: self.open_requests(id)?,
            commands,
        })
    }

    /// The agent's own session id, once `session.agent` recorded it.
    pub fn agent_session_id(&self, id: &SessionId) -> Result<Option<String>> {
        Ok(self.conn().query_row(
            "SELECT acp_session_id FROM sessions WHERE id = ?1",
            [&id.0],
            |r| r.get(0),
        )?)
    }

    /// Whether the agent advertised resume and load (`None` before `session.agent`).
    pub fn agent_can_continue(&self, id: &SessionId) -> Result<(Option<bool>, Option<bool>)> {
        Ok(self.conn().query_row(
            "SELECT can_resume, can_load FROM sessions WHERE id = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn set_agent_process(&self, id: &SessionId, process: Option<&AgentProcess>) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET agent_pid = ?2, agent_started_at = ?3 WHERE id = ?1",
            params![
                id.0,
                process.map(|p| p.pid),
                process.map(|p| p.started_at.clone())
            ],
        )?;
        Ok(())
    }

    pub fn agent_process(&self, id: &SessionId) -> Result<Option<AgentProcess>> {
        let (pid, started_at): (Option<u32>, Option<String>) = self.conn().query_row(
            "SELECT agent_pid, agent_started_at FROM sessions WHERE id = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match (pid, started_at) {
            (Some(pid), Some(started_at)) => Some(AgentProcess { pid, started_at }),
            _ => None,
        })
    }

    // Events

    /// Appends an event: assigns `seq` and `ts`, updates the projections, commits,
    /// then broadcasts.
    pub fn append(&self, session: &SessionId, new: NewEvent) -> Result<Arc<Event>> {
        debug_assert_not_async();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let head: u64 = tx
            .query_row(
                "SELECT head_seq FROM sessions WHERE id = ?1",
                [&session.0],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("session {session}")))?;
        let event = Event {
            seq: head + 1,
            session_id: session.clone(),
            ts: now(),
            turn: new.turn,
            item_id: new.item_id,
            body: new.body,
            raw: new.raw,
        };
        let (kind, data) = split_body(&event.body)?;
        tx.execute(
            "INSERT INTO events (session_id, seq, ts, type, turn, item_id, data_json, raw_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session.0,
                event.seq,
                event.ts,
                kind,
                event.turn,
                event.item_id.as_ref().map(|i| &i.0),
                serde_json::to_string(&data)?,
                event.raw.as_ref().map(serde_json::to_string).transpose()?,
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET head_seq = ?2, updated_at = ?3 WHERE id = ?1",
            params![session.0, event.seq, event.ts],
        )?;
        projection::apply(&tx, &event)?;
        tx.commit()?;
        let event = Arc::new(event);
        // No receivers is fine.
        let _ = self.events_tx.send(event.clone());
        Ok(event)
    }

    /// Stored events with `seq > after`, at most `limit`, in order.
    pub fn events_after(
        &self,
        session: &SessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, type, turn, item_id, data_json, raw_json FROM events
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session.0, after, limit as i64], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<u32>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, ts, kind, turn, item_id, data, raw) = row?;
            let body: EventBody = serde_json::from_value(serde_json::json!({
                "type": kind,
                "data": serde_json::from_str::<Value>(&data)?,
            }))?;
            out.push(Event {
                seq,
                session_id: session.clone(),
                ts,
                turn,
                item_id: item_id.map(ItemId),
                body,
                raw: raw.map(|r| serde_json::from_str(&r)).transpose()?,
            });
        }
        Ok(out)
    }

    // Projections

    pub fn turns(&self, session: &SessionId) -> Result<Vec<Turn>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n, text, started_at, completed_at, stop_reason, tokens_json, cost_amount,
                    cost_currency
             FROM turns WHERE session_id = ?1 ORDER BY n",
        )?;
        let rows = stmt.query_map([&session.0], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<f64>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (n, text, started_at, completed_at, stop, tokens, amount, currency) = row?;
            out.push(Turn {
                n,
                text,
                started_at,
                completed_at,
                stop_reason: stop.map(|s| parse_enum::<StopReason>(&s)).transpose()?,
                tokens: tokens
                    .map(|t| serde_json::from_str::<TokenUsage>(&t))
                    .transpose()?,
                cost: match (amount, currency) {
                    (Some(amount), Some(currency)) => Some(Cost { amount, currency }),
                    _ => None,
                },
            });
        }
        Ok(out)
    }

    /// The config options `agent` advertised in its newest session that has any,
    /// without their current values (that was one session's) and without `mode`.
    pub fn config_options(&self, agent: &str) -> Result<Vec<AgentConfigOption>> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT config_options_json FROM sessions
                 WHERE agent = ?1 AND config_options_json IS NOT NULL
                   AND config_options_json <> '[]'
                 ORDER BY created_at DESC LIMIT 1",
                [agent],
                |r| r.get(0),
            )
            .optional()?;
        let mut options: Vec<AgentConfigOption> = match json {
            Some(j) => serde_json::from_str(&j)?,
            None => Vec::new(),
        };
        // The level sets the mode; it isn't a default (`session/start`, settings).
        options.retain(|o| o.id != "mode");
        for o in &mut options {
            o.current_value = None;
        }
        Ok(options)
    }

    /// Writes a check run as it is now (each change replaces the row).
    pub fn save_check_run(&self, run: &CheckRun) -> Result<()> {
        self.conn().execute(
            "INSERT INTO check_runs (id, repo, state, started_at, json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state = ?3, json = ?5",
            params![
                run.id.0,
                run.repo,
                enum_str(&run.state),
                run.started_at,
                serde_json::to_string(run)?
            ],
        )?;
        Ok(())
    }

    /// A repo's newest check run.
    pub fn last_check_run(&self, repo: &str) -> Result<Option<CheckRun>> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT json FROM check_runs WHERE repo = ?1 ORDER BY started_at DESC LIMIT 1",
                [repo],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    /// A repo's newest finished check run (passed or failed) of `commit`: a lane
    /// run's baseline. Runs that are running or couldn't run don't count.
    pub fn check_run_at(&self, repo: &str, commit: &str) -> Result<Option<CheckRun>> {
        let conn = self.conn();
        let mut q = conn.prepare(
            "SELECT json FROM check_runs WHERE repo = ?1 AND state IN ('passed', 'failed')
             ORDER BY started_at DESC",
        )?;
        let rows = q.query_map([repo], |r| r.get::<_, String>(0))?;
        for json in rows {
            let run: CheckRun = serde_json::from_str(&json?)?;
            if run.commit.as_deref() == Some(commit) {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    /// Check runs in progress.
    pub fn running_check_runs(&self) -> Result<Vec<CheckRun>> {
        let conn = self.conn();
        let mut q = conn.prepare("SELECT json FROM check_runs WHERE state = 'running'")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    /// Check runs left `running` by an actuo that stopped: `error`.
    pub fn end_stale_check_runs(&self, at: &str) -> Result<()> {
        let stale: Vec<String> = {
            let conn = self.conn();
            let mut q = conn.prepare("SELECT json FROM check_runs WHERE state = 'running'")?;
            let rows = q.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for json in stale {
            let mut run: CheckRun = serde_json::from_str(&json)?;
            run.state = CheckRunState::Error;
            run.error = Some("actuo stopped while it ran".into());
            run.ended_at = Some(at.to_string());
            self.save_check_run(&run)?;
        }
        Ok(())
    }

    /// Records an `agent/probe`: the agent's own model's options and each model's.
    pub fn set_agent_options(&self, agent: &str, probe: &AgentOptions) -> Result<()> {
        self.conn().execute(
            "INSERT INTO agent_options (agent, version, own_json, by_model_json, probed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(agent) DO UPDATE SET version = ?2, own_json = ?3,
                by_model_json = ?4, probed_at = ?5",
            params![
                agent,
                probe.version,
                serde_json::to_string(&probe.own)?,
                serde_json::to_string(&probe.by_model)?,
                probe.probed_at,
            ],
        )?;
        Ok(())
    }

    /// The last `agent/probe` of `agent`, if any.
    pub fn agent_options(&self, agent: &str) -> Result<Option<AgentOptions>> {
        let row = self
            .conn()
            .query_row(
                "SELECT version, own_json, by_model_json, probed_at FROM agent_options
                 WHERE agent = ?1",
                [agent],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((version, own, by_model, probed_at)) = row else {
            return Ok(None);
        };
        Ok(Some(AgentOptions {
            version,
            own: serde_json::from_str(&own)?,
            by_model: serde_json::from_str(&by_model)?,
            probed_at,
        }))
    }

    /// The turn a `checkpoint.reverted` since the last `turn.started` reverted to
    /// before, if any: the next prompt says so.
    pub fn revert_since_last_turn(&self, session: &SessionId) -> Result<Option<u32>> {
        let data: Option<String> = self
            .conn()
            .query_row(
                "SELECT data_json FROM events
                 WHERE session_id = ?1 AND type = 'checkpoint.reverted'
                   AND seq > (SELECT COALESCE(MAX(seq), 0) FROM events
                              WHERE session_id = ?1 AND type = 'turn.started')
                 ORDER BY seq DESC LIMIT 1",
                [&session.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match data {
            Some(d) => serde_json::from_str::<Value>(&d)?["toN"]
                .as_u64()
                .map(|n| n as u32 + 1),
            None => None,
        })
    }

    /// Tool call items of `turn` that aren't finished: their latest event isn't
    /// `item.completed`, or their status is still pending or in progress.
    pub fn unfinished_tool_calls(
        &self,
        session: &SessionId,
        turn: u32,
    ) -> Result<Vec<(ItemId, ToolCall)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT e.item_id, e.type, e.data_json FROM events e
             JOIN (SELECT item_id, MAX(seq) AS seq FROM events
                   WHERE session_id = ?1 AND turn = ?2 AND item_id IS NOT NULL
                     AND type IN ('item.started', 'item.updated', 'item.completed')
                   GROUP BY item_id) last
               ON e.item_id = last.item_id AND e.seq = last.seq
             WHERE e.session_id = ?1 ORDER BY e.seq",
        )?;
        let rows = stmt.query_map(params![session.0, turn], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (item_id, kind, data) = row?;
            let item: actuo_protocol::types::Item =
                serde_json::from_value(serde_json::from_str::<Value>(&data)?["item"].clone())?;
            if let actuo_protocol::types::Item::ToolCall(call) = item {
                let unfinished = kind != "item.completed"
                    || matches!(
                        call.status,
                        actuo_protocol::types::ToolStatus::Pending
                            | actuo_protocol::types::ToolStatus::InProgress
                    );
                if unfinished {
                    out.push((ItemId(item_id), call));
                }
            }
        }
        Ok(out)
    }

    /// The turn that started but hasn't completed, if any.
    pub fn running_turn(&self, session: &SessionId) -> Result<Option<u32>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT n FROM turns WHERE session_id = ?1 AND completed_at IS NULL
                 ORDER BY n DESC LIMIT 1",
                [&session.0],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn open_requests(&self, session: &SessionId) -> Result<Vec<PermissionRequest>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT request_id, turn, tool_call_json, options_json, opened_at FROM requests
             WHERE session_id = ?1 AND resolved_at IS NULL ORDER BY opened_at, request_id",
        )?;
        let rows = stmt.query_map([&session.0], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (request_id, turn, tool_call, options, opened_at) = row?;
            out.push(PermissionRequest {
                request_id: RequestId(request_id),
                turn,
                tool_call: serde_json::from_str::<ToolCall>(&tool_call)?,
                options: serde_json::from_str::<Vec<PermissionOption>>(&options)?,
                opened_at,
            });
        }
        Ok(out)
    }

    pub fn checkpoints(&self, session: &SessionId) -> Result<Vec<Checkpoint>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n, commit_sha, files_json, created_at FROM checkpoints
             WHERE session_id = ?1 ORDER BY n",
        )?;
        let rows = stmt.query_map([&session.0], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (n, commit, files, created_at) = row?;
            out.push(Checkpoint {
                n,
                commit,
                files: files
                    .map(|f| serde_json::from_str::<Vec<FileChange>>(&f))
                    .transpose()?
                    .unwrap_or_default(),
                created_at,
            });
        }
        Ok(out)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in migrations::MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// What an agent offers, from an `agent/probe`: the options for its own model and
/// for each model, without current values or `mode`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOptions {
    /// The adapter's pinned version when probed; a different one probes again.
    pub version: Option<String>,
    pub own: Vec<AgentConfigOption>,
    pub by_model: std::collections::BTreeMap<String, Vec<AgentConfigOption>>,
    pub probed_at: String,
}

/// The git ref of checkpoint `n` of a session, in the project repository.
pub fn checkpoint_ref(session: &SessionId, n: u32) -> String {
    format!("refs/actuo/checkpoints/{session}/{n}")
}

pub fn new_session_id() -> SessionId {
    SessionId(new_id())
}

thread_local! {
    /// Set while a store call runs through [`off_async`].
    static OFF_ASYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs a store call, marked as being off the async threads (call it inside
/// `spawn_blocking`). In debug builds, [`Store::append`] panics when called from
/// an async runtime thread without this mark.
pub fn off_async<T>(f: impl FnOnce() -> T) -> T {
    OFF_ASYNC.with(|m| m.set(true));
    let r = f();
    OFF_ASYNC.with(|m| m.set(false));
    r
}

fn debug_assert_not_async() {
    debug_assert!(
        tokio::runtime::Handle::try_current().is_err() || OFF_ASYNC.with(|m| m.get()),
        "a store write ran on an async thread; use spawn_blocking"
    );
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// `type` and `data` of an event body.
fn split_body(body: &EventBody) -> Result<(String, Value)> {
    let mut v = serde_json::to_value(body)?;
    let kind = v["type"].as_str().unwrap_or_default().to_string();
    let data = v.get_mut("data").map(Value::take).unwrap_or(Value::Null);
    Ok((kind, data))
}

/// A unit enum's serde name, e.g. `SessionState::Idle` → `idle`.
pub(crate) fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        other => panic!("not a unit enum: {other:?}"),
    }
}

pub(crate) fn parse_enum<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(Value::String(s.to_string()))?)
}

fn project_from_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: ProjectId(r.get(0)?),
        root: r.get(1)?,
        created_at: r.get(2)?,
        repo: None,
        last_check_run: None,
    })
}

const SESSION_SELECT: &str = "SELECT s.id, s.project_id, s.agent, s.agent_version, s.level,
    s.clean, s.cwd, s.worktree_path, s.branch, s.state, s.end_reason, s.title, s.head_seq,
    s.created_at, s.updated_at,
    (SELECT COUNT(*) FROM turns t WHERE t.session_id = s.id), s.base,
    s.sandbox_json, s.web_tools, s.telemetry
    FROM sessions s";

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<Result<SessionSummary>> {
    let level: String = r.get(4)?;
    let state: String = r.get(9)?;
    Ok((|| {
        Ok(SessionSummary {
            id: SessionId(r.get(0)?),
            project_id: ProjectId(r.get(1)?),
            agent: r.get(2)?,
            agent_version: r.get(3)?,
            level: parse_enum(&level)?,
            clean: r.get(5)?,
            cwd: r.get(6)?,
            worktree_path: r.get(7)?,
            branch: r.get(8)?,
            base: r.get(16)?,
            sandbox: r
                .get::<_, Option<String>>(17)?
                .map(|j| serde_json::from_str(&j))
                .transpose()?,
            web_tools: r.get(18)?,
            telemetry: r.get(19)?,
            state: parse_enum(&state)?,
            end_reason: r.get(10)?,
            title: r.get(11)?,
            head_seq: r.get(12)?,
            created_at: r.get(13)?,
            updated_at: r.get(14)?,
            turn_count: r.get(15)?,
        })
    })())
}

//! Keeps the projection tables in step with the event log. Runs inside the
//! transaction that inserts the event.
//!
//! Order the session task must keep: `checkpoint.created` for `n` before
//! `turn.completed` for turn `n`, because the turn's files are written onto that
//! checkpoint.

use super::{Result, enum_str};
use actuo_protocol::events::{Event, EventBody};
use actuo_protocol::types::{RequestOutcome, SessionState};
use rusqlite::{Transaction, params};

pub(super) fn apply(tx: &Transaction<'_>, event: &Event) -> Result<()> {
    let sid = &event.session_id.0;
    match &event.body {
        EventBody::SessionState(d) => {
            let end_reason = match d.state {
                SessionState::Ended => d.reason.clone(),
                _ => None,
            };
            tx.execute(
                "UPDATE sessions SET state = ?2, end_reason = ?3 WHERE id = ?1",
                params![sid, enum_str(&d.state), end_reason],
            )?;
        }
        EventBody::SessionAgent(d) => {
            tx.execute(
                "UPDATE sessions SET acp_session_id = ?2, can_resume = ?3, can_load = ?4,
                 config_options_json = ?5 WHERE id = ?1",
                params![
                    sid,
                    d.agent_session_id,
                    d.can_resume,
                    d.can_load,
                    serde_json::to_string(&d.config_options)?
                ],
            )?;
        }
        EventBody::SessionCommands(d) => {
            tx.execute(
                "UPDATE sessions SET commands_json = ?2 WHERE id = ?1",
                params![sid, serde_json::to_string(&d.commands)?],
            )?;
        }
        EventBody::TurnStarted(d) => {
            let n = event.turn.expect("turn.started carries its turn");
            tx.execute(
                "INSERT INTO turns (session_id, n, text, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![sid, n, d.text, event.ts],
            )?;
        }
        EventBody::TurnCompleted(d) => {
            let n = event.turn.expect("turn.completed carries its turn");
            tx.execute(
                "UPDATE turns SET completed_at = ?3, stop_reason = ?4, tokens_json = ?5,
                    cost_amount = ?6, cost_currency = ?7
                 WHERE session_id = ?1 AND n = ?2",
                params![
                    sid,
                    n,
                    event.ts,
                    enum_str(&d.stop_reason),
                    d.tokens.as_ref().map(serde_json::to_string).transpose()?,
                    d.cost.as_ref().map(|c| c.amount),
                    d.cost.as_ref().map(|c| c.currency.clone()),
                ],
            )?;
            tx.execute(
                "UPDATE checkpoints SET files_json = ?3 WHERE session_id = ?1 AND n = ?2",
                params![sid, n, serde_json::to_string(&d.files)?],
            )?;
        }
        EventBody::RequestOpened(d) => {
            let turn = event.turn.expect("request.opened carries its turn");
            tx.execute(
                "INSERT INTO requests (session_id, request_id, turn, item_id, tool_call_json,
                    options_json, opened_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    sid,
                    d.request_id.0,
                    turn,
                    event.item_id.as_ref().map(|i| &i.0),
                    serde_json::to_string(&d.tool_call)?,
                    serde_json::to_string(&d.options)?,
                    event.ts,
                ],
            )?;
        }
        EventBody::RequestResolved(d) => {
            let outcome = match d.outcome {
                RequestOutcome::Selected => serde_json::json!({
                    "outcome": "selected", "optionId": d.option_id, "optionKind": d.option_kind,
                }),
                RequestOutcome::Cancelled => serde_json::json!({"outcome": "cancelled"}),
            };
            tx.execute(
                "UPDATE requests SET resolved_at = ?3, outcome_json = ?4, resolved_by = ?5
                 WHERE session_id = ?1 AND request_id = ?2",
                params![
                    sid,
                    d.request_id.0,
                    event.ts,
                    outcome.to_string(),
                    enum_str(&d.by)
                ],
            )?;
        }
        EventBody::CheckpointCreated(d) => {
            tx.execute(
                "INSERT OR REPLACE INTO checkpoints (session_id, n, commit_sha, ref, files_json,
                    created_at)
                 VALUES (?1, ?2, ?3, ?4, CASE WHEN ?2 = 0 THEN '[]' END, ?5)",
                params![
                    sid,
                    d.n,
                    d.commit,
                    crate::store::checkpoint_ref(&event.session_id, d.n),
                    event.ts
                ],
            )?;
        }
        EventBody::AgentUpdate(d) => {
            // ACP's session_info_update carries the session title.
            if d.kind == "session_info_update"
                && let Some(title) = d.data.get("title").and_then(|t| t.as_str())
            {
                tx.execute(
                    "UPDATE sessions SET title = ?2 WHERE id = ?1",
                    params![sid, title],
                )?;
            }
        }
        EventBody::ItemStarted(_)
        | EventBody::ItemUpdated(_)
        | EventBody::ItemCompleted(_)
        | EventBody::ContentDelta(_)
        | EventBody::UsageUpdated(_)
        | EventBody::CheckpointReverted(_)
        | EventBody::RunStep(_)
        | EventBody::Error(_) => {}
    }
    Ok(())
}

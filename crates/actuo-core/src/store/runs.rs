//! Runs of the issue-to-PR lane and their steps. A run is driven by one lane task,
//! which loads its [`RunRow`], changes it and saves it back.

use super::{Result, Store, StoreError, enum_str, new_id, parse_enum};
use crate::time::now;
use actuo_protocol::ids::{RunId, SessionId};
use actuo_protocol::runs::{
    ChangeVia, Diagnosis, Run, RunDetail, RunState, RunStateChange, RunStep, RunStepKind,
    RunStepRecord, StartedVia,
};
use actuo_protocol::types::{ConfigValue, Level};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;
use std::collections::BTreeMap;

/// A run to queue.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub repo: String,
    pub issue: u64,
    pub issue_url: String,
    pub title: Option<String>,
    pub agent: String,
    pub level: Level,
    pub config: BTreeMap<String, ConfigValue>,
    /// The lane and repo limits the run keeps for its whole life.
    pub limits: Value,
    pub branch: String,
    pub started_by: String,
    pub started_via: StartedVia,
    pub previous_run_id: Option<RunId>,
}

/// Who caused a change of a run's state, as recorded in its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedBy {
    Lane,
    /// A client by its `client.name`, or one that isn't known (`None`).
    Client(Option<String>),
}

impl ChangedBy {
    fn column(&self) -> Option<&str> {
        match self {
            ChangedBy::Lane => Some("lane"),
            ChangedBy::Client(name) => name.as_deref(),
        }
    }

    fn via(&self) -> ChangeVia {
        match self {
            ChangedBy::Lane => ChangeVia::Lane,
            ChangedBy::Client(_) => ChangeVia::Client,
        }
    }
}

/// Every column of a run, including what the protocol doesn't show.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub id: RunId,
    pub repo: String,
    pub issue: u64,
    pub issue_url: String,
    pub title: Option<String>,
    pub agent: String,
    pub level: Level,
    pub config: BTreeMap<String, ConfigValue>,
    pub limits: Value,
    pub state: RunState,
    pub reason: Option<String>,
    /// [`Run::reason_text`].
    pub reason_text: Option<String>,
    pub step: RunStep,
    pub session_id: Option<SessionId>,
    pub branch: String,
    pub base: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub status_comment_id: Option<u64>,
    pub started_by: String,
    pub started_via: StartedVia,
    pub previous_run_id: Option<RunId>,
    pub check_rounds: u32,
    pub reject_turns: u32,
    pub restart_turns: u32,
    pub last_prompt_at: Option<String>,
    pub pending_comments: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub ended_at: Option<String>,
}

impl RunRow {
    pub fn summary(&self) -> Run {
        Run {
            id: self.id.clone(),
            repo: self.repo.clone(),
            issue: self.issue,
            issue_url: self.issue_url.clone(),
            title: self.title.clone(),
            agent: self.agent.clone(),
            level: self.level,
            state: self.state,
            reason: self.reason.clone(),
            reason_text: self.reason_text.clone(),
            step: self.step,
            session_id: self.session_id.clone(),
            branch: self.branch.clone(),
            base: self.base.clone(),
            pr_url: self.pr_url.clone(),
            started_by: self.started_by.clone(),
            started_via: Some(self.started_via),
            check_rounds: self.check_rounds,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            ended_at: self.ended_at.clone(),
        }
    }
}

const RUN_SELECT: &str = "SELECT id, repo, issue, issue_url, title, agent, level, config_json,
    limits_json, state, reason, step, session_id, branch, base, pr_number, pr_url,
    status_comment_id, started_by, previous_run_id, check_rounds, reject_turns, restart_turns,
    last_prompt_at, pending_comments_json, created_at, updated_at, ended_at, started_via,
    reason_text
    FROM runs";

fn run_from_row(r: &Row<'_>) -> rusqlite::Result<Result<RunRow>> {
    let level: String = r.get(6)?;
    let config: String = r.get(7)?;
    let limits: String = r.get(8)?;
    let state: String = r.get(9)?;
    let step: String = r.get(11)?;
    let pending: Option<String> = r.get(24)?;
    let session: Option<String> = r.get(12)?;
    let previous: Option<String> = r.get(19)?;
    let (id, repo, issue, issue_url, title, agent): (
        String,
        String,
        u64,
        String,
        Option<String>,
        String,
    ) = (
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    );
    let (reason, branch, base, pr_number, pr_url, status_comment_id, started_by) = (
        r.get(10)?,
        r.get(13)?,
        r.get(14)?,
        r.get(15)?,
        r.get(16)?,
        r.get(17)?,
        r.get(18)?,
    );
    let (check_rounds, reject_turns, restart_turns, last_prompt_at) =
        (r.get(20)?, r.get(21)?, r.get(22)?, r.get(23)?);
    let (created_at, updated_at, ended_at) = (r.get(25)?, r.get(26)?, r.get(27)?);
    let started_via: String = r.get(28)?;
    let reason_text: Option<String> = r.get(29)?;
    Ok((|| {
        Ok(RunRow {
            id: RunId(id),
            repo,
            issue,
            issue_url,
            title,
            agent,
            level: parse_enum(&level)?,
            config: serde_json::from_str(&config)?,
            limits: serde_json::from_str(&limits)?,
            state: parse_enum(&state)?,
            reason,
            reason_text,
            step: parse_enum(&step)?,
            session_id: session.map(SessionId),
            branch,
            base,
            pr_number,
            pr_url,
            status_comment_id,
            started_by,
            started_via: parse_enum(&started_via)?,
            previous_run_id: previous.map(RunId),
            check_rounds,
            reject_turns,
            restart_turns,
            last_prompt_at,
            pending_comments: pending.map(|p| serde_json::from_str(&p)).transpose()?,
            created_at,
            updated_at,
            ended_at,
        })
    })())
}

impl Store {
    /// Queues a run. Fails with [`StoreError::Conflict`] if the issue already has a
    /// run that hasn't ended.
    pub fn create_run(&self, new: NewRun) -> Result<RunRow> {
        let id = RunId(new_id());
        let ts = now();
        let r = self.conn().execute(
            "INSERT INTO runs (id, repo, issue, issue_url, title, agent, level, config_json,
                limits_json, state, step, branch, started_by, previous_run_id, created_at,
                updated_at, started_via)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, ?16)",
            params![
                id.0,
                new.repo,
                new.issue,
                new.issue_url,
                new.title,
                new.agent,
                enum_str(&new.level),
                serde_json::to_string(&new.config)?,
                serde_json::to_string(&new.limits)?,
                enum_str(&RunState::Queued),
                enum_str(&RunStep::Prepare),
                new.branch,
                new.started_by,
                new.previous_run_id.as_ref().map(|r| &r.0),
                ts,
                enum_str(&new.started_via),
            ],
        );
        match r {
            Ok(_) => {
                let via = match new.started_via {
                    StartedVia::Label => ChangeVia::Label,
                    StartedVia::Client => ChangeVia::Client,
                };
                let by = Some(new.started_by.as_str());
                self.record_state(&id, RunState::Queued, None, by, via, &ts)?;
                self.run(&id)
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Conflict(format!(
                    "{}#{} already has a run that hasn't ended",
                    new.repo, new.issue
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn run(&self, id: &RunId) -> Result<RunRow> {
        self.conn()
            .query_row(
                &format!("{RUN_SELECT} WHERE id = ?1"),
                [&id.0],
                run_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("run {id}")))?
    }

    /// Writes back every column a run's task changes; `updated_at` is set, and
    /// `ended_at` when the state is one that ends the run. A changed state or
    /// reason is added to the run's history, caused `by`.
    pub fn save_run(&self, run: &RunRow, by: &ChangedBy) -> Result<RunRow> {
        let ts = now();
        let before = self.run(&run.id)?;
        let ended = run
            .state
            .ended()
            .then(|| run.ended_at.clone().unwrap_or(ts.clone()));
        let n = self.conn().execute(
            "UPDATE runs SET title = ?2, state = ?3, reason = ?4, step = ?5, session_id = ?6,
                base = ?7, pr_number = ?8, pr_url = ?9, status_comment_id = ?10,
                check_rounds = ?11, reject_turns = ?12, restart_turns = ?13,
                last_prompt_at = ?14, pending_comments_json = ?15, updated_at = ?16,
                ended_at = ?17, reason_text = ?18
             WHERE id = ?1",
            params![
                run.id.0,
                run.title,
                enum_str(&run.state),
                run.reason,
                enum_str(&run.step),
                run.session_id.as_ref().map(|s| &s.0),
                run.base,
                run.pr_number,
                run.pr_url,
                run.status_comment_id,
                run.check_rounds,
                run.reject_turns,
                run.restart_turns,
                run.last_prompt_at,
                run.pending_comments
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                ts,
                ended,
                run.reason_text,
            ],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("run {}", run.id)));
        }
        if (before.state, &before.reason) != (run.state, &run.reason) {
            let reason = run.reason.as_deref();
            self.record_state(&run.id, run.state, reason, by.column(), by.via(), &ts)?;
        }
        self.run(&run.id)
    }

    fn record_state(
        &self,
        run: &RunId,
        state: RunState,
        reason: Option<&str>,
        by: Option<&str>,
        via: ChangeVia,
        at: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO run_states (run_id, n, state, reason, by, via, at)
             VALUES (?1, (SELECT COALESCE(MAX(n), 0) + 1 FROM run_states WHERE run_id = ?1),
                     ?2, ?3, ?4, ?5, ?6)",
            params![run.0, enum_str(&state), reason, by, enum_str(&via), at],
        )?;
        Ok(())
    }

    /// A run's state changes, oldest first.
    pub fn run_states(&self, run: &RunId) -> Result<Vec<RunStateChange>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n, state, reason, by, at, via FROM run_states WHERE run_id = ?1 ORDER BY n",
        )?;
        let rows = stmt.query_map([&run.0], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (n, state, reason, by, at, via) = row?;
            out.push(RunStateChange {
                n,
                state: parse_enum(&state)?,
                reason,
                by,
                via: via.as_deref().map(parse_enum).transpose()?,
                at,
            });
        }
        Ok(out)
    }

    /// Runs, newest first, optionally of one repo and in one state.
    pub fn runs(&self, repo: Option<&str>, state: Option<RunState>) -> Result<Vec<RunRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "{RUN_SELECT} WHERE (?1 IS NULL OR repo = ?1) AND (?2 IS NULL OR state = ?2)
             ORDER BY created_at DESC, id DESC"
        ))?;
        let state = state.map(|s| enum_str(&s));
        let rows = stmt.query_map(params![repo, state], run_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Runs in any of `states`, oldest first.
    pub fn runs_in_states(&self, states: &[RunState]) -> Result<Vec<RunRow>> {
        let mut all = self.runs(None, None)?;
        all.retain(|r| states.contains(&r.state));
        all.reverse();
        Ok(all)
    }

    /// The run working in a session that hasn't ended, if any.
    pub fn open_run_for_session(&self, session: &SessionId) -> Result<Option<RunRow>> {
        Ok(self
            .runs(None, None)?
            .into_iter()
            .find(|r| r.session_id.as_ref() == Some(session) && !r.state.ended()))
    }

    /// The issue's newest run on this actuo, ended or not.
    pub fn last_run(&self, repo: &str, issue: u64) -> Result<Option<RunRow>> {
        self.conn()
            .query_row(
                &format!(
                    "{RUN_SELECT} WHERE repo = ?1 AND issue = ?2
                     ORDER BY created_at DESC, id DESC LIMIT 1"
                ),
                params![repo, issue],
                run_from_row,
            )
            .optional()?
            .transpose()
    }

    /// Starts a step; returns its number.
    pub fn add_step(&self, run: &RunId, kind: RunStepKind, command: Option<&str>) -> Result<u32> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n: u32 = tx.query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM run_steps WHERE run_id = ?1",
            [&run.0],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO run_steps (run_id, n, kind, command, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run.0, n, enum_str(&kind), command, now()],
        )?;
        tx.commit()?;
        Ok(n)
    }

    pub fn finish_step(
        &self,
        run: &RunId,
        n: u32,
        exit_code: Option<i32>,
        output_tail: Option<&str>,
        log_path: Option<&str>,
    ) -> Result<RunStepRecord> {
        self.conn().execute(
            "UPDATE run_steps SET exit_code = ?3, output_tail = ?4, log_path = ?5, ended_at = ?6
             WHERE run_id = ?1 AND n = ?2",
            params![run.0, n, exit_code, output_tail, log_path, now()],
        )?;
        self.steps(run)?
            .into_iter()
            .find(|s| s.n == n)
            .ok_or_else(|| StoreError::NotFound(format!("step {n} of run {run}")))
    }

    /// Why a step failed (`lane/diagnose.rs`).
    pub fn set_step_diagnosis(&self, run: &RunId, n: u32, d: &Diagnosis) -> Result<()> {
        self.conn().execute(
            "UPDATE run_steps SET diagnosis_json = ?3 WHERE run_id = ?1 AND n = ?2",
            params![run.0, n, serde_json::to_string(d)?],
        )?;
        Ok(())
    }

    pub fn steps(&self, run: &RunId) -> Result<Vec<RunStepRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n, kind, command, exit_code, output_tail, started_at, ended_at,
                    diagnosis_json
             FROM run_steps WHERE run_id = ?1 ORDER BY n",
        )?;
        let rows = stmt.query_map([&run.0], |r| {
            let kind: String = r.get(1)?;
            let diagnosis: Option<String> = r.get(7)?;
            Ok((
                kind,
                diagnosis,
                RunStepRecord {
                    n: r.get(0)?,
                    kind: RunStepKind::Setup,
                    command: r.get(2)?,
                    exit_code: r.get(3)?,
                    output_tail: r.get(4)?,
                    diagnosis: None,
                    started_at: r.get(5)?,
                    ended_at: r.get(6)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (kind, diagnosis, mut step) = r?;
            step.kind = parse_enum(&kind)?;
            step.diagnosis = diagnosis.map(|d| serde_json::from_str(&d)).transpose()?;
            out.push(step);
        }
        Ok(out)
    }

    pub fn run_detail(&self, id: &RunId) -> Result<RunDetail> {
        Ok(RunDetail {
            run: self.run(id)?.summary(),
            steps: self.steps(id)?,
            states: self.run_states(id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_run(issue: u64) -> NewRun {
        NewRun {
            repo: "o/r".into(),
            issue,
            issue_url: format!("https://github.com/o/r/issues/{issue}"),
            title: Some("Fix it".into()),
            agent: "claude".into(),
            level: Level::Edits,
            config: BTreeMap::new(),
            limits: json!({"max_check_retries": 2}),
            branch: format!("actuo/issue-{issue}"),
            started_by: "me".into(),
            started_via: StartedVia::Label,
            previous_run_id: None,
        }
    }

    #[test]
    fn one_open_run_per_issue() {
        let store = Store::open_in_memory().unwrap();
        let mut run = store.create_run(new_run(1)).unwrap();
        assert_eq!(run.state, RunState::Queued);
        assert_eq!(run.step, RunStep::Prepare);
        assert!(matches!(
            store.create_run(new_run(1)),
            Err(StoreError::Conflict(_))
        ));
        store.create_run(new_run(2)).unwrap();

        run.state = RunState::PrOpened;
        run.pr_url = Some("https://github.com/o/r/pull/3".into());
        run.pr_number = Some(3);
        run.status_comment_id = Some(77);
        run.pending_comments = Some(json!([{"author": "me"}]));
        let saved = store.save_run(&run, &ChangedBy::Lane).unwrap();
        assert!(saved.ended_at.is_some());
        assert_eq!(saved.pr_number, Some(3));
        assert_eq!(saved.status_comment_id, Some(77));
        assert_eq!(saved.pending_comments, Some(json!([{"author": "me"}])));
        assert_eq!(saved.limits, json!({"max_check_retries": 2}));

        // Ended: a new run of the same issue may start.
        let mut again = new_run(1);
        again.previous_run_id = Some(run.id.clone());
        let second = store.create_run(again).unwrap();
        assert_eq!(store.last_run("o/r", 1).unwrap().unwrap().id, second.id);
        assert_eq!(second.previous_run_id, Some(run.id.clone()));

        assert_eq!(store.runs(None, None).unwrap().len(), 3);
        assert_eq!(
            store
                .runs(Some("o/r"), Some(RunState::PrOpened))
                .unwrap()
                .len(),
            1
        );
        let queued = store.runs_in_states(&[RunState::Queued]).unwrap();
        assert_eq!(queued.len(), 2);
        assert!(queued[0].created_at <= queued[1].created_at);
        assert!(store.last_run("o/r", 9).unwrap().is_none());
        assert!(matches!(
            store.run(&RunId::from("nope")),
            Err(StoreError::NotFound(_))
        ));
    }

    /// A state or reason that changes is recorded with who caused it; anything
    /// else saved isn't.
    #[test]
    fn state_history() {
        let store = Store::open_in_memory().unwrap();
        let mut run = store.create_run(new_run(1)).unwrap();
        run.state = RunState::Running;
        run = store.save_run(&run, &ChangedBy::Lane).unwrap();
        run.step = RunStep::Checks;
        run.check_rounds = 1;
        run = store
            .save_run(&run, &ChangedBy::Client(Some("script".into())))
            .unwrap();
        run.state = RunState::Waiting;
        run.reason = Some("request".into());
        run = store.save_run(&run, &ChangedBy::Lane).unwrap();
        run.reason = Some("refusal".into());
        run = store.save_run(&run, &ChangedBy::Lane).unwrap();
        // The reason in words is saved and read back, and adds no history.
        run.reason_text = Some("The agent refused to go on.".into());
        run = store.save_run(&run, &ChangedBy::Lane).unwrap();
        assert_eq!(
            run.summary().reason_text.as_deref(),
            Some("The agent refused to go on.")
        );
        run.state = RunState::TakenOver;
        run.reason_text = None;
        run.reason = Some("the turn was interrupted".into());
        run = store.save_run(&run, &ChangedBy::Client(None)).unwrap();
        run.state = RunState::Cancelled;
        run.reason = Some("cancelled".into());
        store
            .save_run(&run, &ChangedBy::Client(Some("script".into())))
            .unwrap();

        let states = store.run_detail(&run.id).unwrap().states;
        let got: Vec<_> = states
            .iter()
            .map(|s| (s.n, s.state, s.reason.as_deref(), s.by.as_deref(), s.via))
            .collect();
        let (label, lane, client) = (
            Some(ChangeVia::Label),
            Some(ChangeVia::Lane),
            Some(ChangeVia::Client),
        );
        assert_eq!(
            got,
            [
                (1, RunState::Queued, None, Some("me"), label),
                (2, RunState::Running, None, Some("lane"), lane),
                (3, RunState::Waiting, Some("request"), Some("lane"), lane),
                (4, RunState::Waiting, Some("refusal"), Some("lane"), lane),
                (
                    5,
                    RunState::TakenOver,
                    Some("the turn was interrupted"),
                    None,
                    client
                ),
                (
                    6,
                    RunState::Cancelled,
                    Some("cancelled"),
                    Some("script"),
                    client
                ),
            ]
        );
        assert!(states.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn steps_in_order() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run(new_run(1)).unwrap();
        let n1 = store
            .add_step(&run.id, RunStepKind::Setup, Some("pnpm install"))
            .unwrap();
        let n2 = store
            .add_step(&run.id, RunStepKind::Check, Some("pnpm test"))
            .unwrap();
        assert_eq!((n1, n2), (1, 2));
        let done = store
            .finish_step(
                &run.id,
                n2,
                Some(1),
                Some("1 failed"),
                Some("/l/2-check.log"),
            )
            .unwrap();
        assert_eq!(done.exit_code, Some(1));
        assert!(done.ended_at.is_some());
        let detail = store.run_detail(&run.id).unwrap();
        assert_eq!(detail.steps.len(), 2);
        assert_eq!(detail.steps[0].kind, RunStepKind::Setup);
        assert_eq!(detail.steps[0].ended_at, None);
        assert_eq!(detail.run.title.as_deref(), Some("Fix it"));
    }
}

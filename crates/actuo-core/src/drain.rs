//! `actuo/drain`: getting ready for a restart (an update). While a client holds
//! actuo, the lane starts no run, check run or resumed run; the call answers
//! once nothing is in progress, or at its limit with what still is. The hold ends
//! with the connection, so an update stopped halfway leaves actuo as it was.

use crate::session::Host;
use actuo_protocol::methods::ActuoDrainResult;
use actuo_protocol::runs::RunState;
use actuo_protocol::types::SessionState;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// How often what's in progress is looked at again.
const EVERY: Duration = Duration::from_secs(1);

/// Actuo held for a restart while one of these lives.
pub struct Hold(Arc<AtomicUsize>);

impl Hold {
    pub(crate) fn new(holders: &Arc<AtomicUsize>) -> Self {
        holders.fetch_add(1, Ordering::SeqCst);
        Self(holders.clone())
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Waits until nothing is in progress, `limit` passes, or `closed` turns true (the
/// connection ended).
pub async fn drain(
    host: &Host,
    limit: Duration,
    mut closed: watch::Receiver<bool>,
) -> ActuoDrainResult {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let in_progress = in_progress(host).await;
        if in_progress.is_empty() || tokio::time::Instant::now() >= deadline {
            return ActuoDrainResult { in_progress };
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + EVERY)) => {}
            _ = closed.wait_for(|c| *c) => return ActuoDrainResult { in_progress },
        }
    }
}

/// Lane runs a task drives that are running or checking, check runs, and turns in
/// sessions that aren't those runs'.
pub async fn in_progress(host: &Host) -> Vec<String> {
    let mut out = Vec::new();
    let mut run_sessions = Vec::new();
    if let Ok(lane) = host.lane() {
        let runs = host
            .db(|st| st.runs_in_states(&[RunState::Running, RunState::Checking]))
            .await
            .unwrap_or_default();
        for r in runs.into_iter().filter(|r| lane.drives(&r.id)) {
            out.push(format!("run {} of {} (issue #{})", r.id, r.repo, r.issue));
            run_sessions.extend(r.session_id);
        }
        let checks = host
            .db(|st| st.running_check_runs())
            .await
            .unwrap_or_default();
        for c in checks {
            out.push(format!("check run {} of {}", c.id.0, c.repo));
        }
    }
    let sessions = host.db(|st| st.sessions(None)).await.unwrap_or_default();
    for s in sessions {
        if s.state == SessionState::Running && !run_sessions.contains(&s.id) {
            out.push(format!("a turn in session {}", s.id));
        }
    }
    out
}

//! A session's events in order, from a given `seq`: live ones from the store's
//! broadcast, and stored ones when the broadcast ran ahead (`Lagged`).

use crate::session::{Host, HostResult};
use actuo_protocol::events::Event;
use actuo_protocol::ids::SessionId;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct Watch {
    host: Arc<Host>,
    session: SessionId,
    live: broadcast::Receiver<Arc<Event>>,
    last: u64,
    backlog: VecDeque<Arc<Event>>,
    /// Read the store before the broadcast: at the start, and after a gap.
    stale: bool,
}

impl Watch {
    /// Events after the session's current head.
    pub async fn from_head(host: &Arc<Host>, session: &SessionId) -> HostResult<Self> {
        let live = host.store().subscribe();
        let sid = session.clone();
        let head = host.db(move |s| s.session(&sid)).await?.head_seq;
        Ok(Self::new(host, session, live, head))
    }

    /// Events after `seq` (`0`: all of them).
    pub fn from_seq(host: &Arc<Host>, session: &SessionId, seq: u64) -> Self {
        Self::new(host, session, host.store().subscribe(), seq)
    }

    fn new(
        host: &Arc<Host>,
        session: &SessionId,
        live: broadcast::Receiver<Arc<Event>>,
        last: u64,
    ) -> Self {
        Self {
            host: host.clone(),
            session: session.clone(),
            live,
            last,
            backlog: VecDeque::new(),
            stale: true,
        }
    }

    /// The next event; `None` when the store is gone.
    pub async fn next(&mut self) -> Option<Arc<Event>> {
        loop {
            if self.backlog.is_empty() && self.stale {
                self.stale = false;
                self.catch_up().await;
            }
            if let Some(e) = self.backlog.pop_front() {
                self.last = e.seq;
                return Some(e);
            }
            match self.live.recv().await {
                Ok(e) if e.session_id == self.session && e.seq > self.last => {
                    if e.seq == self.last + 1 {
                        self.last = e.seq;
                        return Some(e);
                    }
                    // A gap: read it from the store.
                    self.stale = true;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => self.stale = true,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    async fn catch_up(&mut self) {
        let (sid, after) = (self.session.clone(), self.last);
        if let Ok(page) = self
            .host
            .db(move |s| s.events_after(&sid, after, 500))
            .await
        {
            self.backlog.extend(page.into_iter().map(Arc::new));
        }
    }
}

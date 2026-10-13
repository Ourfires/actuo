//! A turn that streams thousands of text chunks, one stored row each, on a
//! single-threaded runtime: the store writes must not hold up the async thread.

mod common;

use actuo_core::agent::acp::AcpBackend;
use actuo_core::config::Paths;
use actuo_core::session::Host;
use actuo_core::store::Store;
use actuo_protocol::events::EventBody;
use actuo_protocol::ids::ProjectId;
use actuo_protocol::methods::SessionStartParams;
use actuo_protocol::types::{Item, StopReason};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const CHUNKS: usize = 3000;

#[tokio::test] // current_thread: one async thread for everything
async fn thousands_of_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = common::git_repo(&root);
    let steps: Vec<_> = (0..CHUNKS)
        .map(|i| json!({"message": format!("w{i} ")}))
        .collect();
    let agents = common::fake_agents(
        &root,
        &json!({"modes": [{"id": "ask", "name": "Ask"}], "turns": [{"match": "many", "steps": steps}]}),
    );
    let paths = Paths::new(root.join("home"));
    paths.create().unwrap();
    let store = Arc::new(Store::open(&paths.db()).unwrap());
    let host = Host::new(
        store.clone(),
        paths,
        agents,
        Default::default(),
        Arc::new(AcpBackend),
    );

    // A ticker on the same thread: if a store write blocked it, a tick would be late.
    let worst_gap_ms = Arc::new(AtomicU64::new(0));
    let ticker = {
        let worst = worst_gap_ms.clone();
        tokio::spawn(async move {
            let mut last = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let gap = last.elapsed().as_millis() as u64;
                worst.fetch_max(gap, Ordering::SeqCst);
                last = Instant::now();
            }
        })
    };

    let mut live = store.subscribe();
    let project = host.open_project(repo.to_str().unwrap()).await.unwrap();
    let started = Instant::now();
    let sid = host
        .start_session(
            SessionStartParams {
                project_id: ProjectId(project.id.0),
                agent: Some("fake".into()),
                worktree: true,
                base: None,
                branch: None,
                sandbox: None,
                web_tools: true,
                telemetry: true,
                level: Default::default(),
                clean: false,
                config: Default::default(),
                prompt: Some("many chunks".into()),
            },
            None,
        )
        .await
        .unwrap();

    let mut deltas = 0;
    let mut turn_started = None;
    let turn_done = loop {
        let e = tokio::time::timeout(Duration::from_secs(60), live.recv())
            .await
            .expect("events keep coming")
            .unwrap();
        if e.session_id != sid {
            continue;
        }
        match &e.body {
            EventBody::TurnStarted(_) => turn_started = Some(Instant::now()),
            EventBody::ContentDelta(_) => deltas += 1,
            EventBody::TurnCompleted(d) => break d.clone(),
            _ => {}
        }
    };
    let turn_time = turn_started.unwrap().elapsed();
    ticker.abort();

    assert_eq!(turn_done.stop_reason, StopReason::EndTurn);
    assert_eq!(deltas, CHUNKS);

    // Every chunk is a stored row, in order, with no gaps in seq.
    let all = store.events_after(&sid, 0, 100_000).unwrap();
    let seqs: Vec<u64> = all.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=all.len() as u64).collect::<Vec<_>>());
    let stored_deltas = all
        .iter()
        .filter(|e| matches!(e.body, EventBody::ContentDelta(_)))
        .count();
    assert_eq!(stored_deltas, CHUNKS);
    let text = all
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ItemCompleted(d) => match &d.item {
                Item::Message(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(text.starts_with("w0 w1 w2 ") && text.ends_with(&format!("w{} ", CHUNKS - 1)));

    let worst = worst_gap_ms.load(Ordering::SeqCst);
    eprintln!(
        "{CHUNKS} chunks: turn {:.2} s ({:.0} rows/s), session start to end {:.2} s, worst ticker gap {worst} ms",
        turn_time.as_secs_f64(),
        CHUNKS as f64 / turn_time.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    assert!(worst < 100, "the async thread was held for {worst} ms");
    host.stop(&sid).await.unwrap();
}

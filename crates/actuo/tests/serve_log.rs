//! `actuo serve`'s log lines start with a timestamp (serve.log under launchd has
//! none otherwise); under systemd journald stamps them, so they don't.

mod common;

use common::Env;
use serde_json::json;

fn first_line(env: &Env) -> String {
    env.serve_log()
        .lines()
        .find(|l| l.contains("actuo listening on"))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn log_lines_have_a_timestamp() {
    let mut env = Env::new(&json!({"turns": []}));
    env.env.retain(|(k, _)| k != "JOURNAL_STREAM");
    env.start_actuo();
    env.stop_actuo(false);
    let line = first_line(&env);
    let (at, rest) = line.split_once(' ').expect("a line");
    assert_eq!(
        rest,
        format!("actuo listening on {}", env.socket().display())
    );
    assert_eq!(at.len(), "2026-10-03T10:04:12.345Z".len(), "{line}");
    assert!(at.starts_with("20") && at.ends_with('Z'), "{line}");
    let stopping = env
        .serve_log()
        .lines()
        .find(|l| l.ends_with("stopping: letting go of the agents"))
        .unwrap_or_default()
        .to_string();
    assert!(stopping.starts_with("20"), "{stopping}");
}

#[test]
fn not_under_journald() {
    let mut env = Env::new(&json!({"turns": []}));
    env.env.push(("JOURNAL_STREAM".into(), "8:12345".into()));
    env.start_actuo();
    env.stop_actuo(false);
    assert_eq!(
        first_line(&env),
        format!("actuo listening on {}", env.socket().display())
    );
}

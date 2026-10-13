//! The lane across actuo restarts: stopped cleanly or with `kill -9`, a run goes on
//! from its step and ends with one PR and only the PR label.

mod common;

use common::lane::LaneEnv;
use serde_json::{Value, json};

const REPO: &str = "o/r";

fn script(turns: Value) -> Value {
    json!({
        "resume": true,
        "loadSession": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": turns
    })
}

fn state_is(lane: &LaneEnv, n: u64, state: &str) -> bool {
    lane.run_of(REPO, n).is_some_and(|r| r[2] == state)
}

/// `kill -9` while the agent works: at the next start the lane sends one "actuo
/// restarted" turn and goes on to one PR.
#[test]
fn kill_during_a_turn() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "Actuo restarted", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "issue #1 in", "steps": [{"sleepMs": 5000}, {"writeFile": {"path": "fixed.txt", "content": "late\n"}}]},
            {"match": "Review your changes", "steps": []}
        ])),
        &[(REPO, "checks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Slow", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the agent's first turn", 30, |l| {
        l.prompts().iter().any(|p| p.contains("issue #1 in"))
    });
    lane.env.stop_actuo(true);
    lane.env.start_actuo();
    lane.wait("the PR", 60, |l| state_is(l, 1, "pr_opened"));

    assert_eq!(lane.prs_from(REPO, 1), 1);
    assert_eq!(lane.runs_of(REPO, 1).len(), 1);
    assert_eq!(lane.labels(REPO, 1), vec!["actuo:pr"]);
    let restarts = lane
        .prompts()
        .iter()
        .filter(|p| p.contains("Actuo restarted"))
        .count();
    assert_eq!(restarts, 1);
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    assert_eq!(lane.origin_file(REPO, &head, "fixed.txt").unwrap(), "ok\n");
    // One status comment, kept up across the restart.
    let comments = lane.comments(REPO, 1);
    assert_eq!(
        comments.iter().filter(|c| c.contains("**Actuo:**")).count(),
        1,
        "{comments:#?}"
    );
}

/// A restart that cuts off the restart turn too: the run pauses.
#[test]
fn restarted_twice_pauses() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "Actuo restarted", "steps": [{"sleepMs": 5000}]},
            {"match": "issue #2 in", "steps": [{"sleepMs": 5000}]}
        ])),
        &[(REPO, "checks = [\"true\"]")],
        "",
    );
    lane.issue(REPO, 2, "Slower", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the first turn", 30, |l| {
        l.prompts().iter().any(|p| p.contains("issue #2 in"))
    });
    lane.env.stop_actuo(true);
    lane.env.start_actuo();
    lane.wait("the restart turn", 30, |l| {
        l.prompts().iter().any(|p| p.contains("Actuo restarted"))
    });
    lane.env.stop_actuo(true);
    lane.env.start_actuo();
    lane.wait("the pause", 30, |l| state_is(l, 2, "waiting (restarted)"));
    lane.wait("the waiting label", 30, |l| {
        l.labels(REPO, 2) == vec!["actuo:waiting"]
    });
    assert_eq!(
        lane.prompts()
            .iter()
            .filter(|p| p.contains("Actuo restarted"))
            .count(),
        1
    );
    // The state is saved first, then GitHub is told.
    lane.wait("the pause comment", 30, |l| {
        l.comments(REPO, 2)
            .iter()
            .any(|c| c.contains("restarted during the agent's turn, again"))
    });
}

/// `kill -9` while `gh pr create` runs (after the push): the call outlives actuo
/// and opens the PR; the next start finds it instead of opening a second one.
#[test]
fn kill_during_pr_create() {
    let script = script(json!([
        {"match": "issue #3 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
        {"match": "Review your changes", "steps": []}
    ]));
    let mut lane = LaneEnv::new(&script, &[(REPO, "checks = [\"test -f fixed.txt\"]")], "");
    let flag = lane.env.root.join("slow-pr-create");
    std::fs::write(&flag, "").unwrap();
    lane.env
        .env
        .push(("FAKE_GH_SLOW_PR_CREATE".into(), flag.display().to_string()));
    lane.issue(REPO, 3, "Fix", "x", &["actuo"], "me");
    lane.env.start_actuo();
    let started = lane.env.root.join("slow-pr-create.started");
    lane.wait("gh pr create", 60, |_| started.exists());
    lane.env.stop_actuo(true);
    lane.env.start_actuo();
    lane.wait("the PR", 60, |l| state_is(l, 3, "pr_opened"));
    // The first `gh pr create` is done by now too.
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert_eq!(lane.prs_from(REPO, 3), 1);
    assert_eq!(lane.labels(REPO, 3), vec!["actuo:pr"]);
    assert_eq!(lane.runs_of(REPO, 3).len(), 1);
}

/// A clean stop while the checks run: the next start runs them again and goes on.
#[test]
fn clean_stop_during_checks() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #4 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ])),
        &[(REPO, "checks = [\"sleep 3; test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 4, "Fix", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the checks", 30, |l| state_is(l, 4, "checking"));
    lane.env.stop_actuo(false);
    lane.env.start_actuo();
    lane.wait("the PR", 60, |l| state_is(l, 4, "pr_opened"));
    assert_eq!(lane.prs_from(REPO, 4), 1);
    assert_eq!(lane.labels(REPO, 4), vec!["actuo:pr"]);
}

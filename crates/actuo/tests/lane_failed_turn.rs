//! A turn the agent didn't finish: a turn answered with an error pauses
//! `turn_failed` with the error, and each hand-back sends that turn again, once; an
//! agent that stops mid-turn twice pauses `agent_stopped`, and a hand-back sends one
//! "continue" turn.

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

fn prompts_with(lane: &LaneEnv, text: &str) -> usize {
    lane.prompts().iter().filter(|p| p.contains(text)).count()
}

/// Fails twice, then works: each failure pauses with its error (no automatic
/// turn), each hand-back sends the same turn once, on top of what the failed turn
/// wrote.
#[test]
fn a_failed_turn_runs_again_once_per_hand_back() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #5 in", "unlessFile": "attempt1.txt", "steps": [
                {"writeFile": {"path": "attempt1.txt", "content": "1\n"}},
                {"error": {"code": -32603, "message": "Authentication failed: first"}}
            ]},
            // Its marker is outside the worktree: this try changes no files.
            {"match": "issue #5 in", "unlessFile": "../attempt2.marker", "steps": [
                {"writeFile": {"path": "../attempt2.marker", "content": "2\n"}},
                {"error": {"code": -32603, "message": "second ```fence``` <img src=x>"}}
            ]},
            {"match": "issue #5 in", "steps": [
                {"writeFile": {"path": "fixed.txt", "content": "ok\n"}}
            ]},
            {"match": "Review your changes", "steps": []}
        ])),
        &[(REPO, "checks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 5, "Fix it", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the first pause", 30, |l| {
        state_is(l, 5, "waiting (turn_failed)")
    });
    let run = lane.run_of(REPO, 5).unwrap();
    assert_eq!(prompts_with(&lane, "issue #5 in"), 1, "no automatic turn");
    let comment = lane
        .comments(REPO, 5)
        .into_iter()
        .find(|c| c.contains("turn failed"))
        .unwrap();
    assert!(
        comment.contains("```\nagent error -32603: Authentication failed: first")
            || comment.contains("```\nAuthentication failed: first"),
        "{comment}"
    );
    assert!(
        comment.contains("changed files before it failed"),
        "{comment}"
    );
    assert!(
        comment.contains(&format!("`actuo revert {} 1`", run[4])),
        "{comment}"
    );

    // The second try fails differently: paused again, its error in a fence that
    // its own backticks can't close.
    lane.env.ok(&["run", "resume", &run[0][..13]]);
    lane.wait("the second pause", 30, |l| {
        l.comments(REPO, 5).iter().any(|c| c.contains("second"))
            && state_is(l, 5, "waiting (turn_failed)")
    });
    assert_eq!(
        prompts_with(&lane, "issue #5 in"),
        2,
        "one turn per hand-back"
    );
    let comment = lane
        .comments(REPO, 5)
        .into_iter()
        .find(|c| c.contains("second"))
        .unwrap();
    assert!(comment.contains("````\n"), "{comment}");
    assert!(
        !comment.contains("changed files before it failed"),
        "{comment}"
    );

    // The third works, on top of the first try's file.
    lane.env.ok(&["run", "resume", &run[0][..13]]);
    lane.wait("the PR", 60, |l| state_is(l, 5, "pr_opened"));
    assert_eq!(prompts_with(&lane, "issue #5 in"), 3);
    for f in ["attempt1.txt", "fixed.txt"] {
        assert!(
            lane.origin_file(REPO, "actuo/issue-5", f).is_some(),
            "{f} on the branch"
        );
    }
}

/// An error with a credential format in it isn't posted: the run waits
/// `secret_detected`, and handing back still sends the failed turn again.
#[test]
fn a_failed_turn_with_a_secret_in_its_error() {
    let token = format!("sk-ant-api03-{}", "aZ3kQ9xR2mT7vB4nL8pW6cY1".repeat(4));
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #6 in", "unlessFile": "attempt1.txt", "steps": [
                {"writeFile": {"path": "attempt1.txt", "content": "1\n"}},
                {"error": {"code": -32603, "message": format!("bad key {token}")}}
            ]},
            {"match": "issue #6 in", "steps": [
                {"writeFile": {"path": "fixed.txt", "content": "ok\n"}}
            ]},
            {"match": "Review your changes", "steps": []}
        ])),
        &[(REPO, "checks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 6, "Fix it", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the hold", 30, |l| {
        state_is(l, 6, "waiting (secret_detected)")
    });
    assert!(
        !lane.comments(REPO, 6).iter().any(|c| c.contains("sk-ant-")),
        "the token was posted"
    );
    let run = lane.run_of(REPO, 6).unwrap();
    lane.env.ok(&["run", "resume", &run[0][..13]]);
    lane.wait("the PR", 60, |l| state_is(l, 6, "pr_opened"));
    assert_eq!(prompts_with(&lane, "issue #6 in"), 2);
}

/// The agent hits its own limit on requests in a turn: one automatic "continue"
/// turn; a second time, `agent_stopped`, saying which; a hand-back sends one more.
#[test]
fn the_agent_stops_twice() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #7 in", "steps": [], "stopReason": "max_turn_requests"},
            {"match": "stopped before it finished", "unlessFile": "c1.txt", "steps": [
                {"writeFile": {"path": "c1.txt", "content": "1\n"}}
            ], "stopReason": "max_turn_requests"},
            {"match": "stopped before it finished", "steps": [
                {"writeFile": {"path": "fixed.txt", "content": "ok\n"}}
            ]},
            {"match": "Review your changes", "steps": []}
        ])),
        &[(REPO, "checks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 7, "Fix it", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the pause", 30, |l| {
        state_is(l, 7, "waiting (agent_stopped)")
    });
    assert_eq!(prompts_with(&lane, "stopped before it finished"), 1);
    assert!(
        lane.comments(REPO, 7)
            .iter()
            .any(|c| c.contains("hit its own limit on requests in one turn")),
        "{:#?}",
        lane.comments(REPO, 7)
    );
    let run = lane.run_of(REPO, 7).unwrap();
    lane.env.ok(&["run", "resume", &run[0][..13]]);
    lane.wait("the PR", 60, |l| state_is(l, 7, "pr_opened"));
    assert_eq!(prompts_with(&lane, "stopped before it finished"), 2);
}

/// The agent's process exits mid-turn twice: `agent_stopped`, with what
/// actuo saw of the exit.
#[test]
fn the_agent_exits_twice() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #8 in", "steps": [{"sleepMs": 200}, {"exit": 3}]},
            {"match": "stopped before it finished", "steps": [{"sleepMs": 200}, {"exit": 3}]}
        ])),
        &[(REPO, "checks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 8, "Fix it", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the pause", 60, |l| {
        state_is(l, 8, "waiting (agent_stopped)")
    });
    let comment = lane
        .comments(REPO, 8)
        .into_iter()
        .find(|c| c.contains("process exited"))
        .unwrap();
    assert!(comment.contains("What actuo saw"), "{comment}");
}

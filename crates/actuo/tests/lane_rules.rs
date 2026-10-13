//! Rules that allow, with `fake-agent` requests of each kind against a real `actuo
//! serve`: answered by the rules inside and outside the worktree, paused when no rule
//! covers them, a follow-up turn after a reject (at most `max_reject_turns`, then a
//! pause), and the note after a time-limit interrupt. A session without the sandbox
//! gets no allow rules: the lane's sessions always have it, so that is a unit test
//! (`lane::rules`).

mod common;

use common::lane::{LaneEnv, Rpc};
use serde_json::{Value, json};

const REPO: &str = "o/r";
const SLOW: &str = "o/slow";

/// A tool call and its request, offering allow always, allow once and reject.
fn request(id: &str, title: &str, kind: &str, path: Option<&str>, allowed: Value) -> Vec<Value> {
    let mut call = json!({"id": id, "title": title, "kind": kind,
                          "rawInput": {"command": title}});
    if let Some(p) = path {
        call["diff"] = json!({"path": p, "newText": "x\n"});
    }
    vec![
        json!({"toolCall": call}),
        json!({"permission": {"toolCallId": id,
            "options": [{"id": "allow-always", "name": "Always", "kind": "allow_always"},
                        {"id": "allow-once", "name": "Yes", "kind": "allow_once"},
                        {"id": "reject", "name": "No", "kind": "reject_once"}],
            "allowed": allowed,
            "rejected": [{"message": "Not allowed, stopping."}]}}),
    ]
}

fn wait_state(lane: &LaneEnv, repo: &str, n: u64, state: &str) {
    lane.wait(state, 60, |l| {
        l.run_of(repo, n).is_some_and(|r| r[2] == state)
    });
}

#[test]
fn rules_answer_and_pause() {
    let mut first = Vec::new();
    first.extend(request(
        "t1",
        "make fix",
        "execute",
        None,
        json!([{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]),
    ));
    first.extend(request(
        "t2",
        "Edit src/a.txt",
        "edit",
        Some("src/a.txt"),
        json!([{"writeFile": {"path": "src/a.txt", "content": "x\n"}}]),
    ));
    first.extend(request(
        "t3",
        "Edit ../outside-1.txt",
        "edit",
        Some("../outside-1.txt"),
        json!([{"writeFile": {"path": "../outside-1.txt", "content": "x\n"}}]),
    ));
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": first},
            // The follow-up after the reject.
            {"match": "rejected Edit ../outside-1.txt: rule `kind = \"edit\", inside_worktree = false, answer = [\"reject_once\"]`.\nContinue without it.",
             "steps": [{"writeFile": {"path": "followed-up.txt", "content": "ok\n"}}]},
            {"match": "issue #2 in", "steps": request("t4", "Fetch docs", "fetch", None, json!([]))},
            // Asks for the same thing after every reject.
            {"match": "issue #3 in", "steps": request("t5", "Edit ../outside-3.txt", "edit", Some("../outside-3.txt"), json!([]))},
            {"match": "rejected Edit ../outside-3.txt", "steps": request("t5", "Edit ../outside-3.txt", "edit", Some("../outside-3.txt"), json!([]))},
            // Stopped at the turn time limit with a request open.
            {"match": "issue #4 in", "steps": [
                {"writeFile": {"path": "fixed.txt", "content": "ok\n"}},
                "waitForCancel"
            ]},
            {"match": "The previous turn was stopped by actuo's time limit, not by a person.\n\nReview your changes",
             "steps": [{"writeFile": {"path": "noted.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let rules = r#"
[[lane.rules.fake]]
kind = "execute"
answer = ["allow_once"]

[[lane.rules.fake]]
kind = "edit"
inside_worktree = true
answer = ["allow_once"]

[[lane.rules.fake]]
kind = "edit"
inside_worktree = false
answer = ["reject_once"]
"#;
    let mut lane = LaneEnv::new(
        &script,
        &[
            (REPO, "checks = [\"test -f fixed.txt\"]"),
            (
                SLOW,
                "checks = [\"test -f fixed.txt\"]\nturn_time_limit = \"3s\"",
            ),
        ],
        rules,
    );
    lane.issue(REPO, 1, "Rules answer", "x", &["actuo"], "me");
    lane.issue(REPO, 2, "Nothing covers fetch", "x", &["actuo"], "me");
    lane.issue(REPO, 3, "Keeps asking", "x", &["actuo"], "me");
    lane.issue(SLOW, 4, "Too slow", "x", &["actuo"], "me");
    lane.env.start_actuo();

    // Execute and the edit inside the worktree allowed once (never always), the edit
    // outside rejected; one follow-up turn; nothing asked on the issue.
    wait_state(&lane, REPO, 1, "pr_opened");
    let branch = "actuo/issue-1";
    for f in ["fixed.txt", "src/a.txt", "followed-up.txt"] {
        assert!(lane.origin_file(REPO, branch, f).is_some(), "{f}");
    }
    let session = lane.run_of(REPO, 1).unwrap()[4].clone();
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &session])).unwrap();
    let worktree = std::path::PathBuf::from(detail["worktreePath"].as_str().unwrap());
    assert!(!worktree.parent().unwrap().join("outside-1.txt").exists());
    let comments = lane.comments(REPO, 1);
    assert!(
        !comments.iter().any(|c| c.contains("The agent asks")),
        "{comments:#?}"
    );
    let answers = resolved(&lane, &session);
    assert_eq!(
        answers,
        vec![
            ("allow-once".to_string(), "lane".to_string()),
            ("allow-once".into(), "lane".into()),
            ("reject".into(), "lane".into()),
        ]
    );

    // No rule for fetch: the run waits.
    wait_state(&lane, REPO, 2, "waiting (request)");
    assert!(
        lane.comments(REPO, 2)
            .iter()
            .any(|c| c.contains("The agent asks to fetch from the network:")
                && !c.contains("Fetch docs"))
    );

    // Rejected again after each of the 2 follow-up turns: the run waits.
    wait_state(&lane, REPO, 3, "waiting (rejects)");
    let c = lane.comments(REPO, 3);
    let paused = c
        .iter()
        .find(|c| c.contains("after 2 follow-up turns"))
        .unwrap_or_else(|| panic!("{c:#?}"));
    assert!(
        paused.contains("The agent asks to edit files outside the worktree:")
            && paused.contains("outside-3.txt\n```")
            && !paused.contains("**Edit"),
        "{paused}"
    );
    let session = lane.run_of(REPO, 3).unwrap()[4].clone();
    assert_eq!(resolved(&lane, &session).len(), 3);

    // The time limit pauses the run; after resume, the next turn says it was the limit.
    wait_state(&lane, SLOW, 4, "waiting (turn_time_limit)");
    let run = lane.run_of(SLOW, 4).unwrap()[0].clone();
    lane.env.ok(&["run", "resume", &run]);
    wait_state(&lane, SLOW, 4, "pr_opened");
    assert!(
        lane.origin_file(SLOW, "actuo/issue-4", "noted.txt")
            .is_some()
    );
}

/// The session's answered requests: option and client, in order.
fn resolved(lane: &LaneEnv, session: &str) -> Vec<(String, String)> {
    let mut rpc = Rpc::connect(&lane.env.socket());
    rpc.call("session/subscribe", json!({"sessionId": session}));
    let head = rpc.call("session/get", json!({"sessionId": session}))["session"]["headSeq"]
        .as_u64()
        .unwrap();
    rpc.events_until(head)
        .into_iter()
        .filter(|e| e["type"] == "request.resolved")
        .map(|e| {
            (
                e["data"]["optionId"].as_str().unwrap_or("").to_string(),
                e["data"]["client"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

//! The one push sends one branch: an agent that moves `main`, tags its commit, turns
//! on `push.followTags` and checks out `main` still gets only `actuo/issue-1` onto
//! `origin`.

mod common;

use common::lane::LaneEnv;
use serde_json::json;
use std::process::Command;

const REPO: &str = "o/r";

/// Every ref of `origin`, one `<sha> <name>` line each.
fn origin_refs(lane: &LaneEnv) -> Vec<String> {
    let out = Command::new("git")
        .args(["--git-dir", lane.origin(REPO).to_str().unwrap()])
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn push_sends_only_the_run_branch() {
    // Unsandboxed under fake-srt, so the agent can write the clone's `.git`, which a
    // real one can't.
    let agent = [
        "export GIT_AUTHOR_NAME=a GIT_AUTHOR_EMAIL=a@a GIT_COMMITTER_NAME=a GIT_COMMITTER_EMAIL=a@a",
        "g='git -c commit.gpgsign=false -c tag.gpgsign=false'",
        "echo agent > agent.txt",
        "$g add agent.txt",
        "$g commit -q -m agent",
        "$g update-ref refs/heads/main HEAD",
        "$g tag -a agent-tag -m x",
        "$g config push.followTags true",
        "$g checkout -q --ignore-other-worktrees main",
        "echo more > more.txt",
    ]
    .join(" && ");
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"command": ["sh", "-c", agent]}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let mut lane = LaneEnv::new(&script, &[(REPO, "checks = [\"test -f more.txt\"]")], "");
    lane.issue(REPO, 1, "Add a file", "Please add one.", &["actuo"], "me");
    let before = origin_refs(&lane);
    lane.env.start_actuo();

    lane.wait("the draft PR", 60, |l| {
        l.run_of(REPO, 1).is_some_and(|r| r[2] == "pr_opened")
    });

    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    let mut expected = before.clone();
    expected.push(format!("{head} refs/heads/actuo/issue-1"));
    expected.sort();
    let mut after = origin_refs(&lane);
    after.sort();
    assert_eq!(after, expected, "before: {before:#?}");
    assert_eq!(
        lane.origin_file(REPO, &head, "agent.txt").unwrap(),
        "agent\n"
    );
    assert_eq!(lane.origin_file(REPO, &head, "more.txt").unwrap(), "more\n");
    assert_eq!(lane.pr(REPO, 1).unwrap()["head"], "actuo/issue-1");
}

//! The lane's clone runs no hooks: the test's git config turns hooks on through a
//! tracked `.githooks`, and a repo whose `post-checkout` writes a marker leaves none
//! after a run's clone.

mod common;

use common::lane::{LaneEnv, git};
use serde_json::json;

const REPO: &str = "o/r";

#[test]
fn clone_runs_no_hooks() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let mut lane = LaneEnv::new(&script, &[(REPO, "checks = [\"test -f fixed.txt\"]")], "");
    let marker = lane.env.root.join("hook-ran");
    let seed = lane.env.root.join("seed-o-r");
    let hook = seed.join(".githooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "a post-checkout hook"]);
    git(&seed, &["push", "-q", "origin", "main"]);
    lane.issue(REPO, 1, "Fix it", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();

    lane.wait("the draft PR", 60, |l| {
        l.run_of(REPO, 1).is_some_and(|r| r[2] == "pr_opened")
    });
    assert!(!marker.exists(), "a hook ran");
    let clone = lane.env.home.join("repos").join(REPO);
    assert_eq!(git(&clone, &["config", "core.hooksPath"]), "/dev/null");
}

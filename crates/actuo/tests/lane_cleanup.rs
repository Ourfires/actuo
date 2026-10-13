//! A run whose PR is merged or closed ends, and what it left on actuo is
//! removed: the session (stopped), its worktree, the local branch, the session's
//! checkpoint refs, the temp dirs and sandbox settings files.
//! The run's records and logs, the clone and GitHub's branch stay.

mod common;

use common::lane::{LaneEnv, git};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "o/r";

fn script() -> Value {
    json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Title: Fix the thing\n\nIt's fixed."}]}
        ]
    })
}

fn wait_state(lane: &LaneEnv, state: &str) {
    lane.wait(state, 60, |l| {
        l.run_of(REPO, 1).is_some_and(|r| r[2] == state)
    });
}

/// Every ref of `origin`, one `<sha> <name>` line each.
fn origin_refs(lane: &LaneEnv) -> String {
    let out = Command::new("git")
        .args(["--git-dir", lane.origin(REPO).to_str().unwrap()])
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn set_pr_state(lane: &LaneEnv, state: &str) {
    lane.edit_gh(|v| {
        for p in v["repos"][REPO]["pulls"].as_array_mut().unwrap() {
            if p["head"] == "actuo/issue-1" {
                p["state"] = json!(state);
            }
        }
    });
}

/// What a run leaves on actuo: its worktree, the clone, the session's
/// checkpoint refs, temp dirs and sandbox settings files, and its logs.
struct Left {
    worktree: PathBuf,
    clone: PathBuf,
    session: String,
    removed: Vec<PathBuf>,
    logs: PathBuf,
}

fn left(lane: &LaneEnv) -> Left {
    let run = lane.run_of(REPO, 1).unwrap();
    let (run_id, session) = (run[0].clone(), run[4].clone());
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &session])).unwrap();
    let worktree = PathBuf::from(detail["cwd"].as_str().unwrap());
    let home = lane.env.home.clone();
    let removed = vec![
        home.join("tmp").join(&run_id),
        home.join("tmp").join(&session),
        home.join(format!("sandbox/{session}.json")),
        home.join(format!("sandbox/{run_id}-setup.json")),
        home.join(format!("sandbox/{run_id}-checks.json")),
    ];
    Left {
        worktree,
        clone: home.join("repos").join(REPO),
        session,
        removed,
        logs: home.join("logs/runs").join(&run_id),
    }
}

fn checkpoint_refs(clone: &Path, session: &str) -> String {
    git(
        clone,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/actuo/checkpoints/{session}/"),
        ],
    )
}

/// Everything the run left is there.
fn assert_there(l: &Left) {
    assert!(l.worktree.is_dir(), "{}", l.worktree.display());
    assert!(!checkpoint_refs(&l.clone, &l.session).is_empty());
    for p in &l.removed {
        assert!(p.exists(), "{}", p.display());
    }
}

/// Removed: the worktree, the local branch, the checkpoint refs, the temp dirs and
/// sandbox settings; the session ended; the lane's labels off the issue. Kept: the
/// logs, the clone, origin's refs.
fn assert_cleaned(lane: &LaneEnv, l: &Left, origin_before: &str) {
    assert!(!l.worktree.exists(), "{}", l.worktree.display());
    let worktrees = git(&l.clone, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains(&*l.worktree.to_string_lossy()),
        "{worktrees}"
    );
    assert_eq!(git(&l.clone, &["branch", "--list", "actuo/issue-1"]), "");
    assert_eq!(checkpoint_refs(&l.clone, &l.session), "");
    for p in &l.removed {
        assert!(!p.exists(), "{}", p.display());
    }
    assert!(l.logs.is_dir(), "the run's logs stay");
    assert!(l.clone.join(".git").is_dir(), "the clone stays");
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &l.session])).unwrap();
    assert_eq!(detail["state"], "ended");
    let labels = lane.labels(REPO, 1);
    assert!(!labels.iter().any(|l| l.starts_with("actuo")), "{labels:?}");
    assert_eq!(origin_refs(lane), origin_before, "origin changed");
}

#[test]
fn merged_pr_ends_the_run_and_cleans_up() {
    let mut lane = LaneEnv::new(
        &script(),
        &[(REPO, "setup = [\"true\"]\nchecks = [\"test -f fixed.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, "pr_opened");
    let l = left(&lane);
    assert_there(&l);

    // Polls with the PR open change nothing.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(lane.run_of(REPO, 1).unwrap()[2], "pr_opened");
    assert_there(&l);

    let origin = origin_refs(&lane);
    set_pr_state(&lane, "merged");
    wait_state(&lane, "merged");
    // Files go last: then the worktree, branch and refs are gone too.
    lane.wait("the cleanup", 30, |_| {
        !l.worktree.exists() && l.removed.iter().all(|p| !p.exists())
    });
    assert_cleaned(&lane, &l, &origin);
}

#[test]
fn closed_pr_ends_a_waiting_run_and_cleans_up() {
    let mut lane = LaneEnv::new(
        &script(),
        &[(
            REPO,
            "setup = [\"true\"]\nchecks = [\"test ! -f fixed.txt\"]\nmax_check_retries = 0",
        )],
        "",
    );
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, "waiting (checks_failed)");
    let l = left(&lane);
    assert_there(&l);

    let origin = origin_refs(&lane);
    set_pr_state(&lane, "closed");
    wait_state(&lane, "closed");
    // Files go last: then the worktree, branch and refs are gone too.
    lane.wait("the cleanup", 30, |_| {
        !l.worktree.exists() && l.removed.iter().all(|p| !p.exists())
    });
    assert_cleaned(&lane, &l, &origin);
}

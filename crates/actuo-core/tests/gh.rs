//! The lane's `gh` calls against `fake-gh`, with a local bare repo as GitHub's.

mod common;

use actuo_core::lane::gh::Gh;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A wrapper script that runs fake-gh with this test's state file.
fn gh(dir: &Path, state: &Path) -> Gh {
    let script = dir.join("gh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nFAKE_GH_STATE='{}' exec '{}' \"$@\"\n",
            state.display(),
            common::fake_gh().display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    Gh::new(script)
}

#[tokio::test]
async fn lane_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    git(
        &root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    let seed = root.join("seed");
    git(
        &root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    std::fs::write(seed.join("README.md"), "readme\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["push", "-q", "origin", "main"]);

    let title = "He said \"hi\" $(touch /tmp/x) `id`";
    let state = root.join("gh-state.json");
    std::fs::write(
        &state,
        json!({
            "login": "me",
            "repos": {"o/r": {
                "origin": origin,
                "default_branch": "main",
                "labels": ["actuo"],
                "issues": [
                    {"number": 1, "title": title, "body": "body", "state": "open", "author": "you",
                     "labels": ["actuo", "bug"],
                     "events": [{"event": "labeled", "label": "actuo", "actor": "mallory"},
                                {"event": "labeled", "label": "bug", "actor": "x"},
                                {"event": "labeled", "label": "actuo", "actor": "me"}],
                     "comments": [{"id": 5, "author": "you", "body": "more", "created_at": "2026-10-01T00:00:01Z"}]},
                    {"number": 2, "title": "other", "state": "open", "labels": []}
                ]
            }}
        })
        .to_string(),
    )
    .unwrap();
    let gh = gh(&root, &state);

    assert_eq!(gh.login().await.unwrap(), "me");
    let issues = gh.issues_with_label("o/r", "actuo").await.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, title);
    assert_eq!(issues[0].labels, vec!["actuo", "bug"]);
    assert_eq!(issues[0].url, "https://github.com/o/r/issues/1");
    assert_eq!(issues[0].author, "you");
    assert_eq!(issues[0].state, "open");
    assert_eq!(
        gh.label_actor("o/r", 1, "actuo").await.unwrap().as_deref(),
        Some("me")
    );
    assert_eq!(gh.label_actor("o/r", 2, "actuo").await.unwrap(), None);
    let comments = gh.comments("o/r", 1).await.unwrap();
    assert_eq!(
        (comments[0].author.as_str(), comments[0].body.as_str()),
        ("you", "more")
    );

    gh.ensure_label("o/r", "actuo:running").await.unwrap();
    gh.ensure_label("o/r", "actuo:running").await.unwrap();
    gh.edit_labels("o/r", 1, &["actuo:running"], &["actuo"])
        .await
        .unwrap();
    assert_eq!(
        gh.issue("o/r", 1).await.unwrap().labels,
        vec!["bug", "actuo:running"]
    );
    assert!(
        gh.edit_labels("o/r", 1, &["no-such-label"], &[])
            .await
            .is_err()
    );

    let body = "status: `running` $(id) \"quoted\"\n";
    let id = gh.add_comment("o/r", 1, body).await.unwrap();
    gh.edit_comment("o/r", id, "status: done\n").await.unwrap();
    let comments = gh.comments("o/r", 1).await.unwrap();
    assert_eq!(comments.last().unwrap().body, "status: done\n");

    assert_eq!(gh.default_branch("o/r").await.unwrap(), "main");
    let clone = root.join("repos/o/r");
    gh.clone_repo("o/r", &clone).await.unwrap();
    assert!(clone.join("README.md").exists());

    assert_eq!(gh.open_pr("o/r", "actuo/issue-1").await.unwrap(), None);
    assert!(
        gh.create_draft_pr("o/r", "main", "actuo/issue-1", title, "b")
            .await
            .is_err(),
        "the head branch must exist"
    );
    git(
        &clone,
        &["push", "-q", "origin", "HEAD:refs/heads/actuo/issue-1"],
    );
    let pr = gh
        .create_draft_pr(
            "o/r",
            "main",
            "actuo/issue-1",
            title,
            "Body with $(id)\n\nCloses #1\n",
        )
        .await
        .unwrap();
    assert_eq!(pr.number, 3);
    assert_eq!(
        gh.open_pr("o/r", "actuo/issue-1").await.unwrap(),
        Some(pr.clone())
    );
    gh.edit_pr_body("o/r", pr.number, "new body\n")
        .await
        .unwrap();

    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    let pull = &saved["repos"]["o/r"]["pulls"][0];
    assert_eq!(pull["title"], title);
    assert_eq!(pull["draft"], true);
    assert_eq!(pull["base"], "main");
    assert_eq!(pull["body"], "new body\n");

    // Every text went as argv or stdin; the call log keeps both.
    let calls = std::fs::read_to_string(root.join("gh-state.json.calls.jsonl")).unwrap();
    let create = calls
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .rfind(|c| c["args"][0] == "pr" && c["args"][1] == "create")
        .unwrap();
    assert!(
        create["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == title)
    );
    assert_eq!(create["stdin"], "Body with $(id)\n\nCloses #1\n");
}

//! What actuo publishes: images, HTML and mentions in agent text turned into
//! text, and the credential check that holds back a commit, push or post
//! (`secret_detected`). Every token here is
//! synthetic; each test checks it reached none of `gh`'s calls and not `origin`.

mod common;

use common::lane::{LaneEnv, git};
use serde_json::{Value, json};

const REPO: &str = "o/r";

/// A synthetic random part with plenty of distinct characters.
fn random(len: usize) -> String {
    "aZ3kQ9xR2mT7vB4nL8pW6cY1"
        .chars()
        .cycle()
        .take(len)
        .collect()
}

fn script(turns: Value) -> Value {
    json!({
        "resume": true,
        "loadSession": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": turns
    })
}

fn wait_state(lane: &LaneEnv, n: u64, state: &str) {
    lane.wait(state, 60, |l| {
        l.run_of(REPO, n).is_some_and(|r| r[2] == state)
    });
}

/// Nothing with `token` went to GitHub.
fn not_published(lane: &LaneEnv, token: &str) {
    assert!(!lane.gh_calls().contains(token), "a gh call had the token");
    for c in lane.comments(REPO, 1) {
        assert!(!c.contains(token));
    }
}

fn worktree(lane: &LaneEnv, session: &str) -> std::path::PathBuf {
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", session])).unwrap();
    std::path::PathBuf::from(detail["cwd"].as_str().unwrap())
}

/// A token in a file the agent wrote: no commit, no push, no PR; the comment and
/// the notification name the file and line. Fixed in the worktree, `run resume`
/// reaches the PR.
#[test]
fn a_token_in_the_diff_holds_the_push() {
    let token = format!("ghp_{}", random(36));
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "src.txt", "content": format!("one\nconst t = \"{token}\";\n")}}]},
            {"match": "Review your changes", "steps": [{"message": "Add src\n\nDone."}]}
        ])),
        &[(REPO, "checks = [\"test -f src.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Add src", "x", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, 1, "waiting (secret_detected)");

    assert!(lane.origin_ref(REPO, "refs/heads/actuo/issue-1").is_none());
    assert!(lane.pr(REPO, 1).is_none());
    assert!(lane.labels(REPO, 1).contains(&"actuo:waiting".into()));
    let comments = lane.comments(REPO, 1);
    assert!(
        comments.iter().any(|c| c.contains(
            "Actuo held back the change, `src.txt` line 2: it has text in the format of a GitHub token."
        )),
        "{comments:#?}"
    );
    // macOS only (a LaunchAgent in your login session).
    if cfg!(target_os = "macos") {
        lane.wait("the notification naming the file", 10, |l| {
            l.notifications().iter().any(|n| {
                n.iter()
                    .any(|a| a == "Waiting for you (secret_detected): the change, `src.txt` line 2")
            })
        });
    }
    not_published(&lane, &token);

    let run = lane.run_of(REPO, 1).unwrap();
    // `show` gives the notice in words, never the text held back.
    let shown = lane.env.ok(&["show", &run[0]]);
    let detail: Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(
        detail["reasonText"],
        "Actuo held back the change, `src.txt` line 2: it has text in the format of a GitHub token. Nothing from this step was published."
    );
    assert!(!shown.contains(&token));
    let wt = worktree(&lane, &run[4]);
    std::fs::write(wt.join("src.txt"), "one\nconst t = process.env.T;\n").unwrap();
    lane.env.ok(&["run", "resume", &run[0]]);
    wait_state(&lane, 1, "pr_opened");
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    assert_eq!(
        lane.origin_file(REPO, &head, "src.txt").unwrap(),
        "one\nconst t = process.env.T;\n"
    );
    not_published(&lane, &token);
}

/// A token in the review reply's description: the PR isn't opened; `run resume`
/// runs the review turn again (here the same reply, so it's held again).
#[test]
fn a_token_in_the_pr_description_holds_the_pr() {
    let token = format!("sk-ant-api03-{}", random(95));
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "ok.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": [{"message": format!("Add ok\n\nIt uses the key {token}.")}]}
        ])),
        &[(REPO, "checks = [\"test -f ok.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Add ok", "x", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, 1, "waiting (secret_detected)");
    assert!(lane.pr(REPO, 1).is_none());
    let comments = lane.comments(REPO, 1);
    assert!(
        comments.iter().any(|c| c.contains(
            "Actuo held back the PR description: it has text in the format of an Anthropic key or token."
        )),
        "{comments:#?}"
    );
    not_published(&lane, &token);

    let reviews = |l: &LaneEnv| {
        l.prompts()
            .iter()
            .filter(|p| p.contains("Review your changes"))
            .count()
    };
    assert_eq!(reviews(&lane), 1);
    let run = lane.run_of(REPO, 1).unwrap();
    lane.env.ok(&["run", "resume", &run[0]]);
    lane.wait("the review turn again, held again", 60, |l| {
        reviews(l) == 2
            && l.run_of(REPO, 1)
                .is_some_and(|r| r[2] == "waiting (secret_detected)")
    });
    assert!(lane.pr(REPO, 1).is_none());
    not_published(&lane, &token);
}

/// A check that prints an npm token and fails, with no retries: the PR description
/// would quote its output, so the PR is held.
#[test]
fn a_token_in_a_check_output_holds_the_pr() {
    let part = random(36);
    let token = format!("npm_{part}");
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "ok.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Add ok"}]}
        ])),
        &[(
            REPO,
            &format!(
                "checks = [\"test ! -f ok.txt || {{ printf 'npm_%s\\\\n' {part}; exit 1; }}\"]"
            ),
        )],
        "max_check_retries = 0",
    );
    lane.issue(REPO, 1, "Add ok", "x", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, 1, "waiting (secret_detected)");
    assert!(lane.pr(REPO, 1).is_none());
    assert!(
        lane.comments(REPO, 1).iter().any(|c| c
            .contains("held back the PR description: it has text in the format of an npm token")),
        "{:#?}",
        lane.comments(REPO, 1)
    );
    not_published(&lane, &token);
}

/// A commit someone made in the worktree outside actuo (as you would after a
/// take-over) goes out with the next push, so its message is checked too.
#[test]
fn a_token_in_another_commit_message_holds_the_push() {
    let token = format!("ghs_{}", random(36));
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "New comments on issue #1", "steps": [{"writeFile": {"path": "b.txt", "content": "b\n"}}]},
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "a.txt", "content": "a\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Add a"}]}
        ])),
        &[(REPO, "checks = [\"test -f a.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Add a", "x", &["actuo"], "me");
    lane.env.start_actuo();
    wait_state(&lane, 1, "pr_opened");
    let first_head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();

    let run = lane.run_of(REPO, 1).unwrap();
    let wt = worktree(&lane, &run[4]);
    std::fs::write(wt.join("mine.txt"), "mine\n").unwrap();
    git(&wt, &["add", "mine.txt"]);
    git(
        &wt,
        &[
            "commit",
            "-q",
            "--no-verify",
            "-m",
            &format!("Mine\n\nkey {token}"),
        ],
    );
    let mine = git(&wt, &["rev-parse", "HEAD"]);

    lane.comment(REPO, 1, "me", "Please also add b.");
    lane.relabel(REPO, 1, "me");
    lane.wait("the re-run held", 60, |l| {
        let r = l.runs_of(REPO, 1);
        r.len() == 2 && r[0][2] == "waiting (secret_detected)"
    });
    assert_eq!(
        lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap(),
        first_head,
        "nothing pushed"
    );
    let comments = lane.comments(REPO, 1);
    assert!(
        comments.iter().any(|c| c.contains(&format!(
            "Actuo held back the commit `{}`'s message: it has text in the format of a GitHub token.",
            &mine[..12]
        ))),
        "{comments:#?}"
    );
    not_published(&lane, &token);
}

/// Images, links, HTML and mentions in the agent's last message and in the PR
/// description are posted as text, every link's target shown in a code span; an
/// email address is left alone.
#[test]
fn agent_markdown_is_posted_as_text() {
    let mut lane = LaneEnv::new(
        &script(json!([
            {"match": "issue #1 in", "steps": [{"message": "I can't. ![x](https://e.test/?d=1) <img src=\"https://e.test/i\"> cc @someone. See [docs](https://e.test/d), [the ref][r] and <a href=\"https://e.test/a\">here</a>.\n\n[r]: https://e.test/r"}]},
            {"match": "issue #2 in", "steps": [{"writeFile": {"path": "ok.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Add ok\n\nThanks @someone and @org/team; mail a@example.com.\n\n![y](https://e.test/?d=2)\n\nDetails in [the notes](https://e.test/n)."}]}
        ])),
        &[(REPO, "checks = [\"test -f ok.txt\"]")],
        "",
    );
    lane.issue(REPO, 1, "Can't be done", "x", &["actuo"], "me");
    lane.issue(REPO, 2, "Add ok", "x", &["actuo"], "me");
    lane.env.start_actuo();

    wait_state(&lane, 1, "waiting (no_changes)");
    let comments = lane.comments(REPO, 1);
    let c = comments
        .iter()
        .find(|c| c.contains("made no changes"))
        .unwrap_or_else(|| panic!("{comments:#?}"));
    assert!(c.contains("[image: `x`] `https://e.test/?d=1`"), "{c}");
    assert!(c.contains("`<img src=\"https://e.test/i\">`"), "{c}");
    assert!(c.contains("`@someone`"), "{c}");
    assert!(!c.contains("![x]"), "{c}");
    assert!(c.contains("See docs `https://e.test/d`, the ref `https://e.test/r` and `<a href=\"https://e.test/a\">`here`</a>`."), "{c}");
    assert!(!c.contains("](") && !c.contains("][r]"), "{c}");

    wait_state(&lane, 2, "pr_opened");
    let pr = lane.pr(REPO, 2).unwrap();
    let body = pr["body"].as_str().unwrap();
    assert!(
        body.contains("Thanks `@someone` and `@org/team`; mail a@example.com."),
        "{body}"
    );
    assert!(
        body.contains("[image: `y`] `https://e.test/?d=2`") && !body.contains("![y]"),
        "{body}"
    );
    assert!(
        body.contains("Details in the notes `https://e.test/n`."),
        "{body}"
    );
}

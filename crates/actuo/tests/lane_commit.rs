//! Repo rules for actuo's commits: `commit_check` on a file holding the whole
//! message, sandboxed like the checks; `commit_subject.prompt` in the review prompt;
//! `co_authored_by = false`. The review prompt asks the agent to run the check on its
//! title; a refusal gets one automatic turn with it, a second pauses `commit_format`
//! with nothing pushed; `actuo run resume` runs the review turn again with the refusal.

mod common;

use common::lane::LaneEnv;
use serde_json::{Value, json};

const REPO: &str = "o/r";
const RULE: &str = "Title: `type(scope): summary`.";

fn wait_state(lane: &LaneEnv, state: &str) {
    lane.wait(state, 60, |l| {
        l.run_of(REPO, 1).is_some_and(|r| r[2] == state)
    });
}

/// The agent writes a file, and the review turn's reply is `review`; after a refusal,
/// `fixed` (writing `again.txt` too when `writes`).
fn script(review: &str, fixed: &str, writes: bool) -> Value {
    let mut steps = vec![json!({"message": fixed})];
    if writes {
        steps.insert(
            0,
            json!({"writeFile": {"path": "again.txt", "content": "ok\n"}}),
        );
    }
    json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Actuo didn't commit", "steps": steps},
            {"match": "Review your changes", "steps": [{"message": review}]}
        ]
    })
}

/// How many times the run ran `test -f fixed.txt` (the check, not the commit check).
fn check_runs(lane: &LaneEnv, run: &str) -> usize {
    lane.srt_calls(&format!("{run}-checks"))
        .iter()
        .filter(|c| c["argv"].to_string().contains("test -f fixed.txt"))
        .count()
}

/// A refused message gets one automatic turn with the refusal; its corrected title
/// passes, and the PR opens. That turn changed no files: the checks ran once.
#[test]
fn a_refused_message_is_corrected_by_the_agent() {
    let settings = format!(
        "checks = [\"test -f fixed.txt\"]\ncommit_check = \"sh {{root}}/commit-check.sh\"\ncommit_subject = {{ prompt = \"{RULE}\" }}"
    );
    let mut lane = LaneEnv::new(
        &script(
            "Fix the thing\n\nIt's fixed.",
            "fix(repo): the thing\n\nIt's fixed.",
            false,
        ),
        &[(REPO, &settings)],
        "",
    );
    let seen = stand_in(&lane);
    let root = lane.env.root.clone();
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();

    wait_state(&lane, "pr_opened");
    let run = lane.run_of(REPO, 1).unwrap();
    let session = run[4].clone();
    assert_eq!(lane.pr(REPO, 1).unwrap()["title"], "fix(repo): the thing");
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!("fix(repo): the thing\n\nCloses #1\n\nActuo-Session: {session} (fake)\n")
    );
    assert_eq!(check_runs(&lane, &run[0]), 1);

    // The commit check ran sandboxed like the checks: no network.
    let checks = lane.srt_calls(&format!("{}-checks", run[0]));
    let last = checks.last().unwrap();
    assert!(
        last["argv"].to_string().contains("commit-message"),
        "{last}"
    );
    assert_eq!(last["settings"]["network"]["allowedDomains"], json!([]));

    // The review prompt gave the subject rule and asked the agent to run the check
    // on its title itself.
    let command = format!("sh {}/commit-check.sh", root.display());
    let prompts = lane.prompts();
    let review = prompts
        .iter()
        .find(|p| p.starts_with("Review your changes"))
        .unwrap();
    assert!(review.contains(RULE), "{review}");
    assert!(
        review.contains(&format!("and run `{command} <that file>`")),
        "{review}"
    );
    // The corrective turn: the refusal, then the review prompt again.
    let refused = prompts
        .iter()
        .find(|p| p.starts_with("Actuo didn't commit"))
        .unwrap();
    assert!(refused.contains("subject has no type"), "{refused}");
    assert!(
        refused.contains("Review your changes") && refused.contains(RULE),
        "{refused}"
    );
}

/// The corrected message is refused too: the run pauses `commit_format` with
/// nothing pushed. The corrective turn changed a file, so the checks ran again
/// before it. `actuo run resume` runs the review turn again.
#[test]
fn a_second_refusal_pauses_then_hand_back() {
    let settings = "checks = [\"test -f fixed.txt\"]\ncommit_check = \"sh {root}/commit-check.sh\"";
    let mut lane = LaneEnv::new(
        &script("Fix the thing\n\nIt's fixed.", "Fix it\n\nFixed.", true),
        &[(REPO, settings)],
        "",
    );
    stand_in(&lane);
    let root = lane.env.root.clone();
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();

    wait_state(&lane, "waiting (commit_format)");
    assert!(
        lane.origin_ref(REPO, "refs/heads/actuo/issue-1").is_none(),
        "pushed"
    );
    assert!(lane.pr(REPO, 1).is_none());
    let run = lane.run_of(REPO, 1).unwrap();
    assert_eq!(check_runs(&lane, &run[0]), 2);
    let comments = lane.comments(REPO, 1);
    let paused = comments.last().unwrap();
    let command = format!("sh {}/commit-check.sh", root.display());
    assert!(
        paused.contains(&format!(
            "\nThe commit message was refused by `{command}` (exit code 1)"
        )),
        "{paused}"
    );
    assert!(paused.contains("```\nFix it\n"), "{paused}");
    assert!(
        paused.contains("The agent had one turn to correct it")
            && paused.contains("Handing the run back runs the review turn again"),
        "{paused}"
    );

    // The check accepts anything now: the hand-back's review turn goes through.
    std::fs::write(root.join("commit-check.sh"), "exit 0\n").unwrap();
    lane.env.ok(&["run", "resume", &run[0]]);
    wait_state(&lane, "pr_opened");
    assert_eq!(lane.pr(REPO, 1).unwrap()["title"], "Fix it");
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    assert!(lane.origin_file(REPO, &head, "again.txt").is_some());
}

/// A `Co-Authored-By` line in a repo that doesn't allow one: the corrective turn's
/// title goes through.
#[test]
fn co_authored_by_off_gets_a_corrective_turn() {
    let mut lane = LaneEnv::new(
        &script(
            "Co-Authored-By: a <a@a>\n\nIt's fixed.",
            "Fix the thing\n\nIt's fixed.",
            false,
        ),
        &[(
            REPO,
            "checks = [\"test -f fixed.txt\"]\nco_authored_by = false",
        )],
        "",
    );
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();

    wait_state(&lane, "pr_opened");
    let refused = lane
        .prompts()
        .into_iter()
        .find(|p| p.starts_with("Actuo didn't commit"))
        .unwrap();
    assert!(refused.contains("`Co-Authored-By` line"), "{refused}");
    assert_eq!(lane.pr(REPO, 1).unwrap()["title"], "Fix the thing");
}

/// A stand-in for a commit-message checker in `{root}/commit-check.sh`: keeps the message it was
/// given in `{root}/seen-message`, and refuses a subject without `:`.
fn stand_in(lane: &LaneEnv) -> std::path::PathBuf {
    let root = lane.env.root.clone();
    let seen = root.join("seen-message");
    std::fs::write(
        root.join("commit-check.sh"),
        format!(
            "cp \"$1\" '{}'\nhead -n 1 \"$1\" | grep -q : && exit 0\necho 'subject has no type' >&2\nexit 1\n",
            seen.display()
        ),
    )
    .unwrap();
    seen
}

/// Checks that fail until `{root}/pass` exists, no retries, commit rules; the agent
/// answers each kind of review prompt in the repo's format.
fn failing_checks_lane() -> (LaneEnv, std::path::PathBuf) {
    let settings = format!(
        "checks = [\"test -f {{root}}/pass -o ! -f fixed.txt\"]\nmax_check_retries = 0\ncommit_check = \"sh {{root}}/commit-check.sh\"\ncommit_subject = {{ prompt = \"{RULE}\" }}"
    );
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "`Commit:` and a one-line subject", "steps": [
                {"writeFile": {"path": "more.txt", "content": "more\n"}},
                {"message": "Commit: fix(repo): pass now\nfix(repo): the thing\n\nIt's fixed."}
            ]},
            {"match": "after the retries", "steps": [{"message":
                "fix(repo): the thing\n\nIt's fixed.\n\nCheck failure: The check looks for a file this machine doesn't have; my change doesn't cause it."}]},
            {"match": "Review your changes", "steps": [{"message": "fix(repo): the thing\n\nIt's fixed."}]}
        ]
    });
    let mut lane = LaneEnv::new(&script, &[(REPO, &settings)], "");
    let seen = stand_in(&lane);
    lane.issue(REPO, 1, "Fix the thing", "Please fix it.", &["actuo"], "me");
    lane.env.start_actuo();
    (lane, seen)
}

const NOTE: &str = "The agent's view of the failure: The check looks for a file this machine doesn't have; my change doesn't cause it.";

/// Failing checks: the review turn still runs before the commit, with the checks'
/// output, so the commit has the agent's title; its view of the failure is in the
/// PR and the `checks_failed` comment.
#[test]
fn failing_checks_get_a_title_and_the_agents_view() {
    let (lane, seen) = failing_checks_lane();
    wait_state(&lane, "waiting (checks_failed)");

    let first = std::fs::read_to_string(&seen).unwrap();
    assert!(
        first.starts_with("fix(repo): the thing\n\nCloses #1\n"),
        "{first}"
    );
    let pr = lane.pr(REPO, 1).unwrap();
    assert_eq!(pr["title"], "fix(repo): the thing");
    let body = pr["body"].as_str().unwrap();
    assert!(body.contains(NOTE), "{body}");
    assert!(!body.contains("Check failure:"), "{body}");
    let comments = lane.comments(REPO, 1);
    let last = comments.last().unwrap();
    assert!(
        last.contains("still failed (exit 1) after 0 retries"),
        "{last}"
    );
    assert!(last.contains(NOTE), "{last}");
    let prompts = lane.prompts();
    let review = prompts
        .iter()
        .find(|p| p.starts_with("The check `test -f "))
        .unwrap();
    assert!(
        review.contains("after the retries") && review.contains("starting `Check failure:`"),
        "{review}"
    );
    assert!(
        review.contains("Review your changes") && review.contains(RULE),
        "{review}"
    );
}

/// `actuo run resume` from `checks_failed` once the checks pass: the follow-up commit gets
/// the agent's `Commit:` subject (a review turn first), not the fallback.
#[test]
fn hand_back_from_checks_failed_gets_a_subject() {
    let (lane, seen) = failing_checks_lane();
    wait_state(&lane, "waiting (checks_failed)");
    std::fs::write(lane.env.root.join("pass"), "").unwrap();
    let run = lane.run_of(REPO, 1).unwrap();
    lane.env.ok(&["run", "resume", &run[0]]);

    wait_state(&lane, "pr_opened");
    let last = std::fs::read_to_string(&seen).unwrap();
    assert!(
        last.starts_with("fix(repo): pass now\n\nCloses #1\n"),
        "{last}"
    );
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    assert_eq!(lane.origin_file(REPO, &head, "more.txt").unwrap(), "more\n");
}

//! The issue-to-PR lane against `fake-agent`, `fake-gh` and local bare repos, with a
//! real `actuo serve`. `srt` is `fake-srt`: it records the sandbox settings and runs
//! the command without a sandbox.

mod common;

use common::lane::{LaneEnv, Rpc, git};
use serde_json::{Value, json};

const REPO: &str = "o/r";

fn hooks() -> Vec<Value> {
    [
        "pre-commit",
        "commit-msg",
        "post-commit",
        "pre-push",
        "reference-transaction",
    ]
    .iter()
    .map(|h| {
        json!({"writeFile": {"path": format!(".githooks/{h}"),
                   "content": "#!/bin/sh\ntouch \"$HOOK_MARKER\"\n",
                   "executable": true}})
    })
    .collect()
}

/// Pick-up to draft PR; a hook the agent wrote doesn't run at commit or push; a
/// title with quotes and `$(…)` reaches the PR unchanged; `actuo run <URL>` on an
/// unlabeled issue; an untrusted label is ignored; `agent:nope` (no such agent) fails.
#[test]
fn issues_to_draft_prs() {
    let mut steps1 = vec![
        json!({"message": "Fixing it"}),
        json!({"writeFile": {"path": "fixed.txt", "content": "ok\n"}}),
    ];
    steps1.extend(hooks());
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "configOptions": [{"id": "model", "name": "Model", "values": ["small", "large"], "current": "small"}],
        "turns": [
            {"match": "issue #1 in", "steps": steps1},
            {"match": "issue #2 in", "steps": [{"writeFile": {"path": "two.txt", "content": "2\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[(
            REPO,
            "setup = [\"echo installing\"]\nchecks = [\"test -f fixed.txt -o -f two.txt\"]\nconfig = { model = \"large\" }",
        )],
        "",
    );
    let hook_marker = lane.env.root.join("hook-ran");
    lane.env
        .env
        .push(("HOOK_MARKER".into(), hook_marker.display().to_string()));
    let marker = lane.env.root.join("title-ran");
    let title = format!(
        "He said \"hi\" $(touch {}) `touch {}`",
        marker.display(),
        marker.display()
    );
    lane.issue(
        REPO,
        1,
        &title,
        "Please fix it.<!-- hidden note -->",
        &["actuo"],
        "me",
    );
    lane.issue(REPO, 2, "Unlabeled", "Add two.txt", &[], "me");
    lane.issue(
        REPO,
        3,
        "Labeled by someone else",
        "x",
        &["actuo"],
        "mallory",
    );
    lane.issue(
        REPO,
        4,
        "Wants an unknown agent",
        "x",
        &["actuo", "agent:nope"],
        "me",
    );
    let main = lane.origin_ref(REPO, "refs/heads/main").unwrap();
    lane.env.start_actuo();

    let url = format!("https://github.com/{REPO}/issues/2");
    let e = lane.env.fails(&["run", &url, "--agent", "nope"]);
    assert!(e.contains("agent_not_found"), "{e}");
    let e = lane
        .env
        .fails(&["run", "https://github.com/o/other/issues/1"]);
    assert!(e.contains("isn't a project here"), "{e}");
    let e = lane
        .env
        .fails(&["run", &format!("https://github.com/{REPO}/issues/99")]);
    assert!(e.contains("issue_not_found"), "{e}");
    let run2 = lane.env.ok(&["run", &url]).trim().to_string();
    // `run watch` follows it to its PR.
    let watched = lane.env.ok(&["run", "watch", &run2]);
    assert!(
        watched.starts_with(&format!("run {run2}: {REPO}#2 Unlabeled\n")),
        "{watched}"
    );
    assert!(watched.contains(" pr_opened"), "{watched}");
    assert!(
        watched.contains(". check `test -f fixed.txt -o -f two.txt`: ok"),
        "{watched}"
    );

    lane.wait("two PRs and a failed run", 60, |l| {
        l.pr(REPO, 1).is_some()
            && l.pr(REPO, 2).is_some()
            && l.labels(REPO, 4).contains(&"actuo:failed".to_string())
            && l.run_of(REPO, 1).is_some_and(|r| r[2] == "pr_opened")
            && l.run_of(REPO, 2).is_some_and(|r| r[2] == "pr_opened")
    });

    // --json: the runs and the projects.
    let runs: Value = serde_json::from_str(&lane.env.ok(&["runs", "--json"])).unwrap();
    assert!(
        runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == run2.as_str())
    );
    let projects: Value =
        serde_json::from_str(&lane.env.ok(&["project", "list", "--json"])).unwrap();
    assert_eq!(projects[REPO]["lane"], true);

    // A run that failed: `run watch` says so and exits 1.
    let run4 = lane.run_of(REPO, 4).unwrap()[0].clone();
    let out = lane.env.run(&["run", "watch", &run4]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains(" failed"));

    // Who started each run, and how: issue 1 by its label, issue 2 by `actuo run`
    // (the CLI's client name).
    let mut rpc = Rpc::connect(&lane.env.socket());
    for (n, by, via) in [(1, "me", "label"), (2, "actuo-cli", "client")] {
        let id = lane.run_of(REPO, n).unwrap()[0].clone();
        let run = rpc.call("run/get", json!({"runId": id}))["run"].clone();
        assert_eq!(
            (run["startedBy"].as_str(), run["startedVia"].as_str()),
            (Some(by), Some(via))
        );
        assert_eq!(run["states"][0]["by"], by);
        assert_eq!(run["states"][0]["via"], via);
    }

    // Issue 1: a draft PR from actuo/issue-1, titled exactly as the issue.
    let pr = lane.pr(REPO, 1).unwrap();
    assert_eq!(pr["title"], title.as_str());
    assert_eq!(pr["draft"], true);
    assert_eq!(pr["base"], "main");
    let body = pr["body"].as_str().unwrap();
    assert!(body.contains("Closes #1"), "{body}");
    assert!(
        body.contains("Check `test -f fixed.txt -o -f two.txt`: passed"),
        "{body}"
    );
    assert!(!marker.exists(), "the title was run");
    assert!(!hook_marker.exists(), "a hook ran");

    // macOS: a notification per PR and for the failure, the issue title as its own
    // argument (never in the script); caffeinate held while runs worked, for this
    // actuo's pid, and let go once none did.
    let notes = lane.notifications();
    if cfg!(target_os = "macos") {
        let script = [
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
        ];
        assert!(notes.iter().all(|n| n[..6] == script), "{notes:#?}");
        let texts: Vec<(&str, &str)> = notes.iter().map(|n| (&*n[6], &*n[7])).collect();
        let pr1 = format!("Draft PR opened: {title}");
        assert!(texts.contains(&("Actuo: o/r#1", &pr1)), "{texts:#?}");
        assert!(
            texts.contains(&(
                "Actuo: o/r#4",
                "Failed (agent_not_found): Wants an unknown agent"
            )),
            "{texts:#?}"
        );
        assert_eq!(notes.iter().filter(|n| n[6] == "Actuo: o/r#1").count(), 1);
        let pid = lane.env.serve.as_ref().unwrap().id();
        let calls = lane.caffeinate_calls();
        assert!(!calls.is_empty());
        for (cpid, args) in &calls {
            assert_eq!(args, &format!("-i -w {pid}"));
            lane.wait("caffeinate let go", 10, |_| {
                !std::process::Command::new("kill")
                    .args(["-0", &cpid.to_string()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            });
        }
    } else {
        assert!(notes.is_empty() && lane.caffeinate_calls().is_empty());
    }
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-1").unwrap();
    assert_eq!(lane.origin_file(REPO, &head, "fixed.txt").unwrap(), "ok\n");
    assert_eq!(
        lane.origin_ref(REPO, "refs/heads/main").unwrap(),
        main,
        "main moved"
    );
    let origin = lane.origin(REPO);
    let log = git(
        &lane.env.root,
        &[
            "--git-dir",
            origin.to_str().unwrap(),
            "log",
            "-1",
            "--format=%an|%B",
            &head,
        ],
    );
    assert!(
        log.starts_with(&format!(
            "Test User|{title}\n\nCloses #1\n\nActuo-Session: "
        )),
        "{log}"
    );
    assert!(log.ends_with("(fake)"), "{log}");

    // Labels and comments: running label gone, pr label on, status comment kept up.
    let labels = lane.labels(REPO, 1);
    assert!(labels.contains(&"actuo:pr".into()), "{labels:?}");
    assert!(
        !labels.iter().any(|l| l == "actuo" || l == "actuo:running"),
        "{labels:?}"
    );
    let comments = lane.comments(REPO, 1);
    assert_eq!(comments.len(), 2, "{comments:#?}");
    assert!(
        comments[0].contains("**Actuo:** pr_opened"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("Setup `echo installing`: passed"),
        "{}",
        comments[0]
    );
    assert!(comments[1].contains(&format!("Draft PR: {}", pr["url"].as_str().unwrap())));

    // The prompt: the issue between marker lines, without the hidden comment.
    let prompts = lane.prompts();
    let first = prompts.iter().find(|p| p.contains("issue #1 in")).unwrap();
    assert!(first.contains("Please fix it."), "{first}");
    assert!(!first.contains("hidden note"), "{first}");
    assert!(first.contains("actuo will run: `test -f fixed.txt -o -f two.txt`"));

    // Issue 2 ran from `actuo run` without a label; started by the CLI.
    assert_eq!(lane.pr(REPO, 2).unwrap()["title"], "Unlabeled");
    assert!(lane.labels(REPO, 2).contains(&"actuo:pr".into()));

    // Issue 3: an untrusted label starts nothing.
    assert!(lane.run_of(REPO, 3).is_none());
    assert!(lane.comments(REPO, 3).is_empty());
    assert_eq!(lane.labels(REPO, 3), vec!["actuo"]);

    // Issue 4: agent:nope names an agent actuo hasn't.
    let r4 = lane.run_of(REPO, 4).unwrap();
    assert_eq!(r4[2], "failed (agent_not_found)");
    let c4 = lane.comments(REPO, 4);
    assert!(
        c4.iter().any(|c| c.contains("Actuo has no agent `nope`")),
        "{c4:#?}"
    );

    // The hooks were real: plain git in the run's worktree runs them, once told where
    // (the clone's own config turns hooks off).
    let session = lane.run_of(REPO, 1).unwrap()[4].clone();
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &session])).unwrap();
    // The repo's config reached the run's session.
    assert_eq!(detail["config"], json!({"model": "large"}));
    let worktree = std::path::PathBuf::from(detail["cwd"].as_str().unwrap());
    std::fs::write(worktree.join("more.txt"), "x\n").unwrap();
    let out = std::process::Command::new("git")
        .current_dir(&worktree)
        .args(["-c", "core.hooksPath=.githooks", "commit", "-qam", "plain"])
        .args(["--allow-empty"])
        .env("GIT_CONFIG_GLOBAL", lane.env.root.join("gitconfig"))
        .env("HOOK_MARKER", &hook_marker)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        hook_marker.exists(),
        "plain git should run the planted hooks"
    );

    // Text from GitHub went to gh as argv or stdin, never through a shell.
    let calls = std::fs::read_to_string(lane.gh_state.with_extension("json.calls.jsonl")).unwrap();
    let create = calls
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|c| {
            c["args"][0] == "pr"
                && c["args"][1] == "create"
                && c["args"].to_string().contains("issue-1")
        })
        .unwrap();
    assert!(
        create["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == title.as_str())
    );
}

/// A check that fails once goes back to the agent, then passes; the review turn's
/// reply titles the PR.
#[test]
fn a_failing_check_goes_back_to_the_agent() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #5 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "bad\n"}}]},
            {"match": "failed with exit code", "steps": [{"writeFile": {"path": "fixed.txt", "content": "good\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Fix the widget\n\nIt works now."}]}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[(REPO, "checks = [\"grep -q good fixed.txt\"]")],
        "",
    );
    // Passing on main: the failure is the change's (the baseline says so).
    lane.commit_to_main(REPO, "fixed.txt", "good\n");
    lane.env.start_actuo();
    let mut rpc = Rpc::connect(&lane.env.socket());
    rpc.call("run/subscribe", json!({}));
    lane.issue(REPO, 5, "Widget broken", "Fix it", &["actuo"], "me");
    lane.wait("a PR", 60, |l| {
        l.run_of(REPO, 5).is_some_and(|r| r[2] == "pr_opened")
    });

    // run.state notifications followed the run through its steps.
    let seen: Vec<(String, String)> = rpc
        .drain("run.state")
        .iter()
        .map(|r| {
            (
                r["state"].as_str().unwrap().into(),
                r["step"].as_str().unwrap().into(),
            )
        })
        .collect();
    for want in [
        ("queued", "prepare"),
        ("running", "turn"),
        ("checking", "checks"),
        ("running", "review"),
        ("running", "push"),
        ("pr_opened", "pr"),
    ] {
        assert!(
            seen.contains(&(want.0.into(), want.1.into())),
            "{want:?} not in {seen:?}"
        );
    }
    let run_id = lane.run_of(REPO, 5).unwrap()[0].clone();
    let detail = rpc.call("run/get", json!({"runId": run_id}))["run"].clone();
    assert_eq!(detail["checkRounds"], 1);
    assert_eq!(detail["startedBy"], "me");
    assert_eq!(detail["base"], "origin/main");
    let kinds: Vec<(String, Value)> = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["kind"].as_str().unwrap().into(), s["exitCode"].clone()))
        .collect();
    let want: Vec<(String, Value)> = [
        ("fetch", json!(0)),
        ("check", json!(1)),
        ("baseline", json!(0)),
        ("check", json!(0)),
        ("commit", json!(0)),
        ("push", json!(0)),
        ("pr", json!(0)),
    ]
    .iter()
    .map(|(k, c)| (k.to_string(), c.clone()))
    .collect();
    assert_eq!(kinds, want);
    // The same steps are in the session's log, after the clone.
    let session = detail["sessionId"].as_str().unwrap().to_string();
    rpc.call("session/subscribe", json!({"sessionId": session}));
    let head = rpc.call("session/get", json!({"sessionId": session}))["session"]["headSeq"]
        .as_u64()
        .unwrap();
    let events = rpc.events_until(head);
    let steps: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "run.step")
        .map(|e| e["data"]["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        steps,
        ["check", "baseline", "check", "commit", "push", "pr"]
    );
    let lane_turns = events
        .iter()
        .filter(|e| e["type"] == "turn.started" && e["data"]["client"] == "lane")
        .count();
    assert_eq!(lane_turns, 3, "first, check failure, review");

    let pr = lane.pr(REPO, 5).unwrap();
    assert_eq!(pr["title"], "Fix the widget");
    let body = pr["body"].as_str().unwrap();
    assert!(body.starts_with("It works now."), "{body}");
    let prompts = lane.prompts();
    let retry = prompts
        .iter()
        .find(|p| p.contains("failed with exit code"))
        .unwrap();
    assert!(
        retry.contains("The check `grep -q good fixed.txt` failed with exit code 1"),
        "{retry}"
    );
    assert_eq!(
        prompts
            .iter()
            .filter(|p| p.contains("Review your changes"))
            .count(),
        1
    );
    let head = lane.origin_ref(REPO, "refs/heads/actuo/issue-5").unwrap();
    assert_eq!(
        lane.origin_file(REPO, &head, "fixed.txt").unwrap(),
        "good\n"
    );
    assert!(lane.labels(REPO, 5).contains(&"actuo:pr".into()));
}

/// Checks still failing after the retries: a draft PR with the failure first, the
/// run waiting with the waiting label, and no review turn.
#[test]
fn checks_still_failing_after_the_retries() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #6 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "bad\n"}}]},
            {"match": "failed with exit code", "steps": [{"writeFile": {"path": "fixed.txt", "content": "still bad\n"}}]}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[(
            REPO,
            "checks = [\"echo checking; grep -q good fixed.txt\"]\nmax_check_retries = 1",
        )],
        "",
    );
    lane.commit_to_main(REPO, "fixed.txt", "good\n");
    lane.issue(REPO, 6, "Can't be fixed", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("a waiting run with a PR", 60, |l| {
        l.run_of(REPO, 6)
            .is_some_and(|r| r[2] == "waiting (checks_failed)")
            && l.pr(REPO, 6).is_some()
    });
    let pr = lane.pr(REPO, 6).unwrap();
    assert_eq!(pr["draft"], true);
    let body = pr["body"].as_str().unwrap();
    assert!(
        body.starts_with(
            "**The check `echo checking; grep -q good fixed.txt` still failed (exit 1).**"
        ),
        "{body}"
    );
    assert!(body.contains("checking"), "{body}");
    let labels = lane.labels(REPO, 6);
    assert!(labels.contains(&"actuo:waiting".into()) && !labels.contains(&"actuo:running".into()));
    let prompts = lane.prompts();
    assert_eq!(
        prompts
            .iter()
            .filter(|p| p.contains("failed with exit code"))
            .count(),
        1
    );
    assert!(!prompts.iter().any(|p| p.contains("Review your changes")));
    let comments = lane.comments(REPO, 6);
    assert!(
        comments
            .iter()
            .any(|c| c.contains("still failed (exit 1) after 1 retry")),
        "{comments:#?}"
    );
    // The same reason in words (`reason_text`); without the PR link or what to do.
    let text = reason_text(&lane, 6, REPO).unwrap();
    assert!(
        text.starts_with(
            "The check `echo checking; grep -q good fixed.txt` still failed (exit 1) after 1 retry."
        ) && !text.contains("actuo attach"),
        "{text}"
    );
}

/// `reasonText` of an issue's run, from `actuo show`.
fn reason_text(lane: &LaneEnv, issue: u64, repo: &str) -> Option<String> {
    let id = lane.run_of(repo, issue).unwrap()[0].clone();
    let run: Value = serde_json::from_str(&lane.env.ok(&["show", &id])).unwrap();
    run["reasonText"].as_str().map(str::to_string)
}

/// A request no rule covers pauses the run until another client answers it; a
/// setup that fails fails the run; a protected path pauses before the push.
#[test]
fn pauses_and_failures() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #7 in", "steps": [
                {"toolCall": {"id": "t1", "title": "Run command", "kind": "execute",
                              "rawInput": {"command": "python3 fix.py"}}},
                {"permission": {"toolCallId": "t1",
                    "options": [{"id": "allow_once", "name": "Yes", "kind": "allow_once"},
                                {"id": "cancel", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}},
                                {"toolUpdate": {"id": "t1", "status": "completed"}}]}}
            ]},
            {"match": "issue #8 in", "steps": [
                {"writeFile": {"path": "fixed.txt", "content": "ok\n"}},
                {"writeFile": {"path": ".github/workflows/ci.yml", "content": "on: push\n"}}
            ]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[
            (REPO, "checks = [\"test -f fixed.txt\"]"),
            ("o/broken", "setup = [\"echo boom; exit 3\"]"),
        ],
        "",
    );
    lane.issue(REPO, 7, "Needs a command", "x", &["actuo"], "me");
    lane.issue(REPO, 8, "Touches CI", "x", &["actuo"], "me");
    lane.issue("o/broken", 1, "Setup breaks", "x", &["actuo"], "me");
    lane.env.start_actuo();

    // The request pauses the run.
    lane.wait("a run waiting on a request", 60, |l| {
        l.run_of(REPO, 7)
            .is_some_and(|r| r[2] == "waiting (request)")
    });
    assert!(lane.labels(REPO, 7).contains(&"actuo:waiting".into()));
    let comments = lane.comments(REPO, 7);
    let ask = comments
        .iter()
        .find(|c| c.contains("The agent asks"))
        .unwrap();
    // A fixed phrase by kind and the command in a code block; not the tool call's
    // title.
    assert!(
        ask.contains("The agent asks to run a command:\n\n```\npython3 fix.py")
            && !ask.contains("Run command"),
        "{ask}"
    );
    let text = reason_text(&lane, 7, REPO).unwrap();
    assert!(
        text.starts_with("The agent asks to run a command:")
            && text.ends_with("No rule covers it, so the run waits for you."),
        "{text}"
    );
    let session = lane.run_of(REPO, 7).unwrap()[4].clone();
    let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &session])).unwrap();
    let request = detail["openRequests"][0]["requestId"]
        .as_str()
        .unwrap()
        .to_string();
    // Answered from another client (the CLI); the run goes on to a PR.
    lane.env.ok(&["respond", &session, &request, "allow_once"]);
    lane.wait("the PR after the answer", 60, |l| {
        l.run_of(REPO, 7).is_some_and(|r| r[2] == "pr_opened")
    });
    let labels = lane.labels(REPO, 7);
    assert!(labels.contains(&"actuo:pr".into()) && !labels.contains(&"actuo:waiting".into()));
    assert_eq!(reason_text(&lane, 7, REPO), None);

    // Setup failure: the run fails with the command's output.
    lane.wait("the setup failure", 60, |l| {
        l.run_of("o/broken", 1)
            .is_some_and(|r| r[2] == "failed (setup_failed)")
    });
    let c = lane.comments("o/broken", 1);
    let failed = c
        .iter()
        .find(|c| c.contains("The run failed (setup_failed)"))
        .unwrap();
    assert!(
        failed.contains("echo boom; exit 3")
            && failed.contains("exit 3")
            && failed.contains("boom"),
        "{failed}"
    );
    assert!(lane.labels("o/broken", 1).contains(&"actuo:failed".into()));
    let text = reason_text(&lane, 1, "o/broken").unwrap();
    assert!(
        text.contains("boom") && !text.contains("The run failed"),
        "{text}"
    );

    // A protected path pauses before anything is pushed.
    lane.wait("the protected-path pause", 60, |l| {
        l.run_of(REPO, 8)
            .is_some_and(|r| r[2] == "waiting (protected_path)")
    });
    assert!(lane.origin_ref(REPO, "refs/heads/actuo/issue-8").is_none());
    assert!(lane.pr(REPO, 8).is_none());
    let c = lane.comments(REPO, 8);
    assert!(
        c.iter()
            .any(|c| c.contains("need you:\n\n```\n.github/workflows/ci.yml\n```")),
        "{c:#?}"
    );
    let text = reason_text(&lane, 8, REPO).unwrap();
    assert!(
        text.starts_with("The change touches protected paths, which need you:")
            && !text.contains("The run is waiting for you"),
        "{text}"
    );
}

/// A check that already fails on main (the baseline: a check run at the run's
/// starting commit) isn't the agent's: no retry turn, the PR opens and says so. A
/// second run from the same commit reuses that check run.
#[test]
fn failures_already_on_main_arent_the_agents() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #7 in", "steps": [{"writeFile": {"path": "seven.txt", "content": "7\n"}}]},
            {"match": "issue #8 in", "steps": [{"writeFile": {"path": "eight.txt", "content": "8\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Title: Add a file\n\nAdded."}]}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[(REPO, "checks = [\"test ! -f broken.txt\"]")],
        "",
    );
    lane.commit_to_main(REPO, "broken.txt", "x\n");
    lane.issue(REPO, 7, "Add seven", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("a PR", 90, |l| {
        l.run_of(REPO, 7).is_some_and(|r| r[2] == "pr_opened")
    });
    assert!(
        !lane
            .prompts()
            .iter()
            .any(|p| p.contains("failed with exit code"))
    );
    let id = lane.run_of(REPO, 7).unwrap()[0].clone();
    let run: Value = serde_json::from_str(&lane.env.ok(&["show", &id])).unwrap();
    let kinds: Vec<&str> = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["fetch", "check", "baseline", "commit", "push", "pr"]
    );
    let body = lane.pr(REPO, 7).unwrap()["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        body.contains("- Check `test ! -f broken.txt`: failed (exit 1), already failing on the default branch at `"),
        "{body}"
    );
    let check_runs = || {
        std::fs::read_dir(lane.env.home.join("logs/checks"))
            .unwrap()
            .count()
    };
    assert_eq!(check_runs(), 1);

    lane.issue(REPO, 8, "Add eight", "x", &["actuo"], "me");
    lane.wait("a second PR", 90, |l| {
        l.run_of(REPO, 8).is_some_and(|r| r[2] == "pr_opened")
    });
    assert_eq!(check_runs(), 1, "the same commit's check run again");
}

/// By turbo package: the packages already failing on main are left out; a new one
/// goes to the agent, whose prompt says which were already failing.
#[test]
fn only_new_turbo_failures_go_to_the_agent() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #9 in", "steps": [{"writeFile": {"path": "b.txt", "content": "bad\n"}}]},
            {"match": "failed with exit code", "steps": [{"writeFile": {"path": "b.txt", "content": "fine\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Title: Fix b\n\nFixed."}]}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[(
            REPO,
            r#"checks = ['if grep -q bad b.txt 2>/dev/null; then echo " Failed:    @o/a#test, @o/b#test"; else echo " Failed:    @o/a#test"; fi; exit 1']"#,
        )],
        "",
    );
    lane.issue(REPO, 9, "Fix b", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("a PR", 90, |l| {
        l.run_of(REPO, 9).is_some_and(|r| r[2] == "pr_opened")
    });
    let prompts = lane.prompts();
    let retry = prompts
        .iter()
        .find(|p| p.contains("failed with exit code"))
        .unwrap();
    assert!(
        retry.contains(
            "@o/a already failed on the default branch before your change: leave it. Fix @o/b."
        ),
        "{retry}"
    );
    let body = lane.pr(REPO, 9).unwrap()["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        body.contains("already failing on the default branch at `"),
        "{body}"
    );
    // The package, as the PR shows it (actuo puts names in code spans).
    assert!(body.contains("): `@o/a`"), "{body}");
}

/// A check run at the run's starting commit from before the project's checks
/// changed doesn't have today's check: the baseline starts one with today's.
#[test]
fn a_baseline_from_before_the_checks_changed_runs_again() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #10 in", "steps": [{"writeFile": {"path": "ten.txt", "content": "10\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Title: Add ten\n\nAdded."}]}
        ]
    });
    let mut lane = LaneEnv::new(&script, &[(REPO, "checks = [\"true\"]")], "");
    lane.env.start_actuo();
    lane.env.ok(&["project", "check", REPO]);
    let check_runs = || {
        std::fs::read_dir(lane.env.home.join("logs/checks"))
            .unwrap()
            .count()
    };
    lane.wait("the first check run", 30, |l| {
        l.env.ok(&["project", "show", REPO]).contains("passed:")
    });
    assert_eq!(check_runs(), 1);
    lane.env.ok(&[
        "project",
        "set",
        REPO,
        "checks",
        r#"["echo fails on main too; exit 1"]"#,
    ]);
    lane.issue(REPO, 10, "Add ten", "x", &["actuo"], "me");
    lane.wait("a PR", 90, |l| {
        l.run_of(REPO, 10).is_some_and(|r| r[2] == "pr_opened")
    });
    assert!(
        !lane
            .prompts()
            .iter()
            .any(|p| p.contains("failed with exit code"))
    );
    assert_eq!(check_runs(), 2, "a check run with today's checks");
}

/// The agent gets a failing check's failing lines, not its last ones: here the
/// failure comes before 300 lines of passing tests.
#[test]
fn the_agent_gets_the_failing_lines() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #11 in", "steps": [{"writeFile": {"path": "b.txt", "content": "bad\n"}}]},
            {"match": "failed with exit code", "steps": [{"writeFile": {"path": "b.txt", "content": "fine\n"}}]},
            {"match": "Review your changes", "steps": [{"message": "Title: Fix b\n\nFixed."}]}
        ]
    });
    let check = r#"grep -q bad b.txt 2>/dev/null || exit 0; echo "@o/a:test:  FAIL  a.test.ts > breaks"; echo "@o/a:test: AssertionError: expected 1 to be 2"; for i in $(seq 300); do echo "@o/b:test:  ✓ t$i"; done; echo "Failed:    @o/a#test"; exit 1"#;
    let mut lane = LaneEnv::new(&script, &[(REPO, &format!("checks = ['{check}']"))], "");
    lane.issue(REPO, 11, "Fix b", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("a PR", 90, |l| {
        l.run_of(REPO, 11).is_some_and(|r| r[2] == "pr_opened")
    });
    let prompts = lane.prompts();
    let retry = prompts
        .iter()
        .find(|p| p.contains("failed with exit code"))
        .unwrap();
    assert!(
        retry.contains("Its failing lines (the rest of its output left out) are between"),
        "{retry}"
    );
    assert!(
        retry.contains(
            "── @o/a (test) ──\n FAIL  a.test.ts > breaks\nAssertionError: expected 1 to be 2"
        ),
        "{retry}"
    );
    // The passing tests' lines (the command itself, quoted above, says `✓ t$i`).
    assert!(!retry.contains("✓ t1\n"), "{retry}");
}

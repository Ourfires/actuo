//! Projects: a project with its lane off starts no run and
//! refuses `actuo run`; a project's own trigger label and level apply to its runs.

mod common;

use common::lane::LaneEnv;
use serde_json::json;

const ON: &str = "o/on";
const OFF: &str = "o/off";

#[test]
fn a_projects_lane_off_and_its_own_label() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let mut lane = LaneEnv::new(
        &script,
        &[
            (
                ON,
                "label = \"go\"\nlevel = \"ask\"\nchecks = [\"test -f fixed.txt\"]",
            ),
            (OFF, "lane = false\nchecks = [\"true\"]"),
        ],
        "",
    );
    // `go` starts runs on o/on; `actuo` (the lane's label) doesn't any more.
    lane.issue(ON, 1, "Fix it", "x", &["go"], "me");
    lane.issue(ON, 2, "Not this", "x", &["actuo"], "me");
    lane.issue(OFF, 1, "Off", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("o/on#1's PR", 60, |l| {
        l.run_of(ON, 1).is_some_and(|r| r[2] == "pr_opened")
    });
    assert!(lane.run_of(ON, 2).is_none());
    assert!(lane.run_of(OFF, 1).is_none());
    // The project's level reached the run.
    let run = lane.run_of(ON, 1).unwrap();
    let detail: serde_json::Value = serde_json::from_str(&lane.env.ok(&["show", &run[0]])).unwrap();
    assert_eq!(detail["level"], "ask");

    let e = lane
        .env
        .fails(&["run", &format!("https://github.com/{OFF}/issues/1")]);
    assert!(e.contains("lane is off"), "{e}");
}

/// `actuo project add`: the access check, the clone, the proposal from the repo's
/// files, saved with `--yes`; refused without push access, for a missing repo, for
/// a project already here. `set` and `remove` (with the clone).
#[test]
fn adding_and_removing_a_project() {
    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(ON, "checks = [\"true\"]")],
        "",
    );
    lane.github_repo(
        "o/new",
        &[
            ("package.json", r#"{"packageManager": "pnpm@10.33.0"}"#),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'\npackages:\n  pkg:\n    resolution: {tarball: https://downloads.example.com/pkg.tgz}"),
            ("pnpm-workspace.yaml", "packages:\n  - apps/*\n"),
            ("turbo.json", "{}"),
            (
                "apps/web/package.json",
                r#"{"name": "web", "scripts": {"lint": "x", "test": "x"}}"#,
            ),
        ],
        json!({}),
    );
    lane.github_repo(
        "o/readonly",
        &[("README.md", "x")],
        json!({"permission": "READ"}),
    );
    lane.env.start_actuo();

    let out = lane.env.ok(&["project", "add", "o/new", "--yes"]);
    assert!(
        out.contains("o/new: ADMIN access, private repo, default branch main (not protected)"),
        "{out}"
    );
    assert!(out.contains("detected: pnpm 10.33.0, turbo"), "{out}");
    assert!(out.contains("added o/new"), "{out}");
    assert!(out.contains("its lane is off"), "{out}");
    // The proposal shows only what differs from the defaults.
    assert!(!out.contains("setup_time_limit"), "{out}");
    // Commands follow the repo; the reviewed network proposal is saved.
    assert!(out.contains("pnpm install --frozen-lockfile"), "{out}");
    assert!(out.contains("pnpm turbo test --affected"), "{out}");
    let shown = lane.env.ok(&["project", "show", "o/new"]);
    assert!(
        shown.contains("# setup, checks: detected from the repo at each run"),
        "{shown}"
    );
    let file = std::fs::read_to_string(lane.env.home.join("settings.toml")).unwrap();
    assert!(file.contains("[projects.\"o/new\"]"), "{file}");
    assert!(!file.contains("setup = "), "{file}");
    assert!(file.contains("codeload.github.com"), "{file}");
    assert!(file.contains("downloads.example.com"), "{file}");
    // Only what differs from the defaults is written.
    assert!(!file.contains("setup_time_limit"), "{file}");
    let clone = lane.env.home.join("repos/o/new");
    assert!(clone.join("pnpm-workspace.yaml").exists());
    assert!(
        lane.env
            .ok(&["project", "list"])
            .contains("o/new\tlane off\tlabel actuo")
    );
    assert!(file.contains("lane = false"), "{file}");

    for (repo, kind) in [
        ("o/readonly", "no_push_access"),
        ("o/missing", "repo_not_found"),
        (ON, "project_exists"),
    ] {
        let e = lane.env.fails(&["project", "add", repo, "--yes"]);
        assert!(e.contains(kind), "{repo}: {e}");
    }

    assert_eq!(
        lane.env.ok(&["project", "set", "o/new", "lane", "true"]),
        "true\n"
    );
    assert!(lane.env.ok(&["project", "list"]).contains("o/new\tlane on"));
    // --keep-clone: the clone stays.
    let out = lane.env.ok(&["project", "remove", "o/new", "--keep-clone"]);
    assert!(out.contains("removed o/new"), "{out}");
    assert!(!lane.env.ok(&["project", "list"]).contains("o/new"));
    assert!(clone.join("pnpm-workspace.yaml").exists());
    // Added again (over the kept clone), then removed: by default the clone goes.
    lane.env.ok(&["project", "add", "o/new", "--yes"]);
    let out = lane.env.ok(&["project", "remove", "o/new"]);
    assert!(out.contains("its clone is deleted"), "{out}");
    assert!(!lane.env.ok(&["project", "list"]).contains("o/new"));
    assert!(!clone.exists());
    assert!(
        lane.env
            .fails(&["project", "remove", "o/new"])
            .contains("project_not_found")
    );
}

/// `actuo project check`: setup, then each check, in the sandbox, measured; a
/// failing check fails the run and the rest still run; "affected" is everything
/// (`TURBO_SCM_BASE` is the first commit); the worktree goes after.
#[test]
fn a_check_run() {
    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(
            ON,
            r#"setup = ["echo installing; sleep 1.5"]
checks = ["test \"$TURBO_SCM_BASE\" = \"$(git rev-list --max-parents=0 HEAD)\"", "echo broken; exit 3", "test -f README.md"]"#,
        )],
        "",
    );
    lane.env.start_actuo();
    let out = lane.env.run(&["project", "check", ON]);
    assert!(!out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.contains("1. setup `echo installing; sleep 1.5`: ok in 1."),
        "{out}"
    );
    assert!(out.contains(", peak "), "{out}");
    assert!(out.contains("2. check `test"), "{out}");
    assert!(
        out.contains("3. check `echo broken; exit 3`: failed (exit 3)"),
        "{out}"
    );
    assert!(out.contains("   | broken"), "{out}");
    assert!(out.contains("4. check `test -f README.md`: ok"), "{out}");
    assert!(out.contains("failed: 4 steps in"), "{out}");
    // The second step passed: TURBO_SCM_BASE was the first commit.
    assert!(
        !out.contains(
            "2. check `test \"$TURBO_SCM_BASE\" = \"$(git rev-list --max-parents=0 HEAD)\"`: failed"
        ),
        "{out}"
    );
    let shown = lane.env.ok(&["project", "show", ON]);
    assert!(shown.contains("last check run"), "{shown}");
    // The worktree goes after the check run has ended.
    let checks = lane.env.home.join("checks");
    lane.wait("the check run's worktree removed", 10, |_| {
        std::fs::read_dir(&checks).map_or(true, |mut d| d.next().is_none())
    });
}

/// A check that fails only because the sandbox refuses Unix sockets (fake-srt tells
/// the command when they're allowed): the check run's second try finds it, names
/// the package and offers the fixes. A check that fails for its own reason gets no
/// diagnosis.
#[test]
fn a_check_run_tells_why() {
    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(
            ON,
            r#"checks = ["test \"$FAKE_SRT_UNIX_SOCKETS\" = 1 || { echo ' Failed:    @o/needs-socket#test'; exit 1; }", "echo nope; exit 2"]"#,
        )],
        "",
    );
    lane.env.start_actuo();
    let out = lane.env.run(&["project", "check", ON]);
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.contains("why: the sandbox refuses Unix sockets, and this needs one; failing for it: @o/needs-socket; with Unix sockets allowed it passed"),
        "{out}"
    );
    assert!(
        out.contains("- Exclude them from this check: add `--filter=!@o/needs-socket` to it."),
        "{out}"
    );
    assert!(out.contains("checks_unix_sockets true"), "{out}");
    // The second check fails for its own reason: no diagnosis under it.
    let second = out.split("2. check `echo nope; exit 2`").nth(1).unwrap();
    assert!(!second.contains("why:"), "{out}");

    // With the project's sockets on, the check passes the first time.
    lane.env
        .ok(&["project", "set", ON, "checks_unix_sockets", "true"]);
    let out = lane.env.run(&["project", "check", ON]);
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(out.contains("1. check `test"), "{out}");
    assert!(out.contains("`: ok in"), "{out}");
}

/// `actuo/drain` (an update about to restart actuo): while a connection holds
/// actuo, a new check run is refused; the call names the check run still in
/// progress at its limit, and answers with nothing once it has finished. The hold
/// ends with the connection.
#[test]
fn an_update_waits_for_a_check_run() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(
            ON,
            r#"setup = ["sleep 3"]
checks = ["true"]"#,
        )],
        "",
    );
    lane.env.start_actuo();
    let mut check = Command::new(env!("CARGO_BIN_EXE_actuo"))
        .args(["project", "check", ON])
        .env("ACTUO_HOME", &lane.env.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // The check run has started once the store has it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !lane
        .env
        .ok(&["project", "show", ON])
        .contains("last check run")
    {
        assert!(std::time::Instant::now() < deadline, "no check run");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let mut stdio = Command::new(env!("CARGO_BIN_EXE_actuo"))
        .arg("stdio")
        .env("ACTUO_HOME", &lane.env.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = stdio.stdin.take().unwrap();
    let mut lines = BufReader::new(stdio.stdout.take().unwrap()).lines();
    let mut answer = |id: u64| -> serde_json::Value {
        loop {
            let v: serde_json::Value =
                serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    };
    let mut call = |id: u64, method: &str, params: serde_json::Value| {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{line}").unwrap();
    };
    call(
        1,
        "initialize",
        json!({"protocolVersion": 1, "client": {"name": "test", "version": "0"}}),
    );
    call(2, "actuo/drain", json!({"limitS": 0}));
    let held = answer(2);
    let in_progress = held["result"]["inProgress"].as_array().unwrap();
    assert_eq!(in_progress.len(), 1, "{held}");
    assert!(
        in_progress[0].as_str().unwrap().starts_with("check run "),
        "{held}"
    );
    // Held: no new check run.
    let refused = lane.env.fails(&["project", "check", ON]);
    assert!(refused.contains("about to restart"), "{refused}");
    call(3, "actuo/drain", json!({"limitS": 30}));
    let done = answer(3);
    assert_eq!(done["result"]["inProgress"], json!([]), "{done}");
    assert!(check.wait().unwrap().success());

    // The connection ends: the hold with it.
    drop(stdin);
    stdio.wait().unwrap();
    let out = lane.env.ok(&["project", "check", ON]);
    assert!(out.contains("passed: 2 steps"), "{out}");
}

/// A repo actuo's gh login can't see yet, with a pending invitation to it:
/// adding it accepts that invitation (only that one) and goes on; without one, the
/// refusal names the login to give access to. `actuo status` shows the login.
#[test]
fn adding_a_project_accepts_its_invitation() {
    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(ON, "checks = [\"true\"]")],
        "",
    );
    lane.github_repo("o/invited", &[("README.md", "x")], json!({"hidden": true}));
    lane.github_repo("o/other", &[("README.md", "x")], json!({"hidden": true}));
    lane.github_repo(
        "o/uninvited",
        &[("README.md", "x")],
        json!({"hidden": true}),
    );
    lane.edit_gh(|v| {
        v["invitations"] = json!([{"id": 7, "repo": "o/invited"}, {"id": 8, "repo": "o/other"}]);
    });
    lane.env.start_actuo();

    let login = lane.env.ok(&["status"]);
    let login = login
        .lines()
        .find_map(|l| l.strip_prefix("github\t"))
        .unwrap()
        .to_string();
    assert!(!login.is_empty());

    let out = lane.env.ok(&["project", "add", "o/invited", "--yes"]);
    assert!(
        out.contains("accepted actuo's invitation to o/invited"),
        "{out}"
    );
    assert!(out.contains("o/invited: WRITE access"), "{out}");
    assert!(out.contains("added o/invited"), "{out}");
    // Only that invitation.
    let left: Vec<u64> = lane.gh_state()["invitations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_u64().unwrap())
        .collect();
    assert_eq!(left, vec![8]);

    let e = lane.env.fails(&["project", "add", "o/uninvited", "--yes"]);
    assert!(e.contains("repo_not_found"), "{e}");
    assert!(
        e.contains(&format!("Give {login} write access to o/uninvited")),
        "{e}"
    );
}

/// A project with nothing set, on a repo with no lockfile yet (an empty site the
/// agent will scaffold): no setup runs, the first prompt says nothing was
/// installed, and the agent's session reaches npm's registry (to scaffold and
/// install) though `agent_network` is off.
#[test]
fn a_new_repo_is_detected_at_each_run() {
    const NEW: &str = "o/site";
    let mut lane = LaneEnv::new(
        &json!({
            "resume": true,
            "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
            "turns": [
                {"match": "issue #1 in", "steps": [{"writeFile": {"path": "index.html", "content": "hi\n"}}]},
                {"match": "Review your changes", "steps": [{"message": "Title: Add a page\n\nAdds it."}]}
            ]
        }),
        &[(NEW, "")],
        "",
    );
    lane.issue(NEW, 1, "Create the site", "Scaffold it.", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("pr_opened", 60, |l| {
        l.run_of(NEW, 1).is_some_and(|r| r[2] == "pr_opened")
    });
    let run = lane.run_of(NEW, 1).unwrap();
    let session = lane.srt_calls(&run[4]);
    let hosts = session[0]["settings"]["network"]["allowedDomains"].to_string();
    assert!(hosts.contains("registry.npmjs.org"), "{hosts}");
    assert!(lane.srt_calls(&format!("{}-setup", run[0])).is_empty());
    let first = lane.prompts().into_iter().next().unwrap();
    assert!(
        first.contains("Nothing was installed: the project has no setup yet"),
        "{first}"
    );
    assert!(first.contains("actuo will run: nothing."), "{first}");
}

/// The change-size limits leave lockfiles out: a scaffold's 3,000-line
/// pnpm-lock.yaml doesn't pause a run. A change over the limits pauses it, and
/// the comment says how to go on (`actuo run resume`, or reply and add the label again).
#[test]
fn lockfiles_dont_count_toward_the_limits() {
    let lock: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    let big: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let mut lane = LaneEnv::new(
        &json!({
            "resume": true,
            "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
            "turns": [
                {"match": "issue #1 in", "steps": [
                    {"writeFile": {"path": "pnpm-lock.yaml", "content": lock}},
                    {"writeFile": {"path": "index.html", "content": "hi\n"}}
                ]},
                {"match": "issue #2 in", "steps": [{"writeFile": {"path": "big.txt", "content": big}}]},
                {"match": "Review your changes", "steps": [{"message": "Title: A change\n\nIt."}]}
            ]
        }),
        &[(ON, "checks = []\nmax_changed_lines = 100")],
        "",
    );
    lane.issue(ON, 1, "Scaffold", "x", &["actuo"], "me");
    lane.issue(ON, 2, "Big", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("both", 90, |l| {
        l.run_of(ON, 1).is_some_and(|r| r[2] == "pr_opened")
            && l.run_of(ON, 2)
                .is_some_and(|r| r[2] == "waiting (diff_too_large)")
    });
    let paused = lane.comments(ON, 2);
    let paused = paused.last().unwrap();
    assert!(
        paused.contains("The change is over the limits: 1 file (limit 50), 200 lines (limit 100)."),
        "{paused}"
    );
    assert!(
        paused
            .contains("To go on past this for this run: hand the run back with `actuo run resume "),
        "{paused}"
    );
    assert!(
        paused.contains("reply here and add the `actuo` label again"),
        "{paused}"
    );
}

/// `allow-host` adds to the saved hosts and can be run again; a failed check
/// after it keeps the host and changes no other project.
#[test]
fn allow_host_keeps_the_hosts_and_reruns_checks() {
    let mut lane = LaneEnv::new(
        &json!({"resume": true, "turns": []}),
        &[(
            OFF,
            "lane = false\nnetwork = [\"registry.npmjs.org\", \"custom.example.com\"]\nsetup = [\"false\"]\nchecks = [\"true\"]",
        )],
        "",
    );
    lane.env.start_actuo();
    let result = lane.env.run(&[
        "project",
        "allow-host",
        OFF,
        "codeload.github.com",
        "--check",
    ]);
    assert!(!result.status.success());
    let error = String::from_utf8(result.stdout).unwrap();
    // The host is reported saved before the failed check.
    assert!(error.contains("saved on this actuo"), "{error}");
    let shown = lane.env.ok(&["project", "show", OFF]);
    for host in [
        "registry.npmjs.org",
        "custom.example.com",
        "codeload.github.com",
    ] {
        assert!(shown.contains(host), "{shown}");
    }
    lane.env
        .ok(&["project", "allow-host", OFF, "CODELOAD.GITHUB.COM"]);
    let file = std::fs::read_to_string(lane.env.home.join("settings.toml")).unwrap();
    assert_eq!(file.matches("codeload.github.com").count(), 1);
    for invalid in [
        "*.github.com",
        "https://github.com",
        "127.0.0.1",
        "localhost",
    ] {
        assert!(
            lane.env
                .fails(&["project", "allow-host", OFF, invalid])
                .contains("invalid_params")
        );
    }
    assert!(
        lane.env
            .fails(&["project", "allow-host", "o/missing", "example.com"])
            .contains("project_not_found")
    );
    assert_eq!(
        std::fs::read_to_string(lane.env.home.join("settings.toml")).unwrap(),
        file
    );
}

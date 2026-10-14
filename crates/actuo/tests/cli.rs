//! The CLI against a real `actuo serve`.

mod common;

use common::{Attach, Env};
use serde_json::{Value, json};

fn script() -> Value {
    json!({
        "loadSession": true,
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "configOptions": [{"id": "model", "name": "Model", "values": ["small", "large"], "current": "small"}],
        "turns": [
            {"match": "notes", "steps": [
                {"message": "Writing NOTES.md"},
                {"toolCall": {"id": "t1", "title": "Write NOTES.md", "kind": "edit",
                              "diff": {"path": "NOTES.md", "newText": "notes\n"}}},
                {"permission": {"toolCallId": "t1",
                    "options": [{"id": "allow-once", "name": "Yes", "kind": "allow_once"},
                                {"id": "reject", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "NOTES.md", "content": "notes\n"}},
                                {"toolUpdate": {"id": "t1", "status": "completed"}}]}}
            ]}
        ]
    })
}

#[test]
fn commands() {
    let mut env = Env::new(&script());
    assert!(
        env.fails(&["sessions"])
            .contains("start it with `actuo service install`")
    );
    env.start_actuo();

    assert!(env.ok(&["agents"]).lines().any(|l| l.starts_with("fake\t")));
    assert!(
        env.ok(&["agents"])
            .lines()
            .any(|l| l.starts_with("claude\t0.84.0\t") && l.contains(
                "agents/@agentclientprotocol/claude-agent-acp@0.84.0/node_modules/.bin/claude-agent-acp"
            ))
    );

    let id = env
        .ok(&[
            "start",
            env.repo.to_str().unwrap(),
            "--agent",
            "fake",
            "--level",
            "edits",
            "--config",
            "model=large",
            "--prompt",
            "hello",
        ])
        .trim()
        .to_string();
    let short = &id[..13];

    // Attach replays from the start and streams; a typed line is a prompt, and a
    // number answers the open permission request.
    let mut attach = Attach::start(&env, short);
    let agent = attach.wait_for("fake-agent");
    assert!(
        agent.contains("mode edits") && agent.contains("model=large"),
        "{agent}"
    );
    attach.wait_for("t1 > hello");
    attach.wait_for("t1 agent: ok");
    attach.wait_for("t1 turn ended: end_turn");
    attach.type_line("write notes");
    attach.wait_for("t2 PERMISSION 2.r1");
    attach.wait_for("1. allow-once [allow_once] Yes");
    attach.wait_for("answer 2.r1 with a number");
    attach.type_line("1");
    attach.wait_for("t2 request 2.r1 allowed once (allow-once) by client");
    let ended = attach.wait_for("t2 turn ended");
    assert!(ended.contains("files: added NOTES.md"), "{ended}");

    // A second attach replays the answered request without asking about it.
    let mut replay = Attach::start(&env, short);
    replay.wait_for("t2 turn ended");
    assert!(
        !replay.seen.iter().any(|l| l.contains("answer 2.r1")),
        "{:#?}",
        replay.seen
    );
    drop(replay);

    let sessions = env.ok(&["sessions"]);
    assert!(sessions.contains(&id) && sessions.contains("idle") && sessions.contains("2 turns"));

    // --json: actuo's replies as the protocol has them.
    let json = |args: &[&str]| -> Value { serde_json::from_str(&env.ok(args)).unwrap() };
    assert_eq!(
        json(&["sessions", "--json"])["sessions"][0]["id"],
        id.as_str()
    );
    assert_eq!(
        json(&["checkpoints", short, "--json"])["checkpoints"][2]["files"][0]["path"],
        "NOTES.md"
    );
    assert!(json(&["status", "--json"])["actuo"]["commit"].is_string());
    assert!(
        json(&["agents", "--json"])["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "fake")
    );
    assert_eq!(json(&["settings", "get", "lane.max_runs", "--json"]), 2);
    assert_eq!(
        json(&["settings", "get", "--json"])["sessions"]["agent"],
        "claude"
    );
    let detail: Value = serde_json::from_str(&env.ok(&["show", short])).unwrap();
    let cwd = std::path::PathBuf::from(detail["cwd"].as_str().unwrap());
    assert!(cwd.join("NOTES.md").exists());

    // The session's changes, and its checkpoints with what each turn changed.
    let diff = env.ok(&["diff", short]);
    assert!(
        diff.contains("+++ b/NOTES.md") && diff.contains("+notes"),
        "{diff}"
    );
    let checkpoints = env.ok(&["checkpoints", short]);
    let lines: Vec<_> = checkpoints.lines().collect();
    assert_eq!(lines.len(), 3, "{checkpoints}");
    assert!(
        lines[0].starts_with("0\t") && lines[2].starts_with("2\t"),
        "{checkpoints}"
    );
    assert!(lines[2].ends_with("\tadded NOTES.md"), "{checkpoints}");

    assert!(
        env.ok(&["revert", short, "2"])
            .contains("deleted [\"NOTES.md\"]")
    );
    assert!(!cwd.join("NOTES.md").exists());
    assert_eq!(env.ok(&["send", short, "hi again"]).trim(), "turn 3");
    attach.wait_for("t3 turn ended");

    assert!(
        env.fails(&["send", "nope", "x"])
            .contains("no session nope")
    );
    assert!(
        env.fails(&["interrupt", short])
            .contains("no turn is running")
    );
    assert!(
        env.fails(&["respond", short, "3.r1", "allow-once"])
            .contains("no open request")
    );
    env.ok(&["stop", short]);
    attach.wait_for("state ended (stopped)");
    assert!(env.fails(&["send", short, "x"]).contains("session_ended"));
}

/// `actuo settings unset` removes a key from the file: its default applies again,
/// and a key without one is gone.
#[test]
fn settings_unset() {
    let mut env = Env::new(&script());
    env.start_actuo();
    let path = env.home.join("settings.toml");

    env.ok(&["settings", "set", "lane.max_runs", "3"]);
    env.ok(&[
        "settings",
        "set",
        "lane.rules.fake",
        "[{kind = \"execute\", answer = [\"allow_once\"]}]",
    ]);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("max_runs = 3") && text.contains("fake = ["),
        "{text}"
    );

    assert_eq!(env.ok(&["settings", "unset", "lane.max_runs"]), "2\n");
    assert_eq!(env.ok(&["settings", "unset", "lane.rules.fake"]), "");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("max_runs") && !text.contains("fake"),
        "{text}"
    );
    // A table the removal left empty goes too: no stray `[lane.rules]` header.
    assert!(
        !text.contains("[lane.rules]") && !text.contains("rules"),
        "{text}"
    );
    assert!(!env.ok(&["settings", "get", "lane.rules"]).contains("fake"));
    // A key that isn't there: nothing to remove, still fine.
    assert_eq!(env.ok(&["settings", "unset", "lane.max_runs"]), "2\n");

    env.stop_actuo(false);
    env.start_actuo();
    assert_eq!(env.ok(&["settings", "get", "lane.max_runs"]), "2\n");
}

/// `actuo settings get` and `set` change the default agent, poll interval, runs at
/// once and a limit, and the change persists across a restart.
#[test]
fn settings() {
    let mut env = Env::new(&script());
    env.start_actuo();

    assert_eq!(env.ok(&["settings", "get", "lane.max_runs"]), "2\n");
    assert_eq!(
        env.ok(&["settings", "get", "sessions.agent"]),
        "\"claude\"\n"
    );
    let all = env.ok(&["settings", "get"]);
    assert!(
        all.contains("[lane.labels]") && all.contains("trigger = \"actuo\""),
        "{all}"
    );

    assert_eq!(
        env.ok(&["settings", "set", "sessions.agent", "fake"]),
        "\"fake\"\n"
    );
    assert_eq!(
        env.ok(&["settings", "set", "lane.poll_interval", "30s"]),
        "\"30s\"\n"
    );
    assert_eq!(env.ok(&["settings", "set", "lane.max_runs", "3"]), "3\n");
    assert_eq!(
        env.ok(&["settings", "set", "lane.turn_time_limit", "45m"]),
        "\"45m\"\n"
    );
    env.ok(&["settings", "set", "defaults.fake.model", "large"]);
    // Any of actuo's agents may be a project's.
    env.ok(&["settings", "set", "projects.\"o/r\".agent", "claude"]);
    env.ok(&[
        "settings",
        "set",
        "projects.\"o/r\".config",
        "{ effort = \"low\" }",
    ]);
    let repo = env.ok(&[
        "settings",
        "set",
        "projects.\"o/r\".checks",
        "[\"pnpm test\"]",
    ]);
    assert_eq!(repo, "[\"pnpm test\"]\n");

    let e = env.fails(&["settings", "set", "lane.max_runs", "0"]);
    assert!(
        e.contains("invalid_settings") && e.contains("lane.max_runs"),
        "{e}"
    );
    let e = env.fails(&["settings", "set", "projects.\"o/r\".agent", "nope"]);
    assert!(
        e.contains("projects.o/r.agent") && e.contains("no agent named nope"),
        "{e}"
    );
    let e = env.fails(&[
        "settings",
        "set",
        "projects.\"o/r\".config.mode",
        "\"plan\"",
    ]);
    assert!(e.contains("projects.o/r.config.mode"), "{e}");
    assert!(
        env.fails(&["settings", "get", "lane.nope"])
            .contains("no setting lane.nope")
    );

    // A session without --agent gets sessions.agent, with the agent's [defaults].
    let id = env
        .ok(&["start", env.repo.to_str().unwrap(), "--prompt", "hello"])
        .trim()
        .to_string();
    let detail: Value = serde_json::from_str(&env.ok(&["show", &id])).unwrap();
    assert_eq!(detail["agent"], "fake");
    assert_eq!(detail["config"], json!({"model": "large"}));
    let mut agents = env.ok(&["agents"]);
    assert!(agents.lines().any(|l| l.starts_with("fake\t")), "{agents}");
    // Once its session is up: the options it advertised; Claude's built-in lane rules.
    let started = std::time::Instant::now();
    while !agents.contains("\n  option model: small, large\n")
        && started.elapsed() < std::time::Duration::from_secs(10)
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        agents = env.ok(&["agents"]);
    }
    assert!(
        agents.contains("\n  option model: small, large\n"),
        "{agents}"
    );
    assert!(
        agents.contains("  built-in lane rule: kind = \"execute\", answer = [\"allow_once\"]\n"),
        "{agents}"
    );
    // Asked model by model.
    let probed = env.ok(&["agents", "probe", "fake"]);
    assert!(probed.starts_with("fake: 2 models, probed "), "{probed}");
    let agents_now = env.ok(&["agents"]);
    assert!(
        agents_now.contains("\n  with model large: \n"),
        "{agents_now}"
    );

    // Edited by hand while actuo runs: kept by the next update.
    let path = env.home.join("settings.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n# mine\n");
    std::fs::write(&path, &text).unwrap();
    env.ok(&["settings", "set", "lane.max_changed_files", "40"]);
    assert!(std::fs::read_to_string(&path).unwrap().contains("# mine"));

    env.stop_actuo(false);
    env.start_actuo();
    assert_eq!(env.ok(&["settings", "get", "sessions.agent"]), "\"fake\"\n");
    assert_eq!(
        env.ok(&["settings", "get", "lane.poll_interval"]),
        "\"30s\"\n"
    );
    assert_eq!(env.ok(&["settings", "get", "lane.max_runs"]), "3\n");
    assert_eq!(
        env.ok(&["settings", "get", "lane.turn_time_limit"]),
        "\"45m\"\n"
    );
    assert_eq!(
        env.ok(&["settings", "get", "lane.max_changed_files"]),
        "40\n"
    );
    let repo = env.ok(&["settings", "get", "projects.\"o/r\""]);
    assert!(repo.contains("checks = [\"pnpm test\"]"), "{repo}");
}

/// `actuo agents install` ends with the probe sessions get: `true` inside srt with
/// `~` denied and srt's own seccomp helper dir re-allowed (on Linux srt runs it
/// inside the sandbox).
#[test]
fn agents_install_ends_with_the_probe() {
    // Both built-in agents run a command of their own: nothing to install with npm.
    let env = Env::with_agents(&[("claude", script()), ("codex", script())]);
    let srt_dir = env
        .home
        .join("agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/.bin");
    std::fs::create_dir_all(&srt_dir).unwrap();
    std::os::unix::fs::symlink(common::lane::fake_srt(), srt_dir.join("srt")).unwrap();

    let out = env.ok(&["agents", "install"]);
    assert!(out.ends_with("srt probe\tok\n"), "{out}");
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(env.home.join("sandbox/probe.json")).unwrap(),
    )
    .unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(settings["filesystem"]["denyRead"][0], json!(home));
    let helper = env
        .home
        .join("agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/@anthropic-ai/sandbox-runtime/vendor/seccomp");
    assert_eq!(
        settings["filesystem"]["allowRead"],
        json!([helper.display().to_string()])
    );
    let calls = std::fs::read_to_string(env.home.join("sandbox/probe.json.calls.jsonl")).unwrap();
    let call: Value = serde_json::from_str(calls.lines().last().unwrap()).unwrap();
    assert_eq!(call["argv"], json!(["true"]));
}

/// With nothing reading its stdout any more (`actuo agents | head -1` once `head`
/// has quit), a command ends quietly with exit 0 instead of panicking, with or
/// without an actuo to ask.
#[test]
fn a_closed_stdout_ends_quietly() {
    let mut env = Env::new(&script());
    let closed = |env: &Env, args: &[&str]| {
        // The read end is gone before the command starts: every write fails.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_actuo"))
            .args(args)
            .env("ACTUO_HOME", &env.home)
            .stdin(std::process::Stdio::null())
            .stdout(writer)
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "actuo {args:?}: {err}");
        assert!(
            !err.contains("panicked") && !err.contains("Broken pipe"),
            "actuo {args:?}: {err}"
        );
    };
    // `print!`, the settings file read directly.
    closed(&env, &["settings", "get"]);
    env.start_actuo();
    // `println!`, through actuo.
    closed(&env, &["agents"]);
    closed(&env, &["settings", "get"]);
}

/// `actuo serve` that can't start and wouldn't by trying again (another actuo on
/// the socket, settings that don't validate) exits with the code the service
/// doesn't restart on: 3 (the systemd unit's `RestartPreventExitStatus`), 0 under
/// the launch agent (launchd's `KeepAlive` restarts on anything else).
#[test]
fn serve_that_wont_start_stays_stopped() {
    let stays = 3;
    let mut env = Env::new(&script());
    env.start_actuo();
    let out = env.run(&["serve"]);
    assert_eq!(out.status.code(), Some(stays));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("an actuo is already listening on")
            && stderr.contains("not restarting until it's fixed"),
        "{stderr}"
    );

    let other = Env::new(&script());
    std::fs::write(other.home.join("settings.toml"), "[sessions]\nagent = 3\n").unwrap();
    let out = other.run(&["serve"]);
    assert_eq!(out.status.code(), Some(stays));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("settings.toml"), "{stderr}");
    // As launchd starts it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_actuo"))
        .arg("serve")
        .env("ACTUO_HOME", &other.home)
        .env("ACTUO_AGENTS_ENV", other.root.join("agents.env"))
        .env("XPC_SERVICE_NAME", "co.ourfires.actuo")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

/// `actuo settings keys`: every key with its default and doc, without an actuo;
/// each actuo-wide key it lists is one `settings get` knows.
#[test]
fn settings_keys() {
    let env = Env::new(&script());
    let keys = env.ok(&["settings", "keys"]);
    for line in [
        "sessions.agent = \"claude\"\n",
        "lane.max_runs = 2\n  Runs at once; paused runs don't count.\n",
        "lane.labels.trigger = \"actuo\"\n",
        "projects.\"owner/repo\".lane = false\n",
        "projects.\"owner/repo\".level  (unset: lane.level applies)\n",
        "projects.\"owner/repo\".checks  (unset)\n",
        "projects.\"owner/repo\".read = []\n",
    ] {
        assert!(keys.contains(line), "{line:?} in:\n{keys}");
    }
    for key in keys
        .lines()
        .filter(|l| !l.starts_with(' ') && !l.starts_with("projects."))
        .filter_map(|l| l.split(" = ").next())
    {
        env.ok(&["settings", "get", key]);
    }
}

/// `actuo settings edit` with an editor script that writes the file it's given:
/// saved only when valid, again after a no, applied by a running actuo.
#[test]
fn settings_edit() {
    let mut env = Env::new(&script());
    let edits = env.root.join("edits");
    // Each run writes the next file in `edits` (1, 2, …) over the one it's given.
    let editor = env.root.join("editor");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\nn=$(($(cat '{e}/n' 2>/dev/null || echo 0) + 1))\necho $n > '{e}/n'\ncp '{e}/'$n \"$1\"\n",
            e = edits.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let home = env.home.clone();
    let edit = |answers: &str, files: &[&str]| {
        let _ = std::fs::remove_dir_all(&edits);
        std::fs::create_dir_all(&edits).unwrap();
        for (i, text) in files.iter().enumerate() {
            std::fs::write(edits.join((i + 1).to_string()), text).unwrap();
        }
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_actuo"))
            .args(["settings", "edit"])
            .env("ACTUO_HOME", &home)
            .env("EDITOR", &editor)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answers.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let file = env.home.join("settings.toml");

    // No actuo: written to the file.
    let out = edit("", &["# mine\n[lane]\nmax_runs = 3\n"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("actuo reads it when it starts"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "# mine\n[lane]\nmax_runs = 3\n"
    );

    // Not valid, and no to editing again: nothing saved.
    let out = edit("n\n", &["[lane]\nmax_runs = \"x\"\n"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not valid, nothing saved: lane.max_runs"),
        "{stderr}"
    );
    assert!(stderr.contains("edit again?"), "{stderr}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("max_runs = 3")
    );

    // Not valid, then fixed: saved, and the running actuo has it.
    env.start_actuo();
    let out = edit("y\n", &["[lane]\nmax_runs = 0\n", "[lane]\nmax_runs = 4\n"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("and applied"));
    assert_eq!(env.ok(&["settings", "get", "lane.max_runs"]).trim(), "4");

    // Saved unchanged: nothing to do.
    let out = edit("", &["[lane]\nmax_runs = 4\n"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no change\n");
}

/// `actuo start` waits for the agent. One that needs a login makes it fail with the
/// command that logs that agent in (here a stand-in named `claude`); one that
/// starts returns once it's ready.
#[test]
fn start_waits_for_the_agent_and_names_the_login() {
    let mut env = Env::with_agents(&[
        ("claude", json!({"authRequired": true, "turns": []})),
        ("fake", script()),
    ]);
    env.start_actuo();
    let repo = env.repo.to_str().unwrap().to_string();
    let e = env.fails(&["start", &repo, "--agent", "claude"]);
    assert!(
        e.contains("the session didn't start: the agent needs a login")
            && e.contains("`claude auth login`"),
        "{e}"
    );
    let id = env.ok(&["start", &repo, "--agent", "fake"]);
    let shown: Value = serde_json::from_str(&env.ok(&["show", id.trim()])).unwrap();
    assert_eq!(shown["state"], "idle", "{shown}");
}

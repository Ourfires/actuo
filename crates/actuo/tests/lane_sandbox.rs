//! The sandbox around lane runs, checked through what `fake-srt` was given: the
//! settings for the agent's session, setup and checks, and the environment each
//! started with. The real `srt` isn't run here.

mod common;

use common::lane::LaneEnv;
use serde_json::{Value, json};

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn settings_and_environment() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #1 in", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    let common = "setup = [\"echo installing\"]\nchecks = [\"test -f fixed.txt\"]\nnetwork = [\"registry.npmjs.org\"]\nread = [\"/opt/repo-toolchain\"]";
    let mut lane = LaneEnv::new(
        &script,
        &[
            ("o/closed", common),
            (
                "o/open",
                &format!("{common}\nagent_network = true\nsetup_reads_npmrc = true"),
            ),
        ],
        "",
    );
    lane.agents_toml(
        "\n[agents.fake.sandbox]\nhosts = [\"api.fake.test\"]\nread = [\"/opt/fake-toolchain\"]\ntmp_env = [\"FAKE_TMP\"]\n",
    );
    // Things actuo has that a sandboxed process must not get.
    lane.env
        .env
        .push(("SSH_AUTH_SOCK".into(), "/tmp/agent.sock".into()));
    lane.env.env.push(("GH_TOKEN".into(), "ghp_secret".into()));
    lane.env
        .env
        .push(("GIT_ASKPASS".into(), "/bin/echo".into()));
    lane.issue("o/closed", 1, "Fix", "x", &["actuo"], "me");
    lane.issue("o/open", 1, "Fix", "x", &["actuo"], "me");
    let mains: std::collections::HashMap<&str, String> = ["o/closed", "o/open"]
        .into_iter()
        .map(|r| (r, lane.origin_ref(r, "refs/heads/main").unwrap()))
        .collect();
    lane.env.start_actuo();
    lane.wait("both PRs", 60, |l| {
        ["o/closed", "o/open"]
            .iter()
            .all(|r| l.run_of(r, 1).is_some_and(|x| x[2] == "pr_opened"))
    });

    let home_dir = std::env::var("HOME").unwrap();
    let actuo_home = lane.env.home.clone();
    let seccomp = actuo_home
        .join("agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/@anthropic-ai/sandbox-runtime/vendor/seccomp")
        .display()
        .to_string();
    // o/open also lets its setup read ~/.npmrc.
    for (repo, npmrc) in [("o/closed", false), ("o/open", true)] {
        let run = lane.run_of(repo, 1).unwrap();
        let (run_id, session) = (run[0].clone(), run[4].clone());
        let detail: Value = serde_json::from_str(&lane.env.ok(&["show", &session])).unwrap();
        let worktree = detail["cwd"].as_str().unwrap().to_string();
        let tmp = actuo_home.join("tmp").join(&session).display().to_string();
        let gitconfig = actuo_home.join("sandbox/gitconfig").display().to_string();
        assert_eq!(detail["sandbox"]["read"], json!(["/opt/repo-toolchain"]));
        // What srt runs inside the sandbox (its seccomp helper on Linux), in every
        // settings file: the session's, setup's and the checks'.
        let reallows_srt = |s: &Value| strings(&s["filesystem"]["allowRead"]).contains(&seccomp);
        assert_eq!(detail["webTools"], false);
        assert_eq!(detail["telemetry"], false);

        // The agent's session.
        let calls = lane.srt_calls(&session);
        assert_eq!(calls.len(), 1, "{repo}");
        let s = &calls[0]["settings"];
        assert!(reallows_srt(s), "session");
        // The package registry in the project's network always (to install and
        // scaffold); the rest of it only with `agent_network` (here it's only
        // the registry, so both get the same).
        let hosts = vec!["api.fake.test".to_string(), "registry.npmjs.org".into()];
        assert_eq!(strings(&s["network"]["allowedDomains"]), hosts, "{repo}");
        assert_eq!(s["network"]["strictAllowlist"], true);
        assert_eq!(s["filesystem"]["denyRead"][0], home_dir.as_str());
        let read = strings(&s["filesystem"]["allowRead"]);
        for want in [
            &worktree,
            &tmp,
            &gitconfig,
            "/opt/fake-toolchain",
            "/opt/repo-toolchain",
        ] {
            assert!(read.iter().any(|r| r == want), "{want} not in {read:?}");
        }
        assert!(
            read.iter()
                .any(|r| r.ends_with("repos/o/") || r.contains("/repos/") && r.ends_with(".git")),
            "{read:?}"
        );
        assert!(!read.iter().any(|r| r.contains(".npmrc")), "{read:?}");
        assert_eq!(
            strings(&s["filesystem"]["allowWrite"]),
            vec![worktree.clone(), tmp.clone()]
        );
        let env = calls[0]["env"].as_object().unwrap();
        for gone in [
            "SSH_AUTH_SOCK",
            "GH_TOKEN",
            "GIT_ASKPASS",
            "FAKE_GH_STATE",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert!(!env.contains_key(gone), "{gone} reached the agent");
        }
        assert_eq!(env["TMPDIR"], tmp.as_str());
        // srt sets TMPDIR inside from it (else to /tmp/claude, shared).
        assert_eq!(env["CLAUDE_CODE_TMPDIR"], tmp.as_str());
        assert_eq!(env["FAKE_TMP"], tmp.as_str());
        assert_eq!(env["GIT_CONFIG_GLOBAL"], gitconfig.as_str());
        assert_eq!(env["HOME"], home_dir.as_str());
        assert!(
            std::fs::read_to_string(&gitconfig)
                .unwrap()
                .contains("hooksPath = /dev/null")
        );

        // Setup: the repo's network, the store writable, ~/.npmrc readable only with
        // setup_reads_npmrc.
        let store = actuo_home.join("stores").join(repo).display().to_string();
        let setup = lane.srt_calls(&format!("{run_id}-setup"));
        assert_eq!(setup.len(), 1);
        let s = &setup[0]["settings"];
        assert!(reallows_srt(s), "setup");
        assert_eq!(
            strings(&s["network"]["allowedDomains"]),
            ["registry.npmjs.org"]
        );
        let write = strings(&s["filesystem"]["allowWrite"]);
        assert!(
            write.contains(&store) && write.contains(&worktree),
            "{write:?}"
        );
        let read = strings(&s["filesystem"]["allowRead"]);
        assert_eq!(
            read.iter().any(|r| r.ends_with("/.npmrc")),
            npmrc,
            "{repo}: {read:?}"
        );
        assert_eq!(setup[0]["argv"], json!(["sh", "-c", "echo installing"]));
        let env = setup[0]["env"].as_object().unwrap();
        assert_eq!(env["npm_config_store_dir"], store.as_str());
        assert_eq!(env["CLAUDE_CODE_TMPDIR"], env["TMPDIR"]);
        assert!(!env["TMPDIR"].as_str().unwrap().starts_with("/tmp/claude"));
        assert_eq!(env["HUSKY"], "0");
        assert_eq!(env["NODE_USE_ENV_PROXY"], "1");
        assert_eq!(
            env["electron_config_cache"],
            format!("{}/electron", env["TMPDIR"].as_str().unwrap())
        );
        assert!(!env.contains_key("ACTUO_SRT_LOCAL_PORTS"));
        assert!(!env.contains_key("GH_TOKEN") && !env.contains_key("SSH_AUTH_SOCK"));
        // pnpm stays on the installed version; turbo's variables are the checks'.
        assert_eq!(env["npm_config_manage_package_manager_versions"], "false");
        assert!(
            !env.contains_key("TURBO_SCM_BASE")
                && !env.contains_key("TURBO_TELEMETRY_DISABLED")
                && !env.contains_key("TURBO_ENV_MODE")
        );

        // Checks: no network, the store readable but not writable, no ~/.npmrc.
        let checks = lane.srt_calls(&format!("{run_id}-checks"));
        assert!(!checks.is_empty());
        let s = &checks[0]["settings"];
        assert!(reallows_srt(s), "checks");
        let env = checks[0]["env"].as_object().unwrap();
        assert_eq!(env["CLAUDE_CODE_TMPDIR"], env["TMPDIR"]);
        // turbo's `--affected` from where the branch left `origin/main`, no telemetry,
        // pnpm on the installed version.
        assert_eq!(env["TURBO_SCM_BASE"], mains[repo].as_str());
        assert_eq!(env["TURBO_TELEMETRY_DISABLED"], "1");
        assert_eq!(env["TURBO_ENV_MODE"], "loose");
        assert_eq!(env["TURBO_CACHE_DIR"], format!("{worktree}/.turbo/cache"));
        assert!(!env.contains_key("NODE_USE_ENV_PROXY"));
        assert!(!env.contains_key("electron_config_cache"));
        assert_eq!(env["npm_config_userconfig"], "/dev/null");
        for (key, folder) in [
            ("TURBO_CONFIG_DIR_PATH", "turbo-config"),
            ("VERCEL_CONFIG_DIR_PATH", "vercel-config"),
        ] {
            assert_eq!(
                env[key],
                format!("{}/{}", env["TMPDIR"].as_str().unwrap(), folder)
            );
        }
        assert!(!env.contains_key("GH_TOKEN") && !env.contains_key("SSH_AUTH_SOCK"));
        assert_eq!(env["npm_config_manage_package_manager_versions"], "false");
        assert_eq!(s["network"]["allowedDomains"], json!([]));
        let write = strings(&s["filesystem"]["allowWrite"]);
        assert!(!write.contains(&store), "{write:?}");
        let read = strings(&s["filesystem"]["allowRead"]);
        assert!(
            read.contains(&store) && !read.iter().any(|r| r.ends_with("/.npmrc")),
            "{read:?}"
        );
    }

    // The probe ran srt once with nothing allowed but what srt runs inside.
    let probe = lane.srt_calls("probe");
    assert_eq!(probe.len(), 1);
    assert_eq!(probe[0]["argv"], json!(["true"]));
    let env = probe[0]["env"].as_object().unwrap();
    assert_eq!(env["CLAUDE_CODE_TMPDIR"], env["TMPDIR"]);
    assert_eq!(
        probe[0]["settings"]["filesystem"]["allowRead"],
        json!([seccomp])
    );
}

/// A token in actuo's environment that an agent's `pass_env` names: only
/// that agent's session gets it; setup, checks and gh don't, nor does another agent
/// whose `pass_env` doesn't name it. The log and `actuo agents` show the names,
/// never the values.
#[test]
fn pass_env_reaches_only_its_agent() {
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
        &[(
            "o/r",
            "setup = [\"echo installing\"]\nchecks = [\"test -f fixed.txt\"]",
        )],
        "",
    );
    // Ends the [agents.fake] table the file ends with.
    lane.agents_toml("pass_env = [\"FAKE_PASS_TOKEN\"]\n");
    let fake_value = "fake-pass-value-0123456789abcdef";
    let claude_value = "claude-pass-value-0123456789abcdef";
    lane.env
        .env
        .push(("FAKE_PASS_TOKEN".into(), fake_value.into()));
    // The built-in claude's pass_env names it; the fake agent's doesn't. This one
    // comes from agents.env (mode 0600), not the environment.
    let env_file = lane.env.root.join("agents.env");
    std::fs::write(
        &env_file,
        format!("# from claude setup-token\nCLAUDE_CODE_OAUTH_TOKEN=\"{claude_value}\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(
        &env_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    lane.issue("o/r", 1, "Fix", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the PR", 60, |l| {
        l.run_of("o/r", 1).is_some_and(|x| x[2] == "pr_opened")
    });

    let run = lane.run_of("o/r", 1).unwrap();
    let (run_id, session) = (run[0].clone(), run[4].clone());
    let calls = lane.srt_calls(&session);
    assert_eq!(calls.len(), 1);
    let env = calls[0]["env"].as_object().unwrap();
    assert_eq!(env["FAKE_PASS_TOKEN"], fake_value);
    assert!(!env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
    for step in ["setup", "checks"] {
        let calls = lane.srt_calls(&format!("{run_id}-{step}"));
        assert!(!calls.is_empty(), "{step}");
        for c in calls {
            let env = c["env"].as_object().unwrap();
            for name in ["FAKE_PASS_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"] {
                assert!(!env.contains_key(name), "{name} reached {step}");
            }
        }
    }
    let gh = lane.gh_calls();
    assert!(gh.lines().count() > 3, "{gh}");
    for line in gh.lines() {
        let names = strings(&serde_json::from_str::<Value>(line).unwrap()["envNames"]);
        for name in ["FAKE_PASS_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"] {
            assert!(
                !names.iter().any(|n| n == name),
                "{name} reached gh: {line}"
            );
        }
    }

    let log = lane.env.serve_log();
    assert!(log.contains("passing FAKE_PASS_TOKEN to fake"), "{log}");
    assert!(
        log.contains("passing CLAUDE_CODE_OAUTH_TOKEN to claude"),
        "{log}"
    );
    let agents = lane.env.ok(&["agents"]);
    assert!(
        agents
            .lines()
            .any(|l| l.starts_with("fake\t") && l.ends_with("\tpasses FAKE_PASS_TOKEN")),
        "{agents}"
    );
    assert!(
        agents
            .lines()
            .any(|l| l.starts_with("claude\t") && l.ends_with("\tpasses CLAUDE_CODE_OAUTH_TOKEN")),
        "{agents}"
    );
    for value in [fake_value, claude_value] {
        assert!(!log.contains(value) && !agents.contains(value));
        let shown = lane.env.ok(&["show", &run_id]);
        assert!(!shown.contains(value));
        assert!(!lane.env.ok(&["show", &session]).contains(value));
    }
}

/// A value actuo passes to an agent, in a file the agent wrote: held back like
/// any credential, though it has no known format.
#[test]
fn a_passed_value_in_the_diff_holds_the_push() {
    let value = "no-known-format-but-passed-7Hq2";
    let mut lane = LaneEnv::new(
        &json!({
            "resume": true,
            "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
            "turns": [
                {"match": "issue #1 in", "steps": [{"writeFile": {"path": "leak.txt", "content": format!("x\n{value}\n")}}]},
                {"match": "Review your changes", "steps": []}
            ]
        }),
        &[("o/r", "checks = [\"true\"]")],
        "",
    );
    lane.agents_toml("pass_env = [\"FAKE_PASS_TOKEN\"]\n");
    lane.env.env.push(("FAKE_PASS_TOKEN".into(), value.into()));
    lane.issue("o/r", 1, "Fix", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("held back", 60, |l| {
        l.run_of("o/r", 1)
            .is_some_and(|x| x[2] == "waiting (secret_detected)")
    });
    assert!(lane.pr("o/r", 1).is_none());
    assert!(!lane.gh_calls().contains(value));
    let comments = lane.comments("o/r", 1);
    assert!(
        comments.iter().any(|c| c.contains(
            "Actuo held back the change, `leak.txt` line 2: it contains a token or key actuo passes to an agent."
        )),
        "{comments:#?}"
    );
}

/// A repo `read` entry that is a credential path is refused when the settings load.
#[test]
fn credential_paths_cant_be_read() {
    let lane = LaneEnv::new(&json!({}), &[("o/r", "read = [\"~/.ssh\"]")], "");
    let mut env = lane.env;
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_actuo"))
        .arg("serve")
        .env("ACTUO_HOME", &env.home)
        .env("ACTUO_AGENTS_ENV", env.root.join("agents.env"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("projects.o/r.read"), "{err}");
    env.serve = None;
}

/// srt's records (as `fake-srt` stands in for the preload): a refusal becomes one
/// line in the session's agent log and actuo's log; srt's own message joins
/// the output where its stderr went; every other debug line, one of them over
/// several lines, is dropped. None of it reaches a step's output, a prompt, a
/// comment or the PR. The check's own `--settings` and `-s` reach it unread by srt
/// (`fake-srt` refuses anything before `--` but its own options).
#[test]
fn srt_records_stay_out_of_output() {
    let script = json!({
        "resume": true,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "issue #2 in", "steps": [{"writeFile": {"path": "other.txt", "content": "x\n"}}]},
            {"match": "failed with exit code", "steps": [{"writeFile": {"path": "fixed.txt", "content": "ok\n"}}]},
            {"match": "Review your changes", "steps": []}
        ]
    });
    // Passes on main (no other.txt), so its failure is the change's.
    let check = "echo --settings -s; test -s fixed.txt -o ! -f other.txt";
    let mut lane = LaneEnv::new(&script, &[("o/r", &format!("checks = [\"{check}\"]"))], "");
    let records = [
        json!({"debug": true, "text": "[SandboxDebug] Initializing sandbox..."}),
        json!({"debug": true, "text": "[SandboxDebug] {\n  \"allowedHosts\": [\n    \"api.fake.test\"\n  ]\n}"}),
        json!({"debug": true, "text": "[SandboxDebug] No matching config rule, denying: platform.claude.com:443"}),
        json!({"debug": true, "text": "[SandboxDebug] Connection blocked to platform.claude.com:443"}),
        json!({"debug": false, "text": "srt says hello"}),
    ];
    let sandbox = lane.env.home.join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(
        sandbox.join("fake-srt-records.jsonl"),
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    lane.issue("o/r", 2, "Fix", "x", &["actuo"], "me");
    lane.env.start_actuo();
    lane.wait("the PR", 60, |l| {
        l.run_of("o/r", 2).is_some_and(|x| x[2] == "pr_opened")
    });
    let run = lane.run_of("o/r", 2).unwrap();

    let leaked = |text: &str| text.contains("SandboxDebug") || text.contains("allowedHosts");
    // The check ran as written, its options after `--`.
    let calls = std::fs::read_dir(&sandbox)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.display()
                .to_string()
                .ends_with("-checks.json.calls.jsonl")
        })
        .unwrap();
    let first: Value = serde_json::from_str(
        std::fs::read_to_string(calls)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(strings(&first["argv"]), ["sh", "-c", check]);
    // The failing check's output went to the agent with srt's message, no debug.
    let prompts = lane.prompts();
    let retry = prompts
        .iter()
        .find(|p| p.contains("failed with exit code"))
        .unwrap();
    assert!(retry.contains("\n--settings -s\n"), "{retry}");
    assert!(retry.contains("srt says hello"), "{retry}");
    assert!(!prompts.iter().any(|p| leaked(p)), "{prompts:#?}");
    // Steps, comments and the PR: no debug.
    let shown = lane.env.ok(&["show", &run[0][..13]]);
    assert!(!leaked(&shown), "{shown}");
    assert!(shown.contains("srt says hello"), "{shown}");
    for c in lane.comments("o/r", 2) {
        assert!(!leaked(&c), "{c}");
    }
    let pr = lane.pr("o/r", 2).unwrap();
    assert!(!leaked(pr["body"].as_str().unwrap()));
    // The refusal: one line each time srt ran, in the session's agent log (with its
    // time) and actuo's log; nothing else of the debug log.
    let agent_log = std::fs::read_to_string(
        lane.env
            .home
            .join("logs")
            .join(format!("{}.agent.log", run[4])),
    )
    .unwrap();
    assert!(!leaked(&agent_log), "{agent_log}");
    let refused: Vec<&str> = agent_log
        .lines()
        .filter(|l| l.contains("sandbox refused"))
        .collect();
    assert!(refused.len() >= 2, "{agent_log}");
    for l in &refused {
        assert!(
            l.ends_with(" sandbox refused platform.claude.com:443 (not on the allow list)"),
            "{l}"
        );
    }
    let serve = lane.env.serve_log();
    assert!(!leaked(&serve), "{serve}");
    assert!(
        serve.contains(&format!(
            "session {}: sandbox refused platform.claude.com:443",
            run[4]
        )),
        "{serve}"
    );
    assert!(
        serve.contains(&format!("run {} check `{check}`: sandbox refused", run[0])),
        "{serve}"
    );
}

/// An agents.env others may read: `actuo serve` doesn't start, and says how to fix
/// it without showing what's in it.
#[test]
fn an_agents_env_others_can_read_stops_serve() {
    let lane = LaneEnv::new(&json!({}), &[], "");
    let file = lane.env.root.join("agents.env");
    std::fs::write(&file, "CLAUDE_CODE_OAUTH_TOKEN=secret-value-0123456789\n").unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let out = lane.env.run(&["serve"]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chmod 600"), "{err}");
    assert!(!err.contains("secret-value"), "{err}");
}

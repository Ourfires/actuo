//! Shared by the integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

/// The `fake-agent` binary, built once per test run so it is never stale.
pub fn fake_agent() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "fake-agent"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo");
        assert!(
            out.status.success(),
            "building fake-agent: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // target/<profile>/deps/<test> → target/<profile>/fake-agent
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fake-agent")
    })
    .clone()
}

/// The `fake-gh` binary, built once per test run.
pub fn fake_gh() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "fake-gh"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo");
        assert!(
            out.status.success(),
            "building fake-gh: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fake-gh")
    })
    .clone()
}

/// A git repository with one commit (README.md) in a temp dir.
pub fn git_repo(dir: &std::path::Path) -> PathBuf {
    let root = dir.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&root)
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
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "readme\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    root
}

/// An agents config with one agent, `fake`, running fake-agent with `script`.
/// Its modes map `ask` to `ask` and `edits` to `edits`.
pub fn fake_agents(
    dir: &std::path::Path,
    script: &serde_json::Value,
) -> actuo_core::config::AgentsConfig {
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let toml = format!(
        r#"
        [agents.fake]
        command = "{}"
        args = ["--script", "{}", "--state", "{}", "--log", "{}"]
        modes = {{ ask = "ask", edits = "edits" }}
        "#,
        fake_agent().display(),
        script_path.display(),
        dir.join("agent-state").display(),
        dir.join("agent-log.jsonl").display(),
    );
    std::fs::write(dir.join("agents.toml"), toml).unwrap();
    actuo_core::config::AgentsConfig::load(&dir.join("agents.toml")).unwrap()
}

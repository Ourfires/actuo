//! `actuo settings` with no actuo running edits `settings.toml` directly (the
//! same re-read, patch, validation and write as `settings/update`); with an actuo
//! running, even one that doesn't answer, it never touches the file.

mod common;

use common::Env;
use serde_json::json;
use std::process::{Command, Stdio};
use std::time::Duration;

const DIRECT: &str = "no actuo is running: edited";

fn settings(env: &Env) -> String {
    std::fs::read_to_string(env.home.join("settings.toml")).unwrap_or_default()
}

#[test]
fn no_actuo_edits_the_file() {
    let env = Env::new(&json!({"turns": []}));
    std::fs::write(
        env.home.join("settings.toml"),
        "# mine\n[lane]\npoll_interval = \"15s\"\n\n[projects.\"o/r\"]\nchecks = [\"true\"]\n",
    )
    .unwrap();

    // No socket file.
    let out = env.run(&["settings", "set", "lane.poll_interval", "20s"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.trim(),
        format!(
            "{DIRECT} {} directly",
            env.home.join("settings.toml").display()
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "\"20s\"");
    assert!(settings(&env).starts_with("# mine\n"), "{}", settings(&env));
    assert!(settings(&env).contains("poll_interval = \"20s\""));

    // A stale socket file: nothing listening on it (connection refused).
    drop(std::os::unix::net::UnixListener::bind(env.socket()).unwrap());
    assert!(env.socket().exists());
    let out = env.run(&["settings", "unset", "projects.\"o/r\""]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains(DIRECT));
    assert!(!settings(&env).contains("o/r"), "{}", settings(&env));

    // Reading says so too; a value that doesn't validate changes nothing.
    let out = env.run(&["settings", "get", "lane.poll_interval"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no actuo is running: read"));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "\"20s\"");
    let before = settings(&env);
    let e = env.fails(&["settings", "set", "lane.max_runs", "\"many\""]);
    assert!(e.contains("max_runs"), "{e}");
    assert_eq!(settings(&env), before);
}

#[test]
fn a_running_actuo_is_always_asked() {
    let mut env = Env::new(&json!({"turns": []}));

    // Listening but never answering: it waits, and the file is left alone.
    let listener = std::os::unix::net::UnixListener::bind(env.socket()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_actuo"))
        .args(["settings", "set", "lane.poll_interval", "20s"])
        .env("ACTUO_HOME", &env.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(child.try_wait().unwrap().is_none(), "it waits for actuo");
    assert!(!env.home.join("settings.toml").exists());
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stderr).contains(DIRECT));
    drop(listener);
    std::fs::remove_file(env.socket()).unwrap();

    // A real actuo: through it, as always.
    env.start_actuo();
    let out = env.run(&["settings", "set", "lane.poll_interval", "25s"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("no actuo is running"));
    assert!(settings(&env).contains("poll_interval = \"25s\""));
}

//! A newer release in `actuo status`, and `actuo update`, against a release
//! folder (`file://`): the latest is bbb2222, this actuo says it's aaa1111
//! (`ACTUO_TEST_COMMIT`), and release bbb2222's binary is a script that records
//! its arguments instead of installing anything.

mod common;

use common::Env;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Releases {
    env: Env,
    /// Release bbb2222's binary for this machine.
    binary: PathBuf,
    /// What the binary was run with.
    installed: PathBuf,
}

/// `None` where there's no release binary (this test's machine isn't one).
fn releases() -> Option<Releases> {
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "actuo-darwin-arm64",
        ("linux", "x86_64") => "actuo-linux-x86_64",
        ("linux", "aarch64") => "actuo-linux-arm64",
        _ => return None,
    };
    let mut env = Env::new(&json!({"turns": []}));
    let dir = env.root.join("releases");
    let latest = dir.join("latest/download");
    std::fs::create_dir_all(&latest).unwrap();
    std::fs::write(latest.join("commit"), "bbb2222\n").unwrap();
    let old = dir.join("download/aaa1111");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join(format!("{asset}.sha256")), "x  asset\n").unwrap();
    let new = dir.join("download/bbb2222");
    std::fs::create_dir_all(&new).unwrap();
    let installed = env.root.join("installed");
    let binary = new.join(asset);
    std::fs::write(
        &binary,
        format!("#!/bin/sh\necho \"$*\" > '{}'\n", installed.display()),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
    let sum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&binary)
        .output()
        .unwrap();
    std::fs::write(
        new.join(format!("{asset}.sha256")),
        String::from_utf8(sum.stdout).unwrap(),
    )
    .unwrap();
    env.env = vec![
        (
            "ACTUO_RELEASES_URL".into(),
            format!("file://{}", dir.display()),
        ),
        ("ACTUO_TEST_COMMIT".into(), "aaa1111".into()),
    ];
    Some(Releases {
        env,
        binary,
        installed,
    })
}

/// An actuo command with the release folder, as actuo commit `commit`.
fn run(r: &Releases, args: &[&str], commit: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_actuo"))
        .args(args)
        .env("ACTUO_HOME", &r.env.home)
        .envs(r.env.env.iter().map(|(k, v)| (k, v)))
        .env("ACTUO_TEST_COMMIT", commit)
        .env("SHELL", "/bin/sh")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn status_tells_of_a_newer_release() {
    let Some(mut r) = releases() else { return };
    r.env.start_actuo();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = r.env.ok(&["status"]);
        if status.contains("update\trelease bbb2222 is out; actuo update installs it\n") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn update() {
    let Some(r) = releases() else { return };

    // At the latest: nothing to do.
    let out = run(&r, &["update"], "bbb2222");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "actuo is at the latest release (bbb2222)\n"
    );

    // Not a release (a local build): not replaced.
    let out = run(&r, &["update"], "ccc3333");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("actuo (ccc3333) isn't a release")
            && stderr.contains("/download/bbb2222/install.sh | sh"),
        "{stderr}"
    );
    assert!(!r.installed.exists());

    // A download that doesn't match its sha256: not run.
    let script = std::fs::read(&r.binary).unwrap();
    std::fs::write(&r.binary, b"#!/bin/sh\necho tampered\n").unwrap();
    let out = run(&r, &["update"], "aaa1111");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("the download's sha256 is "), "{stderr}");
    assert!(!r.installed.exists());
    std::fs::write(&r.binary, script).unwrap();

    // The release's binary installs itself, and the download goes.
    let out = run(&r, &["update", "--now"], "aaa1111");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&r.installed).unwrap(),
        "service install --now\n"
    );
    assert!(!r.env.home.join("tmp/update-bbb2222").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("downloading ") && stdout.contains("actuo agents install"),
        "{stdout}"
    );
}

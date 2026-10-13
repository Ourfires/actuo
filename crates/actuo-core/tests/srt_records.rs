//! The records preload (`src/srt-records.cjs`) with real Node, around a stand-in for
//! srt's `cli.js` that logs like srt (`--debug` lines, one of them over several
//! lines, a refusal, one of its own errors) and starts a command the way srt does
//! (`spawn(command, {shell: true, stdio: 'inherit'})`). The command lists the
//! descriptors open in it: actuo's pipe isn't among them, and neither srt's
//! debug output nor `NODE_OPTIONS`, `SRT_DEBUG` or `ACTUO_SRT_RECORDS_FD` reach it.
//! (The real sandbox sits between srt and the command; it can only close more.)
//! Needs `node` on `PATH`.

use actuo_core::config::Paths;
use actuo_core::sandbox;
use std::sync::{Arc, Mutex};

const CLI: &str = r#"
import { spawn } from 'node:child_process';
process.env.SRT_DEBUG = 'true';
console.error('[SandboxDebug] Initializing sandbox...');
console.error('[SandboxDebug] ' + JSON.stringify({ allowedHosts: ['example.com'] }, null, 2));
console.error('[SandboxDebug] No matching config rule, denying: platform.claude.com:443');
console.error('[SandboxDebug] Connection blocked to platform.claude.com:443');
console.error('Error: an srt message');
// A burst: BURST refusals of about 200 bytes each, far more than a pipe holds,
// then a pause and one more (with a reader, the pause lets it catch up, so the
// count of what was dropped goes out before that last line).
const burst = Number(process.env.BURST || 0);
for (let i = 0; i < burst; i++) {
  console.error(`[SandboxDebug] No matching config rule, denying: h${i}-${'x'.repeat(120)}.test:443`);
}
if (burst > 0) {
  await new Promise(r => setTimeout(r, 300));
  console.error('[SandboxDebug] No matching config rule, denying: last.test:443');
}
const child = spawn(process.argv[2], { shell: true, stdio: 'inherit' });
child.on('exit', code => process.exit(code ?? 0));
"#;

#[test]
fn exact_local_service_policy_and_preload() {
    let result = std::process::Command::new("node")
        .args([
            "--test",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/check-local-network.test.cjs"
            ),
        ])
        .output()
        .expect("node on PATH");
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Lists the descriptors open in the command (`/dev/fd`, which is
/// `/proc/self/fd` on Linux), each with what it refers to (`fd <n> <inode>`), then
/// the variables.
const PROBE: &str = r#"bash -c 'for f in /dev/fd/*; do i=$(stat -L -c %i "$f" 2>/dev/null || stat -L -f %i "$f" 2>/dev/null); echo "fd ${f##*/} $i"; done; echo "SRT_DEBUG=${SRT_DEBUG-unset} NODE_OPTIONS=${NODE_OPTIONS-unset} RECORDS=${ACTUO_SRT_RECORDS_FD-unset}"'"#;

struct Ran {
    stdout: String,
    stderr: String,
    messages: Vec<String>,
    agent_log: String,
    /// The refusals collected for the step (`read_records`' sink).
    refused: Vec<String>,
    /// The pipe's inode (both ends).
    pipe: u64,
}

impl Ran {
    /// The descriptors in the command that refer to actuo's pipe.
    fn pipe_fds(&self) -> Vec<&str> {
        pipe_fds(&self.stdout, self.pipe)
    }
}

/// The probe's lines (`<n> <inode>`) for descriptors whose inode is `ino`: in full,
/// or cut to 32 bits (macOS `stat` prints a pipe's that way).
fn pipe_fds(stdout: &str, ino: u64) -> Vec<&str> {
    let forms = [ino.to_string(), (ino & 0xffff_ffff).to_string()];
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("fd "))
        .filter(|l| {
            l.split_whitespace()
                .nth(1)
                .is_some_and(|i| forms.iter().any(|f| f == i))
        })
        .collect()
}

async fn run(with_preload: bool) -> Ran {
    run_with(with_preload, 0, false).await
}

/// `burst` refusals before the command starts; with `stall`, nothing reads the
/// records pipe until the command has ended.
async fn run_with(with_preload: bool, burst: usize, stall: bool) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let cli = dir.path().join("cli.mjs");
    std::fs::write(&cli, CLI).unwrap();
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(&cli)
        .arg(PROBE)
        .env_remove("NODE_OPTIONS")
        .env("BURST", burst.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if with_preload {
        cmd.env(
            "NODE_OPTIONS",
            sandbox::records_node_options(&paths).unwrap(),
        );
    }
    let (reader, writer) = sandbox::records(&mut cmd).unwrap();
    let pipe = {
        use std::os::unix::fs::MetadataExt;
        let file = std::fs::File::from(writer.try_clone().unwrap());
        file.metadata().unwrap().ino()
    };
    let child = actuo_core::process::spawn_passing(&mut cmd, writer)
        .expect("`node` on PATH (the preload test needs it)");
    let log = dir.path().join("agent.log");
    let messages = Arc::new(Mutex::new(Vec::new()));
    let m = messages.clone();
    let log_for_reader = log.clone();
    let refused = Arc::new(Mutex::new(Vec::new()));
    let sink = refused.clone();
    let mut read = Some(move || {
        sandbox::read_records(
            reader,
            Some(log_for_reader),
            "test".into(),
            Some(sink),
            move |t: &str| m.lock().unwrap().push(t.to_string()),
        )
    });
    let early = (!stall).then(|| read.take().unwrap()());
    // srt never waits on the pipe, read or not.
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("srt stalled")
        .unwrap();
    let records = early.unwrap_or_else(|| read.take().unwrap()());
    records.join().unwrap();
    Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        messages: messages.lock().unwrap().clone(),
        agent_log: std::fs::read_to_string(&log).unwrap_or_default(),
        refused: refused.lock().unwrap().clone(),
        pipe,
    }
}

#[tokio::test]
async fn the_command_srt_starts_has_neither_the_pipe_nor_the_debug_log() {
    let r = run(true).await;
    // No descriptor in the command refers to actuo's pipe: not the one it
    // passed, not the one the preload opened. The probe sees what descriptors refer
    // to (stdio has inodes) ...
    for n in 0..3 {
        let line = r
            .stdout
            .lines()
            .find(|l| l.starts_with(&format!("fd {n} ")));
        assert!(
            line.is_some_and(|l| l
                .split_whitespace()
                .nth(2)
                .is_some_and(|i| i.parse::<u64>().is_ok())),
            "{}",
            r.stdout
        );
    }
    // ... and none of them is the pipe.
    assert!(r.pipe_fds().is_empty(), "pipe {}: {}", r.pipe, r.stdout);
    assert!(
        r.stdout
            .contains("SRT_DEBUG=unset NODE_OPTIONS=unset RECORDS=unset"),
        "{}",
        r.stdout
    );
    // srt's stderr (shared with the command) got nothing of srt's.
    assert_eq!(r.stderr, "");
    for out in [&r.stdout, &r.stderr, &r.agent_log] {
        assert!(
            !out.contains("SandboxDebug") && !out.contains("allowedHosts"),
            "{out}"
        );
    }
    // The refusal, once, with its time; srt's own message passed on.
    let refused: Vec<&str> = r.agent_log.lines().collect();
    assert_eq!(refused.len(), 1, "{}", r.agent_log);
    // The same refusal reaches the step's sink, for its diagnosis.
    assert_eq!(
        r.refused,
        vec!["platform.claude.com:443 (not on the allow list)"]
    );
    assert!(
        refused[0].ends_with(" sandbox refused platform.claude.com:443 (not on the allow list)"),
        "{}",
        r.agent_log
    );
    assert_eq!(r.messages, ["Error: an srt message"]);
}

/// Without the preload, srt's debug log is on the stderr it shares with the
/// command: the preload is what keeps it off.
#[tokio::test]
async fn without_the_preload_the_debug_log_is_on_stderr() {
    let r = run(false).await;
    assert!(r.stderr.contains("[SandboxDebug]"), "{}", r.stderr);
}

/// The probe sees the pipe when it is there: started by actuo itself with the
/// pipe passed (no Node in between; Node's spawn passes only stdio, on Linux as on
/// macOS, so the preload's close is a second layer).
#[tokio::test]
async fn the_probe_sees_a_passed_pipe() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(PROBE)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    let (_reader, writer) = sandbox::records(&mut cmd).unwrap();
    let fd = writer.as_raw_fd();
    let ino = std::fs::File::from(writer.try_clone().unwrap())
        .metadata()
        .unwrap()
        .ino();
    let out = actuo_core::process::spawn_passing(&mut cmd, writer)
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let found = pipe_fds(&stdout, ino);
    assert_eq!(found.len(), 1, "fd {fd} {ino}: {stdout}");
    assert!(found[0].starts_with(&format!("{fd} ")), "fd {fd}: {stdout}");
}

/// The agent log's refusal lines, and the drops it reports.
fn refused_and_dropped(log: &str) -> (Vec<&str>, u64) {
    let mut refused = Vec::new();
    let mut dropped = 0;
    for l in log.lines() {
        let (_, text) = l.split_once(' ').unwrap();
        if let Some(n) = text
            .strip_prefix("srt's records pipe was full: ")
            .and_then(|t| t.strip_suffix(" dropped"))
        {
            dropped += n.parse::<u64>().unwrap();
        } else {
            let what = text.strip_prefix("sandbox refused ").expect(l);
            assert!(what.ends_with(":443 (not on the allow list)"), "{l}");
            refused.push(what);
        }
    }
    (refused, dropped)
}

/// A burst far bigger than the pipe, read as it comes: every refusal is recorded
/// or counted as dropped, no line is cut or run into another, and srt doesn't wait.
#[tokio::test]
async fn a_burst_is_recorded_or_counted() {
    let started = std::time::Instant::now();
    let r = run_with(true, 5000, false).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert_eq!(r.stderr, "");
    let (refused, dropped) = refused_and_dropped(&r.agent_log);
    assert_eq!(
        refused.len() as u64 + dropped,
        5002, // the burst, the one before it and the one after
        "{} refused, {dropped} dropped",
        refused.len()
    );
    assert_eq!(
        refused.last(),
        Some(&"last.test:443 (not on the allow list)")
    );
    assert!(r.pipe_fds().is_empty());
}

/// The same burst with nothing reading the pipe until the command has ended: srt
/// drops what doesn't fit instead of waiting, and what the pipe holds is whole.
#[tokio::test]
async fn a_stalled_reader_doesnt_stall_srt() {
    let r = run_with(true, 5000, true).await;
    assert!(r.stdout.contains("SRT_DEBUG=unset"), "{}", r.stdout);
    assert_eq!(r.stderr, "");
    let (refused, _) = refused_and_dropped(&r.agent_log);
    assert!(
        !refused.is_empty() && refused.len() < 5002,
        "{}",
        refused.len()
    );
}

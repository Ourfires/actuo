//! Setup and check commands: a repo's own commands from the settings (never text
//! from GitHub), run with `sh -c` in the worktree, in their own process group, with
//! a time limit. Output goes to a log file; the caller gets its tail.
//!
//! The lane runs them inside `srt` (`sandbox`), with a cleared environment plus the
//! one it passes.

use crate::lane::prompt::tail;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How a command runs inside `srt`.
pub struct Srt<'a> {
    /// Actuo's pinned `node`, which runs `program`.
    pub node: &'a Path,
    /// `srt` itself.
    pub program: &'a Path,
    pub settings: &'a Path,
    /// `NODE_OPTIONS` with the records preload (`sandbox::records_node_options`).
    pub node_options: &'a str,
    /// Where refusals go besides actuo's log: the session's agent log.
    pub refusal_log: Option<std::path::PathBuf>,
    /// What actuo's log says a refusal came from.
    pub label: String,
    pub local_ports: &'a [u16],
    pub local_listen_ports: &'a [u16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// `None` when the command was stopped at its time limit or killed by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Stopped before its end by the caller's `stop` (the run was cancelled or
    /// taken over).
    pub stopped: bool,
    pub tail: String,
    pub duration: Duration,
    /// The largest total and single-process resident memory seen, in KB, sampled
    /// each second (`None` before the first sample).
    pub peak_kb: Option<u64>,
    pub peak_process_kb: Option<u64>,
    /// What srt refused during the command (`host:port (reason)`).
    pub refusals: Vec<String>,
}

impl CommandOutcome {
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// The `srt` settings file and environment for a setup or check command in
/// `worktree` (a run's, or a check run's; `name` names the settings and the temp
/// dir). Setup writes the repo's package store, reaches `network` and, with `npmrc`
/// (the project's `setup_reads_npmrc`), reads `~/.npmrc`; checks read the store
/// and reach nothing. Both read and write the worktree and the temp dir, and read
/// the repo's git dir and its `read` list.
///
/// Both keep pnpm on the version installed: its switch to a repo's
/// `packageManager` version would download it, into a home that is fresh each
/// step. Checks also get turbo's base for `--affected` (`scm_base`) and no turbo
/// telemetry.
#[allow(clippy::too_many_arguments)]
pub fn sandbox_for(
    paths: &crate::config::Paths,
    name: &str,
    worktree: &Path,
    repo: &str,
    network: &[String],
    read: &[String],
    setup: bool,
    scm_base: Option<String>,
    unix_sockets: bool,
    local_ports: &[u16],
    local_listen_ports: &[u16],
    npmrc: bool,
) -> Result<
    (
        std::path::PathBuf,
        std::collections::BTreeMap<String, String>,
    ),
    String,
> {
    use std::path::PathBuf;
    let tmp = crate::sandbox::temp_dir(paths, name).map_err(|e| e.to_string())?;
    let git = crate::sandbox::gitconfig(paths).map_err(|e| e.to_string())?;
    // From the clone, not the worktree: the worktree's `.git` is agent-writable.
    let common = crate::git::common_dir(&paths.lane_clone(repo)).map_err(|e| e.to_string())?;
    let store = paths.home.join("stores").join(repo);
    std::fs::create_dir_all(&store).map_err(|e| e.to_string())?;
    let mut r = vec![
        worktree.to_path_buf(),
        common,
        tmp.clone(),
        store.clone(),
        git.clone(),
    ];
    r.extend(
        read.iter()
            .map(|x| PathBuf::from(crate::config::expand_home(x))),
    );
    let mut w = vec![worktree.to_path_buf(), tmp.clone()];
    let mut hosts = Vec::new();
    let local_services = !setup && (!local_ports.is_empty() || !local_listen_ports.is_empty());
    if local_services {
        if !cfg!(target_os = "macos") {
            return Err("local TCP check ports are macOS only".into());
        }
        crate::sandbox::local_network_wrapper(paths).map_err(|e| e.to_string())?;
    }
    if setup && npmrc {
        r.push(PathBuf::from(crate::config::expand_home("~/.npmrc")));
    }
    if setup {
        w.push(store.clone());
        hosts = network.to_vec();
    }
    let policy = crate::sandbox::Policy {
        read: r,
        write: w,
        hosts,
        local_binding: false,
        unix_sockets,
    };
    let settings_name = format!("{name}-{}", if setup { "setup" } else { "checks" });
    let file = crate::sandbox::write_settings(paths, &settings_name, &policy)
        .map_err(|e| e.to_string())?;
    let mut extra = std::collections::BTreeMap::from([
        ("HUSKY".to_string(), "0".to_string()),
        (
            "npm_config_store_dir".to_string(),
            store.display().to_string(),
        ),
    ]);
    for (k, v) in crate::sandbox::PACKAGE_MANAGER_ENV {
        extra.insert(k.to_string(), v.to_string());
    }
    if setup {
        // Node's built-in fetch (Electron's installer uses it) ignores srt's
        // HTTP(S)_PROXY unless asked; without this it connects directly, which the
        // sandbox refuses before the proxy can record the host. Node 22.21 and 24
        // read it; it allows no more network.
        extra.insert("NODE_USE_ENV_PROXY".to_string(), "1".to_string());
        // Electron's cache is ~/Library/Caches on macOS whatever HOME is: its
        // downloads go to the step's temp dir instead.
        extra.insert(
            "electron_config_cache".to_string(),
            tmp.join("electron").display().to_string(),
        );
    } else {
        // Checks run offline: no registry or remote-cache logins. Turbo fails on a
        // login file it can't read, so its config dirs are empty ones it can write.
        extra.insert("npm_config_userconfig".to_string(), "/dev/null".to_string());
        for (key, directory) in [
            ("TURBO_CONFIG_DIR_PATH", "turbo-config"),
            ("VERCEL_CONFIG_DIR_PATH", "vercel-config"),
        ] {
            extra.insert(key.to_string(), tmp.join(directory).display().to_string());
        }
        // Turbo's strict mode drops pnpm's version setting from its commands, which
        // then try to download pnpm, offline. The environment is already cleared
        // down to what's allowed, and the sandbox still applies.
        extra.insert("TURBO_ENV_MODE".to_string(), "loose".to_string());
        extra.insert("TURBO_TELEMETRY_DISABLED".to_string(), "1".to_string());
        // Turbo's cache is otherwise the main checkout's, which the check can't
        // write.
        extra.insert(
            "TURBO_CACHE_DIR".to_string(),
            worktree.join(".turbo/cache").display().to_string(),
        );
        if let Some(c) = scm_base {
            extra.insert("TURBO_SCM_BASE".to_string(), c);
        }
    }
    Ok((file, crate::sandbox::env(&tmp, &git, &extra)))
}

/// Runs `command` in `cwd`; stdout and stderr go to `log`. When `stop` resolves
/// first, the command's group is stopped as at the time limit, and the tail ends
/// with `stop`'s words. Dropped before its end (actuo stopping), the group gets
/// SIGKILL.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    command: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    sandbox: Option<Srt<'_>>,
    limit: Duration,
    log: &Path,
    tail_lines: usize,
    tail_bytes: usize,
    stop: impl Future<Output = String>,
) -> std::io::Result<CommandOutcome> {
    if let Some(parent) = log.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Output goes through a pipe actuo copies to the log: inside the sandbox
    // the log file isn't readable, and node aborts at start when its stdout is a file
    // it can't stat.
    let (reader, writer) = crate::process::pipe()?;
    let err = writer.try_clone()?;
    // srt's own messages join the output through a writer of their own.
    let srt_out = match &sandbox {
        Some(_) => Some(writer.try_clone()?),
        None => None,
    };
    let mut file = std::fs::File::create(log)?;
    let mut reader = reader;
    let copy = tokio::task::spawn_blocking(move || std::io::copy(&mut reader, &mut file));
    let started = Instant::now();
    // Inside the sandbox: `node srt --debug --settings <file> -- sh -c <command>`, with
    // only `env` (and the records preload): after `--`, nothing is read as srt's
    // own options. srt's debug log comes on a pipe of its own, never in the output.
    let mut records = None;
    let mut cmd = match &sandbox {
        Some(srt) => {
            let mut c = tokio::process::Command::new(srt.node);
            c.arg(srt.program)
                .args(["--debug", "--settings"])
                .arg(srt.settings)
                .arg("--")
                .env_clear()
                .env("NODE_OPTIONS", srt.node_options);
            if !srt.local_ports.is_empty() || !srt.local_listen_ports.is_empty() {
                // The wrapper sits next to the settings, in actuo's `sandbox`
                // folder, which the step can't write.
                let wrapper = srt
                    .settings
                    .parent()
                    .ok_or_else(|| std::io::Error::other("sandbox settings have no parent"))?
                    .join("check-local-network.cjs");
                c.env("ACTUO_SRT_LOCAL_WRAPPER", wrapper).env(
                    "ACTUO_SRT_LOCAL_PORTS",
                    serde_json::to_string(&serde_json::json!({
                        "connect": srt.local_ports,
                        "listen": srt.local_listen_ports,
                    }))?,
                );
            }
            c.args(["sh", "-c"]);
            records = Some(crate::sandbox::records(&mut c)?);
            c
        }
        None => {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c");
            c
        }
    };
    cmd.arg(command)
        .current_dir(cwd)
        // Git hooks a repo installs through husky stay off.
        .env("HUSKY", "0")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(err)
        .process_group(0)
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let (spawned, records) = match records {
        Some((reader, writer)) => (
            crate::process::spawn_passing(&mut cmd, writer),
            Some(reader),
        ),
        None => (crate::process::spawn(&mut cmd), None),
    };
    // The pipes' write ends now live only in the child.
    drop(cmd);
    let mut child = spawned?;
    let refused = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    if let (Some(reader), Some(srt), Some(mut out)) = (records, sandbox, srt_out) {
        // srt's own messages go into the output, where its stderr went.
        let sink = Some(refused.clone());
        crate::sandbox::read_records(reader, srt.refusal_log, srt.label, sink, move |text| {
            use std::io::Write;
            // One write, so the command's output can't land inside the line.
            let _ = out.write_all(format!("{text}\n").as_bytes());
        });
    }
    let pid = child.id().unwrap_or_default();
    let mut guard = crate::process::GroupGuard::new(pid);
    // Memory, every second until the command ends: its whole process tree.
    let peaks = std::sync::Arc::new(std::sync::Mutex::new((None::<u64>, None::<u64>)));
    let sampler = {
        let peaks = peaks.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Ok(Some((total, largest))) =
                    tokio::task::spawn_blocking(move || crate::process::tree_memory(pid)).await
                else {
                    continue;
                };
                let mut p = peaks.lock().unwrap();
                p.0 = Some(p.0.unwrap_or(0).max(total));
                p.1 = Some(p.1.unwrap_or(0).max(largest));
            }
        })
    };
    let waited = tokio::select! {
        w = tokio::time::timeout(limit, child.wait()) => Ok(w),
        why = stop => Err(why),
    };
    let (exit_code, timed_out, stopped) = match waited {
        Ok(Ok(status)) => (status?.code(), false, None),
        Ok(Err(_)) => {
            crate::process::kill_group(pid, crate::process::TERM_GRACE).await;
            let _ = child.wait().await;
            (None, true, None)
        }
        Err(why) => {
            crate::process::kill_group(pid, crate::process::TERM_GRACE).await;
            let _ = child.wait().await;
            (None, false, Some(why))
        }
    };
    // Leftovers of the command (a server it started) go with it.
    if pid != 0 && crate::process::group_alive(pid) {
        crate::process::kill_group(pid, crate::process::TERM_GRACE).await;
    }
    guard.disarm();
    // The copy ends when the last writer is gone; a process that left the group and
    // kept the pipe open doesn't hold the step up for long.
    let _ = tokio::time::timeout(Duration::from_secs(5), copy).await;
    let text = String::from_utf8_lossy(&tokio::fs::read(log).await?).into_owned();
    let mut t = tail(&text, tail_lines, tail_bytes);
    if timed_out {
        t.push_str(&format!("\n[stopped after {}s]", limit.as_secs()));
    }
    if let Some(why) = &stopped {
        t.push_str(&format!("\n[stopped: {why}]"));
    }
    sampler.abort();
    let (peak_kb, peak_process_kb) = *peaks.lock().unwrap();
    Ok(CommandOutcome {
        exit_code,
        timed_out,
        stopped: stopped.is_some(),
        tail: t,
        duration: started.elapsed(),
        peak_kb,
        peak_process_kb,
        refusals: std::mem::take(&mut *refused.lock().unwrap()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_with_a_limit_and_a_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/1-check.log");
        let ok = run(
            "echo one; echo two >&2; echo $HUSKY; echo \"$X\"; exit 3",
            dir.path(),
            &[("X", "from env")],
            None,
            Duration::from_secs(10),
            &log,
            3,
            1000,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(ok.exit_code, Some(3));
        assert!(!ok.passed());
        assert_eq!(ok.tail, "two\n0\nfrom env");
        assert!(std::fs::read_to_string(&log).unwrap().starts_with("one\n"));

        let slow = run(
            "echo started; sleep 30",
            dir.path(),
            &[],
            None,
            Duration::from_secs(1),
            &log,
            10,
            1000,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(slow.timed_out);
        assert_eq!(slow.exit_code, None);
        assert!(
            slow.tail.starts_with("started\n[stopped after 1s]"),
            "{}",
            slow.tail
        );
        assert!(slow.duration < Duration::from_secs(10));

        let pass = run(
            "true",
            dir.path(),
            &[],
            None,
            Duration::from_secs(5),
            &log,
            10,
            10,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(pass.passed());
    }

    /// The process started in the background, from its pid file: still running?
    fn running(pid_file: &Path) -> bool {
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // A zombie waits to be reaped by whoever adopted it: not running.
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
    }

    async fn gone_within(pid_file: &Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if !running(pid_file) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// `stop` ends the command and what it started, with its words in the tail.
    #[tokio::test]
    async fn stop_ends_the_command_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("1-check.log");
        let pid_file = dir.path().join("pid");
        let started = Instant::now();
        let out = run(
            &format!(
                "sleep 60 & echo $! > {}; echo started; wait",
                pid_file.display()
            ),
            dir.path(),
            &[],
            None,
            Duration::from_secs(60),
            &log,
            10,
            1000,
            async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                "the run was cancelled".to_string()
            },
        )
        .await
        .unwrap();
        assert!(out.stopped && !out.timed_out, "{out:?}");
        assert_eq!(out.exit_code, None);
        assert_eq!(out.tail, "started\n[stopped: the run was cancelled]");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(gone_within(&pid_file, Duration::from_secs(5)).await);
    }

    /// Dropped before its end (actuo stopping), the command's whole group goes.
    #[tokio::test]
    async fn dropped_takes_the_group_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("1-check.log");
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
        let step = run(
            &command,
            dir.path(),
            &[],
            None,
            Duration::from_secs(60),
            &log,
            10,
            1000,
            std::future::pending(),
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), step)
                .await
                .is_err()
        );
        assert!(gone_within(&pid_file, Duration::from_secs(5)).await);
    }
}

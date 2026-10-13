//! `actuo service install|uninstall` and `actuo restart`: runs `actuo serve` as a service of your
//! user, started at login (or boot, with linger) and restarted if it crashes, not
//! if it stops cleanly. On macOS a launch agent; on Linux a systemd user unit.
//! `install.sh` from a release runs it too.
//!
//! install copies this binary to `<actuo home>/bin/actuo`, so the service
//! doesn't depend on where it was run from, writes the service file and (re)loads
//! it: `~/Library/LaunchAgents/co.ourfires.actuo.plist`, or
//! `~/.config/systemd/user/actuo.service`. `PATH` is captured now, so actuo
//! finds Node, git, gh and pnpm as the calling shell does; `ACTUO_HOME` too, if
//! set. On macOS output goes to `<actuo home>/logs/serve.log`; on Linux to the
//! journal (`journalctl --user -u actuo`). Run it again after changing `PATH`.
//!
//! Before the restart it asks the running actuo to finish what's in progress
//! (`actuo/drain`, up to 30 min; `--now` doesn't wait), keeps the binary it
//! replaces as `<actuo home>/bin/actuo.prev`, and after the restart waits for the
//! new actuo to answer with its build; if it doesn't within 30 s, the previous
//! binary goes back and is restarted.
//!
//! `actuo restart` restarts the installed service the same way, after what's in
//! progress (`--now` doesn't wait), and waits for actuo to answer.
//!
//! On Linux it refuses while a system-wide unit for actuo exists
//! (`/etc/systemd/user`, installed by root): a user unit would shadow it. Root
//! removes that file first.

use crate::client::Client;
use actuo_core::config::Paths;
use actuo_core::process::output_std;
use actuo_protocol::methods::{ActuoDrain, ActuoDrainResult, Method};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "co.ourfires.actuo";

/// How long `bootout` may take before launchd has dropped the service.
const DROP_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the running actuo may take to finish what's in progress.
const DRAIN_LIMIT_S: u64 = 30 * 60;

/// How long the new actuo may take to answer after the restart.
fn start_limit() -> std::time::Duration {
    // Tests shorten it (debug builds only).
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("ACTUO_TEST_START_LIMIT_S")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return std::time::Duration::from_secs(s);
    }
    std::time::Duration::from_secs(30)
}

pub async fn install(paths: &Paths, now: bool) -> Result<()> {
    if cfg!(target_os = "linux") {
        refuse_under_root_unit()?;
    } else {
        macos_only()?;
    }
    // Held until the restart: nothing new starts in between.
    let held = if now { None } else { hold(paths).await };
    let bin = paths.home.join("bin/actuo");
    keep_previous(&bin)?;
    if cfg!(target_os = "linux") {
        install_systemd(paths)?;
    } else {
        install_launchd(paths)?;
    }
    drop(held);
    check_started(paths, &bin).await
}

/// `actuo restart`: the installed service, after what's in progress unless `now`.
pub async fn restart_installed(paths: &Paths, now: bool) -> Result<()> {
    let installed = if cfg!(target_os = "linux") {
        unit_path(&user_home()?)
    } else {
        macos_only()?;
        plist_path(&user_home()?)
    };
    if !installed.exists() {
        bail!(
            "actuo isn't installed as a service here ({} is missing); \
             `actuo service install` installs it",
            installed.display()
        );
    }
    let held = if now { None } else { hold(paths).await };
    restart()?;
    drop(held);
    if answers(paths, None).await {
        return Ok(());
    }
    bail!(
        "actuo didn't answer within {} s of the restart. Its last log lines:\n{}",
        start_limit().as_secs(),
        log_tail(paths)
    )
}

/// Asks the running actuo, if any, to finish what's in progress; the connection
/// holds it until it's dropped. `None` when no actuo answers.
async fn hold(paths: &Paths) -> Option<Client> {
    let (client, _events) = Client::connect(&paths.socket()).await.ok()?;
    let ask =
        |limit_s: u64| client.raw_call(ActuoDrain::NAME, serde_json::json!({ "limitS": limit_s }));
    // What's in progress now, then the wait.
    let first = match ask(0).await {
        Ok(v) => serde_json::from_value::<ActuoDrainResult>(v).unwrap_or(ActuoDrainResult {
            in_progress: Vec::new(),
        }),
        Err(e) => {
            out!(
                "couldn't ask the running actuo to wait ({}); restarting it now",
                e.message
            );
            return Some(client);
        }
    };
    if first.in_progress.is_empty() {
        return Some(client);
    }
    out!(
        "waiting for what's in progress to finish, up to {} min (--now doesn't wait):",
        DRAIN_LIMIT_S / 60
    );
    for line in &first.in_progress {
        out!("  {line}");
    }
    match ask(DRAIN_LIMIT_S).await {
        Ok(v) => {
            let left = serde_json::from_value::<ActuoDrainResult>(v)
                .map(|r| r.in_progress)
                .unwrap_or_default();
            if left.is_empty() {
                out!("finished");
            } else {
                out!("still in progress at the limit; the restart cuts:");
                for line in &left {
                    out!("  {line}");
                }
            }
        }
        Err(e) => out!("stopped waiting ({}); restarting it now", e.message),
    }
    Some(client)
}

/// The binary about to be replaced, kept as `actuo.prev` (unless it's this one).
fn keep_previous(bin: &Path) -> Result<()> {
    let me = std::env::current_exe().context("finding this binary")?;
    let Ok(old) = std::fs::read(bin) else {
        return Ok(());
    };
    if std::fs::read(&me).ok().as_deref() == Some(old.as_slice()) {
        return Ok(());
    }
    replace(bin, &prev_path(bin))
}

fn prev_path(bin: &Path) -> PathBuf {
    bin.with_file_name("actuo.prev")
}

/// Copies `from` over `to` through a temp file, executable.
fn replace(from: &Path, to: &Path) -> Result<()> {
    let tmp = to.with_extension("tmp");
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, to).with_context(|| format!("writing {}", to.display()))?;
    Ok(())
}

/// Waits for the new actuo to answer with this build; if it doesn't, puts
/// `actuo.prev` back and restarts it.
async fn check_started(paths: &Paths, bin: &Path) -> Result<()> {
    let limit = start_limit();
    if answers(paths, Some(actuo_core::COMMIT)).await {
        return Ok(());
    }
    let log = log_tail(paths);
    let prev = prev_path(bin);
    if !prev.exists() {
        bail!(
            "the new actuo didn't answer within {} s. Its last log lines:\n{log}",
            limit.as_secs()
        );
    }
    replace(&prev, bin)?;
    restart()?;
    bail!(
        "the new actuo didn't answer within {} s; the previous one ({}) is back and restarted. The new one's last log lines:\n{log}",
        limit.as_secs(),
        prev.display()
    )
}

/// Waits up to [`start_limit`] for actuo to answer (with build `commit`, when
/// given) and says what answered.
async fn answers(paths: &Paths, commit: Option<Option<&str>>) -> bool {
    let deadline = tokio::time::Instant::now() + start_limit();
    loop {
        if let Ok((c, _)) = Client::connect(&paths.socket()).await
            && let Some(info) = &c.actuo
            && commit.is_none_or(|commit| info.commit.as_deref() == commit)
        {
            out!(
                "actuo answers: {} {}{}",
                info.name,
                info.version,
                info.commit
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            );
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Restarts the installed service.
fn restart() -> Result<()> {
    if cfg!(target_os = "linux") {
        return run("systemctl", &["--user", "restart", UNIT]);
    }
    let plist = plist_path(&user_home()?);
    let domain = domain()?;
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    wait_until_dropped(&domain)?;
    run(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
    )
}

/// The service's last 20 log lines: the journal, or `serve.log` on macOS.
fn log_tail(paths: &Paths) -> String {
    let text = if cfg!(target_os = "linux") {
        output_std(Command::new("journalctl").args([
            "--user",
            "-u",
            UNIT,
            "-n",
            "20",
            "--no-pager",
            "-o",
            "cat",
        ]))
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
    } else {
        std::fs::read_to_string(paths.home.join("logs/serve.log")).unwrap_or_default()
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

fn install_launchd(paths: &Paths) -> Result<()> {
    let home = user_home()?;
    let path = std::env::var("PATH").context("PATH isn't set")?;
    let bin = paths.home.join("bin/actuo");
    copy_self(&bin)?;
    let logs = paths.home.join("logs");
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    let log = logs.join("serve.log");

    let mut env = vec![("PATH", path)];
    if let Ok(actuo_home) = std::env::var("ACTUO_HOME") {
        env.push(("ACTUO_HOME", actuo_home));
    }
    let plist = plist_path(&home);
    let dir = plist.parent().expect("a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = plist.with_extension("plist.tmp");
    std::fs::write(&tmp, plist_text(&bin, &env, &home, &log))
        .with_context(|| format!("writing {}", tmp.display()))?;
    run("plutil", &["-lint", "-s", &tmp.display().to_string()])?;
    std::fs::rename(&tmp, &plist).with_context(|| format!("writing {}", plist.display()))?;

    // Loaded already (an earlier install): unload first, so the new file is read,
    // and wait until launchd has dropped it: bootstrapping too soon fails with
    // "Bootstrap failed: 5: Input/output error".
    let domain = domain()?;
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    wait_until_dropped(&domain)?;
    run(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
    )?;
    out!(
        "installed {}: {} serve, logs in {}",
        plist.display(),
        bin.display(),
        log.display()
    );
    Ok(())
}

pub fn uninstall(paths: &Paths) -> Result<()> {
    if cfg!(target_os = "linux") {
        return uninstall_systemd(paths);
    }
    macos_only()?;
    let plist = plist_path(&user_home()?);
    let domain = domain()?;
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    wait_until_dropped(&domain)?;
    for file in [plist, paths.home.join("bin/actuo")] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
        }
    }
    out!("uninstalled {LABEL}");
    Ok(())
}

/// Waits until launchd no longer has the service (`launchctl print` fails), up to
/// [`DROP_LIMIT`].
fn wait_until_dropped(domain: &str) -> Result<()> {
    let target = format!("{domain}/{LABEL}");
    let start = std::time::Instant::now();
    while launchctl(&["print", &target]).is_ok() {
        if start.elapsed() > DROP_LIMIT {
            bail!(
                "launchd still has {LABEL} {} s after `launchctl bootout`; try again",
                DROP_LIMIT.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

fn macos_only() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("actuo service is for macOS and Linux");
    }
    Ok(())
}

/// The systemd user unit's name (Linux).
const UNIT: &str = "actuo.service";

/// A system-wide unit for actuo, installed by root: a user install would
/// shadow it.
const SYSTEM_UNIT: &str = "/etc/systemd/user/actuo.service";

fn refuse_under_root_unit() -> Result<()> {
    if Path::new(SYSTEM_UNIT).exists() {
        bail!(
            "{SYSTEM_UNIT} exists: a system-wide unit for actuo, which a user unit \
             would shadow. As root, remove that file first."
        );
    }
    Ok(())
}

fn install_systemd(paths: &Paths) -> Result<()> {
    let home = user_home()?;
    let path = std::env::var("PATH").context("PATH isn't set")?;
    let bin = paths.home.join("bin/actuo");
    copy_self(&bin)?;
    let mut env = vec![("PATH", path)];
    if let Ok(actuo_home) = std::env::var("ACTUO_HOME") {
        env.push(("ACTUO_HOME", actuo_home));
    }
    let unit = unit_path(&home);
    let dir = unit.parent().expect("a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = unit.with_extension("service.tmp");
    std::fs::write(&tmp, unit_text(&bin, &env))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &unit).with_context(|| format!("writing {}", unit.display()))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", UNIT])?;
    run("systemctl", &["--user", "restart", UNIT])?;
    out!(
        "installed {}: {} serve, logs with journalctl --user -u actuo",
        unit.display(),
        bin.display()
    );
    let user = output_std(Command::new("id").arg("-un"))
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let linger =
        output_std(Command::new("loginctl").args(["show-user", &user, "-p", "Linger", "--value"]))
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
    if linger != "yes" {
        out!(
            "linger is off: actuo stops when you log out. To keep it running, as \
             root: loginctl enable-linger {user}"
        );
    }
    Ok(())
}

fn uninstall_systemd(paths: &Paths) -> Result<()> {
    let unit = unit_path(&user_home()?);
    if unit.exists() {
        run("systemctl", &["--user", "disable", "--now", UNIT])?;
    }
    for file in [&unit, &paths.home.join("bin/actuo")] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
        }
    }
    run("systemctl", &["--user", "daemon-reload"])?;
    out!("uninstalled {UNIT}");
    Ok(())
}

fn unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user").join(UNIT)
}

/// Restarted after a crash or `kill -9`, not after a clean stop, nor when `actuo
/// serve` exits 3 (it can't start and starting again won't help); stopping kills
/// the cgroup.
fn unit_text(bin: &Path, env: &[(&str, String)]) -> String {
    let env: String = env
        .iter()
        .map(|(k, v)| format!("Environment=\"{k}={}\"\n", systemd_quote(v)))
        .collect();
    format!(
        "[Unit]\n\
         Description=actuo: agent sessions and the issue lane\n\
         \n\
         [Service]\n\
         WorkingDirectory=%h\n\
         {env}\
         ExecStart=\"{bin}\" serve\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         RestartPreventExitStatus=3\n\
         KillMode=control-group\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = systemd_quote(&bin.display().to_string()).replace('$', "$$"),
    )
}

/// Inside a double-quoted systemd value: `\` and `"` escaped, `%` doubled.
fn systemd_quote(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME isn't set")
}

fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

/// Copies the running binary to `to` (through a temp file, so a running copy is
/// replaced whole), executable. Nothing to do when it already runs from there.
fn copy_self(to: &Path) -> Result<()> {
    let me = std::env::current_exe().context("finding this binary")?;
    if me.canonicalize().ok() == to.canonicalize().ok() {
        return Ok(());
    }
    let dir = to.parent().expect("a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = to.with_extension("tmp");
    std::fs::copy(&me, &tmp)
        .with_context(|| format!("copying {} to {}", me.display(), tmp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, to).with_context(|| format!("writing {}", to.display()))?;
    Ok(())
}

/// `gui/<uid>`: your login session's launchd domain.
fn domain() -> Result<String> {
    let out = output_std(Command::new("id").arg("-u")).context("running id -u")?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&out.stdout).trim()
    ))
}

fn launchctl(args: &[&str]) -> Result<()> {
    run("launchctl", args)
}

/// Runs a tool found on `PATH` (tests put a recording `launchctl` first).
fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = output_std(Command::new(program).args(args))
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn plist_text(bin: &Path, env: &[(&str, String)], home: &Path, log: &Path) -> String {
    let env: String = env
        .iter()
        .map(|(k, v)| format!("    <key>{k}</key><string>{}</string>\n", xml(v)))
        .collect();
    let log = xml(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>WorkingDirectory</key><string>{home}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        bin = xml(&bin.display().to_string()),
        home = xml(&home.display().to_string()),
    )
}

/// Text for an XML string.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_with_odd_paths() {
        let text = unit_text(
            Path::new("/home/a b/.actuo/bin/actuo"),
            &[("PATH", "/x/50%\"q\":/usr/bin".into())],
        );
        assert!(text.contains("ExecStart=\"/home/a b/.actuo/bin/actuo\" serve\n"));
        assert!(text.contains("Environment=\"PATH=/x/50%%\\\"q\\\":/usr/bin\"\n"));
        assert!(text.contains("Restart=on-failure\n"));
        assert!(text.contains("RestartPreventExitStatus=3\n"));
        assert!(text.contains("WantedBy=default.target\n"));
        let dollar = unit_text(Path::new("/home/$me/actuo"), &[]);
        assert!(dollar.contains("ExecStart=\"/home/$$me/actuo\" serve"));
    }
}

//! The sandbox around agent sessions, setup and checks: Anthropic's sandbox-runtime
//! (`srt`; Seatbelt on macOS, bubblewrap on Linux, a proxy that allows only listed
//! hosts).
//!
//! Reads are denied for your whole home folder and re-allowed only for what a process
//! needs; writes are allowed only where listed; the network reaches only listed hosts.
//! A sandboxed process starts with a cleared environment plus an allowlist, and git
//! inside reads a minimal config actuo writes, never your `~/.gitconfig`.

use crate::config::{CREDENTIAL_PATHS, Paths, expand_home};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SRT_PACKAGE: &str = "@anthropic-ai/sandbox-runtime";
/// Pinned: [`refusal`] reads this version's debug wording.
pub const SRT_VERSION: &str = "0.0.78";

/// Variables a sandboxed process gets from actuo's environment; nothing else.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// Variables actuo sets for every sandboxed process ([`env`]).
pub const ACTUO_SET: &[&str] = &[
    "TMPDIR",
    "TMPPREFIX",
    "CLAUDE_CODE_TMPDIR",
    "GIT_CONFIG_GLOBAL",
];

/// The git config sandboxed processes read (`GIT_CONFIG_GLOBAL`).
const GITCONFIG: &str = "\
# Written by actuo: the git config for sandboxed processes. Your ~/.gitconfig
# isn't readable inside the sandbox; neither are git's default ignore and attributes
# files under ~/.config/git, so git would warn on every call.
[core]
\thooksPath = /dev/null
\texcludesFile = /dev/null
\tattributesFile = /dev/null
[user]
\tname = actuo sandbox
\temail = actuo@localhost
";

/// What a sandboxed process may read (under your home folder), write, and reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub hosts: Vec<String>,
    /// Lets the process listen on localhost (`actuo login`'s OAuth callback).
    pub local_binding: bool,
    /// Lets the process use Unix sockets (a project's `checks_unix_sockets`, a check
    /// run's second try).
    pub unix_sockets: bool,
}

impl Policy {
    /// The `srt` settings: deny reading `~` (and, for clarity, each credential path),
    /// re-allow `read` and `write`; allow writing `write`; reach only `hosts`.
    pub fn settings(&self) -> Value {
        let mut deny = vec![home().display().to_string()];
        deny.extend(CREDENTIAL_PATHS.iter().map(|c| expand_home(c)));
        let mut read: Vec<String> = Vec::new();
        for p in self.read.iter().chain(&self.write) {
            let p = p.display().to_string();
            if !read.contains(&p) {
                read.push(p);
            }
        }
        let mut hosts = self.hosts.clone();
        hosts.sort();
        hosts.dedup();
        let mut network = json!({
            "allowedDomains": hosts,
            "deniedDomains": [],
            "strictAllowlist": true,
        });
        if self.local_binding {
            network["allowLocalBinding"] = json!(true);
        }
        if self.unix_sockets {
            network["allowAllUnixSockets"] = json!(true);
        }
        json!({
            "network": network,
            "filesystem": {
                "denyRead": deny,
                "allowRead": read,
                "allowWrite": self.write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "denyWrite": [],
            },
        })
    }
}

fn home() -> PathBuf {
    PathBuf::from(expand_home("~/"))
        .components()
        .collect::<PathBuf>()
}

/// Where `srt` is installed.
pub fn srt(paths: &Paths) -> PathBuf {
    install_dir(paths, SRT_PACKAGE, SRT_VERSION).join("node_modules/.bin/srt")
}

/// For pnpm and npm in the sandbox, where only the project's `network` hosts are
/// reachable: no download of the version a repo's `packageManager` pins (pnpm
/// fails without it) and no update check against the registry.
pub const PACKAGE_MANAGER_ENV: [(&str, &str); 2] = [
    ("npm_config_manage_package_manager_versions", "false"),
    ("npm_config_update_notifier", "false"),
];

/// What `srt` itself runs inside the sandbox: on Linux its seccomp helper
/// (`vendor/seccomp/<arch>/apply-seccomp`), started inside the `bwrap` namespace.
/// It lives under `~/.actuo`, so every settings file re-allows it, read-only.
pub fn srt_inside(paths: &Paths) -> PathBuf {
    install_dir(paths, SRT_PACKAGE, SRT_VERSION)
        .join("node_modules")
        .join(SRT_PACKAGE)
        .join("vendor/seccomp")
}

pub fn install_dir(paths: &Paths, package: &str, version: &str) -> PathBuf {
    paths
        .home
        .join("agents")
        .join(format!("{package}@{version}"))
}

/// Installs an npm package at its pinned version into its own directory under
/// `~/.actuo/agents`, once (outside any sandbox). One install at a time.
pub async fn install(dir: &Path, package: &str, version: &str, bin: &str) -> Result<(), String> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = LOCK.lock().await;
    if dir.join("node_modules/.bin").join(bin).exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let spec = format!("{package}@{version}");
    let out = crate::process::output(
        tokio::process::Command::new("npm")
            .args([
                "install",
                "--no-audit",
                "--no-fund",
                "--loglevel=error",
                "--prefix",
            ])
            .arg(dir)
            .arg(&spec),
    )
    .await
    .map_err(|e| format!("npm: {e}"))?;
    if !out.status.success() || !dir.join("node_modules/.bin").join(bin).exists() {
        return Err(format!(
            "npm install {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Writes the minimal git config for sandboxed processes; returns its path.
pub fn gitconfig(paths: &Paths) -> std::io::Result<PathBuf> {
    let path = paths.home.join("sandbox").join("gitconfig");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(GITCONFIG) {
        write_private(&path, GITCONFIG)?;
    }
    Ok(path)
}

/// Writes a process's `srt` settings to `~/.actuo/sandbox/<name>.json` (0600), with
/// what `srt` itself needs inside re-allowed ([`srt_inside`]).
pub fn write_settings(paths: &Paths, name: &str, policy: &Policy) -> std::io::Result<PathBuf> {
    let path = paths.home.join("sandbox").join(format!("{name}.json"));
    let mut policy = policy.clone();
    policy.read.push(srt_inside(paths));
    let text = serde_json::to_string_pretty(&policy.settings()).expect("settings serialize");
    write_private(&path, &text)?;
    Ok(path)
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Shared helpers may be first requested by several sessions at once. Each
    // writer needs its own temporary file; a shared .tmp can be renamed away
    // by another writer or expose partial content in the destination.
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = f
        .write_all(text.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Where [`temp_dir`] makes `name`'s temp dir.
pub fn temp_path(paths: &Paths, name: &str) -> PathBuf {
    paths.home.join("tmp").join(name)
}

/// An empty temp dir for one process (`~/.actuo/tmp/<name>`), made fresh.
pub fn temp_dir(paths: &Paths, name: &str) -> std::io::Result<PathBuf> {
    let dir = temp_path(paths, name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The environment a sandboxed process starts with: the allowlist from actuo's
/// own, `TMPDIR` (and zsh's `TMPPREFIX`, for here-documents; its default is under
/// `/tmp`, which isn't writable), `CLAUDE_CODE_TMPDIR` (srt sets `TMPDIR` inside the
/// sandbox from it, else to `/tmp/claude`), the generated git config, then
/// `extra`. Nothing else: no `SSH_AUTH_SOCK`, no `GH_TOKEN`, no `GIT_ASKPASS`.
pub fn env(
    tmp: &Path,
    gitconfig: &Path,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = ENV_ALLOWLIST
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    env.insert("TMPDIR".into(), tmp.display().to_string());
    env.insert("TMPPREFIX".into(), tmp.join("zsh").display().to_string());
    env.insert("CLAUDE_CODE_TMPDIR".into(), tmp.display().to_string());
    env.insert("GIT_CONFIG_GLOBAL".into(), gitconfig.display().to_string());
    env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// `srt [--debug] --settings <file> -- <program> <args>`: after `--`, nothing is
/// read as srt's own options. `debug` only with [`records`] attached, which takes
/// srt's debug log off the stderr it shares with the program.
pub fn wrap(
    paths: &Paths,
    settings: &Path,
    debug: bool,
    program: &str,
    args: &[String],
) -> (String, Vec<String>) {
    let mut a = Vec::new();
    if debug {
        a.push("--debug".to_string());
    }
    a.extend([
        "--settings".to_string(),
        settings.display().to_string(),
        "--".to_string(),
        program.to_string(),
    ]);
    a.extend(args.iter().cloned());
    (srt(paths).display().to_string(), a)
}

/// The preload that takes srt's console output off its stderr (see the file).
const RECORDS_PRELOAD: &str = include_str!("srt-records.cjs");

pub fn local_network_wrapper(paths: &Paths) -> std::io::Result<PathBuf> {
    const SCRIPT: &str = include_str!("check-local-network.cjs");
    let path = paths.home.join("sandbox").join("check-local-network.cjs");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        write_private(&path, SCRIPT)?;
    }
    Ok(path)
}

/// `NODE_OPTIONS` for an `srt` run with [`records`]: requires the preload, written
/// to `~/.actuo/sandbox/srt-records.cjs`. The preload removes it from srt's
/// environment at once, so nothing srt starts gets it.
pub fn records_node_options(paths: &Paths) -> std::io::Result<String> {
    let path = paths.home.join("sandbox").join("srt-records.cjs");
    let shown = path.display().to_string();
    if shown.contains(['"', '\\']) {
        return Err(std::io::Error::other(format!(
            "{shown}: a path NODE_OPTIONS can't quote"
        )));
    }
    if std::fs::read_to_string(&path).ok().as_deref() != Some(RECORDS_PRELOAD) {
        write_private(&path, RECORDS_PRELOAD)?;
    }
    Ok(format!("--require \"{shown}\""))
}

/// The variable that tells the preload which descriptor is the records pipe.
pub const RECORDS_FD_VAR: &str = "ACTUO_SRT_RECORDS_FD";

/// A pipe for srt's console output (`cmd` is an `srt --debug` with
/// [`records_node_options`]): its descriptor number goes in [`RECORDS_FD_VAR`].
/// Spawn `cmd` with [`crate::process::spawn_passing`] and the writer; read the
/// reader with [`read_records`].
pub fn records(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<(std::io::PipeReader, std::os::fd::OwnedFd)> {
    use std::os::fd::AsRawFd;
    let (reader, writer) = crate::process::pipe()?;
    // srt never waits on a full pipe: the preload drops instead. On macOS its own
    // open of the pipe shares this flag; on Linux it asks for it itself.
    rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK)?;
    cmd.env(RECORDS_FD_VAR, writer.as_raw_fd().to_string());
    Ok((reader, writer.into()))
}

/// A line srt sent on the records pipe, as actuo keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrtRecord {
    /// A connection the proxy refused: `host:port (reason)`.
    Refused(String),
    /// One of srt's own messages (an error), as srt wrote it.
    Message(String),
    /// Messages the preload dropped because the pipe was full.
    Dropped(u64),
}

/// Reads one line of the records pipe: a refusal from srt's debug log, or one of
/// its own messages. Every other debug line is dropped.
pub fn parse_record(line: &str) -> Option<SrtRecord> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Line {
        Message { debug: bool, text: String },
        Dropped { dropped: u64 },
    }
    match serde_json::from_str(line).ok()? {
        Line::Dropped { dropped } => Some(SrtRecord::Dropped(dropped)),
        Line::Message { debug: false, text } => Some(SrtRecord::Message(text)),
        Line::Message { debug: true, text } => refusal(&text).map(SrtRecord::Refused),
    }
}

/// `host:port (reason)` from one of srt 0.0.78's debug lines for a refused
/// connection (`sandbox-manager.js`, `resolved-address-guard.js`, `socks-proxy.js`);
/// `None` for any other line. Depends on srt's wording: recheck on every srt update.
pub fn refusal(text: &str) -> Option<String> {
    let text = text.strip_prefix("[SandboxDebug] ")?;
    let (target, reason) = if let Some(t) = text.strip_prefix("No matching config rule, denying: ")
    {
        (t.to_string(), "not on the allow list".to_string())
    } else if let Some(t) = text.strip_prefix("Denied by config rule: ") {
        (t.to_string(), "on the deny list".to_string())
    } else if let Some(t) = text.strip_prefix("Denying malformed host: ") {
        (t.to_string(), "malformed host".to_string())
    } else if let Some(t) = text.strip_prefix("Rejecting malformed SOCKS host: ") {
        (t.to_string(), "malformed host".to_string())
    } else {
        // `Denied address(es) for host:port: <addresses> (<classes>)`: the classes,
        // not the addresses.
        let t = text.strip_prefix("Denied address(es) for ")?;
        let (target, rest) = t.split_once(": ")?;
        let classes = rest
            .rfind(" (")
            .map(|i| rest[i + 2..].trim_end_matches(')'))
            .unwrap_or("denied");
        (
            target.to_string(),
            format!("resolved to a denied address: {classes}"),
        )
    };
    // The host is whatever the sandboxed process asked for: printable, one line.
    let target: String = target.chars().flat_map(char::escape_debug).collect();
    let reason: String = reason.chars().flat_map(char::escape_debug).collect();
    Some(format!("{target} ({reason})"))
}

/// Reads srt's records until srt and everything it started are gone, on a thread
/// of its own. A refusal becomes one line, `<time> sandbox refused <host:port>
/// (<reason>)`, appended to `log` (the session's agent log) and, with `label`, in
/// actuo's log; srt's own messages go to `message`. Diagnosis only: nothing
/// decides on them, and the sandboxed process can write to srt's stderr, not here.
pub fn read_records(
    reader: std::io::PipeReader,
    log: Option<PathBuf>,
    label: String,
    refused: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    mut message: impl FnMut(&str) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        let append = |text: &str| {
            if let Some(path) = &log
                && let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                let _ = writeln!(f, "{} {text}", crate::time::now());
            }
        };
        // Bytes, not `lines()`: a line that isn't UTF-8 is dropped, not the end.
        let mut reader = std::io::BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                continue;
            };
            match parse_record(line.trim_end_matches('\n')) {
                Some(SrtRecord::Refused(what)) => {
                    crate::log!("{label}: sandbox refused {what}");
                    append(&format!("sandbox refused {what}"));
                    if let Some(r) = &refused {
                        r.lock().unwrap().push(what);
                    }
                }
                Some(SrtRecord::Dropped(n)) => {
                    crate::log!("{label}: srt's records pipe was full: {n} dropped");
                    append(&format!("srt's records pipe was full: {n} dropped"));
                }
                Some(SrtRecord::Message(text)) => message(&text),
                None => {}
            }
        }
    })
}

/// Whether `srt` runs here: installs it if needed, then runs `true` inside it with
/// nothing allowed. The error says why not (missing Node, `bwrap`, `socat`,
/// `ripgrep`, or user namespaces on Linux).
pub async fn probe(paths: &Paths) -> Result<(), String> {
    install(
        &install_dir(paths, SRT_PACKAGE, SRT_VERSION),
        SRT_PACKAGE,
        SRT_VERSION,
        "srt",
    )
    .await?;
    let settings = write_settings(paths, "probe", &Policy::default()).map_err(|e| e.to_string())?;
    let tmp = temp_dir(paths, "probe").map_err(|e| e.to_string())?;
    let git = gitconfig(paths).map_err(|e| e.to_string())?;
    let (program, args) = wrap(paths, &settings, false, "true", &[]);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .env_clear()
        .envs(env(&tmp, &git, &BTreeMap::new()))
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), crate::process::output(&mut cmd))
        .await
        .map_err(|_| "srt didn't finish within 60 s".to_string())?
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "srt failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_private_writes_are_complete_and_do_not_race_on_a_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.cjs");
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    barrier.wait();
                    write_private(&path, &"fixture".repeat(4096)).unwrap();
                });
            }
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fixture".repeat(4096)
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn settings_deny_home_and_reallow() {
        let h = home();
        let p = Policy {
            read: vec![h.join("wt"), PathBuf::from("/opt/tool")],
            write: vec![h.join("wt"), h.join(".actuo/tmp/s1")],
            hosts: vec!["b.example".into(), "a.example".into(), "a.example".into()],
            local_binding: false,
            unix_sockets: false,
        };
        let s = p.settings();
        assert_eq!(
            s["network"]["allowedDomains"],
            json!(["a.example", "b.example"])
        );
        assert_eq!(s["network"]["strictAllowlist"], true);
        assert!(s["network"].get("allowAllUnixSockets").is_none());
        let sockets = Policy {
            unix_sockets: true,
            ..p.clone()
        };
        assert_eq!(sockets.settings()["network"]["allowAllUnixSockets"], true);
        let deny = s["filesystem"]["denyRead"].as_array().unwrap();
        assert_eq!(deny[0], h.display().to_string());
        assert!(deny.contains(&json!(h.join(".ssh").display().to_string())));
        let read = s["filesystem"]["allowRead"].as_array().unwrap();
        assert!(read.contains(&json!(h.join("wt").display().to_string())));
        assert!(read.contains(&json!(h.join(".actuo/tmp/s1").display().to_string())));
        assert_eq!(
            s["filesystem"]["allowWrite"],
            json!([
                h.join("wt").display().to_string(),
                h.join(".actuo/tmp/s1").display().to_string()
            ])
        );
        assert!(s.get("enableWeakerNetworkIsolation").is_none());
        assert!(s["network"].get("allowLocalBinding").is_none());
        let p = Policy {
            local_binding: true,
            unix_sockets: false,
            ..p
        };
        assert_eq!(p.settings()["network"]["allowLocalBinding"], true);
    }

    #[test]
    fn env_is_an_allowlist() {
        let e = env(
            Path::new("/t"),
            Path::new("/g"),
            &BTreeMap::from([("X".to_string(), "1".to_string())]),
        );
        assert_eq!(e["TMPDIR"], "/t");
        assert_eq!(e["TMPPREFIX"], "/t/zsh");
        assert_eq!(e["CLAUDE_CODE_TMPDIR"], "/t");
        assert_eq!(e["GIT_CONFIG_GLOBAL"], "/g");
        assert_eq!(e["X"], "1");
        for k in e.keys() {
            assert!(
                ENV_ALLOWLIST.contains(&k.as_str())
                    || [
                        "TMPDIR",
                        "TMPPREFIX",
                        "CLAUDE_CODE_TMPDIR",
                        "GIT_CONFIG_GLOBAL",
                        "X"
                    ]
                    .contains(&k.as_str()),
                "{k}"
            );
        }
    }

    /// Every settings file, the probe's too, keeps `~` denied and re-allows srt's
    /// seccomp helper dir (on Linux srt runs it inside the sandbox).
    #[test]
    fn settings_files_reallow_what_srt_runs_inside() {
        let helper = srt_inside(&Paths::new("/r")).display().to_string();
        assert!(
            helper.ends_with(
                "agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/@anthropic-ai/sandbox-runtime/vendor/seccomp"
            ),
            "{helper}"
        );
        for (name, policy) in [
            ("probe", Policy::default()),
            (
                "s1",
                Policy {
                    read: vec![home().join("wt")],
                    write: vec![home().join("wt")],
                    ..Policy::default()
                },
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let p = Paths::new(tmp.path());
            let f = write_settings(&p, name, &policy).unwrap();
            let s: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
            let want = srt_inside(&p).display().to_string();
            assert!(
                s["filesystem"]["allowRead"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(want)),
                "{name}"
            );
            assert_eq!(s["filesystem"]["denyRead"][0], home().display().to_string());
            assert!(
                !s["filesystem"]["allowWrite"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(want))
            );
        }
    }

    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let f = write_settings(&paths, "s1", &Policy::default()).unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let g = gitconfig(&paths).unwrap();
        assert!(
            std::fs::read_to_string(g)
                .unwrap()
                .contains("hooksPath = /dev/null")
        );
        let (program, args) = wrap(&paths, &f, false, "node", &["x.js".into()]);
        assert!(
            program.ends_with("agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/.bin/srt")
        );
        assert_eq!(args[0], "--settings");
        assert_eq!(&args[2..], ["--", "node", "x.js"]);
        // srt's own options come before `--`; the program's, even ones srt has
        // (`--settings`, `-s`, `-c`), only after it.
        let theirs: Vec<String> = ["--settings", "/x", "-s", "S", "-c", "boom"]
            .map(String::from)
            .into();
        let (_, args) = wrap(&paths, &f, true, "tool", &theirs);
        assert_eq!(
            &args[..3],
            ["--debug", "--settings", &f.display().to_string()]
        );
        assert_eq!(&args[3..5], ["--", "tool"]);
        assert_eq!(&args[5..], theirs.as_slice());
    }

    /// srt 0.0.78's refusal lines, each kept as `host:port (reason)`; every other line dropped.
    #[test]
    fn refusals_from_srt_debug_lines() {
        let cases = [
            (
                "[SandboxDebug] No matching config rule, denying: platform.claude.com:443",
                Some("platform.claude.com:443 (not on the allow list)"),
            ),
            (
                "[SandboxDebug] Denied by config rule: evil.test:80",
                Some("evil.test:80 (on the deny list)"),
            ),
            (
                "[SandboxDebug] Denying malformed host: \"a\\u0000b.test\":443",
                Some("\\\"a\\\\u0000b.test\\\":443 (malformed host)"),
            ),
            (
                "[SandboxDebug] Denied address(es) for rebind.test:443: 127.0.0.1 (loopback)",
                Some("rebind.test:443 (resolved to a denied address: loopback)"),
            ),
            (
                "[SandboxDebug] Connection blocked to platform.claude.com:443",
                None,
            ),
            (
                "[SandboxDebug] Allowed by config rule: api.anthropic.com:443",
                None,
            ),
            ("[SandboxDebug] {", None),
            ("No matching config rule, denying: x:1", None),
        ];
        for (line, want) in cases {
            assert_eq!(refusal(line).as_deref(), want, "{line}");
        }
        // A host with a newline in it stays on one line.
        assert_eq!(
            refusal("[SandboxDebug] Denied by config rule: a\nb:1").as_deref(),
            Some("a\\nb:1 (on the deny list)")
        );
    }

    #[test]
    fn records_from_the_pipe() {
        let line = |debug: bool, text: &str| json!({"debug": debug, "text": text}).to_string();
        assert_eq!(
            parse_record(&line(
                true,
                "[SandboxDebug] No matching config rule, denying: x.test:443"
            )),
            Some(SrtRecord::Refused(
                "x.test:443 (not on the allow list)".into()
            ))
        );
        // A debug message over several lines is one record, and dropped.
        assert_eq!(
            parse_record(&line(true, "[SandboxDebug] {\n  \"allowedHosts\": []\n}")),
            None
        );
        assert_eq!(
            parse_record(&line(false, "Error: bwrap failed")),
            Some(SrtRecord::Message("Error: bwrap failed".into()))
        );
        assert_eq!(
            parse_record(r#"{"dropped": 12}"#),
            Some(SrtRecord::Dropped(12))
        );
        assert_eq!(parse_record("not json"), None);
        assert_eq!(parse_record(r#"{"debug": true, "te"#), None);
    }
}

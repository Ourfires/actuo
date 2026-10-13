//! `actuo login <agent>`: logs an agent in to the config dir its sandboxed sessions
//! use (the lane's own login), inside `srt`.
//!
//! The login reads only the installed adapters, the agent's sandbox dirs and a temp
//! dir, writes only those dirs and the temp dir, reaches only `login.hosts`, and may
//! listen on localhost when `login.local_binding` is set (an OAuth callback). `open`
//! inside is a stand-in that records the URL; actuo opens it from outside.
//! Afterwards `login.status` runs with a session's sandbox settings.

use crate::config::{AgentsConfig, Paths, SessionOptions};
use crate::sandbox::{self, Policy};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Runs the agent's login (its stdin, stdout and stderr are the caller's), calling
/// `open` with each `https://` URL it asks a browser to open; then checks it. Returns
/// the status command's output.
pub async fn login(
    paths: &Paths,
    agents: &AgentsConfig,
    name: &str,
    open: impl Fn(&str),
) -> Result<String, String> {
    let a = agents
        .agents
        .get(name)
        .ok_or_else(|| format!("no agent named {name}"))?;
    let l = a
        .login
        .as_ref()
        .ok_or_else(|| format!("agents.{name} has no login in the agents config"))?;
    if let (Some(dir), Some(p), Some(v), Some(b)) =
        (a.install_dir(paths), &a.package, &a.version, &a.bin)
    {
        sandbox::install(&dir, p, v, b).await?;
    }
    sandbox::probe(paths).await?;
    let program = a.login_program(paths).expect("login is set");
    if !program.exists() {
        return Err(format!("{} doesn't exist", program.display()));
    }
    let program = program.display().to_string();
    // A lane session's options: sandboxed and clean (Codex's home is in `clean.env`).
    let opts = SessionOptions {
        sandboxed: true,
        clean: true,
        ..SessionOptions::plain()
    };
    let dirs = a.sandbox_dirs(opts);
    if dirs.is_empty() {
        return Err(format!(
            "agents.{name}.sandbox has no dirs: the login would have nowhere to go"
        ));
    }
    for d in &dirs {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let agents_dir = paths.home.join("agents");
    let git = sandbox::gitconfig(paths).map_err(|e| e.to_string())?;
    let env_for = |tmp: &Path, path: Option<String>| {
        let mut extra = a.env_for(opts);
        for k in &a.sandbox.tmp_env {
            extra.insert(k.clone(), tmp.display().to_string());
        }
        if let Some(path) = path {
            extra.insert("PATH".into(), path);
        }
        sandbox::env(tmp, &git, &extra)
    };

    // The login.
    let tmp = sandbox::temp_dir(paths, &format!("login-{name}")).map_err(|e| e.to_string())?;
    let urls = tmp.join("open-url");
    let bin = stand_ins(&tmp, &urls).map_err(|e| e.to_string())?;
    let path = match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => format!("{}:{p}", bin.display()),
        _ => format!("{}:/usr/bin:/bin", bin.display()),
    };
    let mut write = dirs.clone();
    write.push(tmp.clone());
    let policy = Policy {
        read: vec![agents_dir.clone()],
        write,
        hosts: l.hosts.clone(),
        local_binding: l.local_binding,
        unix_sockets: false,
    };
    let file = sandbox::write_settings(paths, &format!("login-{name}"), &policy)
        .map_err(|e| e.to_string())?;
    let (srt, args) = sandbox::wrap(paths, &file, false, &program, &l.args);
    let mut cmd = tokio::process::Command::new(&srt);
    // From the temp dir: Claude Code needs a readable working directory, and the
    // caller's (under ~) usually isn't in the sandbox.
    cmd.args(&args)
        .current_dir(&tmp)
        .env_clear()
        .envs(env_for(&tmp, Some(path)))
        .kill_on_drop(true);
    let mut child = crate::process::spawn(&mut cmd).map_err(|e| format!("{srt}: {e}"))?;
    let mut opened = 0;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s.map_err(|e| e.to_string())?,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                opened = open_new(&urls, opened, &open);
            }
        }
    };
    open_new(&urls, opened, &open);
    let _ = std::fs::remove_dir_all(&tmp);
    if !status.success() {
        return Err(format!("the login exited with {status}"));
    }

    // The check, with a session's settings: the agent's hosts, no local binding.
    let tmp =
        sandbox::temp_dir(paths, &format!("login-{name}-status")).map_err(|e| e.to_string())?;
    let mut write = dirs;
    write.push(tmp.clone());
    let policy = Policy {
        read: vec![agents_dir],
        write,
        hosts: a.sandbox.hosts.clone(),
        local_binding: false,
        unix_sockets: false,
    };
    let file = sandbox::write_settings(paths, &format!("login-{name}-status"), &policy)
        .map_err(|e| e.to_string())?;
    let (srt, args) = sandbox::wrap(paths, &file, false, &program, &l.status);
    let mut cmd = tokio::process::Command::new(&srt);
    cmd.args(&args)
        .current_dir(&tmp)
        .env_clear()
        .envs(env_for(&tmp, None))
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), crate::process::output(&mut cmd))
        .await
        .map_err(|_| "the status check didn't finish within 60 s".to_string())?
        .map_err(|e| format!("{srt}: {e}"))?;
    let _ = std::fs::remove_dir_all(&tmp);
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("not logged in after the login: {text}"))
    }
}

/// `open` and `xdg-open` stand-ins in `<tmp>/bin` that append their arguments to
/// `urls`, one per line. Returns the dir, to go first on `PATH`.
fn stand_ins(tmp: &Path, urls: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin)?;
    let quoted = urls.display().to_string().replace('\'', r"'\''");
    let script = format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{quoted}'\n");
    for name in ["open", "xdg-open"] {
        let p = bin.join(name);
        std::fs::write(&p, &script)?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(bin)
}

/// Calls `open` for each `https://` line of `urls` after the first `seen` lines;
/// returns the new count.
fn open_new(urls: &Path, seen: usize, open: &impl Fn(&str)) -> usize {
    let Ok(text) = std::fs::read_to_string(urls) else {
        return seen;
    };
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().skip(seen) {
        if line.starts_with("https://") {
            open(line);
        }
    }
    lines.len().max(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn login_in_the_sandbox_then_status() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("home"));
        // An srt that runs the command as is.
        let srt = sandbox::srt(&paths);
        std::fs::create_dir_all(srt.parent().unwrap()).unwrap();
        std::fs::write(
            &srt,
            "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&srt, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = dir.path().join("state");
        let toml = dir.path().join("agents.toml");
        std::fs::write(
            &toml,
            format!(
                r#"
[agents.fake]
command = "/bin/true"
[agents.fake.sandbox]
hosts = ["api.fake.test"]
env = {{ FAKE_HOME = "{}" }}
dirs = ["FAKE_HOME"]
[agents.codex-like]
command = "/bin/true"
clean.env = {{ LIKE_HOME = "{}" }}
[agents.codex-like.sandbox]
dirs = ["LIKE_HOME"]
[agents.codex-like.login]
command = "/bin/sh"
args = ["-c", "echo in > \"$LIKE_HOME/token\""]
status = ["-c", "cat \"$LIKE_HOME/token\""]
[agents.fake.login]
command = "/bin/sh"
args = ["-c", "open 'https://login.fake.test/a?b=1' && echo ok > \"$FAKE_HOME/token\" && pwd > \"$FAKE_HOME/cwd\" && echo \"$CLAUDE_CODE_TMPDIR\" > \"$FAKE_HOME/tmp\""]
status = ["-c", "cat \"$FAKE_HOME/token\""]
hosts = ["login.fake.test"]
local_binding = true
"#,
                state.display(),
                dir.path().join("like").display()
            ),
        )
        .unwrap();
        let agents = AgentsConfig::load(&toml).unwrap();
        let opened = Mutex::new(Vec::new());
        let out = login(&paths, &agents, "fake", |u| {
            opened.lock().unwrap().push(u.to_string())
        })
        .await
        .unwrap();
        assert_eq!(out, "ok");
        assert_eq!(
            *opened.lock().unwrap(),
            ["https://login.fake.test/a?b=1".to_string()]
        );
        let settings = |n: &str| -> serde_json::Value {
            let f = paths.home.join("sandbox").join(format!("{n}.json"));
            serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
        };
        let s = settings("login-fake");
        assert_eq!(
            s["network"]["allowedDomains"],
            serde_json::json!(["login.fake.test"])
        );
        assert_eq!(s["network"]["allowLocalBinding"], true);
        let w = s["filesystem"]["allowWrite"].as_array().unwrap();
        assert!(w.contains(&serde_json::json!(state.display().to_string())));
        let s = settings("login-fake-status");
        assert_eq!(
            s["network"]["allowedDomains"],
            serde_json::json!(["api.fake.test"])
        );
        assert!(s["network"].get("allowLocalBinding").is_none());
        let cwd = std::fs::read_to_string(state.join("cwd")).unwrap();
        assert!(cwd.trim().ends_with("/home/tmp/login-fake"), "{cwd}");
        // The login's own temp dir, which srt makes its TMPDIR inside.
        let tmp = std::fs::read_to_string(state.join("tmp")).unwrap();
        assert!(tmp.trim().ends_with("/home/tmp/login-fake"), "{tmp}");
        assert!(!paths.home.join("tmp/login-fake").exists());

        // A config dir named in `clean.env`, as Codex's is.
        let out = login(&paths, &agents, "codex-like", |_| {}).await.unwrap();
        assert_eq!(out, "in");

        let err = login(&paths, &agents, "nope", |_| {}).await.unwrap_err();
        assert!(err.contains("no agent named nope"), "{err}");
    }
}

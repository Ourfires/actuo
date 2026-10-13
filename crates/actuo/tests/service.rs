//! `actuo service install|uninstall` and `actuo restart`, against a temp `HOME` and a `launchctl` (or
//! `systemctl`) that records its arguments and starts or stops `actuo serve` in
//! that home itself: nothing is loaded on the machine.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    /// `PATH` for actuo: the recording `launchctl` first, in a folder whose
    /// name needs escaping in XML.
    path: String,
    calls: PathBuf,
    /// How many more times `launchctl print` says the service is still there.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    still: PathBuf,
    /// Shell functions for the fakes: `start` runs the installed `actuo serve`
    /// (unless the file `broken` exists), `stop` ends it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    serve: String,
    root: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.root.join("serve.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn serve_functions(root: &Path) -> String {
    format!(
        r#"start() {{
  [ -e '{root}/broken' ] && return 0
  "${{ACTUO_HOME:-$HOME/.actuo}}/bin/actuo" serve </dev/null >/dev/null 2>&1 &
  echo $! > '{root}/serve.pid'
}}
stop() {{
  [ -e '{root}/serve.pid' ] || return 0
  pid=$(cat '{root}/serve.pid'); rm -f '{root}/serve.pid'
  kill $pid 2>/dev/null
  while kill -0 $pid 2>/dev/null; do sleep 0.1; done
}}
"#,
        root = root.display()
    )
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bin = root.join("bin <a&b>");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = root.join("launchctl.log");
    // Records its arguments. `print` (is the service still there?) answers yes as
    // many times as the number in `still`, then "not found" (113), like launchd
    // dropping a service a moment after `bootout`.
    let still = root.join("still");
    std::fs::write(&still, "0").unwrap();
    let serve = serve_functions(&root);
    executable(
        &bin.join("launchctl"),
        &format!(
            "#!/bin/sh\n{serve}echo \"$*\" >> '{calls}'\n\
             if [ \"$1\" = print ]; then\n\
               n=$(cat '{still}')\n\
               if [ \"$n\" -gt 0 ]; then echo $((n - 1)) > '{still}'; exit 0; fi\n\
               exit 113\n\
             fi\n\
             if [ \"$1\" = bootout ]; then stop; fi\n\
             if [ \"$1\" = bootstrap ]; then start; fi\n",
            calls = calls.display(),
            still = still.display()
        ),
    );
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display());
    Setup {
        _dir: dir,
        home,
        path,
        calls,
        still: root.join("still"),
        serve,
        root,
    }
}

fn run(s: &Setup, args: &[&str], extra: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_actuo"))
        .args(args)
        .env_clear()
        .env("HOME", &s.home)
        .env("PATH", &s.path)
        // The actuo it starts never asks GitHub for releases.
        .env(
            "ACTUO_RELEASES_URL",
            format!("file://{}/releases", s.root.display()),
        )
        .envs(extra.iter().copied())
        .output()
        .unwrap()
}

#[cfg(target_os = "macos")]
fn extract(plist: &Path, key: &str) -> String {
    let out = Command::new("plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(plist)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{key}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[cfg(target_os = "macos")]
fn install_and_uninstall() {
    let s = setup();
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let domain = format!("gui/{}", uid.trim());
    let plist = s.home.join("Library/LaunchAgents/co.ourfires.actuo.plist");
    let actuo_home = s.home.join("actuo home");

    let out = run(
        &s,
        &["service", "install"],
        &[("ACTUO_HOME", actuo_home.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // This binary, copied into actuo's home, executable.
    let bin = actuo_home.join("bin/actuo");
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        std::fs::read(env!("CARGO_BIN_EXE_actuo")).unwrap()
    );
    assert_eq!(
        std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(extract(&plist, "Label"), "co.ourfires.actuo");
    assert_eq!(
        extract(&plist, "ProgramArguments.0"),
        bin.display().to_string()
    );
    assert_eq!(extract(&plist, "ProgramArguments.1"), "serve");
    // PATH as it was at install, `<` and `&` included.
    assert_eq!(extract(&plist, "EnvironmentVariables.PATH"), s.path);
    assert_eq!(
        extract(&plist, "EnvironmentVariables.ACTUO_HOME"),
        actuo_home.display().to_string()
    );
    assert_eq!(
        extract(&plist, "WorkingDirectory"),
        s.home.display().to_string()
    );
    assert_eq!(extract(&plist, "RunAtLoad"), "true");
    // Restarted after a crash, not after a clean stop.
    assert_eq!(extract(&plist, "KeepAlive.SuccessfulExit"), "false");
    let log = actuo_home.join("logs/serve.log").display().to_string();
    assert_eq!(extract(&plist, "StandardOutPath"), log);
    assert_eq!(extract(&plist, "StandardErrorPath"), log);
    assert!(actuo_home.join("logs").is_dir());
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    assert_eq!(
        calls,
        format!(
            "bootout {domain}/co.ourfires.actuo\nprint {domain}/co.ourfires.actuo\nbootstrap {domain} {}\n",
            plist.display()
        )
    );

    // Run again from the installed copy: it stays, and the agent is reloaded once
    // launchd has dropped the old one (here after two more checks).
    std::fs::write(&s.still, "2").unwrap();
    std::fs::write(&s.calls, "").unwrap();
    let out = Command::new(&bin)
        .args(["service", "install"])
        .env_clear()
        .env("HOME", &s.home)
        .env("PATH", &s.path)
        .env("ACTUO_HOME", &actuo_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(bin.exists());
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    assert_eq!(
        calls,
        format!(
            "bootout {domain}/co.ourfires.actuo\nprint {domain}/co.ourfires.actuo\nprint {domain}/co.ourfires.actuo\n\
             print {domain}/co.ourfires.actuo\nbootstrap {domain} {}\n",
            plist.display()
        )
    );

    // Without ACTUO_HOME: under ~/.actuo.
    let out = run(&s, &["service", "install"], &[]);
    assert!(out.status.success());
    let default_bin = s.home.join(".actuo/bin/actuo");
    assert_eq!(
        extract(&plist, "ProgramArguments.0"),
        default_bin.display().to_string()
    );
    assert_eq!(
        extract(&plist, "StandardOutPath"),
        s.home.join(".actuo/logs/serve.log").display().to_string()
    );
    assert!(
        !String::from_utf8_lossy(
            &Command::new("plutil")
                .args(["-p"])
                .arg(&plist)
                .output()
                .unwrap()
                .stdout
        )
        .contains("ACTUO_HOME")
    );

    let out = run(&s, &["service", "uninstall"], &[]);
    assert!(out.status.success());
    assert!(!plist.exists());
    assert!(!default_bin.exists());
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    assert!(
        calls.ends_with(&format!(
            "bootout {domain}/co.ourfires.actuo\nprint {domain}/co.ourfires.actuo\n"
        )),
        "{calls}"
    );
    assert_eq!(run(&s, &["service"], &[]).status.code(), Some(2));
}

/// launchd that never drops the old service: no bootstrap, an error that says so.
#[test]
#[cfg(target_os = "macos")]
fn a_service_that_isnt_dropped() {
    let s = setup();
    std::fs::write(&s.still, "1000").unwrap();
    let started = std::time::Instant::now();
    let out = run(&s, &["service", "install"], &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("launchd still has co.ourfires.actuo 5 s after"),
        "{stderr}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(9));
    assert!(
        !std::fs::read_to_string(&s.calls)
            .unwrap()
            .contains("bootstrap")
    );
}

/// Linux: `systemctl` and `loginctl` that record their arguments; `systemctl`
/// starts and stops `actuo serve`, and `loginctl` says linger is off.
#[cfg(target_os = "linux")]
fn linux_fakes(s: &Setup) {
    // The recorders' folder: the first on actuo's PATH.
    let bin_dir = s.path.split(':').next().unwrap().to_string();
    for tool in ["systemctl", "loginctl"] {
        let answer = if tool == "loginctl" {
            "echo no\n"
        } else {
            "case \"$*\" in\n\
               *restart*) stop; start ;;\n\
               *disable*--now*) stop ;;\n\
             esac\n"
        };
        executable(
            &Path::new(&bin_dir).join(tool),
            &format!(
                "#!/bin/sh\n{}echo \"{tool} $*\" >> '{}'\n{answer}",
                s.serve,
                s.calls.display()
            ),
        );
    }
}

/// A new actuo that doesn't answer after the restart: the binary it replaced goes
/// back and is restarted, and the error says so.
#[test]
fn a_new_actuo_that_doesnt_answer_is_put_back() {
    let s = setup();
    #[cfg(target_os = "linux")]
    linux_fakes(&s);
    let bin = s.home.join(".actuo/bin/actuo");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    let old = "#!/bin/sh\nexit 1\n";
    std::fs::write(&bin, old).unwrap();
    std::fs::write(s.root.join("broken"), "").unwrap();
    let out = run(
        &s,
        &["service", "install"],
        &[("ACTUO_TEST_START_LIMIT_S", "1")],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the new actuo didn't answer within 1 s; the previous one"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), old);
    assert_eq!(
        std::fs::read_to_string(bin.with_file_name("actuo.prev")).unwrap(),
        old
    );
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    let restarts = if cfg!(target_os = "linux") {
        calls.matches("systemctl --user restart").count()
    } else {
        calls.matches("bootstrap").count()
    };
    assert_eq!(restarts, 2, "{calls}");
}

/// `actuo restart`: refused before an install; after one, the service restarts and
/// actuo answers.
#[test]
fn restart() {
    let s = setup();
    #[cfg(target_os = "linux")]
    linux_fakes(&s);
    let out = run(&s, &["restart"], &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("`actuo service install` installs it"),
        "{stderr}"
    );

    assert!(run(&s, &["service", "install"], &[]).status.success());
    std::fs::write(&s.calls, "").unwrap();
    let out = run(&s, &["restart"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("actuo answers: "), "{stdout}");
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    if cfg!(target_os = "linux") {
        assert_eq!(calls, "systemctl --user restart actuo.service\n");
    } else {
        assert!(
            calls.starts_with("bootout ") && calls.contains("\nbootstrap "),
            "{calls}"
        );
    }
}

/// Linux: a systemd user unit, with `systemctl` and `loginctl` that only record
/// their arguments (`loginctl` says linger is off).
#[test]
#[cfg(target_os = "linux")]
fn install_and_uninstall_on_linux() {
    let s = setup();
    linux_fakes(&s);
    let unit = s.home.join(".config/systemd/user/actuo.service");
    let bin = s.home.join(".actuo/bin/actuo");

    let out = run(&s, &["service", "install"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        std::fs::read(env!("CARGO_BIN_EXE_actuo")).unwrap()
    );
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(
        text.contains(&format!("ExecStart=\"{}\" serve\n", bin.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("Environment=\"PATH={}\"\n", s.path)),
        "{text}"
    );
    assert!(text.contains("Restart=on-failure\n"));
    let calls = std::fs::read_to_string(&s.calls).unwrap();
    assert!(
        calls.starts_with(
            "systemctl --user daemon-reload\nsystemctl --user enable actuo.service\n\
             systemctl --user restart actuo.service\nloginctl show-user "
        ),
        "{calls}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("linger is off"), "{stdout}");

    std::fs::write(&s.calls, "").unwrap();
    let out = run(&s, &["service", "uninstall"], &[]);
    assert!(out.status.success());
    assert!(!unit.exists());
    assert!(!bin.exists());
    assert_eq!(
        std::fs::read_to_string(&s.calls).unwrap(),
        "systemctl --user disable --now actuo.service\nsystemctl --user daemon-reload\n"
    );

    // A system-wide unit for actuo, from root: refused, nothing written. Only
    // where this test may write /etc (as root).
    let system = Path::new("/etc/systemd/user/actuo.service");
    if !system.exists()
        && std::fs::create_dir_all("/etc/systemd/user").is_ok()
        && std::fs::write(system, "").is_ok()
    {
        let out = run(&s, &["service", "install"], &[]);
        std::fs::remove_file(system).unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("As root, remove that file first."),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!unit.exists());
        assert!(!bin.exists());
    }
}

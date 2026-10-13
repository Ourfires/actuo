//! Shared by the CLI tests: an actuo started as `actuo serve` in a temp home,
//! with the scripted fake agent as agent `fake`.
#![allow(dead_code)]

pub mod lane;

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The `fake-agent` binary, built once per test run so it is never stale.
pub fn fake_agent() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "fake-agent"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fake-agent")
    })
    .clone()
}

pub struct Env {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub serve: Option<Child>,
    /// Extra environment for `actuo serve`.
    pub env: Vec<(String, String)>,
}

impl Env {
    /// A temp home with an agents.toml for `fake` following `script`, and a repo.
    pub fn new(script: &Value) -> Self {
        Self::with_agents(&[("fake", script.clone())])
    }

    /// Like `new`, with one fake agent per (name, script). Each agent keeps its
    /// sessions in its own state directory and logs to `<name>-log.jsonl`.
    pub fn with_agents(agents: &[(&str, Value)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut toml = String::new();
        for (name, script) in agents {
            let script_path = root.join(format!("{name}-script.json"));
            std::fs::write(&script_path, script.to_string()).unwrap();
            toml.push_str(&format!(
                r#"
                [agents.{name}]
                command = "{}"
                args = ["--script", "{}", "--state", "{}", "--log", "{}"]
                modes = {{ ask = "ask", edits = "edits" }}
                "#,
                fake_agent().display(),
                script_path.display(),
                root.join(format!("{name}-state")).display(),
                root.join(format!("{name}-log.jsonl")).display(),
            ));
        }
        std::fs::write(home.join("agents.toml"), toml).unwrap();
        let repo = root.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
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
            assert!(out.status.success());
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "readme\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        // No release folder: actuo's release check never asks GitHub here.
        let releases = format!("file://{}/releases", root.display());
        let agents_env = root.join("agents.env").display().to_string();
        Self {
            _dir: dir,
            root,
            home,
            repo,
            serve: None,
            // Nor does it read a real ~/.config/actuo/agents.env.
            env: vec![
                ("ACTUO_RELEASES_URL".into(), releases),
                ("ACTUO_AGENTS_ENV".into(), agents_env),
            ],
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("actuo.sock")
    }

    /// Starts `actuo serve` and waits until its socket accepts connections.
    pub fn start_actuo(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("serve.log"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_actuo"))
            .arg("serve")
            // Not the test runner's own (CI's runs under systemd); a test sets it.
            .env_remove("JOURNAL_STREAM")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("ACTUO_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.serve = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("actuo didn't start: {}", self.serve_log());
    }

    /// Stops `actuo serve` with SIGTERM (graceful) or SIGKILL, and waits for it.
    pub fn stop_actuo(&mut self, kill: bool) {
        let mut child = self.serve.take().expect("actuo running");
        let sig = if kill { "-KILL" } else { "-TERM" };
        Command::new("kill")
            .args([sig, &child.id().to_string()])
            .status()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "actuo didn't stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn serve_log(&self) -> String {
        std::fs::read_to_string(self.root.join("serve.log")).unwrap_or_default()
    }

    /// Runs an actuo command and returns its output.
    pub fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_actuo"))
            .args(args)
            .env("ACTUO_HOME", &self.home)
            .env("ACTUO_AGENTS_ENV", self.root.join("agents.env"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs an actuo command that must succeed; returns stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "actuo {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stderr),
            self.serve_log()
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs an actuo command that must fail; returns stderr.
    pub fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "actuo {args:?} succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    /// What the fake agent `name` received, one JSON object per message.
    pub fn agent_log(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.root.join(format!("{name}-log.jsonl")))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.serve.is_some() {
            self.stop_actuo(true);
        }
        // An actuo killed with SIGKILL leaves its agents behind: kill their groups.
        for entry in std::fs::read_dir(&self.root).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with("-log.jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .lines()
            {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if (v["method"] == "started" || v["method"] == "spawned")
                    && let Some(pid) = v["params"]["pid"].as_u64()
                {
                    let _ = Command::new("kill")
                        // `--`: Linux's kill reads `-<pid>` as options and kills
                        // every process it can (`kill(-1)`) without it.
                        .args(["-KILL", "--", &format!("-{pid}")])
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
}

/// The pid of the latest process the fake agent `name` spawned as a step.
pub fn last_spawned_pid(env: &Env, name: &str) -> u32 {
    env.agent_log(name)
        .iter()
        .rev()
        .find(|v| v["method"] == "spawned")
        .and_then(|v| v["params"]["pid"].as_u64())
        .expect("the agent spawned something") as u32
}

/// The pid of the latest fake agent process `name` started.
pub fn last_agent_pid(env: &Env, name: &str) -> u32 {
    env.agent_log(name)
        .iter()
        .rev()
        .find(|v| v["method"] == "started")
        .and_then(|v| v["params"]["pid"].as_u64())
        .expect("the agent started") as u32
}

/// `actuo attach` as a child: its stdout lines, and its stdin.
pub struct Attach {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
    pub seen: Vec<String>,
}

impl Attach {
    pub fn start(env: &Env, session: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_actuo"))
            .args(["attach", session])
            .env("ACTUO_HOME", &env.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines: rx,
            seen: Vec::new(),
        }
    }

    /// Waits for a line containing `text`; returns it.
    pub fn wait_for(&mut self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(text) {
                        return line;
                    }
                }
                Err(_) => panic!("no line with {text:?}; seen:\n{}", self.seen.join("\n")),
            }
        }
    }

    pub fn type_line(&mut self, line: &str) {
        use std::io::Write;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }
}

impl Drop for Attach {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a process with this pid exists and isn't a zombie (exited, not reaped).
pub fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

//! A lane test: `actuo serve` with `fake-gh` as `gh` on `PATH`, a local bare repo
//! as each repo's `origin`, a git identity of its own, and `settings.toml` written
//! before actuo starts.

use super::Env;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The `fake-gh` binary, built once per test run.
pub fn fake_gh() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "fake-gh"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fake-gh")
    })
    .clone()
}

/// The `fake-srt` binary, built once per test run.
pub fn fake_srt() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "fake-srt"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("run cargo");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("fake-srt")
    })
    .clone()
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct LaneEnv {
    pub env: Env,
    pub gh_state: PathBuf,
}

impl LaneEnv {
    /// `repos`: `owner/repo` and its settings table (TOML, under
    /// `[projects."owner/repo"]`; `{root}` stands for the test's temp dir). `lane`:
    /// more `[lane]` keys. Issues are added with [`LaneEnv::issue`] before or while
    /// actuo runs. `srt` is `fake-srt`: it records and runs without a sandbox.
    pub fn new(script: &Value, repos: &[(&str, &str)], lane: &str) -> Self {
        let mut env = Env::new(script);
        let root = env.root.clone();

        // A git identity and config of the test's own (also turns hooks on through a
        // tracked folder, which actuo must ignore).
        let gitconfig = root.join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n[core]\n\thooksPath = .githooks\n",
        )
        .unwrap();

        let mut gh_repos = serde_json::Map::new();
        let mut settings = format!(
            "[sessions]\nagent = \"fake\"\n\n[lane]\nenabled = true\npoll_interval = \"1s\"\n{lane}\n"
        );
        for (name, table) in repos {
            let origin = root.join(format!("{}.git", name.replace('/', "-")));
            git(
                &root,
                &[
                    "init",
                    "-q",
                    "--bare",
                    "-b",
                    "main",
                    origin.to_str().unwrap(),
                ],
            );
            let seed = root.join(format!("seed-{}", name.replace('/', "-")));
            git(
                &root,
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    seed.to_str().unwrap(),
                ],
            );
            std::fs::write(seed.join("README.md"), "readme\n").unwrap();
            git(&seed, &["add", "-A"]);
            git(&seed, &["commit", "-q", "-m", "init"]);
            git(&seed, &["push", "-q", "origin", "main"]);
            gh_repos.insert(
                name.to_string(),
                json!({"origin": origin, "default_branch": "main", "labels": [], "issues": []}),
            );
            let table = table.replace("{root}", &root.display().to_string());
            // Its lane on unless the test says otherwise (a project's is off by
            // default).
            let lane = if table.contains("lane =") {
                ""
            } else {
                "lane = true\n"
            };
            settings.push_str(&format!("\n[projects.\"{name}\"]\n{lane}{table}\n"));
        }
        std::fs::write(env.home.join("settings.toml"), settings).unwrap();
        let gh_state = root.join("gh-state.json");
        std::fs::write(
            &gh_state,
            json!({"login": "me", "repos": gh_repos}).to_string(),
        )
        .unwrap();

        let srt = env
            .home
            .join("agents/@anthropic-ai/sandbox-runtime@0.0.78/node_modules/.bin");
        std::fs::create_dir_all(&srt).unwrap();
        std::os::unix::fs::symlink(fake_srt(), srt.join("srt")).unwrap();

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(fake_gh(), bin.join("gh")).unwrap();
        // Never the real ones: no notifications on the machine, no sleep held. Each
        // records its arguments, one line per call, tab-separated.
        executable(
            &bin.join("osascript"),
            &format!(
                "#!/bin/sh\n{{ for a in \"$@\"; do printf '%s\\t' \"$a\"; done; echo; }} >> '{}'\n",
                root.join("osascript.log").display()
            ),
        );
        // Lives like caffeinate -w: until killed or the pid it watches exits.
        executable(
            &bin.join("caffeinate"),
            &format!(
                "#!/bin/sh\necho \"$$\t$*\" >> '{}'\nwhile kill -0 \"$3\" 2>/dev/null; do sleep 0.1; done\n",
                root.join("caffeinate.log").display()
            ),
        );
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        env.env.extend([
            ("PATH".into(), path),
            ("FAKE_GH_STATE".into(), gh_state.display().to_string()),
            ("GIT_CONFIG_GLOBAL".into(), gitconfig.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]);
        Self { env, gh_state }
    }

    /// The notifications posted: each call's arguments.
    pub fn notifications(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(self.env.root.join("osascript.log")).unwrap_or_default();
        text.lines()
            .map(|l| {
                let mut args: Vec<String> = l.split('\t').map(str::to_string).collect();
                args.pop(); // after the last tab
                args
            })
            .collect()
    }

    /// Each `caffeinate` started: its pid and arguments.
    pub fn caffeinate_calls(&self) -> Vec<(u32, String)> {
        let text =
            std::fs::read_to_string(self.env.root.join("caffeinate.log")).unwrap_or_default();
        text.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(pid, args)| (pid.parse().unwrap(), args.to_string()))
            .collect()
    }

    /// Adds TOML to the agents config (before actuo starts).
    pub fn agents_toml(&self, more: &str) {
        let path = self.env.home.join("agents.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(more);
        std::fs::write(path, text).unwrap();
    }

    /// What `fake-srt` recorded for a settings file (`~/.actuo/sandbox/<name>.json`).
    pub fn srt_calls(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(
            self.env
                .home
                .join(format!("sandbox/{name}.json.calls.jsonl")),
        )
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
    }

    pub fn origin(&self, repo: &str) -> PathBuf {
        self.env
            .root
            .join(format!("{}.git", repo.replace('/', "-")))
    }

    /// GitHub as fake-gh has it now.
    pub fn gh(&self) -> Value {
        let _lock = self.lock();
        serde_json::from_str(&std::fs::read_to_string(&self.gh_state).unwrap()).unwrap()
    }

    /// Every call fake-gh got, as recorded (`{"args", "stdin", "envNames"}` per line).
    pub fn gh_calls(&self) -> String {
        let mut p = self.gh_state.as_os_str().to_owned();
        p.push(".calls.jsonl");
        std::fs::read_to_string(std::path::PathBuf::from(p)).unwrap_or_default()
    }

    /// fake-gh's state as it is now.
    #[allow(dead_code)]
    pub fn gh_state(&self) -> Value {
        let _lock = self.lock();
        serde_json::from_str(&std::fs::read_to_string(&self.gh_state).unwrap()).unwrap()
    }

    /// Changes GitHub under fake-gh's lock.
    pub fn edit_gh(&self, f: impl FnOnce(&mut Value)) {
        let _lock = self.lock();
        let mut v: Value =
            serde_json::from_str(&std::fs::read_to_string(&self.gh_state).unwrap()).unwrap();
        f(&mut v);
        let tmp = self.gh_state.with_extension("json.tmp");
        std::fs::write(&tmp, v.to_string()).unwrap();
        std::fs::rename(&tmp, &self.gh_state).unwrap();
    }

    /// A repo on (fake) GitHub that isn't a project here: its origin with `files`
    /// committed on `main`, and `extra` fields in fake-gh's state (e.g.
    /// `{"permission": "READ"}`).
    pub fn github_repo(&self, name: &str, files: &[(&str, &str)], extra: Value) {
        let root = &self.env.root;
        let origin = root.join(format!("{}.git", name.replace('/', "-")));
        git(
            root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        let seed = root.join(format!("seed-{}", name.replace('/', "-")));
        git(
            root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        for (path, text) in files {
            let path = seed.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        self.edit_gh(|v| {
            let mut repo =
                json!({"origin": origin, "default_branch": "main", "labels": [], "issues": []});
            if let (Some(r), Some(e)) = (repo.as_object_mut(), extra.as_object()) {
                r.extend(e.clone());
            }
            v["repos"][name] = repo;
        });
    }

    /// Adds an issue; `labeled_by` adds each label as that user (an event).
    pub fn issue(
        &self,
        repo: &str,
        number: u64,
        title: &str,
        body: &str,
        labels: &[&str],
        labeled_by: &str,
    ) {
        self.edit_gh(|v| {
            let r = &mut v["repos"][repo];
            for l in labels {
                let known = r["labels"].as_array().unwrap().iter().any(|x| x == l);
                if !known {
                    r["labels"].as_array_mut().unwrap().push(json!(l));
                }
            }
            r["issues"].as_array_mut().unwrap().push(json!({
                "number": number, "title": title, "body": body, "state": "open",
                "author": "you", "labels": labels,
                "events": labels.iter().map(|l| json!({"event": "labeled", "label": l, "actor": labeled_by})).collect::<Vec<_>>(),
            }));
        });
    }

    pub fn issue_json(&self, repo: &str, number: u64) -> Value {
        self.gh()["repos"][repo]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["number"] == number)
            .cloned()
            .unwrap()
    }

    pub fn labels(&self, repo: &str, number: u64) -> Vec<String> {
        self.issue_json(repo, number)["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    }

    pub fn comments(&self, repo: &str, number: u64) -> Vec<String> {
        self.issue_json(repo, number)["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["body"].as_str().unwrap().to_string())
            .collect()
    }

    /// The PR from `actuo/issue-<n>`.
    pub fn pr(&self, repo: &str, number: u64) -> Option<Value> {
        let head = format!("actuo/issue-{number}");
        self.gh()["repos"][repo]["pulls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["head"] == head.as_str())
            .cloned()
    }

    /// `actuo runs`, one line per run: id, repo#n, state (reason), step, session, PR.
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.env
            .ok(&["runs"])
            .lines()
            .map(|l| l.split('\t').map(str::to_string).collect())
            .collect()
    }

    pub fn run_of(&self, repo: &str, number: u64) -> Option<Vec<String>> {
        let key = format!("{repo}#{number}");
        self.runs().into_iter().find(|r| r[1] == key)
    }

    /// Waits until `f` holds, failing with actuo's log after `secs`.
    pub fn wait(&self, what: &str, secs: u64, mut f: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for {what}\nruns: {:#?}\ngh: {:#}\n{}",
            self.runs(),
            self.gh(),
            self.env.serve_log()
        );
    }

    /// The commit a ref of `origin` points at.
    pub fn origin_ref(&self, repo: &str, name: &str) -> Option<String> {
        let out = Command::new("git")
            .args([
                "--git-dir",
                self.origin(repo).to_str().unwrap(),
                "rev-parse",
                "--verify",
                "--quiet",
                name,
            ])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Commits `path` with `contents` to `repo`'s `main` on origin (through the seed
    /// clone): what the default branch has before a run starts.
    pub fn commit_to_main(&self, repo: &str, path: &str, contents: &str) {
        let seed = self
            .env
            .root
            .join(format!("seed-{}", repo.replace('/', "-")));
        std::fs::write(seed.join(path), contents).unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", &format!("add {path}")]);
        git(&seed, &["push", "-q", "origin", "main"]);
    }

    pub fn origin_file(&self, repo: &str, rev: &str, path: &str) -> Option<String> {
        let out = Command::new("git")
            .args([
                "--git-dir",
                self.origin(repo).to_str().unwrap(),
                "show",
                &format!("{rev}:{path}"),
            ])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Adds a comment to an issue (or a PR's conversation) by `author`, later than
    /// anything fake-gh's clock has stamped.
    pub fn comment(&self, repo: &str, number: u64, author: &str, body: &str) {
        self.edit_gh(|v| {
            let id = 5000 + v["next_id"].as_u64().unwrap_or(0) + v["clock"].as_u64().unwrap_or(0);
            v["next_id"] = json!(id);
            let c = json!({"id": id, "author": author, "body": body,
                           "created_at": "2026-10-01T23:00:00Z", "updated_at": "2026-10-01T23:00:00Z"});
            let r = &mut v["repos"][repo];
            if let Some(i) = r["issues"].as_array_mut().unwrap().iter_mut().find(|i| i["number"] == number) {
                i["comments"].as_array_mut().unwrap().push(c);
            } else if let Some(p) = r["pulls"].as_array_mut().unwrap().iter_mut().find(|p| p["number"] == number) {
                p["comments"].as_array_mut().unwrap().push(c);
            }
        });
    }

    /// An inline review comment on the PR from `actuo/issue-<issue>`.
    pub fn review_comment(
        &self,
        repo: &str,
        issue: u64,
        author: &str,
        path: &str,
        line: u64,
        body: &str,
    ) {
        let head = format!("actuo/issue-{issue}");
        self.edit_gh(|v| {
            let id = 7000 + v["clock"].as_u64().unwrap_or(0);
            let p = v["repos"][repo]["pulls"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|p| p["head"] == head.as_str())
                .unwrap();
            p["review_comments"].as_array_mut().unwrap().push(json!({
                "id": id, "author": author, "body": body, "path": path, "line": line,
                "created_at": "2026-10-01T23:00:01Z"}));
        });
    }

    /// Removes the trigger label.
    pub fn unlabel(&self, repo: &str, number: u64) {
        self.edit_gh(|v| {
            let i = v["repos"][repo]["issues"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|i| i["number"] == number)
                .unwrap();
            i["labels"].as_array_mut().unwrap().retain(|l| l != "actuo");
        });
    }

    /// Adds the trigger label again, as `by`.
    pub fn relabel(&self, repo: &str, number: u64, by: &str) {
        self.edit_gh(|v| {
            let i = v["repos"][repo]["issues"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|i| i["number"] == number)
                .unwrap();
            i["labels"].as_array_mut().unwrap().push(json!("actuo"));
            i["events"]
                .as_array_mut()
                .unwrap()
                .push(json!({"event": "labeled", "label": "actuo", "actor": by}));
        });
    }

    /// The runs of an issue, newest first.
    pub fn runs_of(&self, repo: &str, number: u64) -> Vec<Vec<String>> {
        let key = format!("{repo}#{number}");
        self.runs().into_iter().filter(|r| r[1] == key).collect()
    }

    pub fn prs_from(&self, repo: &str, number: u64) -> usize {
        let head = format!("actuo/issue-{number}");
        self.gh()["repos"][repo]["pulls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["head"] == head.as_str())
            .count()
    }

    /// The prompts the fake agent got, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.env
            .agent_log("fake")
            .into_iter()
            .filter(|m| m["event"] == "prompt" || m["method"] == "session/prompt")
            .filter_map(|m| {
                let p = &m["params"]["prompt"];
                let text = p
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .or_else(|| m["text"].as_str().map(str::to_string))?;
                Some(text)
            })
            .collect()
    }

    fn lock(&self) -> Lock {
        let path = self.gh_state.with_extension("json.lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Lock(path),
                Err(_) if start.elapsed() > Duration::from_secs(10) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

/// fake-gh's lock file (`<state>.lock`), held while a test reads or edits the state.
struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A bare JSON-RPC client over actuo's socket.
pub struct Rpc {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next: i64,
    notes: Vec<Value>,
}

impl Rpc {
    pub fn connect(socket: &std::path::Path) -> Self {
        Self::connect_as(socket, "test")
    }

    /// Connects with `name` as the client's name.
    pub fn connect_as(socket: &std::path::Path, name: &str) -> Self {
        let s = std::os::unix::net::UnixStream::connect(socket).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let mut rpc = Self {
            reader: std::io::BufReader::new(s.try_clone().unwrap()),
            writer: s,
            next: 1,
            notes: Vec::new(),
        };
        rpc.call(
            "initialize",
            json!({"protocolVersion": 1, "client": {"name": name, "version": "0"}}),
        );
        rpc
    }

    fn read(&mut self) -> Value {
        use std::io::BufRead;
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    pub fn call(&mut self, method: &str, params: Value) -> Value {
        use std::io::Write;
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{msg}").unwrap();
        loop {
            let v = self.read();
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{method}: {v}");
                return v["result"].clone();
            }
            self.notes.push(v);
        }
    }

    /// Notifications of `method` received so far, after a round trip that flushes
    /// the ones in flight.
    pub fn drain(&mut self, method: &str) -> Vec<Value> {
        self.call("run/list", json!({}));
        let notes = std::mem::take(&mut self.notes);
        notes
            .into_iter()
            .filter(|n| n["method"] == method)
            .map(|n| n["params"].clone())
            .collect()
    }

    /// Session events up to `seq`.
    pub fn events_until(&mut self, seq: u64) -> Vec<Value> {
        let mut out: Vec<Value> = std::mem::take(&mut self.notes)
            .into_iter()
            .filter(|n| n["method"] == "event")
            .map(|n| n["params"].clone())
            .collect();
        while out.last().is_none_or(|e| e["seq"].as_u64().unwrap() < seq) {
            let v = self.read();
            if v["method"] == "event" {
                out.push(v["params"].clone());
            }
        }
        out
    }
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

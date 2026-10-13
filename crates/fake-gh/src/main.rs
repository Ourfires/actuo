//! `fake-gh`: answers the `gh` commands actuo's lane uses from a JSON file
//! that stands in for GitHub, and records every call.
//!
//! - `FAKE_GH_STATE`: the state file ([`State`]); tests write it, this binary
//!   reads and updates it under `<state>.lock`.
//! - Calls are appended to `<state>.calls.jsonl`: `{"args": [...], "stdin": "...", "envNames": [...]}`.
//! - `repo clone` clones the repo's `origin` (a local bare repo); `pr create` needs
//!   the head branch to exist there.
//! - `FAKE_GH_SLOW_PR_CREATE`: a flag file; while it exists, the next `pr create`
//!   removes it, writes `<flag>.started` and takes 3 s.
//!
//! Only the commands and flags the lane uses are understood; anything else exits 2
//! with a message, so a test notices a new call.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The account gh is logged in as.
    pub login: String,
    pub repos: BTreeMap<String, Repo>,
    /// Ids for new comments.
    pub next_id: u64,
    /// The fake clock, in seconds after 2026-10-01T00:00:00Z; one more on every change.
    pub clock: u64,
    /// The login's pending invitations: accepting one makes its repo visible
    /// (`hidden` off) with its permission.
    pub invitations: Vec<Invitation>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Invitation {
    pub id: u64,
    /// `owner/repo`.
    pub repo: String,
    /// The permission it gives (`WRITE` when empty).
    pub permission: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Repo {
    /// A local bare repository standing in for GitHub's.
    pub origin: PathBuf,
    pub default_branch: String,
    /// The login's permission (`ADMIN` when empty), the visibility (`PRIVATE` when
    /// empty), whether it's archived, whether the default branch is protected.
    pub permission: String,
    pub visibility: String,
    pub archived: bool,
    pub protected: bool,
    /// The login can't see it (a private repo it hasn't been invited to, or whose
    /// invitation it hasn't accepted): `repo view` doesn't find it.
    pub hidden: bool,
    pub labels: Vec<String>,
    pub issues: Vec<Issue>,
    pub pulls: Vec<Pull>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author: String,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
    pub events: Vec<IssueEvent>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IssueEvent {
    /// `labeled` or `unlabeled`.
    pub event: String,
    pub label: String,
    pub actor: String,
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    pub state: String,
    pub url: String,
    pub comments: Vec<Comment>,
    /// Inline review comments.
    pub review_comments: Vec<ReviewComment>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub path: String,
    pub line: Option<u64>,
    pub created_at: String,
}

struct Fail(u8, String);

fn fail<T>(msg: impl Into<String>) -> Result<T, Fail> {
    Err(Fail(1, msg.into()))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(state_path) = std::env::var_os("FAKE_GH_STATE").map(PathBuf::from) else {
        eprintln!("fake-gh: FAKE_GH_STATE isn't set");
        return ExitCode::from(2);
    };
    let reads_stdin = args.iter().any(|a| a == "-" || a.ends_with("=@-"));
    let mut stdin = String::new();
    if reads_stdin {
        let _ = std::io::stdin().read_to_string(&mut stdin);
    }
    // A slow `pr create`, once: a test can stop actuo while it runs. The call
    // goes on after actuo is gone, as a real `gh` would.
    if args.starts_with(&["pr".to_string(), "create".to_string()])
        && let Some(flag) = std::env::var_os("FAKE_GH_SLOW_PR_CREATE").map(PathBuf::from)
        && std::fs::remove_file(&flag).is_ok()
    {
        let _ = std::fs::write(with_suffix(&flag, ".started"), "");
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    let lock = Lock::take(with_suffix(&state_path, ".lock"));
    record(&state_path, &args, &stdin);
    let mut state: State = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let result = run(&mut state, &args, &stdin);
    let text = serde_json::to_string_pretty(&state).expect("state serializes");
    let tmp = with_suffix(&state_path, ".tmp");
    std::fs::write(&tmp, text).expect("write state");
    std::fs::rename(&tmp, &state_path).expect("replace state");
    drop(lock);
    match result {
        Ok(out) => {
            print!("{out}");
            let _ = std::io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(Fail(code, msg)) => {
            eprintln!("{msg}");
            ExitCode::from(code)
        }
    }
}

/// A lock file created exclusively and removed on drop (calls from parallel runs,
/// and tests editing the state, take turns). A lock older than 10 s is stale.
struct Lock(PathBuf);

impl Lock {
    fn take(path: PathBuf) -> Self {
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self(path),
                Err(_) if start.elapsed() > std::time::Duration::from_secs(10) => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn record(state: &Path, args: &[String], stdin: &str) {
    // Names only: a test checks what actuo's environment passed on to gh.
    let mut env: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    env.sort();
    let line = json!({"args": args, "stdin": stdin, "envNames": env}).to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(with_suffix(state, ".calls.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Flags of one call: `--name value` pairs (repeatable), `-R`, `-X`, `-F`/`-f`.
struct Flags {
    positional: Vec<String>,
    named: Vec<(String, String)>,
}

/// Flags that take no value.
const SWITCHES: &[&str] = &["--draft", "--paginate", "--include"];

fn parse_flags(args: &[String]) -> Flags {
    let mut positional = Vec::new();
    let mut named = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            positional.extend(args[i + 1..].iter().cloned());
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
                named.push((k.to_string(), v.to_string()));
            } else if SWITCHES.contains(&a.as_str()) {
                named.push((a.clone(), String::new()));
            } else {
                named.push((a.clone(), args.get(i + 1).cloned().unwrap_or_default()));
                i += 1;
            }
        } else {
            positional.push(a.clone());
        }
        i += 1;
    }
    Flags { positional, named }
}

impl Flags {
    fn get(&self, names: &[&str]) -> Option<&str> {
        self.named
            .iter()
            .rev()
            .find(|(k, _)| names.contains(&k.as_str()))
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, names: &[&str]) -> Vec<String> {
        self.named
            .iter()
            .filter(|(k, _)| names.contains(&k.as_str()))
            .flat_map(|(_, v)| v.split(',').map(str::to_string).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .collect()
    }

    fn has(&self, name: &str) -> bool {
        self.named.iter().any(|(k, _)| k == name)
    }

    fn repo(&self) -> Result<String, Fail> {
        self.get(&["-R", "--repo"])
            .map(str::to_string)
            .ok_or(Fail(2, "fake-gh: -R owner/repo is required".into()))
    }

    fn number(&self, at: usize) -> Result<u64, Fail> {
        self.positional
            .get(at)
            .and_then(|n| n.parse().ok())
            .ok_or(Fail(2, "fake-gh: expected a number".into()))
    }
}

fn run(state: &mut State, args: &[String], stdin: &str) -> Result<String, Fail> {
    let (Some(group), Some(cmd)) = (args.first(), args.get(1)) else {
        return Err(Fail(2, format!("fake-gh: can't answer {args:?}")));
    };
    let flags = parse_flags(&args[2..]);
    match (group.as_str(), cmd.as_str()) {
        ("api", _) => {
            let flags = parse_flags(&args[1..]);
            api(state, &flags, stdin)
        }
        ("issue", "list") => {
            let repo = state.repo(&flags.repo()?)?;
            let label = flags.get(&["--label", "-l"]);
            let want = flags.get(&["--state", "-s"]).unwrap_or("open");
            let list: Vec<Value> = repo
                .issues
                .iter()
                .filter(|i| want == "all" || i.state == want)
                .filter(|i| label.is_none_or(|l| i.labels.iter().any(|x| x == l)))
                .map(|i| issue_json(&flags.repo().unwrap_or_default(), i))
                .collect();
            Ok(json!(list).to_string())
        }
        ("issue", "view") => {
            let name = flags.repo()?;
            let n = flags.number(0)?;
            let issue = state.repo(&name)?.issue(n)?;
            Ok(issue_json(&name, issue).to_string())
        }
        ("issue", "edit") => {
            let name = flags.repo()?;
            let n = flags.number(0)?;
            let add = flags.all(&["--add-label"]);
            let remove = flags.all(&["--remove-label"]);
            let login = state.login.clone();
            let ts = state.tick();
            let repo = state.repo_mut(&name)?;
            for l in &add {
                if !repo.labels.contains(l) {
                    return fail(format!("could not add label: '{l}' not found"));
                }
            }
            let issue = repo.issue_mut(n)?;
            for l in add {
                if !issue.labels.contains(&l) {
                    issue.events.push(IssueEvent {
                        event: "labeled".into(),
                        label: l.clone(),
                        actor: login.clone(),
                        created_at: ts.clone(),
                    });
                    issue.labels.push(l);
                }
            }
            for l in remove {
                if issue.labels.contains(&l) {
                    issue.labels.retain(|x| x != &l);
                    issue.events.push(IssueEvent {
                        event: "unlabeled".into(),
                        label: l,
                        actor: login.clone(),
                        created_at: ts.clone(),
                    });
                }
            }
            Ok(format!("https://github.com/{name}/issues/{n}\n"))
        }
        ("label", "create") => {
            let name = flags.repo()?;
            let label = flags
                .positional
                .first()
                .cloned()
                .ok_or(Fail(2, "fake-gh: label name".into()))?;
            let repo = state.repo_mut(&name)?;
            if repo.labels.contains(&label) {
                return fail(format!(
                    "label with name \"{label}\" already exists; use `--force` to update its color and description"
                ));
            }
            repo.labels.push(label);
            Ok(String::new())
        }
        ("repo", "view") => {
            let name = flags
                .positional
                .first()
                .cloned()
                .ok_or(Fail(2, "fake-gh: repo".into()))?;
            let repo = state.repo(&name)?;
            if repo.hidden {
                return Err(Fail(
                    1,
                    format!("GraphQL: Could not resolve to a Repository with the name '{name}'."),
                ));
            }
            let or = |v: &str, d: &str| {
                if v.is_empty() {
                    d.to_string()
                } else {
                    v.to_string()
                }
            };
            Ok(json!({
                "defaultBranchRef": {"name": repo.default_branch},
                "viewerPermission": or(&repo.permission, "ADMIN"),
                "visibility": or(&repo.visibility, "PRIVATE"),
                "isArchived": repo.archived,
            })
            .to_string())
        }
        ("repo", "clone") => {
            let name = flags
                .positional
                .first()
                .cloned()
                .ok_or(Fail(2, "fake-gh: repo".into()))?;
            let dir = flags
                .positional
                .get(1)
                .cloned()
                .ok_or(Fail(2, "fake-gh: directory".into()))?;
            let origin = state.repo(&name)?.origin.clone();
            // As gh does: what follows `--` goes to `git clone`.
            let mut args = vec!["clone"];
            args.extend(flags.positional[2..].iter().map(String::as_str));
            args.extend([origin.to_str().unwrap_or_default(), &dir]);
            git(&args)?;
            Ok(String::new())
        }
        ("pr", "list") => {
            let name = flags.repo()?;
            let head = flags.get(&["--head", "-H"]);
            let want = flags.get(&["--state", "-s"]).unwrap_or("open");
            let list: Vec<Value> = state
                .repo(&name)?
                .pulls
                .iter()
                .filter(|p| want == "all" || p.state == want)
                .filter(|p| head.is_none_or(|h| p.head == h))
                .map(pull_json)
                .collect();
            Ok(json!(list).to_string())
        }
        ("pr", "create") => {
            let name = flags.repo()?;
            let head = flags.get(&["--head", "-H"]).unwrap_or_default().to_string();
            let base = flags.get(&["--base", "-B"]).unwrap_or_default().to_string();
            let title = flags
                .get(&["--title", "-t"])
                .unwrap_or_default()
                .to_string();
            let body = match flags.get(&["--body-file", "-F"]) {
                Some("-") => stdin.to_string(),
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|e| Fail(1, format!("reading {path}: {e}")))?,
                None => flags.get(&["--body", "-b"]).unwrap_or_default().to_string(),
            };
            let origin = state.repo(&name)?.origin.clone();
            let has_head = git(&[
                "--git-dir",
                origin.to_str().unwrap_or_default(),
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{head}"),
            ])
            .is_ok();
            if !has_head {
                return fail(format!("pull request create failed: head {head} not found"));
            }
            let repo = state.repo_mut(&name)?;
            if repo
                .pulls
                .iter()
                .any(|p| p.head == head && p.state == "open")
            {
                return fail(format!(
                    "a pull request for branch \"{head}\" into branch \"{base}\" already exists"
                ));
            }
            let number = repo
                .issues
                .iter()
                .map(|i| i.number)
                .chain(repo.pulls.iter().map(|p| p.number))
                .max()
                .unwrap_or(0)
                + 1;
            let url = format!("https://github.com/{name}/pull/{number}");
            repo.pulls.push(Pull {
                number,
                title,
                body,
                head,
                base,
                draft: flags.has("--draft"),
                state: "open".into(),
                url: url.clone(),
                ..Default::default()
            });
            Ok(format!("{url}\n"))
        }
        ("pr", "edit") => {
            let name = flags.repo()?;
            let n = flags.number(0)?;
            let body = match flags.get(&["--body-file", "-F"]) {
                Some("-") => Some(stdin.to_string()),
                Some(path) => Some(
                    std::fs::read_to_string(path)
                        .map_err(|e| Fail(1, format!("reading {path}: {e}")))?,
                ),
                None => None,
            };
            let title = flags.get(&["--title", "-t"]).map(str::to_string);
            let pull = state
                .repo_mut(&name)?
                .pulls
                .iter_mut()
                .find(|p| p.number == n)
                .ok_or(Fail(1, format!("no pull request {n}")))?;
            if let Some(b) = body {
                pull.body = b;
            }
            if let Some(t) = title {
                pull.title = t;
            }
            Ok(format!("{}\n", pull.url))
        }
        ("pr", "view") => {
            let name = flags.repo()?;
            let n = flags.number(0)?;
            let pull = state
                .repo(&name)?
                .pulls
                .iter()
                .find(|p| p.number == n)
                .ok_or(Fail(1, format!("no pull request {n}")))?;
            Ok(json!({"state": pull.state.to_uppercase()}).to_string())
        }
        _ => Err(Fail(2, format!("fake-gh: can't answer {args:?}"))),
    }
}

/// `gh api [-X METHOD] PATH [-F key=value | -F key=@-]...`
fn api(state: &mut State, flags: &Flags, stdin: &str) -> Result<String, Fail> {
    let method = flags.get(&["-X", "--method"]).unwrap_or("GET").to_string();
    let path = flags
        .positional
        .first()
        .cloned()
        .ok_or(Fail(2, "fake-gh: api path".into()))?;
    let path = path.trim_start_matches('/');
    let path = path.split('?').next().unwrap_or_default();
    let fields: BTreeMap<String, String> = flags
        .named
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "-F" | "-f" | "--field" | "--raw-field"))
        .filter_map(|(_, v)| v.split_once('='))
        .map(|(k, v)| {
            let v = if v == "@-" {
                stdin.to_string()
            } else {
                v.to_string()
            };
            (k.to_string(), v)
        })
        .collect();
    let parts: Vec<&str> = path.split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["user"]) => Ok(json!({"login": state.login}).to_string()),
        ("GET", ["user", "repository_invitations"]) => {
            let list: Vec<Value> = state
                .invitations
                .iter()
                .map(|i| json!({"id": i.id, "repository": {"full_name": i.repo}}))
                .collect();
            Ok(Value::Array(list).to_string())
        }
        ("PATCH", ["user", "repository_invitations", id]) => {
            let id = parse(id)?;
            let i = state
                .invitations
                .iter()
                .position(|i| i.id == id)
                .ok_or(Fail(1, "HTTP 404: Not Found".into()))?;
            let invitation = state.invitations.remove(i);
            let repo = state.repo_mut(&invitation.repo)?;
            repo.hidden = false;
            repo.permission = if invitation.permission.is_empty() {
                "WRITE".into()
            } else {
                invitation.permission
            };
            Ok(String::new())
        }
        ("GET", ["repos", o, r, "branches", branch @ ..]) => {
            let repo = state.repo(&format!("{o}/{r}"))?;
            Ok(json!({"name": branch.join("/"), "protected": repo.protected}).to_string())
        }
        ("GET", ["repos", o, r, "issues", n, "events"]) => {
            let issue = state.repo(&format!("{o}/{r}"))?.issue(parse(n)?)?;
            let events: Vec<Value> = issue
                .events
                .iter()
                .map(|e| {
                    json!({"event": e.event, "label": {"name": e.label},
                           "actor": {"login": e.actor}, "created_at": e.created_at})
                })
                .collect();
            Ok(json!(events).to_string())
        }
        ("GET", ["repos", o, r, "issues", n, "comments"]) => {
            let repo = state.repo(&format!("{o}/{r}"))?;
            let n = parse(n)?;
            let comments = match repo.issues.iter().find(|i| i.number == n) {
                Some(i) => &i.comments,
                None => &repo.pull(n)?.comments,
            };
            Ok(json!(comments.iter().map(comment_json).collect::<Vec<_>>()).to_string())
        }
        ("POST", ["repos", o, r, "issues", n, "comments"]) => {
            let body = fields.get("body").cloned().unwrap_or_default();
            let id = state.new_id();
            let ts = state.tick();
            let login = state.login.clone();
            let repo = state.repo_mut(&format!("{o}/{r}"))?;
            let n = parse(n)?;
            let comment = Comment {
                id,
                author: login,
                body,
                created_at: ts.clone(),
                updated_at: ts,
            };
            let json = comment_json(&comment);
            match repo.issues.iter_mut().find(|i| i.number == n) {
                Some(i) => i.comments.push(comment),
                None => repo.pull_mut(n)?.comments.push(comment),
            }
            Ok(json.to_string())
        }
        ("PATCH", ["repos", o, r, "issues", "comments", id]) => {
            let body = fields.get("body").cloned().unwrap_or_default();
            let ts = state.tick();
            let id = parse(id)?;
            let repo = state.repo_mut(&format!("{o}/{r}"))?;
            let comment = repo
                .issues
                .iter_mut()
                .flat_map(|i| i.comments.iter_mut())
                .chain(repo.pulls.iter_mut().flat_map(|p| p.comments.iter_mut()))
                .find(|c| c.id == id)
                .ok_or(Fail(1, format!("HTTP 404: comment {id} not found")))?;
            comment.body = body;
            comment.updated_at = ts;
            Ok(comment_json(comment).to_string())
        }
        ("GET", ["repos", o, r, "pulls", n, "comments"]) => {
            let pull = state.repo(&format!("{o}/{r}"))?.pull(parse(n)?)?;
            let list: Vec<Value> = pull
                .review_comments
                .iter()
                .map(|c| {
                    json!({"id": c.id, "user": {"login": c.author}, "body": c.body,
                           "path": c.path, "line": c.line, "created_at": c.created_at})
                })
                .collect();
            Ok(json!(list).to_string())
        }
        _ => Err(Fail(
            2,
            format!("fake-gh: can't answer api {method} {path}"),
        )),
    }
}

fn parse(n: &str) -> Result<u64, Fail> {
    n.parse()
        .map_err(|_| Fail(2, format!("fake-gh: {n} isn't a number")))
}

fn git(args: &[&str]) -> Result<(), Fail> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|e| Fail(1, format!("git: {e}")))?;
    if !out.status.success() {
        return fail(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

fn issue_json(repo: &str, i: &Issue) -> Value {
    json!({
        "number": i.number,
        "title": i.title,
        "body": i.body,
        "state": i.state.to_uppercase(),
        "url": format!("https://github.com/{repo}/issues/{}", i.number),
        "author": {"login": i.author},
        "labels": i.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
    })
}

fn pull_json(p: &Pull) -> Value {
    json!({"number": p.number, "url": p.url, "title": p.title, "isDraft": p.draft,
           "headRefName": p.head, "baseRefName": p.base, "state": p.state.to_uppercase()})
}

fn comment_json(c: &Comment) -> Value {
    json!({"id": c.id, "user": {"login": c.author}, "body": c.body,
           "created_at": c.created_at, "updated_at": c.updated_at,
           "html_url": format!("https://github.com/x/y/issues/0#issuecomment-{}", c.id)})
}

impl State {
    fn repo(&self, name: &str) -> Result<&Repo, Fail> {
        self.repos.get(name).ok_or(Fail(
            1,
            format!("GraphQL: Could not resolve to a Repository with the name '{name}'."),
        ))
    }

    fn repo_mut(&mut self, name: &str) -> Result<&mut Repo, Fail> {
        self.repos.get_mut(name).ok_or(Fail(
            1,
            format!("GraphQL: Could not resolve to a Repository with the name '{name}'."),
        ))
    }

    fn new_id(&mut self) -> u64 {
        self.next_id = self.next_id.max(1000) + 1;
        self.next_id
    }

    /// The fake clock as an RFC 3339 time, one second later on every change.
    fn tick(&mut self) -> String {
        self.clock += 1;
        let s = self.clock;
        format!(
            "2026-10-01T{:02}:{:02}:{:02}Z",
            (s / 3600) % 24,
            (s / 60) % 60,
            s % 60
        )
    }
}

impl Repo {
    fn issue(&self, n: u64) -> Result<&Issue, Fail> {
        self.issues.iter().find(|i| i.number == n).ok_or(Fail(
            1,
            format!(
                "GraphQL: Could not resolve to an issue or pull request with the number of {n}."
            ),
        ))
    }

    fn issue_mut(&mut self, n: u64) -> Result<&mut Issue, Fail> {
        self.issues.iter_mut().find(|i| i.number == n).ok_or(Fail(
            1,
            format!(
                "GraphQL: Could not resolve to an issue or pull request with the number of {n}."
            ),
        ))
    }

    fn pull(&self, n: u64) -> Result<&Pull, Fail> {
        self.pulls
            .iter()
            .find(|p| p.number == n)
            .ok_or(Fail(1, format!("no pull request {n}")))
    }

    fn pull_mut(&mut self, n: u64) -> Result<&mut Pull, Fail> {
        self.pulls
            .iter_mut()
            .find(|p| p.number == n)
            .ok_or(Fail(1, format!("no pull request {n}")))
    }
}

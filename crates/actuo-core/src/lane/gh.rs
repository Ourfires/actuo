//! Every GitHub call the lane makes, through the `gh` CLI (its own login; actuo
//! stores no token). Output is read from `--json` or the REST API's JSON.
//!
//! Text from GitHub or from the agent (titles, bodies, comments) is passed as a
//! separate argv entry or on stdin (`-F body=@-`, `--body-file -`), never inside a
//! shell string: that text is untrusted.

use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("gh: {0}")]
    Io(#[from] std::io::Error),
    #[error("`gh {command}` failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("`gh {command}`: unexpected output: {reason}")]
    Output { command: String, reason: String },
    #[error("`gh {0}` isn't a call actuo makes (refused)")]
    Refused(String),
}

pub type Result<T, E = GhError> = std::result::Result<T, E>;

/// A repo as actuo's gh login sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub permission: String,
    pub visibility: String,
    pub default_branch: String,
    pub archived: bool,
}

/// An issue as `gh issue list` and `gh issue view` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// `open` or `closed`.
    pub state: String,
    pub author: String,
    pub labels: Vec<String>,
}

/// A comment on an issue or a PR's conversation, or an inline review comment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// For an inline review comment: the file and line.
    pub path: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// The `gh` program to run (`gh` from `PATH` normally; tests point it elsewhere).
#[derive(Debug, Clone)]
pub struct Gh {
    program: PathBuf,
}

impl Default for Gh {
    fn default() -> Self {
        Self::new("gh")
    }
}

impl Gh {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        if !allowed(args) {
            crate::log!("gh: refused `gh {}`", describe(args));
            return Err(GhError::Refused(describe(args)));
        }
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "cat")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::process::spawn(&mut cmd)?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            pipe.write_all(input.as_bytes()).await?;
            drop(pipe);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            return Err(GhError::Failed {
                command: describe(args),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, args: &[&str]) -> Result<T> {
        let out = self.run(args, None).await?;
        serde_json::from_str(&out).map_err(|e| GhError::Output {
            command: describe(args),
            reason: e.to_string(),
        })
    }

    /// The account gh is logged in as.
    pub async fn login(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        Ok(self.json::<User>(&["api", "user"]).await?.login)
    }

    /// A PR's state: `OPEN`, `MERGED` or `CLOSED`.
    pub async fn pr_state(&self, repo: &str, number: u64) -> Result<String> {
        #[derive(Deserialize)]
        struct View {
            state: String,
        }
        let n = number.to_string();
        let v: View = self
            .json(&["pr", "view", &n, "-R", repo, "--json", "state"])
            .await?;
        Ok(v.state)
    }

    /// Open issues with `label`.
    pub async fn issues_with_label(&self, repo: &str, label: &str) -> Result<Vec<Issue>> {
        let list: Vec<IssueJson> = self
            .json(&[
                "issue",
                "list",
                "-R",
                repo,
                "--label",
                label,
                "--state",
                "open",
                "--limit",
                "100",
                "--json",
                ISSUE_FIELDS,
            ])
            .await?;
        Ok(list.into_iter().map(Issue::from).collect())
    }

    pub async fn issue(&self, repo: &str, number: u64) -> Result<Issue> {
        let n = number.to_string();
        let issue: IssueJson = self
            .json(&["issue", "view", &n, "-R", repo, "--json", ISSUE_FIELDS])
            .await?;
        Ok(issue.into())
    }

    /// Who last added `label` to the issue, from its event timeline.
    pub async fn label_actor(
        &self,
        repo: &str,
        number: u64,
        label: &str,
    ) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Named {
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize)]
        struct Login {
            login: String,
        }
        #[derive(Deserialize)]
        struct Event {
            event: String,
            #[serde(default)]
            label: Option<Named>,
            #[serde(default)]
            actor: Option<Login>,
        }
        let path = format!("repos/{repo}/issues/{number}/events");
        let events: Vec<Event> = self.json(&["api", "--paginate", &path]).await?;
        Ok(events
            .into_iter()
            .rev()
            .find(|e| e.event == "labeled" && e.label.as_ref().is_some_and(|l| l.name == label))
            .and_then(|e| e.actor.map(|a| a.login)))
    }

    /// Comments on an issue, or on a PR's conversation (a PR is an issue there).
    pub async fn comments(&self, repo: &str, number: u64) -> Result<Vec<Comment>> {
        let path = format!("repos/{repo}/issues/{number}/comments");
        let list: Vec<CommentJson> = self.json(&["api", "--paginate", &path]).await?;
        Ok(list.into_iter().map(Comment::from).collect())
    }

    /// A PR's inline review comments.
    pub async fn review_comments(&self, repo: &str, pr: u64) -> Result<Vec<Comment>> {
        let path = format!("repos/{repo}/pulls/{pr}/comments");
        let list: Vec<CommentJson> = self.json(&["api", "--paginate", &path]).await?;
        Ok(list.into_iter().map(Comment::from).collect())
    }

    /// Adds and removes labels in one call; a label that isn't there to remove is fine.
    pub async fn edit_labels(
        &self,
        repo: &str,
        number: u64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let n = number.to_string();
        let mut args = vec!["issue", "edit", n.as_str(), "-R", repo];
        for l in add {
            args.extend(["--add-label", l]);
        }
        for l in remove {
            args.extend(["--remove-label", l]);
        }
        self.run(&args, None).await.map(|_| ())
    }

    /// Creates a label; one that exists already is left as it is (your colors stay).
    pub async fn ensure_label(&self, repo: &str, name: &str) -> Result<()> {
        match self.run(&["label", "create", name, "-R", repo], None).await {
            Err(GhError::Failed { stderr, .. }) if stderr.contains("already exists") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Posts a comment; returns its id.
    pub async fn add_comment(&self, repo: &str, number: u64, body: &str) -> Result<u64> {
        let path = format!("repos/{repo}/issues/{number}/comments");
        let out = self
            .run(&["api", "-X", "POST", &path, "-F", "body=@-"], Some(body))
            .await?;
        comment_id(&out, &path)
    }

    pub async fn edit_comment(&self, repo: &str, id: u64, body: &str) -> Result<()> {
        let path = format!("repos/{repo}/issues/comments/{id}");
        self.run(&["api", "-X", "PATCH", &path, "-F", "body=@-"], Some(body))
            .await
            .map(|_| ())
    }

    pub async fn default_branch(&self, repo: &str) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct View {
            default_branch_ref: Named,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        let v: View = self
            .json(&["repo", "view", repo, "--json", "defaultBranchRef"])
            .await?;
        Ok(v.default_branch_ref.name)
    }

    /// What actuo's login may do with the repo: its permission, visibility,
    /// default branch and whether it's archived.
    pub async fn repo_access(&self, repo: &str) -> Result<RepoInfo> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct View {
            viewer_permission: Option<String>,
            visibility: Option<String>,
            default_branch_ref: Option<Branch>,
            is_archived: Option<bool>,
        }
        #[derive(Deserialize)]
        struct Branch {
            name: String,
        }
        let v: View = self
            .json(&[
                "repo",
                "view",
                repo,
                "--json",
                "viewerPermission,visibility,defaultBranchRef,isArchived",
            ])
            .await?;
        Ok(RepoInfo {
            permission: v.viewer_permission.unwrap_or_default(),
            visibility: v.visibility.unwrap_or_default(),
            default_branch: v.default_branch_ref.map(|b| b.name).unwrap_or_default(),
            archived: v.is_archived.unwrap_or(false),
        })
    }

    /// The id of the gh login's pending invitation to `repo`, if it has one.
    pub async fn invitation_to(&self, repo: &str) -> Result<Option<u64>> {
        let list: Vec<Value> = self.json(&["api", "user/repository_invitations"]).await?;
        Ok(list
            .iter()
            .find(|i| {
                i["repository"]["full_name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(repo))
            })
            .and_then(|i| i["id"].as_u64()))
    }

    /// Accepts the gh login's invitation `id`.
    pub async fn accept_invitation(&self, id: u64) -> Result<()> {
        let path = format!("user/repository_invitations/{id}");
        self.run(&["api", "-X", "PATCH", &path], None).await?;
        Ok(())
    }

    /// Whether `branch` is protected; `None` when GitHub doesn't say.
    pub async fn branch_protected(&self, repo: &str, branch: &str) -> Option<bool> {
        #[derive(Deserialize)]
        struct View {
            protected: bool,
        }
        let path = format!("repos/{repo}/branches/{branch}");
        self.json::<View>(&["api", &path])
            .await
            .ok()
            .map(|v| v.protected)
    }

    pub async fn clone_repo(&self, repo: &str, dir: &Path) -> Result<()> {
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let dir = dir.to_string_lossy();
        // Hooks off from the first checkout: a global `core.hooksPath` would run the
        // repo's own `post-checkout`. The setting stays in the clone's config.
        let args = [
            "repo",
            "clone",
            repo,
            &dir,
            "--",
            "--quiet",
            "--config",
            "core.hooksPath=/dev/null",
        ];
        self.run(&args, None).await.map(|_| ())
    }

    /// The open PR from `head`, if there is one.
    pub async fn open_pr(&self, repo: &str, head: &str) -> Result<Option<PullRequest>> {
        #[derive(Deserialize)]
        struct Pr {
            number: u64,
            url: String,
        }
        let list: Vec<Pr> = self
            .json(&[
                "pr",
                "list",
                "-R",
                repo,
                "--head",
                head,
                "--state",
                "open",
                "--json",
                "number,url",
            ])
            .await?;
        Ok(list.into_iter().next().map(|p| PullRequest {
            number: p.number,
            url: p.url,
        }))
    }

    /// Opens a draft PR. Never a ready one; the lane never merges.
    pub async fn create_draft_pr(
        &self,
        repo: &str,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        let args = [
            "pr",
            "create",
            "-R",
            repo,
            "--draft",
            "--base",
            base,
            "--head",
            head,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        let out = self.run(&args, Some(body)).await?;
        let url = out
            .lines()
            .map(str::trim)
            .rfind(|l| l.starts_with("https://"))
            .ok_or_else(|| GhError::Output {
                command: describe(&args),
                reason: "no PR URL".into(),
            })?
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| GhError::Output {
                command: describe(&args),
                reason: format!("no PR number in {url}"),
            })?;
        Ok(PullRequest { number, url })
    }

    pub async fn edit_pr_body(&self, repo: &str, pr: u64, body: &str) -> Result<()> {
        let n = pr.to_string();
        self.run(
            &["pr", "edit", &n, "-R", repo, "--body-file", "-"],
            Some(body),
        )
        .await
        .map(|_| ())
    }
}

const ISSUE_FIELDS: &str = "number,title,body,url,state,author,labels";

/// The gh calls actuo makes, and nothing else (`Gh::run` refuses the rest):
/// reads; labels and comments on issues; the draft PR (created as a draft, its
/// body edited); the clone. No merge, no ready, no close, no branch, release or
/// repo setting change, whatever a bug in actuo asks (on some repos a merge
/// to `main` deploys).
fn allowed(args: &[&str]) -> bool {
    match args {
        ["api", "user" | "user/repository_invitations"] => true,
        ["api", "-X", "PATCH", path] => path
            .strip_prefix("user/repository_invitations/")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())),
        ["api", path] => {
            let parts: Vec<&str> = path.split('/').collect();
            parts.len() >= 5
                && parts[0] == "repos"
                && parts[3] == "branches"
                && parts.iter().all(|p| !p.is_empty() && *p != "..")
        }
        ["api", "--paginate", path] => {
            api_path(path, &["issues", "*", "events"])
                || api_path(path, &["issues", "*", "comments"])
                || api_path(path, &["pulls", "*", "comments"])
        }
        ["api", "-X", "POST", path, "-F", "body=@-"] => {
            api_path(path, &["issues", "*", "comments"])
        }
        ["api", "-X", "PATCH", path, "-F", "body=@-"] => {
            api_path(path, &["issues", "comments", "*"])
        }
        ["issue", "list" | "view", ..] | ["label", "create", ..] => true,
        ["issue", "edit", _, "-R", _, rest @ ..] => {
            !rest.is_empty()
                && rest
                    .chunks(2)
                    .all(|pair| matches!(pair, ["--add-label" | "--remove-label", _]))
        }
        ["pr", "view" | "list", ..] => true,
        ["pr", "create", rest @ ..] => rest.contains(&"--draft"),
        ["pr", "edit", _, "-R", _, "--body-file", "-"] => true,
        ["repo", "view", _, "--json", _] | ["repo", "clone", ..] => true,
        _ => false,
    }
}

/// `repos/<owner>/<repo>/` followed by `rest`, where `*` is a number.
fn api_path(path: &str, rest: &[&str]) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() == 3 + rest.len()
        && parts[0] == "repos"
        && !parts[1].is_empty()
        && !parts[2].is_empty()
        && parts[3..].iter().zip(rest).all(|(p, want)| {
            if *want == "*" {
                !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
            } else {
                p == want
            }
        })
}

#[derive(Deserialize)]
struct IssueJson {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    author: Option<Value>,
    #[serde(default)]
    labels: Vec<Value>,
}

impl From<IssueJson> for Issue {
    fn from(i: IssueJson) -> Self {
        Issue {
            number: i.number,
            title: i.title,
            body: i.body,
            url: i.url,
            state: i.state.to_lowercase(),
            author: i
                .author
                .and_then(|a| a["login"].as_str().map(str::to_string))
                .unwrap_or_default(),
            labels: i
                .labels
                .iter()
                .filter_map(|l| l["name"].as_str().map(str::to_string))
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct CommentJson {
    id: u64,
    #[serde(default)]
    user: Option<Value>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
}

impl From<CommentJson> for Comment {
    fn from(c: CommentJson) -> Self {
        Comment {
            id: c.id,
            author: c
                .user
                .and_then(|u| u["login"].as_str().map(str::to_string))
                .unwrap_or_default(),
            body: c.body,
            created_at: c.created_at,
            path: c.path,
            line: c.line,
        }
    }
}

fn comment_id(out: &str, path: &str) -> Result<u64> {
    serde_json::from_str::<Value>(out)
        .ok()
        .and_then(|v| v["id"].as_u64())
        .ok_or_else(|| GhError::Output {
            command: format!("api POST {path}"),
            reason: "no comment id".into(),
        })
}

/// The command for an error message, without the text it carried on stdin.
fn describe(args: &[&str]) -> String {
    args.join(" ")
}

/// `owner/repo` and the number from an issue URL
/// (`https://github.com/owner/repo/issues/12`).
pub fn parse_issue_url(url: &str) -> Option<(String, u64)> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/');
    let mut parts = rest.split('/');
    let (owner, repo, kind, n) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || kind != "issues" || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let n = n.split(['#', '?']).next()?.parse().ok()?;
    Some((format!("{owner}/{repo}"), n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every call actuo makes is allowed; anything that merges, readies,
    /// closes or deletes, or writes elsewhere, is refused.
    #[test]
    fn the_gh_allowlist() {
        for ok in [
            &["api", "user"][..],
            &["api", "--paginate", "repos/o/r/issues/7/events"],
            &["api", "--paginate", "repos/o/r/issues/7/comments"],
            &["api", "--paginate", "repos/o/r/pulls/8/comments"],
            &[
                "api",
                "-X",
                "POST",
                "repos/o/r/issues/7/comments",
                "-F",
                "body=@-",
            ],
            &[
                "api",
                "-X",
                "PATCH",
                "repos/o/r/issues/comments/123",
                "-F",
                "body=@-",
            ],
            &["issue", "list", "-R", "o/r", "--label", "actuo"],
            &["issue", "view", "7", "-R", "o/r", "--json", "number"],
            &[
                "issue",
                "edit",
                "7",
                "-R",
                "o/r",
                "--add-label",
                "a",
                "--remove-label",
                "b",
            ],
            &["label", "create", "actuo:pr", "-R", "o/r"],
            &["pr", "view", "8", "-R", "o/r", "--json", "state"],
            &["pr", "list", "-R", "o/r", "--head", "actuo/issue-7"],
            &[
                "pr",
                "create",
                "-R",
                "o/r",
                "--draft",
                "--base",
                "main",
                "--head",
                "actuo/issue-7",
            ],
            &["pr", "edit", "8", "-R", "o/r", "--body-file", "-"],
            &["repo", "view", "o/r", "--json", "defaultBranchRef"],
            &["api", "repos/o/r/branches/main"],
            &["repo", "clone", "o/r", "/x", "--", "--quiet"],
            &["api", "user/repository_invitations"],
            &["api", "-X", "PATCH", "user/repository_invitations/7"],
        ] {
            assert!(allowed(ok), "{ok:?}");
        }
        for refused in [
            &["pr", "merge", "8", "-R", "o/r"][..],
            &["pr", "ready", "8", "-R", "o/r"],
            &["pr", "close", "8", "-R", "o/r"],
            &["pr", "create", "-R", "o/r", "--base", "main", "--head", "x"],
            &["pr", "edit", "8", "-R", "o/r", "--base", "main"],
            &["issue", "close", "7", "-R", "o/r"],
            &["issue", "edit", "7", "-R", "o/r", "--title", "x"],
            &["issue", "edit", "7", "-R", "o/r"],
            &["repo", "delete", "o/r", "--yes"],
            &["repo", "edit", "o/r", "--visibility", "public"],
            &["release", "create", "v1", "-R", "o/r"],
            &["api", "-X", "PUT", "repos/o/r/pulls/8/merge"],
            &["api", "-X", "DELETE", "repos/o/r/git/refs/heads/main"],
            &["api", "-X", "POST", "repos/o/r/merges", "-F", "body=@-"],
            &["api", "-X", "PATCH", "repos/o/r/issues/7", "-F", "body=@-"],
            &["api", "--paginate", "repos/o/r/../../user/keys"],
            &["api", "graphql", "-f", "query=mutation"],
            &["api", "repos/o/r/branches/../../keys"],
            &["api", "repos/o/r/pulls/8/merge"],
            &["auth", "token"],
            &["api", "-X", "DELETE", "user/repository_invitations/7"],
            &[
                "api",
                "-X",
                "PATCH",
                "user/repository_invitations/7/../../keys",
            ],
            &["api", "-X", "PATCH", "user/repository_invitations/"],
        ] {
            assert!(!allowed(refused), "{refused:?}");
        }
    }

    /// gh runs only through `Gh::run`: no other file of actuo names it.
    #[test]
    fn gh_only_through_its_wrapper() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        let mut dirs = vec![here.join("src"), here.join("../actuo/src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("lane/gh.rs")
                    && std::fs::read_to_string(&path).unwrap().contains("\"gh\"")
                {
                    found.push(path.display().to_string());
                }
            }
        }
        assert!(found.is_empty(), "gh named outside lane/gh.rs: {found:?}");
    }

    #[test]
    fn issue_urls() {
        assert_eq!(
            parse_issue_url("https://github.com/o/r/issues/12"),
            Some(("o/r".into(), 12))
        );
        assert_eq!(
            parse_issue_url("https://github.com/o/r/issues/3#issuecomment-1"),
            Some(("o/r".into(), 3))
        );
        for bad in [
            "https://github.com/o/r/pull/3",
            "https://github.com/o/r/issues/x",
            "https://example.com/o/r/issues/3",
            "https://github.com/o/r/issues/3/more",
            "o/r#3",
        ] {
            assert_eq!(parse_issue_url(bad), None, "{bad}");
        }
    }
}

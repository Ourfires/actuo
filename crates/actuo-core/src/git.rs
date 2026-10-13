//! Git through the `git` CLI: repositories, worktrees and checkpoints.
//!
//! A checkpoint is a commit object (no parent) of the working tree as `git add -A`
//! sees it, written with a private index so the user's index is never touched, and
//! kept alive by a ref under `refs/actuo/checkpoints/`. Ignored files are neither
//! captured nor touched by a restore.
//!
//! Hooks never run: every call sets `core.hooksPath` to `/dev/null`. Actuo runs
//! git outside any sandbox, with the user's credentials, in worktrees an agent
//! writes to; a hook there (or a `core.hooksPath` pointing into the tree) would run
//! the agent's code.

use actuo_protocol::types::{FileChange, FileStatus};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git: {0}")]
    Io(#[from] std::io::Error),
    #[error("`git {command}` failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("{0} is not inside a git repository")]
    NotARepo(PathBuf),
}

pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Name and email on checkpoint commits.
const CHECKPOINT_AUTHOR: (&str, &str) = ("actuo", "actuo@localhost");

/// Flags that make git flush objects and refs to disk, so a crash can't leave an
/// empty ref behind.
const FSYNC: [&str; 4] = [
    "-c",
    "core.fsync=objects,reference",
    "-c",
    "core.fsyncMethod=fsync",
];

/// Flags every git call gets: no hooks, no fsmonitor daemon.
const ALWAYS: [&str; 4] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];

/// Runs git in `cwd` and returns stdout.
pub(crate) fn git<I, S>(cwd: &Path, args: I, env: &[(&str, &OsStr)]) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_with_stdin(cwd, args, env, None)
}

/// Runs git in `cwd` with `stdin` (a commit message, say) and returns stdout.
pub(crate) fn git_with_stdin<I, S>(
    cwd: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    stdin: Option<&str>,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::io::Write;
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .args(ALWAYS)
        .args(&args)
        // An actuo started from inside a git hook must not inherit its repository.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = match stdin {
        None => crate::process::output_std(&mut cmd)?,
        Some(input) => {
            let mut child = crate::process::spawn_std(
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped()),
            )?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())?;
            child.wait_with_output()?
        }
    };
    if !out.status.success() {
        return Err(GitError::Failed {
            command: args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository's top level for any path inside it, canonicalized.
pub fn toplevel(path: &Path) -> Result<PathBuf> {
    match git(path, ["rev-parse", "--show-toplevel"], &[]) {
        Ok(out) => Ok(PathBuf::from(out.trim()).canonicalize()?),
        Err(GitError::Failed { .. }) => Err(GitError::NotARepo(path.to_path_buf())),
        Err(e) => Err(e),
    }
}

/// Whether `HEAD` points at a commit.
pub fn has_commit(repo: &Path) -> Result<bool> {
    match git(
        repo,
        ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        &[],
    ) {
        Ok(_) => Ok(true),
        Err(GitError::Failed { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// `git worktree add -b <branch> <path> <base>`: a new branch from `base`. Creates
/// the parent directories. Nothing git ignores is copied.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(
        repo,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(base),
        ],
        &[],
    )?;
    Ok(())
}

/// `git worktree add <path> <branch>`: a worktree for a branch that exists.
pub fn worktree_add_existing(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(
        repo,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            path.as_os_str(),
            OsStr::new(branch),
        ],
        &[],
    )?;
    Ok(())
}

/// The worktree (the main one included) that has `branch` checked out, canonicalized.
pub fn worktree_for_branch(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let out = git(repo, ["worktree", "list", "--porcelain", "-z"], &[])?;
    let want = format!("branch refs/heads/{branch}");
    let mut path = None;
    for field in out.split('\0') {
        if let Some(p) = field.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if field == want {
            // A worktree whose directory is gone (not pruned yet) doesn't count.
            return Ok(path.and_then(|p| p.canonicalize().ok()));
        } else if field.is_empty() {
            path = None;
        }
    }
    Ok(None)
}

/// The repository's git dir shared by its worktrees (`git rev-parse
/// --git-common-dir`), absolute.
pub fn common_dir(cwd: &Path) -> Result<PathBuf> {
    let out = git(cwd, ["rev-parse", "--git-common-dir"], &[])?;
    Ok(absolute(cwd, out.trim()))
}

/// Removes a worktree and its git metadata, with any changes in it.
pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let path = path.to_string_lossy();
    git(repo, ["worktree", "remove", "--force", &*path], &[]).map(|_| ())
}

/// Drops the metadata of worktrees whose folders are gone.
pub fn worktree_prune(repo: &Path) -> Result<()> {
    git(repo, ["worktree", "prune"], &[]).map(|_| ())
}

/// Deletes the local branch `refs/heads/<branch>`, merged or not.
pub fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    git(repo, ["branch", "-D", branch], &[]).map(|_| ())
}

/// Deletes every ref under `prefix` (ending in `/`), in one transaction.
pub fn delete_refs_under(repo: &Path, prefix: &str) -> Result<()> {
    let refs = git(repo, ["for-each-ref", "--format=%(refname)", prefix], &[])?;
    let lines: String = refs
        .lines()
        .filter(|r| !r.is_empty())
        .map(|r| format!("delete {r}\n"))
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    git_with_stdin(repo, ["update-ref", "--stdin"], &[], Some(&lines)).map(|_| ())
}

/// Whether `refs/heads/<branch>` exists.
pub fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    Ok(resolve_ref(repo, &format!("refs/heads/{branch}"))?.is_some())
}

/// Whether `name` is a valid branch name (`git check-ref-format --branch`).
pub fn valid_branch_name(repo: &Path, name: &str) -> bool {
    !name.starts_with('-') && git(repo, ["check-ref-format", "--branch", name], &[]).is_ok()
}

/// The names srt protects in a writable directory (its `DANGEROUS_FILES` and the
/// `DANGEROUS_DIRECTORIES` but `.git`). On Linux its `bwrap` creates them as empty
/// files at the worktree's top level, as mount points, while a sandboxed process
/// runs, and removes them afterwards.
pub const SANDBOX_PLACEHOLDERS: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
    ".vscode",
    ".idea",
];

/// The [`SANDBOX_PLACEHOLDERS`] at the top level of `cwd`'s working tree that the
/// index doesn't track: exactly those paths, nothing under them. Call it before
/// staging, so "untracked" means untracked before.
pub(crate) fn untracked_placeholders(cwd: &Path) -> Result<Vec<String>> {
    let specs: Vec<String> = SANDBOX_PLACEHOLDERS
        .iter()
        .map(|n| format!(":(top,literal){n}"))
        .collect();
    let out = git(
        cwd,
        ["ls-files", "-z", "--others", "--"]
            .into_iter()
            .chain(specs.iter().map(String::as_str)),
        &[],
    )?;
    Ok(out
        .split('\0')
        .filter(|p| SANDBOX_PLACEHOLDERS.contains(p))
        .map(str::to_string)
        .collect())
}

/// Takes `paths` out of the index (`GIT_INDEX_FILE` in `env`, else the real one),
/// leaving the files on disk.
pub(crate) fn unstage(cwd: &Path, paths: &[String], env: &[(&str, &OsStr)]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    git(
        cwd,
        ["update-index", "--force-remove", "--"]
            .into_iter()
            .chain(paths.iter().map(String::as_str)),
        env,
    )
    .map(|_| ())
}

/// A commit of the working tree as it is now, without a ref. Returns its id.
/// Untracked [`SANDBOX_PLACEHOLDERS`] are left out, so turn file lists, guards and
/// restores never see them.
pub fn snapshot(cwd: &Path) -> Result<String> {
    let common_dir = absolute(
        cwd,
        git(cwd, ["rev-parse", "--git-common-dir"], &[])?.trim(),
    );
    let real_index = absolute(
        cwd,
        git(cwd, ["rev-parse", "--git-path", "index"], &[])?.trim(),
    );
    let index = common_dir.join(format!(
        "actuo-checkpoint-index-{}",
        uuid::Uuid::now_v7().simple()
    ));

    let result = (|| {
        // Start from the real index for its stat cache, so unchanged files aren't
        // hashed again. Keep its mtime so git's racy-timestamp check stays right.
        if real_index.exists() {
            std::fs::copy(&real_index, &index)?;
            let modified = std::fs::metadata(&real_index)?.modified()?;
            std::fs::File::options()
                .write(true)
                .open(&index)?
                .set_modified(modified)?;
        }
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        let placeholders = untracked_placeholders(cwd)?;
        git(cwd, ["add", "--all", "--", ":/"], &env)?;
        unstage(cwd, &placeholders, &env)?;
        let tree = git(cwd, FSYNC.iter().chain(&["write-tree"]), &env)?;
        let (name, email) = CHECKPOINT_AUTHOR;
        let author_env = [
            ("GIT_AUTHOR_NAME", OsStr::new(name)),
            ("GIT_AUTHOR_EMAIL", OsStr::new(email)),
            ("GIT_COMMITTER_NAME", OsStr::new(name)),
            ("GIT_COMMITTER_EMAIL", OsStr::new(email)),
        ];
        let commit = git(
            cwd,
            FSYNC.iter().chain(&[
                "commit-tree",
                "--no-gpg-sign",
                tree.trim(),
                "-m",
                "actuo checkpoint",
            ]),
            &author_env,
        )?;
        Ok(commit.trim().to_string())
    })();

    let _ = std::fs::remove_file(&index);
    let mut lock = index.clone().into_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(lock);
    result
}

/// Snapshots the working tree and points `reference` at it. Returns the commit.
pub fn capture_checkpoint(cwd: &Path, reference: &str) -> Result<String> {
    let commit = snapshot(cwd)?;
    git(
        cwd,
        FSYNC.iter().chain(&["update-ref", reference, &commit]),
        &[],
    )?;
    Ok(commit)
}

/// The commit a ref points at, if it exists.
pub fn resolve_ref(cwd: &Path, reference: &str) -> Result<Option<String>> {
    let spec = format!("{reference}^{{commit}}");
    match git(cwd, ["rev-parse", "--verify", "--quiet", &spec], &[]) {
        Ok(out) => Ok(Some(out.trim().to_string())),
        Err(GitError::Failed { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Files that differ between two commits, without rename detection.
pub fn diff_name_status(cwd: &Path, from: &str, to: &str) -> Result<Vec<FileChange>> {
    let out = git(
        cwd,
        [
            "diff",
            "--name-status",
            "--no-renames",
            "--no-ext-diff",
            "-z",
            from,
            to,
        ],
        &[],
    )?;
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    let mut changes = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        let status = match status.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('T') => FileStatus::TypeChanged,
            _ => FileStatus::Modified,
        };
        changes.push(FileChange {
            path: path.to_string(),
            status,
        });
    }
    Ok(changes)
}

/// A file the worktree changes against a base, with its line counts (0 for a
/// binary file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub added: u64,
    pub deleted: u64,
    pub binary: bool,
}

/// What the worktree as it is now (tracked and untracked, ignored files and
/// untracked [`SANDBOX_PLACEHOLDERS`] left out) changes against `base`.
pub fn changes(worktree: &Path, base: &str) -> Result<Vec<Change>> {
    let now = snapshot(worktree)?;
    numstat(worktree, base, &now)
}

/// Line counts per file between two commits, without rename detection. git
/// prints `-` for a binary file's counts.
fn numstat(cwd: &Path, from: &str, to: &str) -> Result<Vec<Change>> {
    let out = git(
        cwd,
        [
            "diff",
            "--numstat",
            "--no-renames",
            "--no-ext-diff",
            "-z",
            from,
            to,
        ],
        &[],
    )?;
    Ok(out
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (a, d, path) = (parts.next()?, parts.next()?, parts.next()?);
            Some(Change {
                path: path.to_string(),
                added: a.parse().unwrap_or(0),
                deleted: d.parse().unwrap_or(0),
                binary: a == "-" && d == "-",
            })
        })
        .collect())
}

/// How much patch text [`diff`] returns at most.
pub const DIFF_PATCH_LIMIT: usize = 2 * 1024 * 1024;

/// A working tree's diff against a commit: every changed file and the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Each file with its status and line counts.
    pub files: Vec<(Change, FileStatus)>,
    /// Unified diff text, cut at [`DIFF_PATCH_LIMIT`] at the end of a line.
    pub patch: String,
    pub truncated: bool,
}

/// What the working tree as it is now (committed, uncommitted and untracked;
/// ignored files and untracked [`SANDBOX_PLACEHOLDERS`] left out, as in commits)
/// changes against `base`. Reads only: the snapshot uses a private index.
pub fn diff(cwd: &Path, base: &str) -> Result<Diff> {
    let now = snapshot(cwd)?;
    let statuses = diff_name_status(cwd, base, &now)?;
    let files = numstat(cwd, base, &now)?
        .into_iter()
        .map(|c| {
            let status = statuses
                .iter()
                .find(|s| s.path == c.path)
                .map_or(FileStatus::Modified, |s| s.status);
            (c, status)
        })
        .collect();
    let patch = git(
        cwd,
        [
            "-c",
            "core.quotePath=false",
            "diff",
            "-p",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            &now,
        ],
        &[],
    )?;
    let (patch, truncated) = cut_at_line(patch, DIFF_PATCH_LIMIT);
    Ok(Diff {
        files,
        patch,
        truncated,
    })
}

/// `text` cut to at most `limit` bytes, at the end of a line; whether it was cut.
fn cut_at_line(mut text: String, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut at = limit;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let end = text[..at].rfind('\n').map_or(0, |i| i + 1);
    text.truncate(end);
    (text, true)
}

/// The best common ancestor of two commits (`git merge-base`).
pub fn merge_base(cwd: &Path, a: &str, b: &str) -> Result<String> {
    Ok(git(cwd, ["merge-base", a, b], &[])?.trim().to_string())
}

/// What a restore changed, as paths relative to the repository's top level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restored {
    /// Files put back to their content at the checkpoint (changed or deleted since).
    pub restored: Vec<String>,
    /// Files removed because they were created after the checkpoint.
    pub deleted: Vec<String>,
}

/// Puts the working tree back to `commit`: restores tracked and untracked files,
/// deletes files created since, leaves ignored files alone, and resets the user's
/// index to `HEAD`.
pub fn restore(cwd: &Path, commit: &str) -> Result<Restored> {
    let current = snapshot(cwd)?;
    let mut result = Restored::default();
    for change in diff_name_status(cwd, commit, &current)? {
        match change.status {
            FileStatus::Added => result.deleted.push(change.path),
            _ => result.restored.push(change.path),
        }
    }

    let checkpoint_empty = git(cwd, ["ls-tree", commit], &[])?.trim().is_empty();
    if !checkpoint_empty {
        git(
            cwd,
            [
                "restore",
                "--source",
                commit,
                "--worktree",
                "--staged",
                "--",
                ":/",
            ],
            &[],
        )?;
    }
    // Restoring can remove the last file of the directory git runs in.
    std::fs::create_dir_all(cwd)?;
    git(cwd, ["clean", "-f", "-d", "--quiet", "--", ":/"], &[])?;
    if has_commit(cwd)? {
        git(cwd, ["reset", "--quiet", "--", ":/"], &[])?;
    }
    Ok(result)
}

fn absolute(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

#[cfg(test)]
mod tests;

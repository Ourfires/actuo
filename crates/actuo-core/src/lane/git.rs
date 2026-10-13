//! The lane's git: fetching its clone, fast-forwarding a run's worktree,
//! actuo's commit and the one push. Through [`crate::git`]'s helper, so hooks never
//! run; commit and push also pass `--no-verify`.

use crate::git::{GitError, Result, git, git_with_stdin};
use std::path::Path;

/// `git fetch --prune origin` in the lane's clone.
pub fn fetch(clone: &Path) -> Result<()> {
    git(clone, ["fetch", "--quiet", "--prune", "origin"], &[]).map(|_| ())
}

/// What [`fast_forward`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastForward {
    /// `origin` has no such branch (nothing pushed yet).
    NoRemote,
    UpToDate,
    Forwarded,
    /// Both sides have commits the other lacks; a rebase would need a force push.
    Diverged,
}

/// Brings a run's worktree up to `origin/<branch>` if that is only ahead (someone
/// pushed to the PR branch). Never rebases, never resets.
pub fn fast_forward(worktree: &Path, branch: &str) -> Result<FastForward> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    match git(worktree, ["fetch", "--quiet", "origin", &refspec], &[]) {
        Ok(_) => {}
        Err(GitError::Failed { stderr, .. }) if stderr.contains("couldn't find remote ref") => {
            return Ok(FastForward::NoRemote);
        }
        Err(e) => return Err(e),
    }
    let remote = format!("refs/remotes/origin/{branch}");
    let ancestor = |a: &str, b: &str| {
        match git(worktree, ["merge-base", "--is-ancestor", a, b], &[]) {
            Ok(_) => Ok(true),
            // Exit 1: not an ancestor; anything else is an error.
            Err(GitError::Failed { status, .. }) if status.ends_with(": 1") => Ok(false),
            Err(e) => Err(e),
        }
    };
    if ancestor(&remote, "HEAD")? {
        return Ok(FastForward::UpToDate);
    }
    if !ancestor("HEAD", &remote)? {
        return Ok(FastForward::Diverged);
    }
    git(worktree, ["merge", "--quiet", "--ff-only", &remote], &[])?;
    Ok(FastForward::Forwarded)
}

/// Actuo's commit of everything in the worktree (`git add -A`, which respects
/// `.gitignore`), with your git identity. The message goes on stdin. Without `sign`,
/// `--no-gpg-sign`; with it, your git config decides. Returns the new commit, or
/// `None` when there was nothing to commit.
pub fn commit(worktree: &Path, message: &str, sign: bool) -> Result<Option<String>> {
    stage(worktree)?;
    if git(
        worktree,
        ["status", "--porcelain", "--untracked-files=no"],
        &[],
    )?
    .trim()
    .is_empty()
    {
        return Ok(None);
    }
    let mut args = vec!["commit", "--quiet", "--no-verify", "-F", "-"];
    if !sign {
        args.push("--no-gpg-sign");
    }
    git_with_stdin(worktree, args, &[], Some(message))?;
    Ok(Some(
        git(worktree, ["rev-parse", "HEAD"], &[])?
            .trim()
            .to_string(),
    ))
}

/// `git add --all`: what actuo's commit would take, staged. Untracked files
/// at srt's placeholder names ([`crate::git::SANDBOX_PLACEHOLDERS`]) are never
/// added; they stay on disk, and tracked files of those names are staged as usual.
pub fn stage(worktree: &Path) -> Result<()> {
    let placeholders = crate::git::untracked_placeholders(worktree)?;
    git(worktree, ["add", "--all", "--", ":/"], &[])?;
    crate::git::unstage(worktree, &placeholders, &[])
}

/// The staged change against `against`, as `diff -U0` text with `a/` and `b/`
/// prefixes and unquoted paths, binary files as text, whatever your git config says.
pub fn staged_diff(worktree: &Path, against: &str) -> Result<String> {
    git(
        worktree,
        [
            "-c",
            "core.quotePath=false",
            "diff",
            "--cached",
            "--text",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "-U0",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            against,
        ],
        &[],
    )
}

/// The commits `HEAD` has that `against` doesn't: each one's id and full message.
pub fn messages(worktree: &Path, against: &str) -> Result<Vec<(String, String)>> {
    let out = git(
        worktree,
        [
            "log",
            "--no-color",
            "--format=%H%x00%B%x1e",
            &format!("{against}..HEAD"),
        ],
        &[],
    )?;
    Ok(out
        .split('\x1e')
        .filter_map(|c| {
            let (id, message) = c.trim_start_matches('\n').split_once('\0')?;
            Some((id.to_string(), message.to_string()))
        })
        .collect())
}

pub use crate::git::{Change, changes};

/// Whether `path` (relative to the repo root) is one a run may not change. An entry
/// with a `*` is a glob over the path from the root ([`protected_glob`]). Otherwise
/// an entry ending in `/` is a folder of that name at any depth, any other entry a
/// file of that name at any depth, or the exact path when it contains a `/`.
pub fn is_protected(path: &str, protected: &[String]) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    protected.iter().any(|p| {
        if p.contains('*') {
            // Settings validation refuses a glob that doesn't compile.
            protected_glob(p).is_ok_and(|g| g.is_match(path))
        } else if let Some(dir) = p.strip_suffix('/') {
            if dir.contains('/') {
                path.starts_with(p.as_str())
            } else {
                parts[..parts.len().saturating_sub(1)].contains(&dir)
            }
        } else if p.contains('/') {
            path == p
        } else {
            parts.last() == Some(&p.as_str())
        }
    })
}

/// A `protected_paths` entry with a `*`, as a glob over the path: `*` stays inside
/// one name, `**` crosses folders, and a trailing `/` means any file under a matching
/// folder.
pub fn protected_glob(pattern: &str) -> std::result::Result<globset::GlobMatcher, String> {
    let glob = match pattern.strip_suffix('/') {
        Some(dir) => format!("{dir}/**"),
        None => pattern.to_string(),
    };
    globset::GlobBuilder::new(&glob)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| e.kind().to_string())
}

/// The commit `HEAD` and `base` have in common: what turbo's `--affected` compares
/// against (`TURBO_SCM_BASE`), since the clone's own default branch is stale.
pub fn merge_base(worktree: &Path, base: &str) -> Result<String> {
    git(worktree, ["merge-base", "HEAD", base], &[]).map(|o| o.trim().to_string())
}

/// How many commits `HEAD` has that `base` doesn't.
pub fn ahead(worktree: &Path, base: &str) -> Result<u64> {
    let out = git(
        worktree,
        ["rev-list", "--count", &format!("{base}..HEAD")],
        &[],
    )?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// The commit `HEAD` is at.
pub fn head(worktree: &Path) -> Result<String> {
    Ok(git(worktree, ["rev-parse", "HEAD"], &[])?
        .trim()
        .to_string())
}

/// Where `refs/heads/<branch>` is on `origin`, if it exists there.
pub fn remote_head(worktree: &Path, branch: &str) -> Result<Option<String>> {
    let out = git(
        worktree,
        ["ls-remote", "origin", &format!("refs/heads/{branch}")],
        &[],
    )?;
    Ok(out.split_whitespace().next().map(str::to_string))
}

/// The one push actuo makes: the worktree's `HEAD` to the run's branch on
/// `origin`, which must be `<prefix>/issue-<issue>` with a one-word prefix (the
/// branch the run was created with), so no other branch can be pushed, and there is
/// no force: a push that isn't a fast-forward fails. No tags either:
/// `--no-follow-tags` overrides a `push.followTags` the agent may have set.
pub fn push_run_branch(worktree: &Path, branch: &str, issue: u64) -> Result<()> {
    let prefix = branch.strip_suffix(&format!("/issue-{issue}"));
    if !prefix.is_some_and(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }) {
        return Err(GitError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("won't push {branch:?}: not a run branch of issue {issue}"),
        )));
    }
    let refspec = format!("HEAD:refs/heads/{branch}");
    git(
        worktree,
        [
            "push",
            "--quiet",
            "--no-verify",
            "--no-follow-tags",
            "origin",
            &refspec,
        ],
        &[],
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) -> String {
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
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare `origin` with one commit on main, a clone of it (the lane's), and a
    /// worktree of the clone on `actuo/issue-7` from `origin/main`.
    struct Repos {
        _dir: tempfile::TempDir,
        root: PathBuf,
        origin: PathBuf,
        clone: PathBuf,
        worktree: PathBuf,
    }

    fn repos() -> Repos {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let origin = root.join("origin.git");
        run(
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
        let seed = root.join("seed");
        run(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        fs::write(seed.join("README.md"), "readme\n").unwrap();
        run(&seed, &["add", "-A"]);
        run(&seed, &["commit", "-q", "-m", "init"]);
        run(&seed, &["push", "-q", "origin", "main"]);
        let clone = root.join("clone");
        run(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["config", "user.name", "Test User"]);
        run(&clone, &["config", "user.email", "test@example.com"]);
        let worktree = root.join("wt");
        crate::git::worktree_add(&clone, &worktree, "actuo/issue-7", "origin/main").unwrap();
        Repos {
            _dir: dir,
            root,
            origin,
            clone,
            worktree,
        }
    }

    fn origin_ref(r: &Repos, name: &str) -> Option<String> {
        let out = Command::new("git")
            .args([
                "--git-dir",
                r.origin.to_str().unwrap(),
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

    /// srt's placeholder files (empty, untracked, at the top level) are never
    /// committed and stay on disk; a tracked file of such a name is committed as
    /// usual; nothing but placeholders is nothing to commit.
    #[test]
    fn commit_leaves_out_untracked_sandbox_placeholders() {
        let r = repos();
        for name in [".bashrc", ".gitconfig", ".mcp.json", ".vscode", ".idea"] {
            fs::write(r.worktree.join(name), "").unwrap();
        }
        assert_eq!(
            commit(&r.worktree, "only placeholders\n", false).unwrap(),
            None
        );

        fs::write(r.worktree.join(".profile"), "mine\n").unwrap();
        fs::write(r.worktree.join("fix.txt"), "fixed\n").unwrap();
        // .profile is a placeholder name, but here it's the agent's file: untracked,
        // so left out too; it goes in once it's tracked.
        commit(&r.worktree, "fix\n", false).unwrap().unwrap();
        let files = run(&r.worktree, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files.trim(), "fix.txt");
        run(&r.worktree, &["add", "-f", ".profile"]);
        run(
            &r.worktree,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "track .profile",
            ],
        );
        fs::write(r.worktree.join(".profile"), "changed\n").unwrap();
        commit(&r.worktree, "tracked\n", false).unwrap().unwrap();
        let files = run(&r.worktree, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files.trim(), ".profile");
        for name in [".bashrc", ".gitconfig", ".mcp.json", ".vscode", ".idea"] {
            assert!(r.worktree.join(name).exists(), "{name} deleted");
        }
        assert!(
            run(
                &r.worktree,
                &["ls-files", ".bashrc", ".mcp.json", ".vscode"]
            )
            .trim()
            .is_empty()
        );
    }

    #[test]
    fn commit_and_push_the_run_branch() {
        let r = repos();
        let main = origin_ref(&r, "refs/heads/main").unwrap();
        assert_eq!(commit(&r.worktree, "nothing\n", false).unwrap(), None);

        fs::write(r.worktree.join("fix.txt"), "fixed\n").unwrap();
        let message =
            "Fix \"it\" $(touch /tmp/x) `id`\n\nCloses #7\n\nActuo-Session: s1 (claude)\n";
        let sha = commit(&r.worktree, message, false).unwrap().unwrap();
        assert_eq!(
            run(&r.worktree, &["log", "-1", "--format=%B"]),
            message.trim_end()
        );
        assert_eq!(
            run(&r.worktree, &["log", "-1", "--format=%an <%ae>"]),
            "Test User <test@example.com>"
        );

        assert_eq!(remote_head(&r.worktree, "actuo/issue-7").unwrap(), None);
        push_run_branch(&r.worktree, "actuo/issue-7", 7).unwrap();
        assert_eq!(
            origin_ref(&r, "refs/heads/actuo/issue-7"),
            Some(sha.clone())
        );
        assert_eq!(
            remote_head(&r.worktree, "actuo/issue-7").unwrap(),
            Some(sha.clone())
        );
        assert_eq!(head(&r.worktree).unwrap(), sha);
        assert_eq!(origin_ref(&r, "refs/heads/main"), Some(main));
        fetch(&r.clone).unwrap();
        assert!(run(&r.clone, &["branch", "-r"]).contains("origin/actuo/issue-7"));
    }

    #[test]
    fn no_force_push() {
        let r = repos();
        // Someone else's commit on the run branch, not in the worktree.
        let other = r.root.join("other");
        run(
            &r.root,
            &[
                "clone",
                "-q",
                r.origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run(&other, &["checkout", "-q", "-b", "actuo/issue-7"]);
        fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
        run(&other, &["add", "-A"]);
        run(&other, &["commit", "-q", "-m", "theirs"]);
        run(&other, &["push", "-q", "origin", "actuo/issue-7"]);
        let theirs = origin_ref(&r, "refs/heads/actuo/issue-7").unwrap();

        fs::write(r.worktree.join("ours.txt"), "ours\n").unwrap();
        commit(&r.worktree, "ours\n", false).unwrap();
        assert!(push_run_branch(&r.worktree, "actuo/issue-7", 7).is_err());
        assert_eq!(origin_ref(&r, "refs/heads/actuo/issue-7"), Some(theirs));
        assert_eq!(
            fast_forward(&r.worktree, "actuo/issue-7").unwrap(),
            FastForward::Diverged
        );
    }

    #[test]
    fn fast_forward_to_pushed_commits() {
        let r = repos();
        assert_eq!(
            fast_forward(&r.worktree, "actuo/issue-7").unwrap(),
            FastForward::NoRemote
        );
        fs::write(r.worktree.join("a.txt"), "a\n").unwrap();
        commit(&r.worktree, "a\n", false).unwrap();
        push_run_branch(&r.worktree, "actuo/issue-7", 7).unwrap();
        assert_eq!(
            fast_forward(&r.worktree, "actuo/issue-7").unwrap(),
            FastForward::UpToDate
        );
        // A person pushes to the PR branch.
        let other = r.root.join("other");
        run(
            &r.root,
            &[
                "clone",
                "-q",
                "-b",
                "actuo/issue-7",
                r.origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        fs::write(other.join("b.txt"), "b\n").unwrap();
        run(&other, &["add", "-A"]);
        run(&other, &["commit", "-q", "-m", "b"]);
        run(&other, &["push", "-q", "origin", "actuo/issue-7"]);
        assert_eq!(
            fast_forward(&r.worktree, "actuo/issue-7").unwrap(),
            FastForward::Forwarded
        );
        assert!(r.worktree.join("b.txt").exists());
    }

    #[test]
    fn signing_follows_the_setting() {
        let r = repos();
        // Your git config signs every commit, with a signer that always fails.
        run(&r.clone, &["config", "commit.gpgsign", "true"]);
        run(&r.clone, &["config", "gpg.program", "false"]);
        fs::write(r.worktree.join("a.txt"), "a\n").unwrap();
        assert!(commit(&r.worktree, "signed?\n", true).is_err());
        assert!(commit(&r.worktree, "unsigned\n", false).unwrap().is_some());
    }

    /// The hooks an agent could plant never run at commit or push.
    #[test]
    fn hooks_never_run() {
        let r = repos();
        let marker = r.root.join("hook-ran");
        let hooks = r.worktree.join(".githooks");
        fs::create_dir_all(&hooks).unwrap();
        for name in [
            "pre-commit",
            "commit-msg",
            "post-commit",
            "pre-push",
            "reference-transaction",
        ] {
            let p = hooks.join(name);
            fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        run(&r.clone, &["config", "core.hooksPath", ".githooks"]);
        fs::write(r.worktree.join("a.txt"), "a\n").unwrap();
        commit(&r.worktree, "a\n", false).unwrap();
        push_run_branch(&r.worktree, "actuo/issue-7", 7).unwrap();
        assert!(!marker.exists(), "a hook ran");
        // Plain git would have run them.
        fs::write(r.worktree.join("b.txt"), "b\n").unwrap();
        run(&r.worktree, &["add", "-A"]);
        run(&r.worktree, &["commit", "-q", "-m", "b"]);
        assert!(marker.exists());
    }

    #[test]
    fn changes_against_the_base() {
        let r = repos();
        assert!(changes(&r.worktree, "origin/main").unwrap().is_empty());
        fs::write(r.worktree.join("README.md"), "readme\nmore\n").unwrap();
        fs::create_dir_all(r.worktree.join(".github/workflows")).unwrap();
        fs::write(r.worktree.join(".github/workflows/ci.yml"), "a\nb\n").unwrap();
        fs::write(r.worktree.join("bin.dat"), [0u8, 1, 2]).unwrap();
        let mut c = changes(&r.worktree, "origin/main").unwrap();
        c.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            c,
            vec![
                Change {
                    path: ".github/workflows/ci.yml".into(),
                    added: 2,
                    deleted: 0,
                    binary: false,
                },
                Change {
                    path: "README.md".into(),
                    added: 1,
                    deleted: 0,
                    binary: false,
                },
                Change {
                    path: "bin.dat".into(),
                    added: 0,
                    deleted: 0,
                    binary: true,
                },
            ]
        );
        // The user's index is untouched.
        assert_eq!(run(&r.worktree, &["diff", "--cached", "--name-only"]), "");
    }

    #[test]
    fn protected_paths() {
        let p: Vec<String> = [
            ".github/",
            ".claude/",
            ".mcp.json",
            "CLAUDE.md",
            "notes/x/",
            "a/b.txt",
        ]
        .map(String::from)
        .to_vec();
        for yes in [
            ".github/workflows/ci.yml",
            "sub/.claude/settings.json",
            ".mcp.json",
            "pkg/CLAUDE.md",
            "notes/x/y.md",
            "a/b.txt",
        ] {
            assert!(is_protected(yes, &p), "{yes}");
        }
        for no in [
            ".githubx/a",
            "src/github.rs",
            "CLAUDE.md.bak",
            "z/a/b.txt",
            ".github",
        ] {
            assert!(!is_protected(no, &p), "{no}");
        }
    }

    #[test]
    fn protected_globs() {
        let p: Vec<String> = [
            "pnpm-lock.yaml",
            "apps/web/src/types/generated.ts",
            "**/*-snapshots/",
            "packages/*/data/generated/",
        ]
        .map(String::from)
        .to_vec();
        for yes in [
            "pnpm-lock.yaml",
            "apps/x/pnpm-lock.yaml",
            "apps/web/src/types/generated.ts",
            "e2e/home.spec.ts-snapshots/home.png",
            "apps/web/tests/a-snapshots/b/c.png",
            "x-snapshots/a.png",
            "packages/shop/data/generated/pages.json",
            "packages/shop/data/generated/deep/a.json",
        ] {
            assert!(is_protected(yes, &p), "{yes}");
        }
        for no in [
            "pnpm-lock.yaml.bak",
            "apps/web/src/types/other.ts",
            "e2e/snapshots/home.png",
            "e2e/home-snapshots",
            "packages/shop/data/generated",
            "packages/a/b/data/generated/pages.json",
            "packages/shop/data/other/pages.json",
        ] {
            assert!(!is_protected(no, &p), "{no}");
        }
        // `*` stays inside one name; `**` crosses folders.
        let one = vec!["src/*.ts".to_string()];
        assert!(is_protected("src/a.ts", &one));
        assert!(!is_protected("src/a/b.ts", &one));
        let any = vec!["src/**/*.ts".to_string()];
        assert!(is_protected("src/a/b.ts", &any) && is_protected("src/a.ts", &any));
        assert!(protected_glob("src/[*.ts").is_err());
    }

    /// Only `<one-word prefix>/issue-<the run's issue>` is pushed.
    #[test]
    fn only_the_runs_own_branch_is_pushed() {
        let r = repos();
        for wrong in [
            "main",
            "actuo/issue-8",
            "a/b/issue-7",
            "/issue-7",
            "Actuo/issue-7",
            "issue-7",
        ] {
            let e = push_run_branch(&r.worktree, wrong, 7)
                .unwrap_err()
                .to_string();
            assert!(e.contains("not a run branch of issue 7"), "{wrong}: {e}");
        }
    }

    /// `push_run_branch` is the only place that pushes, in this crate or actuo's.
    #[test]
    fn one_push_in_the_code() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        let mut dirs = vec![here.join("src"), here.join("../actuo/src")];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = fs::read_to_string(&path).unwrap();
                    let code = text.split("#[cfg(test)]").next().unwrap();
                    for (i, line) in code.lines().enumerate() {
                        if line.contains("\"push\"") {
                            found.push(format!("{}:{}", path.display(), i + 1));
                        }
                    }
                }
            }
        }
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("lane/git.rs"), "{found:#?}");
    }
}

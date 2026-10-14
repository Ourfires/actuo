use super::*;
use std::fs;

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
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repo with a commit: README.md, a.txt, .gitignore ignoring `ignored.txt`.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    run(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("README.md"), "readme\n").unwrap();
    fs::write(root.join("a.txt"), "a1\n").unwrap();
    fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    run(&root, &["add", "-A"]);
    run(&root, &["commit", "-q", "-m", "init"]);
    (dir, root)
}

fn status(root: &Path) -> String {
    run(root, &["status", "--porcelain", "--untracked-files=all"])
}

fn leftover_indexes(root: &Path) -> Vec<String> {
    let common = absolute(root, run(root, &["rev-parse", "--git-common-dir"]).trim());
    fs::read_dir(common)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("actuo-checkpoint-index"))
        .collect()
}

#[test]
fn toplevel_and_has_commit() {
    let (_d, root) = repo();
    fs::create_dir(root.join("sub")).unwrap();
    assert_eq!(toplevel(&root.join("sub")).unwrap(), root);
    assert!(has_commit(&root).unwrap());

    let empty = tempfile::tempdir().unwrap();
    run(empty.path(), &["init", "-q"]);
    assert!(!has_commit(empty.path()).unwrap());

    let not_repo = tempfile::tempdir().unwrap();
    assert!(matches!(
        toplevel(not_repo.path()),
        Err(GitError::NotARepo(_))
    ));
}

#[test]
fn checkpoint_diff_and_restore() {
    let (_d, root) = repo();
    let r0 = "refs/actuo/checkpoints/s/0";
    let c0 = capture_checkpoint(&root, r0).unwrap();
    assert_eq!(resolve_ref(&root, r0).unwrap(), Some(c0.clone()));
    assert_eq!(
        resolve_ref(&root, "refs/actuo/checkpoints/s/9").unwrap(),
        None
    );

    // A turn: modify, delete, create (also in a new directory), and an ignored file.
    fs::write(root.join("a.txt"), "a2\n").unwrap();
    fs::remove_file(root.join("README.md")).unwrap();
    fs::write(root.join("new.txt"), "new\n").unwrap();
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join("dir/n.txt"), "n\n").unwrap();
    fs::write(root.join("ignored.txt"), "keep me\n").unwrap();

    let c1 = capture_checkpoint(&root, "refs/actuo/checkpoints/s/1").unwrap();
    let mut files = diff_name_status(&root, &c0, &c1).unwrap();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let got: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
    assert_eq!(
        got,
        vec![
            ("README.md", FileStatus::Deleted),
            ("a.txt", FileStatus::Modified),
            ("dir/n.txt", FileStatus::Added),
            ("new.txt", FileStatus::Added),
        ]
    );

    let mut restored = restore(&root, &c0).unwrap();
    restored.restored.sort();
    restored.deleted.sort();
    assert_eq!(restored.restored, vec!["README.md", "a.txt"]);
    assert_eq!(restored.deleted, vec!["dir/n.txt", "new.txt"]);

    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a1\n");
    assert_eq!(
        fs::read_to_string(root.join("README.md")).unwrap(),
        "readme\n"
    );
    assert!(!root.join("new.txt").exists());
    assert!(!root.join("dir").exists());
    assert_eq!(
        fs::read_to_string(root.join("ignored.txt")).unwrap(),
        "keep me\n"
    );
    // The user's index is back at HEAD and nothing else is left over.
    assert_eq!(status(&root), "");
    assert!(leftover_indexes(&root).is_empty());
}

#[test]
fn restore_keeps_untracked_files_that_were_in_the_checkpoint() {
    let (_d, root) = repo();
    fs::write(root.join("draft.txt"), "draft\n").unwrap();
    let c0 = capture_checkpoint(&root, "refs/actuo/checkpoints/s/0").unwrap();
    fs::write(root.join("draft.txt"), "changed\n").unwrap();
    restore(&root, &c0).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("draft.txt")).unwrap(),
        "draft\n"
    );
    // Still untracked, as before.
    assert_eq!(status(&root), "?? draft.txt\n");
}

#[test]
fn checkpoint_leaves_the_users_index_alone() {
    let (_d, root) = repo();
    fs::write(root.join("a.txt"), "staged\n").unwrap();
    run(&root, &["add", "a.txt"]);
    fs::write(root.join("b.txt"), "untracked\n").unwrap();
    let before = status(&root);
    capture_checkpoint(&root, "refs/actuo/checkpoints/s/0").unwrap();
    assert_eq!(status(&root), before);
    assert_eq!(run(&root, &["diff", "--cached", "--name-only"]), "a.txt\n");
}

#[test]
fn checkpoint_ignores_commit_signing_config() {
    let (_d, root) = repo();
    run(&root, &["config", "commit.gpgsign", "true"]);
    run(&root, &["config", "gpg.program", "/nonexistent/gpg"]);
    capture_checkpoint(&root, "refs/actuo/checkpoints/s/0").unwrap();
}

#[test]
fn checkpoint_in_a_repo_without_commits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    run(&root, &["init", "-q"]);
    let c0 = capture_checkpoint(&root, "refs/actuo/checkpoints/s/0").unwrap();
    fs::write(root.join("x.txt"), "x\n").unwrap();
    let c1 = snapshot(&root).unwrap();
    assert_eq!(
        diff_name_status(&root, &c0, &c1).unwrap(),
        vec![FileChange {
            path: "x.txt".into(),
            status: FileStatus::Added
        }]
    );
    let restored = restore(&root, &c0).unwrap();
    assert_eq!(restored.deleted, vec!["x.txt"]);
    assert!(!root.join("x.txt").exists());
}

#[test]
fn worktree_checkpoints_use_the_common_dir() {
    let (_d, root) = repo();
    let wt = root.parent().unwrap().join("worktrees/p/s");
    worktree_add(&root, &wt, "actuo/s", "HEAD").unwrap();
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "a1\n");
    assert_eq!(run(&wt, &["branch", "--show-current"]), "actuo/s\n");

    let c0 = capture_checkpoint(&wt, "refs/actuo/checkpoints/s/0").unwrap();
    fs::write(wt.join("a.txt"), "changed in worktree\n").unwrap();
    let c1 = capture_checkpoint(&wt, "refs/actuo/checkpoints/s/1").unwrap();
    // Refs are shared with the main checkout.
    assert_eq!(
        resolve_ref(&root, "refs/actuo/checkpoints/s/1").unwrap(),
        Some(c1)
    );
    restore(&wt, &c0).unwrap();
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "a1\n");
    // The main checkout is untouched.
    assert_eq!(status(&root), "");
    assert!(leftover_indexes(&root).is_empty());
}

#[test]
fn worktrees_by_branch_and_base() {
    let (_d, root) = repo();
    let first = run(&root, &["rev-parse", "HEAD"]);
    fs::write(root.join("a.txt"), "a2\n").unwrap();
    run(&root, &["commit", "-q", "-am", "second"]);

    // A new branch from a base other than HEAD.
    let wt = root.parent().unwrap().join("worktrees/p/s1");
    assert!(!branch_exists(&root, "actuo/issue-1").unwrap());
    worktree_add(&root, &wt, "actuo/issue-1", first.trim()).unwrap();
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "a1\n");
    assert!(branch_exists(&root, "actuo/issue-1").unwrap());
    assert_eq!(
        worktree_for_branch(&root, "actuo/issue-1").unwrap(),
        Some(wt.canonicalize().unwrap())
    );
    assert_eq!(
        worktree_for_branch(&root, "main").unwrap(),
        Some(root.clone())
    );
    assert_eq!(worktree_for_branch(&root, "actuo/none").unwrap(), None);

    // A branch without a worktree gets one, as it is.
    run(&root, &["branch", "kept", first.trim()]);
    let wt2 = root.parent().unwrap().join("worktrees/p/s2");
    worktree_add_existing(&root, &wt2, "kept").unwrap();
    assert_eq!(fs::read_to_string(wt2.join("a.txt")).unwrap(), "a1\n");

    assert!(valid_branch_name(&root, "actuo/issue-1"));
    for bad in ["", "-x", "a..b", "a b", "a~1", "x.lock"] {
        assert!(!valid_branch_name(&root, bad), "{bad}");
    }
}

/// Hooks would run outside any sandbox with the user's credentials: actuo's
/// git calls never run them, from `.git/hooks` or a `core.hooksPath` in the tree.
#[test]
fn hooks_never_run() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = repo();
    let marker = root.parent().unwrap().join("hook-ran");
    let hook = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    let install = |dir: &Path| {
        fs::create_dir_all(dir).unwrap();
        for name in [
            "post-checkout",
            "pre-commit",
            "post-index-change",
            "reference-transaction",
        ] {
            let p = dir.join(name);
            fs::write(&p, &hook).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    install(&root.join(".git/hooks"));
    // The hook works when git runs it.
    run(&root, &["checkout", "-q", "-b", "probe"]);
    assert!(marker.exists(), "the test hook should run under plain git");
    fs::remove_file(&marker).unwrap();
    run(&root, &["checkout", "-q", "main"]);
    fs::remove_file(&marker).unwrap();

    let wt = root.parent().unwrap().join("worktrees/p/s");
    worktree_add(&root, &wt, "actuo/s", "HEAD").unwrap();
    // An agent writes hooks in the worktree and points core.hooksPath at them.
    install(&wt.join(".githooks"));
    run(&root, &["config", "core.hooksPath", ".githooks"]);
    fs::write(wt.join("a.txt"), "changed\n").unwrap();
    let c = capture_checkpoint(&wt, "refs/actuo/checkpoints/s/1").unwrap();
    restore(&wt, &c).unwrap();
    diff_name_status(&wt, "HEAD", &c).unwrap();
    worktree_add(
        &root,
        &root.parent().unwrap().join("worktrees/p/t"),
        "actuo/t",
        "HEAD",
    )
    .unwrap();
    assert!(!marker.exists(), "a hook ran");
}

/// srt's placeholder files (empty, untracked, at the top level) never get into a
/// checkpoint, so they don't show in turn file lists; files under a placeholder
/// name, tracked ones of those names, and the files on disk are untouched.
#[test]
fn snapshots_leave_out_untracked_sandbox_placeholders() {
    let (_d, root) = repo();
    fs::write(root.join(".profile"), "tracked\n").unwrap();
    run(&root, &["add", ".profile"]);
    run(&root, &["commit", "-q", "-m", "a tracked .profile"]);
    let before = snapshot(&root).unwrap();
    for name in [".bashrc", ".gitconfig", ".mcp.json", ".vscode"] {
        fs::write(root.join(name), "").unwrap();
    }
    fs::create_dir(root.join(".idea")).unwrap();
    fs::write(root.join(".idea/workspace.xml"), "<x/>\n").unwrap();
    fs::write(root.join(".profile"), "changed\n").unwrap();
    fs::write(root.join("new.txt"), "n\n").unwrap();
    let after = snapshot(&root).unwrap();
    let mut files: Vec<(String, FileStatus)> = diff_name_status(&root, &before, &after)
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.status))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        files,
        vec![
            (".idea/workspace.xml".to_string(), FileStatus::Added),
            (".profile".to_string(), FileStatus::Modified),
            ("new.txt".to_string(), FileStatus::Added),
        ]
    );
    assert!(root.join(".bashrc").exists() && root.join(".vscode").exists());
    assert_eq!(untracked_placeholders(&root).unwrap().len(), 4);
}

/// `session/diff`'s git side: committed, uncommitted and untracked changes against
/// the base; srt's untracked placeholders left out; binary files counted as such.
#[test]
fn diff_against_a_base() {
    let (_d, root) = repo();
    let base = run(&root, &["rev-parse", "HEAD"]).trim().to_string();
    fs::write(root.join("a.txt"), "a1\na2\n").unwrap();
    run(&root, &["commit", "-q", "-am", "a2"]);
    fs::write(root.join("README.md"), "changed\n").unwrap();
    fs::write(root.join("new.txt"), "new\n").unwrap();
    fs::write(root.join("bin.dat"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    fs::write(root.join(".bashrc"), "").unwrap();
    fs::write(root.join("ignored.txt"), "x\n").unwrap();
    let before = status(&root);

    let d = diff(&root, &base).unwrap();
    let by_path = |p: &str| d.files.iter().find(|(c, _)| c.path == p).cloned();
    let names: Vec<_> = d.files.iter().map(|(c, _)| c.path.as_str()).collect();
    assert_eq!(names, ["README.md", "a.txt", "bin.dat", "new.txt"]);
    let (a, status_a) = by_path("a.txt").unwrap();
    assert_eq!(
        (a.added, a.deleted, a.binary, status_a),
        (1, 0, false, FileStatus::Modified)
    );
    let (n, status_n) = by_path("new.txt").unwrap();
    assert_eq!((n.added, status_n), (1, FileStatus::Added));
    let (b, _) = by_path("bin.dat").unwrap();
    assert!(b.binary && b.added == 0 && b.deleted == 0);
    assert!(d.patch.contains("diff --git a/new.txt b/new.txt"));
    assert!(d.patch.contains("+a2\n"));
    assert!(
        d.patch
            .contains("Binary files /dev/null and b/bin.dat differ")
    );
    assert!(!d.patch.contains(".bashrc") && !d.patch.contains("ignored.txt"));
    assert!(!d.truncated);
    // Nothing changed in the repo: the index and the files are as they were.
    assert_eq!(status(&root), before);
    assert!(leftover_indexes(&root).is_empty());
}

#[test]
fn a_long_patch_is_cut_at_a_line_end() {
    let (cut, truncated) = cut_at_line("ab\ncd\nef\n".to_string(), 7);
    assert_eq!((cut.as_str(), truncated), ("ab\ncd\n", true));
    let (cut, truncated) = cut_at_line("ab\n".to_string(), 7);
    assert_eq!((cut.as_str(), truncated), ("ab\n", false));
    // Never inside a character: "é" is two bytes.
    let (cut, truncated) = cut_at_line("é\néé\n".to_string(), 4);
    assert_eq!((cut.as_str(), truncated), ("é\n", true));

    let (_d, root) = repo();
    let base = run(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let line = "x".repeat(99) + "\n";
    fs::write(
        root.join("big.txt"),
        line.repeat(DIFF_PATCH_LIMIT / 100 + 10),
    )
    .unwrap();
    let d = diff(&root, &base).unwrap();
    assert!(d.truncated);
    assert!(d.patch.len() <= DIFF_PATCH_LIMIT && d.patch.ends_with('\n'));
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.files[0].0.added as usize, DIFF_PATCH_LIMIT / 100 + 10);
}

/// A lane agent can write its worktree's `.git` file. `repair_worktree` restores
/// it from the clone, so git actuo runs on the worktree can't be sent to a config
/// the agent controls (filter drivers, another repo's objects).
#[test]
fn repair_worktree_undoes_a_rewritten_git_file() {
    let (_d, clone) = repo();
    let wt = clone.parent().unwrap().join("wt");
    worktree_add(&clone, &wt, "work", "main").unwrap();
    let real = std::fs::read_to_string(wt.join(".git")).unwrap();

    // The agent points .git at a git dir of its own with a clean filter, and adds
    // attributes that would run it on `git add`.
    let evil = wt.join(".evil");
    std::fs::create_dir(&evil).unwrap();
    run(&evil, &["init", "-q", "--bare", "."]);
    run(&evil, &["config", "core.bare", "false"]);
    let marker = clone.parent().unwrap().join("ESCAPED");
    run(
        &evil,
        &[
            "config",
            "filter.pwn.clean",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ],
    );
    std::fs::write(wt.join(".gitattributes"), "* filter=pwn\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", evil.display())).unwrap();
    std::fs::write(wt.join("app.txt"), "payload\n").unwrap();

    repair_worktree(&clone, &wt).unwrap();
    assert_eq!(std::fs::read_to_string(wt.join(".git")).unwrap(), real);

    // actuo's stage, as `lane::git::stage` runs it, no longer triggers the filter.
    git(&wt, ["add", "--all", "--", ":/"], &[]).unwrap();
    assert!(!marker.exists(), "the filter ran: escape");
    assert_eq!(common_dir(&wt).unwrap(), common_dir(&clone).unwrap());
}

/// The agent rewrites `.git` and plants a filter; actuo's own git in that worktree
/// (the checkpoint taken at the end of every turn, a plain `git add`) runs with the
/// git dir it resolved from the repo, so the filter never runs and nothing reads
/// the rewritten `.git`. No repair needed first, so no window to race.
#[test]
fn a_rewritten_git_file_is_never_read() {
    let (_d, clone) = repo();
    let wt = clone.parent().unwrap().join("wt2");
    worktree_add(&clone, &wt, "work2", "main").unwrap();

    let evil = wt.join(".evil");
    std::fs::create_dir(&evil).unwrap();
    run(&evil, &["init", "-q", "--bare", "."]);
    run(&evil, &["config", "core.bare", "false"]);
    let marker = clone.parent().unwrap().join("ESCAPED2");
    run(
        &evil,
        &[
            "config",
            "filter.pwn.clean",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ],
    );
    std::fs::write(wt.join(".gitattributes"), "* filter=pwn\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", evil.display())).unwrap();
    std::fs::write(wt.join("app.txt"), "payload\n").unwrap();

    capture_checkpoint(&wt, "refs/actuo/checkpoints/s/1").unwrap();
    git(&wt, ["add", "--all", "--", ":/"], &[]).unwrap();
    assert!(!marker.exists(), "the filter ran: escape");
    assert_eq!(common_dir(&wt).unwrap(), common_dir(&clone).unwrap());
    // The checkpoint landed in the real repo, not the agent's.
    run(
        &clone,
        &["rev-parse", "--verify", "refs/actuo/checkpoints/s/1"],
    );
}

/// A worktree in actuo's worktrees folder that was never resolved is refused,
/// rather than trusted through its own `.git`.
#[test]
fn an_unresolved_worktree_is_refused() {
    let (_d, clone) = repo();
    let root = clone.parent().unwrap().join("worktrees");
    add_worktrees_root(&root);
    let wt = root.join("s");
    // Added with plain git, so actuo never resolved it.
    run(
        &clone,
        &["worktree", "add", "-q", "-b", "work3", wt.to_str().unwrap()],
    );
    let e = git(&wt, ["status"], &[]).unwrap_err().to_string();
    assert!(e.contains("isn't resolved"), "{e}");
    trust_worktree(&clone, &wt).unwrap();
    git(&wt, ["status"], &[]).unwrap();
}

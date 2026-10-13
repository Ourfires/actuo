//! Sets `ACTUO_COMMIT` for `initialize` and `--version`: the repo's last commit,
//! with `-dirty` when it has uncommitted changes.

use std::process::Command;

fn main() {
    let actuo = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(actuo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(commit) =
        git(&["log", "-1", "--format=%h", "--abbrev=7", "--", "."]).filter(|c| !c.is_empty())
    {
        let dirty = git(&["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty());
        let suffix = if dirty { "-dirty" } else { "" };
        println!("cargo:rustc-env=ACTUO_COMMIT={commit}{suffix}");
    }
    // A new commit or staged change, or a change to actuo's sources.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
    println!("cargo:rerun-if-changed=..");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
}

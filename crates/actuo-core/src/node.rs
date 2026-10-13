//! The Node.js actuo runs its npm packages on (the agents' adapters and `srt`): a
//! pinned release from nodejs.org, its sha256 checked against [`build`]'s pin,
//! unpacked into `~/.actuo/node/<version>` the first time something needs it.
//! Actuo starts those packages as `<this node> <script>`, never through the `node`
//! on `PATH`, so a missing or old Node there, or the `PATH` the service captured,
//! doesn't matter to them. `PATH` itself is left alone: the agents' commands and
//! a project's setup and checks use your toolchain.

use crate::config::Paths;
use std::path::{Path, PathBuf};

/// The pinned Node release. Bumping it: the version here and the three sha256s in
/// [`build`], from `https://nodejs.org/dist/<version>/SHASUMS256.txt`.
pub const VERSION: &str = "v24.21.0";

/// nodejs.org's name for this machine's build of [`VERSION`] (`.tar.xz`) and its
/// sha256.
pub fn build() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some((
            "darwin-arm64",
            "6239d4cf92d864487ec8cd3615038f7b67e7f58b77b21cd2f09ea9fbd68065fe",
        )),
        ("linux", "x86_64") => Some((
            "linux-x64",
            "fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6",
        )),
        ("linux", "aarch64") => Some((
            "linux-arm64",
            "6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2",
        )),
        _ => None,
    }
}

/// `~/.actuo/node/<version>`.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.home.join("node").join(VERSION)
}

/// The pinned `node`.
pub fn bin(paths: &Paths) -> PathBuf {
    dir(paths).join("bin/node")
}

/// The pinned Node's own npm (a script it runs).
pub fn npm_cli(paths: &Paths) -> PathBuf {
    dir(paths).join("lib/node_modules/npm/bin/npm-cli.js")
}

/// Installs the pinned Node if it isn't yet; returns its `node`. One install at
/// a time.
pub async fn ensure(paths: &Paths) -> Result<PathBuf, String> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = LOCK.lock().await;
    let node = bin(paths);
    if node.exists() {
        return Ok(node);
    }
    let Some((build, sha256)) = build() else {
        return Err(format!(
            "no Node build for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };
    let url = format!("https://nodejs.org/dist/{VERSION}/node-{VERSION}-{build}.tar.xz");
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || install(&paths, &url, build, sha256))
        .await
        .map_err(|e| format!("installing node {VERSION}: {e}"))??;
    Ok(node)
}

/// Downloads `url`, checks it against `sha256`, unpacks it in actuo's `tmp` and
/// moves its `node-<version>-<build>` folder to [`dir`]; then removes other
/// versions. Nothing is left behind when it fails.
fn install(paths: &Paths, url: &str, build: &str, sha256: &str) -> Result<(), String> {
    let name = format!("node-{VERSION}-{build}");
    let tmp = paths.home.join("tmp").join(&name);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let unpacked = unpack(url, sha256, &tmp, &name).and_then(|from| {
        let to = dir(paths);
        std::fs::create_dir_all(to.parent().expect("a parent"))
            .and_then(|()| std::fs::rename(&from, &to))
            .map_err(|e| format!("moving node {VERSION} to {}: {e}", to.display()))
    });
    let _ = std::fs::remove_dir_all(&tmp);
    unpacked?;
    remove_others(&paths.home.join("node"));
    Ok(())
}

/// The download, checked and unpacked in `tmp`: the folder holding `bin/node`.
fn unpack(url: &str, sha256: &str, tmp: &Path, name: &str) -> Result<PathBuf, String> {
    let archive = tmp.join(format!("{name}.tar.xz"));
    crate::release::download(url, &archive)?;
    let got = crate::release::sha256(&archive)?;
    if got != sha256 {
        return Err(format!(
            "node {VERSION}: the download's sha256 is {got}, actuo's pin says {sha256}"
        ));
    }
    let out = crate::process::output_std(
        std::process::Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(tmp),
    )
    .map_err(|e| format!("running tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "unpacking node {VERSION} (tar needs xz on Linux): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let folder = tmp.join(name);
    if !folder.join("bin/node").is_file() {
        return Err(format!("node {VERSION}: no bin/node in the download"));
    }
    Ok(folder)
}

/// Removes the versions under `~/.actuo/node` other than [`VERSION`].
fn remove_others(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != VERSION {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A `.tar.xz` as nodejs.org's, holding `node-<version>-test/bin/node`; its
    /// `file://` URL and sha256.
    fn fake(dir: &Path) -> (String, String) {
        let src = dir.join("src");
        let top = src.join(format!("node-{VERSION}-test/bin"));
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(top.join("node"), "#!/bin/sh\n").unwrap();
        let archive = dir.join("node.tar.xz");
        let out = Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(format!("node-{VERSION}-test"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let sum = crate::release::sha256(&archive).unwrap();
        (format!("file://{}", archive.display()), sum)
    }

    #[test]
    fn installs_checks_and_removes_other_versions() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: d.path().join("home"),
        };
        let old = paths.home.join("node/v20.0.0/bin");
        std::fs::create_dir_all(&old).unwrap();
        let (url, sum) = fake(d.path());

        // A sha256 that isn't the pin's: refused, nothing left behind.
        let e = install(&paths, &url, "test", &"0".repeat(64)).unwrap_err();
        assert!(e.contains("actuo's pin says 000"), "{e}");
        assert!(!bin(&paths).exists());
        assert!(!paths.home.join(format!("tmp/node-{VERSION}-test")).exists());
        assert!(old.exists());

        install(&paths, &url, "test", &sum).unwrap();
        assert!(bin(&paths).is_file());
        assert!(!old.exists(), "other versions go");
        assert!(!paths.home.join(format!("tmp/node-{VERSION}-test")).exists());
    }

    #[test]
    fn a_download_that_fails_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: d.path().join("home"),
        };
        let url = format!("file://{}/missing.tar.xz", d.path().display());
        let e = install(&paths, &url, "test", &"0".repeat(64)).unwrap_err();
        assert!(e.contains("downloading"), "{e}");
        assert!(!dir(&paths).exists());
    }

    #[tokio::test]
    async fn an_installed_node_isnt_downloaded_again() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: d.path().to_path_buf(),
        };
        std::fs::create_dir_all(bin(&paths).parent().unwrap()).unwrap();
        std::fs::write(bin(&paths), "").unwrap();
        assert_eq!(ensure(&paths).await.unwrap(), bin(&paths));
    }

    #[test]
    fn pins_are_sha256s() {
        let (_, sum) = build().expect("CI's machines have a build");
        assert!(sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(VERSION.starts_with('v'));
    }
}

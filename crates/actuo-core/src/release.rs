//! Releases of actuo ([`RELEASES_REPO`], `scripts/release.sh`): which is
//! the latest, whether a commit is one, and their downloads. Through `curl`, which
//! `install.sh` needs already. Actuo asks at start and once a day; a newer
//! release shows as `update` in `actuo/status`.

use crate::session::Host;
use std::sync::Arc;
use std::time::Duration;

/// How often the running actuo asks for the latest release.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The GitHub repo releases are published to: the one place it's written.
/// `scripts/release.sh` reads it from this line and writes it into `install.sh`.
pub const RELEASES_REPO: &str = "Ourfires/actuo";

/// Where releases are; `{base}/download/<commit>/<asset>` and
/// `{base}/latest/download/<asset>`.
pub fn base() -> String {
    // Tests point it at a folder (`file://`; debug builds only).
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("ACTUO_RELEASES_URL") {
        return url;
    }
    format!("https://github.com/{RELEASES_REPO}/releases")
}

/// This build's commit ([`crate::COMMIT`]).
pub fn own_commit() -> Option<String> {
    // Tests say which commit actuo is (debug builds only).
    #[cfg(debug_assertions)]
    if let Ok(commit) = std::env::var("ACTUO_TEST_COMMIT") {
        return Some(commit);
    }
    crate::COMMIT.map(str::to_string)
}

/// The release binary for this machine, as `install.sh` picks it.
pub fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("actuo-darwin-arm64"),
        ("linux", "x86_64") => Some("actuo-linux-x86_64"),
        ("linux", "aarch64") => Some("actuo-linux-arm64"),
        _ => None,
    }
}

/// A commit as releases name them: hex only (never `-dirty`), so it's safe in a
/// URL or a shell command.
pub fn is_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The latest release's commit.
pub async fn latest() -> Result<String, String> {
    let text = get(&format!("{}/latest/download/commit", base())).await?;
    let commit = text.trim();
    if !is_commit(commit) {
        return Err(format!("the latest release says {commit:?}, not a commit"));
    }
    Ok(commit.to_string())
}

/// Whether `commit` is a release with a binary for this machine.
pub async fn is_release(commit: &str) -> bool {
    match asset() {
        Some(asset) if is_commit(commit) => {
            get(&format!("{}/download/{commit}/{asset}.sha256", base()))
                .await
                .is_ok()
        }
        _ => false,
    }
}

/// `curl -fsSL`, 10 s at most; the body as text.
async fn get(url: &str) -> Result<String, String> {
    let out = crate::process::output(tokio::process::Command::new("curl").args([
        "-fsSL",
        "--max-time",
        "10",
        url,
    ]))
    .await
    .map_err(|e| format!("running curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A newer release than this actuo, if this actuo is a release.
pub async fn newer() -> Result<Option<String>, String> {
    let Some(own) = own_commit().filter(|c| is_commit(c)) else {
        return Ok(None);
    };
    let latest = latest().await?;
    if latest == own || !is_release(&own).await {
        return Ok(None);
    }
    Ok(Some(latest))
}

/// Asks for a newer release at start and every [`EVERY`], and keeps the answer on
/// the host for `actuo/status`. A failed question clears it.
pub async fn check(host: Arc<Host>) {
    loop {
        let found = match newer().await {
            Ok(found) => found,
            Err(e) => {
                crate::log!("couldn't ask for the latest release: {e}");
                None
            }
        };
        if let Some(commit) = &found
            && host.update().as_ref() != Some(commit)
        {
            crate::log!("release {commit} is out: `actuo update` installs it");
        }
        host.set_update(found);
        tokio::time::sleep(EVERY).await;
    }
}

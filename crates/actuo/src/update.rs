//! `actuo update`: installs the latest release as its `install.sh` would:
//! downloads this machine's binary into actuo's `tmp`, checks its sha256,
//! then runs its `service install` through your login shell, which waits
//! for what's in progress (`--now` doesn't), restarts actuo and puts the
//! previous one back if the new one doesn't answer. An actuo that isn't a release
//! (a local build) isn't replaced: the error gives the `install.sh` line.

use crate::client::Client;
use actuo_core::config::Paths;
use actuo_core::process::spawn_std;
use actuo_core::release;
use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::process::{Command, Stdio};

pub async fn update(paths: &Paths, now: bool) -> Result<()> {
    let Some(asset) = release::asset() else {
        bail!(
            "no actuo release for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let latest = release::latest()
        .await
        .map_err(|e| anyhow!("couldn't ask for the latest release: {e}"))?;
    // The running actuo's build, else this one's.
    let running = match Client::connect(&paths.socket()).await {
        Ok((c, _)) => c.actuo.and_then(|r| r.commit),
        Err(_) => None,
    };
    let current = running.or_else(release::own_commit);
    match current.as_deref() {
        Some(c) if c == latest => {
            out!("actuo is at the latest release ({latest})");
            return Ok(());
        }
        Some(c) if release::is_release(c).await => {}
        c => bail!(
            "actuo ({}) isn't a release; to replace it with release {latest}, run: \
             curl -fsSL {}/download/{latest}/install.sh | sh",
            c.unwrap_or("an unknown build"),
            release::base()
        ),
    }
    let dir = paths.home.join("tmp").join(format!("update-{latest}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let installed = install(&dir, asset, &latest, now);
    let _ = std::fs::remove_dir_all(&dir);
    installed?;
    out!("if this release changed the agents' pinned versions, also run: actuo agents install");
    Ok(())
}

/// Downloads release `commit`'s `asset` into `dir`, checks it and runs its
/// `service install`.
fn install(dir: &Path, asset: &str, commit: &str, now: bool) -> Result<()> {
    let url = format!("{}/download/{commit}/{asset}", release::base());
    let bin = dir.join("actuo");
    let sum = dir.join("actuo.sha256");
    out!("downloading {asset} ({commit})");
    release::download(&url, &bin).map_err(|e| anyhow!(e))?;
    release::download(&format!("{url}.sha256"), &sum).map_err(|e| anyhow!(e))?;
    let want = std::fs::read_to_string(&sum)?
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let got = release::sha256(&bin).map_err(|e| anyhow!(e))?;
    if got != want {
        bail!("the download's sha256 is {got}, the release says {want}");
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    // Through your login shell, so the service gets the PATH it sets up. The
    // binary's path reaches the shell as $0, never inside the command text.
    let command = if now {
        r#"exec "$0" service install --now"#
    } else {
        r#"exec "$0" service install"#
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = spawn_std(
        Command::new(&shell)
            .args(["-l", "-i", "-c", command])
            .arg(&bin)
            .stdin(Stdio::null()),
    )
    .and_then(|mut child| child.wait())
    .with_context(|| format!("running {shell}"))?;
    if !status.success() {
        bail!("the new actuo's `service install` failed ({status})");
    }
    Ok(())
}

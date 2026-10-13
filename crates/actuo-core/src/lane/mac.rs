//! What the lane does on macOS besides runs: keeps the Mac from idle-sleeping while a
//! run works (`caffeinate`), and posts a notification when a run pauses, opens a PR
//! or fails (your own comments on GitHub don't notify you). Nothing on Linux.

use actuo_protocol::ids::RunId;
use actuo_protocol::runs::{Run, RunState};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::sync::broadcast;

/// Follows every run's state until the lane's broadcast closes. Posts nothing while
/// a connection shows the notifications itself (`notifiers`).
pub async fn watch(mut runs: broadcast::Receiver<Run>, notifiers: super::Notifiers) {
    let mut states: HashMap<RunId, RunState> = HashMap::new();
    let mut awake: Option<tokio::process::Child> = None;
    loop {
        let run = match runs.recv().await {
            Ok(r) => r,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let before = states.insert(run.id.clone(), run.state);
        if let Some((title, text)) = notification(before, &run)
            && !notifiers.any()
        {
            notify(&title, &text);
        }
        let working = states
            .values()
            .any(|s| matches!(s, RunState::Running | RunState::Checking));
        match (&mut awake, working) {
            (None, true) => awake = caffeinate(),
            (Some(child), false) => {
                let _ = child.kill().await;
                awake = None;
            }
            _ => {}
        }
    }
    if let Some(mut child) = awake {
        let _ = child.kill().await;
    }
}

/// The notification for a run that just changed to waiting, a PR or failed: its
/// title and text. Only on a change, so a run saved again in the same state stays
/// quiet.
fn notification(before: Option<RunState>, run: &Run) -> Option<(String, String)> {
    if before == Some(run.state) {
        return None;
    }
    let reason = run.reason.as_deref().unwrap_or("");
    if reason == "secret_detected" {
        // The run's task posts this one, naming where (`notify_held`).
        return None;
    }
    let what = match run.state {
        RunState::Waiting => format!("Waiting for you ({reason})"),
        RunState::PrOpened => "Draft PR opened".to_string(),
        RunState::Failed => format!("Failed ({reason})"),
        _ => return None,
    };
    let title = format!("Actuo: {}#{}", run.repo, run.issue);
    let text = match &run.title {
        Some(t) => format!("{what}: {t}"),
        None => what,
    };
    Some((title, text))
}

/// The notification for a run that held text back (`secret_detected`): where it
/// was, never the text.
pub(super) fn notify_held(run: &Run, field: &str) {
    let title = format!("Actuo: {}#{}", run.repo, run.issue);
    notify(
        &title,
        &format!("Waiting for you (secret_detected): {field}"),
    );
}

/// `osascript` with the title and text as arguments, never inside the script: the
/// issue title is untrusted text.
fn notify(title: &str, text: &str) {
    let child = crate::process::spawn(
        tokio::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                title,
                text,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    match child {
        Ok(mut c) => {
            tokio::spawn(async move {
                let _ = c.wait().await;
            });
        }
        Err(e) => crate::log!("lane: notification: osascript: {e}"),
    }
}

/// `caffeinate -i -w <actuo pid>`: no idle sleep until it's killed or actuo
/// exits.
fn caffeinate() -> Option<tokio::process::Child> {
    crate::process::spawn(
        tokio::process::Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true),
    )
    .map_err(|e| crate::log!("lane: caffeinate: {e}"))
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use actuo_protocol::runs::RunStep;
    use actuo_protocol::types::Level;

    fn run(state: RunState, reason: Option<&str>) -> Run {
        Run {
            id: RunId::from("r1"),
            repo: "o/r".into(),
            issue: 7,
            issue_url: String::new(),
            title: Some("He said \"hi\" $(touch /tmp/x)".into()),
            agent: "claude".into(),
            level: Level::Edits,
            state,
            reason: reason.map(str::to_string),
            reason_text: None,
            step: RunStep::Turn,
            session_id: None,
            branch: "actuo/issue-7".into(),
            base: None,
            pr_url: None,
            started_by: "me".into(),
            started_via: Some(actuo_protocol::runs::StartedVia::Label),
            check_rounds: 0,
            created_at: String::new(),
            updated_at: String::new(),
            ended_at: None,
        }
    }

    #[test]
    fn notifications_on_changes_only() {
        let waiting = run(RunState::Waiting, Some("request"));
        assert_eq!(
            notification(Some(RunState::Running), &waiting),
            Some((
                "Actuo: o/r#7".into(),
                "Waiting for you (request): He said \"hi\" $(touch /tmp/x)".into()
            ))
        );
        assert_eq!(notification(Some(RunState::Waiting), &waiting), None);
        assert_eq!(notification(None, &run(RunState::Running, None)), None);
        let failed = notification(None, &run(RunState::Failed, Some("setup_failed")));
        assert!(failed.unwrap().1.starts_with("Failed (setup_failed): "));
        let pr = notification(Some(RunState::Running), &run(RunState::PrOpened, None));
        assert!(pr.unwrap().1.starts_with("Draft PR opened: "));
        // Posted by the run's task instead, with where the text was.
        let held = run(RunState::Waiting, Some("secret_detected"));
        assert_eq!(notification(Some(RunState::Running), &held), None);
    }
}

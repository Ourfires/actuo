//! Adding and removing projects: the access check, the clone, detection and the
//! proposal; `[projects]` written through the settings, as a hand edit would be.

use super::gh::{GhError, RepoInfo};
use super::{Lane, gh_error};
use crate::detect::{GitFiles, detect, propose};
use crate::session::HostResult;
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::methods::{ProjectInspectResult, RepoAccess};
use actuo_protocol::runs::RunState;
use actuo_protocol::settings::ProjectSettings;
use actuo_protocol::types::{Project, SessionState};
use serde_json::{Value, json};
use std::path::PathBuf;

impl Lane {
    /// `project/inspect`: the access check, the clone (or a fetch), what the default
    /// branch's files say and the proposal from it. Nothing saved.
    pub async fn inspect(&self, repo: &str) -> HostResult<ProjectInspectResult> {
        let (info, protected, accepted_invitation) = self.access(repo).await?;
        let clone = self.clone_or_fetch(repo).await?;
        let rev = format!("origin/{}", info.default_branch);
        let (detected, proposal) = tokio::task::spawn_blocking(move || {
            let files = GitFiles::new(&clone, &rev);
            let detected = detect(&files);
            let proposal = propose(&detected, &files);
            (detected, proposal)
        })
        .await
        .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?;
        Ok(ProjectInspectResult {
            access: RepoAccess {
                permission: info.permission,
                visibility: info.visibility,
                default_branch: info.default_branch,
                protected,
            },
            proposal,
            detected,
            existing: self.host.settings().projects.get(repo).cloned(),
            accepted_invitation,
        })
    }

    /// `project/add`: the access check and the clone again, then
    /// `[projects."owner/repo"]` with `lane` and what differs from the defaults (a
    /// project starts with its lane off unless the settings say on).
    /// Returns its clone as a project, and whether a pending invitation was accepted.
    pub async fn add_project(
        &self,
        repo: &str,
        settings: &ProjectSettings,
    ) -> HostResult<(Project, bool)> {
        if self.host.settings().projects.contains_key(repo) {
            return Err(RpcError::new(
                ErrorKind::ProjectExists,
                format!("{repo} is a project here already; change it in its settings"),
            ));
        }
        let (_, _, accepted) = self.access(repo).await?;
        let clone = self.clone_or_fetch(repo).await?;
        let mut value = serde_json::to_value(settings)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?;
        let defaults = serde_json::to_value(ProjectSettings::default()).expect("serializes");
        if let (Value::Object(v), Value::Object(d)) = (&mut value, &defaults) {
            // `lane` always, so the file says whether its runs start.
            v.retain(|k, x| k == "lane" || d.get(k) != Some(x));
        }
        self.host
            .update_settings(json!({"projects": {repo: value}}))
            .await?;
        let project = self.host.open_project(&clone.to_string_lossy()).await?;
        Ok((self.host.with_repo(project), accepted))
    }

    /// `project/remove`: out of the settings; runs and sessions untouched. Refused
    /// while a run of it works; deleting the clone also while a run of it hasn't
    /// ended (its worktree uses the clone).
    pub async fn remove_project(&self, repo: &str, delete_clone: bool) -> HostResult<()> {
        if !self.host.settings().projects.contains_key(repo) {
            return Err(RpcError::new(
                ErrorKind::ProjectNotFound,
                format!("{repo} isn't a project here"),
            ));
        }
        let r = repo.to_string();
        let runs = self.host.db(move |s| s.runs(Some(&r), None)).await?;
        if runs
            .iter()
            .any(|run| matches!(run.state, RunState::Running | RunState::Checking))
        {
            return Err(RpcError::new(
                ErrorKind::ProjectBusy,
                format!("a run of {repo} is working; cancel it or wait"),
            ));
        }
        if delete_clone && runs.iter().any(|run| !run.state.ended()) {
            return Err(RpcError::new(
                ErrorKind::ProjectBusy,
                format!(
                    "a run of {repo} hasn't ended and its worktree uses the clone; end it, or keep the clone"
                ),
            ));
        }
        if delete_clone {
            // Sessions started by hand in the clone: their worktrees use it too.
            let clone = self.host.paths().lane_clone(repo);
            let open = self
                .host
                .db(move |s| {
                    let ids: Vec<_> = s
                        .projects()?
                        .into_iter()
                        .filter(|p| std::path::Path::new(&p.root) == clone)
                        .map(|p| p.id)
                        .collect();
                    Ok(s.sessions(None)?
                        .into_iter()
                        .any(|x| ids.contains(&x.project_id) && x.state != SessionState::Ended))
                })
                .await?;
            if open {
                return Err(RpcError::new(
                    ErrorKind::ProjectBusy,
                    format!(
                        "a session in {repo}'s clone hasn't ended and its worktree uses the clone; stop it, or keep the clone"
                    ),
                ));
            }
        }
        self.host
            .update_settings(json!({"projects": {repo: null}}))
            .await?;
        if delete_clone {
            let lock = self.repo_lock(repo);
            let _held = lock.lock().await;
            let clone = self.host.paths().lane_clone(repo);
            if clone.starts_with(self.host.paths().home.join("repos")) {
                let _ = tokio::fs::remove_dir_all(&clone).await;
            }
        }
        Ok(())
    }

    /// [`Self::push_access`], after accepting the gh login's pending invitation to
    /// `repo` when it can't see or push to it otherwise; with whether one was
    /// accepted. Only an invitation to this repo: it's what you're adding.
    async fn access(&self, repo: &str) -> HostResult<(RepoInfo, Option<bool>, bool)> {
        let refused = match self.push_access(repo).await {
            Ok((info, protected)) => return Ok((info, protected, false)),
            Err(e) => e,
        };
        if !matches!(
            refused.kind(),
            Some(ErrorKind::RepoNotFound | ErrorKind::NoPushAccess)
        ) {
            return Err(refused);
        }
        let Ok(Some(id)) = self.gh.invitation_to(repo).await else {
            // Who to give access to, so a refusal says what to do.
            return Err(match self.gh_login().await {
                Some(login) => RpcError {
                    message: format!(
                        "{}. Give {login} write access to {repo}; the next try accepts the invitation",
                        refused.message
                    ),
                    ..refused
                },
                None => refused,
            });
        };
        self.gh.accept_invitation(id).await.map_err(gh_error)?;
        crate::log!("lane: accepted the invitation to {repo}");
        let (info, protected) = self.push_access(repo).await?;
        Ok((info, protected, true))
    }

    /// Actuo's gh login can push to `repo` (and it isn't archived); with
    /// whether its default branch is protected.
    async fn push_access(&self, repo: &str) -> HostResult<(RepoInfo, Option<bool>)> {
        if !crate::settings::is_repo_name(repo) {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                format!("{repo}: expected owner/repo"),
            ));
        }
        let info = self.gh.repo_access(repo).await.map_err(|e| match &e {
            GhError::Failed { stderr, .. } if stderr.contains("Could not resolve") => {
                RpcError::new(
                    ErrorKind::RepoNotFound,
                    format!("{repo}: GitHub has no such repo, or actuo's gh login can't see it"),
                )
            }
            _ => gh_error(e),
        })?;
        if info.archived {
            return Err(RpcError::new(
                ErrorKind::NoPushAccess,
                format!("{repo} is archived"),
            ));
        }
        if !matches!(info.permission.as_str(), "WRITE" | "MAINTAIN" | "ADMIN") {
            return Err(RpcError::new(
                ErrorKind::NoPushAccess,
                format!(
                    "actuo's gh login has {} on {repo}; it needs WRITE to push its branches",
                    if info.permission.is_empty() {
                        "no access"
                    } else {
                        &info.permission
                    }
                ),
            ));
        }
        let protected = self.gh.branch_protected(repo, &info.default_branch).await;
        Ok((info, protected))
    }

    /// Actuo's clone of `repo`: cloned (hooks off) or fetched, as a run's
    /// prepare step does, under the repo's lock.
    pub(super) async fn clone_or_fetch(&self, repo: &str) -> HostResult<PathBuf> {
        let lock = self.repo_lock(repo);
        let _held = lock.lock().await;
        let clone = self.host.paths().lane_clone(repo);
        if clone.join(".git").exists() {
            let c = clone.clone();
            tokio::task::spawn_blocking(move || super::git::fetch(&c))
                .await
                .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string()))?
                .map_err(|e| RpcError::new(ErrorKind::GitError, e.to_string()))?;
        } else {
            self.gh.clone_repo(repo, &clone).await.map_err(gh_error)?;
        }
        Ok(clone)
    }
}

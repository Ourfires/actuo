//! Check runs (`project/check`): a project's setup, then each check, once, in the
//! sandbox, on a fresh worktree of its default branch, with no agent; each step's
//! time, exit code, output tail and peak memory, and the install's disk use.
//! "Affected" means everything here: `TURBO_SCM_BASE` is the repo's first commit.

use super::steps::{CommandOutcome, Srt, run, sandbox_for};
use super::{Lane, RunConfig, repo_agent};
use crate::session::HostResult;
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::ids::CheckRunId;
use actuo_protocol::runs::{CheckRun, CheckRunState, CheckStep, RunStepKind};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

impl Lane {
    /// `check.state` notifications.
    pub fn check_subscribe(&self) -> broadcast::Receiver<CheckRun> {
        self.checks_tx.subscribe()
    }

    /// `project/check`: starts a check run of `repo`'s default branch, one at a time
    /// per project.
    pub async fn check_project(self: &Arc<Self>, repo: &str) -> HostResult<CheckRun> {
        self.start_check(repo, None).await
    }

    /// Starts a check run of `repo` at `commit` (a lane run's baseline: the commit it
    /// started from). Refused like `project/check` while one is running.
    pub async fn check_commit(self: &Arc<Self>, repo: &str, commit: &str) -> HostResult<CheckRun> {
        self.start_check(repo, Some(commit.to_string())).await
    }

    async fn start_check(self: &Arc<Self>, repo: &str, at: Option<String>) -> HostResult<CheckRun> {
        if !self.host.settings().projects.contains_key(repo) {
            return Err(RpcError::new(
                ErrorKind::ProjectNotFound,
                format!("{repo} isn't a project here"),
            ));
        }
        if self.host.held() {
            return Err(RpcError::new(
                ErrorKind::ProjectBusy,
                "actuo is about to restart (an update); start the check run after",
            ));
        }
        let r = repo.to_string();
        let last = self.host.db(move |s| s.last_check_run(&r)).await?;
        if last.is_some_and(|l| l.state == CheckRunState::Running) {
            return Err(RpcError::new(
                ErrorKind::ProjectBusy,
                format!("a check run of {repo} is running"),
            ));
        }
        let check = CheckRun {
            id: CheckRunId(uuid::Uuid::now_v7().to_string()),
            repo: repo.to_string(),
            state: CheckRunState::Running,
            commit: None,
            error: None,
            steps: Vec::new(),
            node_modules_kb: None,
            store_kb_before: None,
            store_kb_after: None,
            started_at: crate::time::now(),
            ended_at: None,
        };
        self.save_check(&check).await;
        tokio::spawn(self.clone().check_task(check.clone(), at));
        Ok(check)
    }

    async fn save_check(&self, check: &CheckRun) {
        let c = check.clone();
        if let Err(e) = self.host.db(move |s| s.save_check_run(&c)).await {
            crate::log!("check {}: saving: {}", check.id, e.message);
        }
        let _ = self.checks_tx.send(check.clone());
    }

    async fn check_task(self: Arc<Self>, mut check: CheckRun, at: Option<String>) {
        let name = format!("check-{}", check.id);
        let worktree = self.host.paths().home.join("checks").join(&check.id.0);
        let result = self.check_steps(&mut check, &name, &worktree, at).await;
        check.state = match result {
            Err(e) => {
                check.error = Some(e);
                CheckRunState::Error
            }
            Ok(()) if check.steps.iter().all(|s| s.exit_code == Some(0)) => CheckRunState::Passed,
            Ok(()) => CheckRunState::Failed,
        };
        check.ended_at = Some(crate::time::now());
        self.save_check(&check).await;
        // The worktree and the step's sandbox files go; the logs stay.
        let clone = self.host.paths().lane_clone(&check.repo);
        let (paths, wt) = (self.host.paths().clone(), worktree.clone());
        let _ = tokio::task::spawn_blocking(move || {
            let _ = crate::git::worktree_remove(&clone, &wt);
            let _ = crate::git::worktree_prune(&clone);
            let _ = std::fs::remove_dir_all(&wt);
            let _ = std::fs::remove_dir_all(crate::sandbox::temp_path(&paths, &name));
            for kind in ["setup", "checks"] {
                let _ = std::fs::remove_file(
                    paths
                        .home
                        .join("sandbox")
                        .join(format!("{name}-{kind}.json")),
                );
            }
        })
        .await;
    }

    async fn check_steps(
        &self,
        check: &mut CheckRun,
        name: &str,
        worktree: &Path,
        at: Option<String>,
    ) -> Result<(), String> {
        let repo = check.repo.clone();
        let s = self.host.settings();
        let mut cfg = RunConfig::new(&s, &repo, &repo_agent(&s, &repo), "")
            .ok_or_else(|| format!("{repo} isn't a project here"))?;
        self.host.sandbox_ready().await.map_err(|e| e.message)?;
        let clone = self.clone_or_fetch(&repo).await.map_err(|e| e.message)?;
        // Its clone as a project, so `project/list` shows this check run.
        self.host
            .open_project(&clone.to_string_lossy())
            .await
            .map_err(|e| e.message)?;
        // The default branch's head, unless a commit was asked for.
        let base = match at {
            Some(commit) => commit,
            None => {
                let branch = self
                    .gh
                    .default_branch(&repo)
                    .await
                    .map_err(|e| e.to_string())?;
                format!("origin/{branch}")
            }
        };
        let (c, b, wt) = (clone.clone(), base.clone(), worktree.to_path_buf());
        let (commit, first) =
            tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
                let git = |args: &[&str]| crate::git::git(&c, args, &[]).map_err(|e| e.to_string());
                let commit = git(&["rev-parse", &b])?.trim().to_string();
                let roots = git(&["rev-list", "--max-parents=0", &b])?;
                let first = roots.lines().last().unwrap_or_default().to_string();
                std::fs::create_dir_all(wt.parent().expect("a parent"))
                    .map_err(|e| e.to_string())?;
                git(&["worktree", "add", "--detach", &wt.to_string_lossy(), &b])?;
                Ok((commit, first))
            })
            .await
            .map_err(|e| e.to_string())??;
        check.commit = Some(commit);
        self.save_check(check).await;
        // What the project sets, else what the default branch says.
        let r = crate::detect::resolve(
            cfg.setup.as_ref(),
            cfg.checks.as_ref(),
            cfg.network.as_ref(),
            &crate::detect::DirFiles {
                root: worktree.to_path_buf(),
            },
        );
        cfg.setup = Some(r.setup);
        cfg.checks = Some(r.checks);
        cfg.network = Some(r.network);

        let store = self.host.paths().home.join("stores").join(&repo);
        check.store_kb_before = disk_kb(&store).await;
        let tails = (
            cfg.output_tail_lines as usize,
            cfg.output_tail_bytes as usize,
        );
        for command in cfg.setup.as_deref().unwrap_or_default() {
            let step = self
                .check_step(
                    check,
                    name,
                    worktree,
                    &cfg,
                    RunStepKind::Setup,
                    command,
                    Duration::from_secs(cfg.setup_time_limit_s),
                    None,
                    tails,
                    cfg.checks_unix_sockets,
                )
                .await?
                .0;
            check.steps.push(step);
            self.save_check(check).await;
            if check.steps.last().is_some_and(|s| s.exit_code != Some(0)) {
                return Ok(());
            }
        }
        check.node_modules_kb = disk_kb(&worktree.join("node_modules")).await;
        check.store_kb_after = disk_kb(&store).await;
        let limit = Duration::from_secs(cfg.check_time_limit_s * 3);
        for command in cfg.checks.as_deref().unwrap_or_default() {
            let step = self
                .check_step(
                    check,
                    name,
                    worktree,
                    &cfg,
                    RunStepKind::Check,
                    command,
                    limit,
                    Some(first.clone()),
                    tails,
                    cfg.checks_unix_sockets,
                )
                .await?;
            let (mut step, output) = step;
            // A failing check actuo can't explain runs once more with Unix
            // sockets allowed (still sandboxed otherwise): if fewer packages fail, or
            // none, the sockets are why. Only here (the default branch, no agent's
            // change), never in a lane run.
            // A refused host alone isn't taken as the cause here: it may be a tool's
            // update check, refused in passing.
            let unexplained = step
                .diagnosis
                .as_ref()
                .is_none_or(|d| d.kind == actuo_protocol::runs::DiagnosisKind::Network);
            if step.exit_code != Some(0)
                && !step.timed_out
                && unexplained
                && !cfg.checks_unix_sockets
            {
                check.steps.push(step.clone());
                self.save_check(check).await;
                check.steps.pop();
                let (again, again_output) = self
                    .check_step(
                        check,
                        name,
                        worktree,
                        &cfg,
                        RunStepKind::Check,
                        command,
                        limit,
                        Some(first.clone()),
                        tails,
                        true,
                    )
                    .await?;
                let before = super::diagnose::turbo_failed(&output);
                let after = super::diagnose::turbo_failed(&again_output);
                let fixed: Vec<String> = if again.exit_code == Some(0) {
                    before.clone()
                } else {
                    before
                        .iter()
                        .filter(|p| !after.contains(p))
                        .cloned()
                        .collect()
                };
                if again.exit_code == Some(0) || !fixed.is_empty() {
                    let mut d = super::diagnose::unix_socket(
                        &check.repo,
                        None,
                        &fixed,
                        output.contains("tsx"),
                    );
                    d.detail = format!(
                        "{}; with Unix sockets allowed it {} in {} s",
                        d.detail,
                        if again.exit_code == Some(0) {
                            "passed"
                        } else {
                            "failed in fewer packages"
                        },
                        again.duration_ms / 1000
                    );
                    step.diagnosis = Some(d);
                }
            }
            check.steps.push(step);
            self.save_check(check).await;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_step(
        &self,
        check: &CheckRun,
        name: &str,
        worktree: &Path,
        cfg: &RunConfig,
        kind: RunStepKind,
        command: &str,
        limit: Duration,
        scm_base: Option<String>,
        (tail_lines, tail_bytes): (usize, usize),
        unix_sockets: bool,
    ) -> Result<(CheckStep, String), String> {
        let n = check.steps.len() as u32 + 1;
        let paths = self.host.paths().clone();
        let (nm, wt, repo, network, read) = (
            name.to_string(),
            worktree.to_path_buf(),
            check.repo.clone(),
            cfg.network.clone().unwrap_or_default(),
            cfg.read.clone(),
        );
        let setup = kind == RunStepKind::Setup;
        let local_ports = cfg.checks_local_ports.clone();
        let local_listen_ports = cfg.checks_local_listen_ports.clone();
        let npmrc = cfg.setup_reads_npmrc;
        let (settings, env) = tokio::task::spawn_blocking(move || {
            sandbox_for(
                &paths,
                &nm,
                &wt,
                &repo,
                &network,
                &read,
                setup,
                scm_base,
                unix_sockets,
                &local_ports,
                &local_listen_ports,
                npmrc,
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let paths = self.host.paths();
        let srt = crate::sandbox::srt(paths);
        let node = crate::node::bin(paths);
        let node_options =
            crate::sandbox::records_node_options(paths).map_err(|e| e.to_string())?;
        let kind_name = if setup { "setup" } else { "check" };
        let log = paths.logs().join("checks").join(&check.id.0).join(format!(
            "{n}-{kind_name}{}.log",
            if unix_sockets && !cfg.checks_unix_sockets {
                "-sockets"
            } else {
                ""
            }
        ));
        let outcome = run(
            command,
            worktree,
            &env,
            Some(Srt {
                node: &node,
                program: &srt,
                settings: &settings,
                node_options: &node_options,
                refusal_log: None,
                label: format!("check run {} {kind_name} `{command}`", check.id),
                local_ports: if setup { &[] } else { &cfg.checks_local_ports },
                local_listen_ports: if setup {
                    &[]
                } else {
                    &cfg.checks_local_listen_ports
                },
            }),
            limit,
            &log,
            tail_lines,
            tail_bytes,
            // A check run has no cancel; actuo stopping drops it.
            std::future::pending(),
        )
        .await
        .unwrap_or_else(|e| CommandOutcome {
            exit_code: None,
            timed_out: false,
            stopped: false,
            tail: format!("couldn't run it: {e}"),
            duration: Duration::ZERO,
            peak_kb: None,
            peak_process_kb: None,
            refusals: Vec::new(),
        });
        // Why it failed, from srt's refusals and the whole output.
        let output = tokio::fs::read_to_string(&log).await.unwrap_or_default();
        let diagnosis = if outcome.passed() {
            None
        } else {
            let tmp = crate::sandbox::temp_path(paths, name);
            super::diagnose::diagnose(&super::diagnose::Seen {
                repo: &check.repo,
                setup,
                output: &output,
                refusals: &outcome.refusals,
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                limit_s: limit.as_secs(),
                peak_kb: outcome.peak_kb,
                worktree,
                tmp: &tmp,
            })
        };
        let failed = super::diagnose::turbo_failed(&output);
        let failed_packages = (!outcome.passed() && !failed.is_empty()).then_some(failed);
        Ok((
            CheckStep {
                n,
                kind,
                command: command.to_string(),
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                output_tail: Some(outcome.tail),
                duration_ms: outcome.duration.as_millis() as u64,
                peak_kb: outcome.peak_kb,
                peak_process_kb: outcome.peak_process_kb,
                diagnosis,
                failed_packages,
            },
            output,
        ))
    }
}

/// A folder's disk use in KB (`du -sk`); `None` if it isn't there.
async fn disk_kb(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let out =
            crate::process::output_std(std::process::Command::new("du").arg("-sk").arg(&p)).ok()?;
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
    .await
    .ok()
    .flatten()
}

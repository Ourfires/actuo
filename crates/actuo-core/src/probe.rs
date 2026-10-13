//! `agent/probe`: what an agent offers, model by model. The agent is started as a
//! session would start it (as the lane does, sandboxed with its login, when a
//! project with its lane on uses it), without a prompt; each model is set in turn
//! and the whole option list the agent returns is kept (effort levels and fast mode
//! depend on the model). No prompt is sent, so no tokens are spent.

use crate::agent::AgentHandle;
use crate::config::SessionOptions;
use crate::session::{Host, HostResult, Launch, agent_spec, setup_error};
use crate::store::AgentOptions;
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::ids::SessionId;
use actuo_protocol::types::{AgentConfigOption, ConfigValue, SandboxOptions};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a probe may take, start to stop.
const LIMIT: Duration = Duration::from_secs(60);

/// How long the start-up probe waits for the lane to have no run going.
const WAIT: Duration = Duration::from_secs(600);

/// A session that shows other options doesn't probe again sooner than this.
const AGAIN: Duration = Duration::from_secs(600);

/// Asks `agent` and records what it offers. The last probe's results stay if it
/// fails.
pub async fn probe(host: &Arc<Host>, agent: &str) -> HostResult<AgentOptions> {
    let config = host.agents.agents.get(agent).cloned().ok_or_else(|| {
        RpcError::new(ErrorKind::AgentNotFound, format!("no agent named {agent}"))
    })?;
    let _one = Probing::start(host, agent)?;
    if let (Some(dir), Some(bin)) = (config.install_dir(host.paths()), &config.bin)
        && !dir.join("node_modules/.bin").join(bin).exists()
    {
        return Err(RpcError::new(
            ErrorKind::AgentError,
            format!("{agent} isn't installed yet: actuo agents install"),
        ));
    }
    let s = host.settings();
    let lane = s.lane.enabled
        && s.projects
            .iter()
            .any(|(repo, p)| p.lane && crate::lane::repo_agent(&s, repo) == agent);
    let opts = if lane {
        SessionOptions {
            clean: true,
            sandboxed: true,
            web_tools: s.lane.web_tools,
            telemetry: s.lane.telemetry,
        }
    } else {
        SessionOptions::plain()
    };
    // A folder of its own, a git repo (the sandbox reads the repo's git dir).
    let name = format!("probe-{agent}");
    let cwd = host.paths().home.join("tmp").join(format!("{name}-cwd"));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd)
        .map_err(|e| RpcError::new(ErrorKind::Internal, format!("{}: {e}", cwd.display())))?;
    let init = crate::process::output_std(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&cwd),
    );
    if !init.as_ref().is_ok_and(|o| o.status.success()) {
        return Err(RpcError::new(ErrorKind::GitError, "git init failed"));
    }
    let sandbox = lane.then(SandboxOptions::default);
    let result = async {
        let spec = agent_spec(Launch {
            host,
            agent_config: &config,
            opts,
            sandbox: sandbox.as_ref(),
            name: &name,
            cwd: &cwd,
            stderr_log: Some(host.paths().logs().join(format!("{name}.log"))),
            records: format!("probe {agent}"),
        })
        .await
        .map_err(|e| RpcError::new(e.0, e.1))?;
        let (handle, _events) = host
            .backend
            .spawn(spec)
            .map_err(|e| RpcError::new(ErrorKind::AgentError, e.to_string()))?;
        let asked = tokio::time::timeout(LIMIT, ask(&handle, config.meta_for(opts))).await;
        handle.shutdown().await;
        match asked {
            Ok(Ok(found)) => Ok(found),
            Ok(Err(e)) => {
                let e = setup_error(e);
                Err(RpcError::new(e.0, e.1))
            }
            Err(_) => Err(RpcError::new(
                ErrorKind::AgentError,
                format!("{agent} didn't answer within {} s", LIMIT.as_secs()),
            )),
        }
    }
    .await;
    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(crate::sandbox::temp_path(host.paths(), &name));
    let _ = std::fs::remove_file(
        host.paths()
            .home
            .join("sandbox")
            .join(format!("{name}.json")),
    );
    let (own, by_model) = result?;
    let found = AgentOptions {
        version: config.version.clone(),
        own,
        by_model,
        probed_at: crate::time::now(),
    };
    let (store_agent, store_found) = (agent.to_string(), found.clone());
    host.db(move |s| s.set_agent_options(&store_agent, &store_found))
        .await?;
    Ok(found)
}

/// The agent's own model's options, then each model's.
async fn ask(
    agent: &AgentHandle,
    meta: Option<serde_json::Value>,
) -> crate::agent::Result<(
    Vec<AgentConfigOption>,
    BTreeMap<String, Vec<AgentConfigOption>>,
)> {
    agent.initialize().await?;
    let setup = agent.new_session(meta).await?;
    let own = setup.config_options;
    let mut by_model = BTreeMap::new();
    if let Some(model) = own.iter().find(|o| o.id == "model") {
        for choice in &model.values {
            let after = agent
                .set_config("model", ConfigValue::String(choice.value.clone()))
                .await?;
            by_model.insert(choice.value.clone(), clean(after));
        }
    }
    Ok((clean(own), by_model))
}

/// Without current values (they were this probe's) and without `mode` (the level
/// sets it).
fn clean(mut options: Vec<AgentConfigOption>) -> Vec<AgentConfigOption> {
    options.retain(|o| o.id != "mode");
    for o in &mut options {
        o.current_value = None;
    }
    options
}

/// In the background, at actuo start and after each settings change: each agent
/// in use (actuo's default, a project's) with a pinned adapter
/// version that hasn't been probed at that version (so an adapter update, or an
/// agent newly in use, probes). An agent run from a command of its own (no pinned
/// version) is probed only on request. It waits while the lane has a run going (a
/// probe uses the same login as its runs), at most [`WAIT`]: a run waiting for you
/// can last days.
pub async fn in_use(host: Arc<Host>) {
    let s = host.settings();
    let mut agents: Vec<String> = std::iter::once(s.sessions.agent.clone())
        .chain(s.projects.values().filter_map(|r| r.agent.clone()))
        .filter(|a| {
            host.agents
                .agents
                .get(a)
                .is_some_and(|c| c.version.is_some())
        })
        .collect();
    agents.sort();
    agents.dedup();
    for agent in agents {
        let version = host.agents.agents[&agent].version.clone();
        let name = agent.clone();
        let known = host
            .db(move |s| s.agent_options(&name))
            .await
            .ok()
            .flatten();
        if known.is_some_and(|k| k.version == version) {
            continue;
        }
        let waited = tokio::time::Instant::now();
        while host.lane().is_ok_and(|l| l.busy()) && waited.elapsed() < WAIT {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        match probe(&host, &agent).await {
            Ok(found) => crate::log!("probe: {agent}: {} models", found.by_model.len()),
            Err(e) => crate::log!("probe: {agent}: {}", e.message),
        }
    }
}

/// After a session's agent started: if what it offers differs from what the last
/// probe found for that model (a login or subscription change, a new model), probe
/// again in the background. Only for pinned agents, only for a session started the
/// way the probe starts it (a different login can offer other models), and not
/// within [`AGAIN`] of the last probe.
pub async fn check(
    host: Arc<Host>,
    session: SessionId,
    sandboxed: bool,
    seen: Vec<AgentConfigOption>,
) {
    let sid = session.clone();
    let Ok(summary) = host.db(move |s| s.session(&sid)).await else {
        return;
    };
    let agent = summary.agent;
    let Some(config) = host.agents.agents.get(&agent) else {
        return;
    };
    if config.version.is_none() || sandboxed != lane_style(&host, &agent) {
        return;
    }
    let name = agent.clone();
    let Ok(known) = host.db(move |s| s.agent_options(&name)).await else {
        return;
    };
    let model = seen
        .iter()
        .find(|o| o.id == "model")
        .and_then(|o| o.current_value.clone());
    if let Some(known) = &known {
        let expected = match &model {
            Some(ConfigValue::String(m)) => known.by_model.get(m),
            _ => Some(&known.own),
        };
        if expected == Some(&clean(seen)) {
            return;
        }
        // Timestamps in one format sort as text.
        let since = crate::time::format(::time::OffsetDateTime::now_utc() - AGAIN);
        let recent = known.probed_at > since;
        if recent {
            return;
        }
    }
    match probe(&host, &agent).await {
        Ok(found) => crate::log!(
            "probe: {agent} offered something new in a session; probed again: {} models",
            found.by_model.len()
        ),
        Err(e) => crate::log!("probe: {agent}: {}", e.message),
    }
}

/// Whether the probe starts `agent` as the lane does (sandboxed, its login).
fn lane_style(host: &Host, agent: &str) -> bool {
    let s = host.settings();
    s.lane.enabled
        && s.projects
            .iter()
            .any(|(repo, p)| p.lane && crate::lane::repo_agent(&s, repo) == agent)
}

/// Marks an agent as being probed until dropped.
struct Probing<'a> {
    host: &'a Host,
    agent: String,
}

impl<'a> Probing<'a> {
    fn start(host: &'a Host, agent: &str) -> HostResult<Self> {
        if !host.probing.lock().unwrap().insert(agent.to_string()) {
            return Err(RpcError::new(
                ErrorKind::InvalidParams,
                format!("{agent} is being probed already"),
            ));
        }
        Ok(Self {
            host,
            agent: agent.to_string(),
        })
    }
}

impl Drop for Probing<'_> {
    fn drop(&mut self) {
        self.host.probing.lock().unwrap().remove(&self.agent);
    }
}

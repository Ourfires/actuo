//! `actuo serve` runs actuo in the foreground; the other commands are
//! clients of it over its Unix socket (`~/.actuo/actuo.sock`, or under
//! `ACTUO_HOME`).

/// `println!` that ends the program quietly (exit 0) when stdout is gone, as in
/// `actuo runs | head -1`, the way other command-line tools do; `println!` panics.
/// `out!(no_newline; …)` is `print!`.
macro_rules! out {
    (no_newline; $($arg:tt)*) => {
        $crate::stdout_or_exit(format_args!($($arg)*), false)
    };
    ($($arg:tt)*) => {
        $crate::stdout_or_exit(format_args!($($arg)*), true)
    };
}

mod client;
#[cfg(test)]
mod coverage;
mod service;
mod settings_keys;
mod show;
mod update;

/// Writes to stdout; see [`out!`].
fn stdout_or_exit(args: std::fmt::Arguments<'_>, newline: bool) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let written = stdout.write_fmt(args).and_then(|()| {
        if newline {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()
    });
    exit_if_stdout_closed(written);
}

/// Ends the program quietly (exit 0) when a write failed because nothing reads
/// stdout any more; other write errors go back to the caller.
fn exit_if_stdout_closed(written: std::io::Result<()>) {
    if let Err(e) = written
        && e.kind() == std::io::ErrorKind::BrokenPipe
    {
        std::process::exit(0);
    }
}

use actuo_core::agent::acp::AcpBackend;
use actuo_core::config::{AgentsConfig, Paths};
use actuo_core::lane::Lane;
use actuo_core::lane::gh::Gh;
use actuo_core::session::Host;
use actuo_core::settings;
use actuo_core::store::Store;
use actuo_protocol::events::{Event, EventBody};
use actuo_protocol::ids::{PermissionOptionId, RequestId, SessionId};
use actuo_protocol::methods::*;
use actuo_protocol::types::{
    AgentConfigOption, ConfigValue, Level, PermissionOption, SessionState, SessionSummary,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::{Client, RunNotice};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "actuo",
    about = "Hosts agent sessions and drives them",
    version = actuo_core::version()
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run actuo in the foreground until Ctrl-C or SIGTERM.
    Serve,
    /// Join stdin and stdout to the running actuo's socket until either side
    /// closes: how an actuo on another machine is reached over SSH (`ssh <host>
    /// actuo stdio`).
    Stdio,
    /// List the configured agents; `install` installs their pinned adapters and the
    /// sandbox (srt) into actuo's home.
    Agents {
        #[command(subcommand)]
        command: Option<AgentsCmd>,
        /// Actuo's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Log AGENT in to the config dir its sandboxed sessions use (the lane's login),
    /// inside the sandbox; the login page opens in your browser. Doesn't need
    /// actuo running.
    Login { agent: String },
    /// Start a session on the repository at PATH. Prints its id.
    Start {
        path: PathBuf,
        /// Default: the settings' sessions.agent.
        #[arg(long)]
        agent: Option<String>,
        /// Work in the project checkout instead of a new worktree.
        #[arg(long)]
        no_worktree: bool,
        /// What a new worktree branch starts from (default HEAD).
        #[arg(long)]
        base: Option<String>,
        /// The worktree's branch (default actuo/<session id>); an existing worktree
        /// on it is reused.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, value_enum, default_value = "ask")]
        level: LevelArg,
        /// Start the agent without the user's global agent config.
        #[arg(long)]
        clean: bool,
        /// An agent config option, e.g. --config model=sonnet (repeatable).
        #[arg(long = "config", value_name = "ID=VALUE")]
        config: Vec<String>,
        /// A first prompt.
        #[arg(long)]
        prompt: Option<String>,
        /// Attach right away.
        #[arg(long)]
        attach: bool,
    },
    /// List sessions.
    Sessions {
        /// Actuo's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// A session's details as JSON; or, given a run's id, the run with its steps
    /// and state changes.
    Show { session: String },
    /// Stream a session's events (its history first). While a permission request
    /// is open, a line answers it (number, optionId, or c to cancel); otherwise a
    /// line is sent as a prompt. /interrupt, /stop and /quit are commands.
    Attach {
        session: String,
        /// Replay from after this event (0: everything).
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Send a prompt.
    Send {
        session: String,
        text: String,
        /// Interrupt a running turn first (its open requests are cancelled) and send
        /// when it has ended: takes a waiting lane run over in one step.
        #[arg(long)]
        interrupt: bool,
    },
    /// Answer a permission request with one of its options, or cancel it.
    Respond {
        session: String,
        request: String,
        option: Option<String>,
        #[arg(long)]
        cancel: bool,
    },
    /// Cancel the running turn.
    Interrupt { session: String },
    /// Restore the files to how they were before TURN.
    Revert {
        session: String,
        turn: u32,
        /// Needed when the session works in the project checkout.
        #[arg(long)]
        force: bool,
    },
    /// End a session and its agent. The worktree is kept.
    Stop { session: String },
    /// A session's changes as a unified diff, against where it started (the merge
    /// base with its base branch, or its first checkpoint); given a run's id, its
    /// session's.
    Diff { session: String },
    /// A session's checkpoints, one line each: 0 is before turn 1, n after turn n,
    /// with the files it changed. `revert SESSION TURN` goes back to checkpoint
    /// TURN - 1. Given a run's id, its session's.
    Checkpoints {
        session: String,
        /// Actuo's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run `actuo serve` as a service of your user (a launch agent on macOS, a
    /// systemd user unit on Linux): install copies this binary to actuo's home
    /// and starts it; uninstall stops and removes it.
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
    /// Install the latest release if actuo is an older one: download, check
    /// its sha256, then its `service install` (waits for what's in progress, as
    /// restart does).
    Update {
        /// Don't wait for what's in progress.
        #[arg(long)]
        now: bool,
    },
    /// Restart the installed service: first waits up to 30 min for what's in
    /// progress (lane runs, check runs, turns), then waits for actuo to answer.
    Restart {
        /// Don't wait for what's in progress.
        #[arg(long)]
        now: bool,
    },
    /// Run the issue-to-PR lane on an issue by URL, without the label check (prints
    /// the run's id); or follow a run (watch), hand it back (resume) or end it (cancel).
    Run(RunArgs),
    /// List the lane's runs, newest first.
    Runs {
        /// Actuo's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Actuo's build, the machine it runs on (disk, memory), the lane's last
    /// poll, and what isn't right with them.
    Status {
        /// Actuo's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read or change actuo's settings (settings.toml in actuo's home).
    Settings {
        #[command(subcommand)]
        command: SettingsCmd,
    },
    /// Projects: GitHub repos on this actuo, each with its lane on or off.
    Project {
        #[command(subcommand)]
        command: ProjectCmd,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Check actuo can push to REPO (owner/repo), clone it, read it and propose
    /// its setup and checks; save that with --yes, else edit it in $EDITOR first.
    Add {
        repo: String,
        #[arg(long)]
        yes: bool,
    },
    /// The projects, with their lane on or off.
    List {
        /// Their settings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// A project's settings, as TOML.
    Show { repo: String },
    /// Run REPO's setup and checks once, in the sandbox, with no agent, and print each
    /// step's time, exit code and peak memory as it ends, then the totals.
    Check { repo: String },
    /// Add one host REPO's setup may reach, keeping the others; with --check, run setup and checks again.
    AllowHost {
        repo: String,
        host: String,
        #[arg(long)]
        check: bool,
    },
    /// Set one of a project's settings (checks, lane, label, …) to a TOML value.
    Set {
        repo: String,
        key: String,
        value: String,
    },
    /// Remove a project and delete its clone; its runs and sessions stay. Refused
    /// while a run or session in the clone hasn't ended (--keep-clone keeps it).
    Remove {
        repo: String,
        /// Keep actuo's clone of the repo (sessions started with `actuo start` can go on in it).
        #[arg(long)]
        keep_clone: bool,
    },
}

#[derive(Subcommand)]
enum AgentsCmd {
    /// Install each agent's pinned adapter (agents with a package) and srt, once,
    /// then check srt runs with `~` denied (the probe sessions get).
    Install,
    /// Ask AGENT what it offers, model by model: actuo starts it without a
    /// prompt, sets each model in turn and records the options (no tokens spent).
    Probe { agent: String },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Copy this binary to <actuo home>/bin/actuo (the one it replaces kept as
    /// actuo.prev), write the launch agent or systemd user unit (with the current
    /// PATH) and restart it. Waits first for what's in progress to finish (up to 30
    /// min), and after for the new actuo to answer (else the previous one goes back).
    Install {
        /// Restart without waiting for what's in progress.
        #[arg(long)]
        now: bool,
    },
    /// Stop the service and remove its launch agent or unit file, and the binary.
    Uninstall,
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct RunArgs {
    #[command(subcommand)]
    command: Option<RunCmd>,
    /// https://github.com/<owner>/<repo>/issues/<n>
    issue_url: Option<String>,
    /// Default: an agent:<name> label, else the project's agent, else sessions.agent.
    #[arg(long)]
    agent: Option<String>,
    #[arg(long, value_enum)]
    level: Option<LevelArg>,
}

#[derive(Subcommand)]
enum RunCmd {
    /// Hand a taken-over or waiting run back to the lane.
    Resume { run: String },
    /// End a run; its session and branch stay.
    Cancel { run: String },
    /// Follow a run: its state changes and finished steps so far, then live, until
    /// it ends (or Ctrl-C). Exits 1 unless it ends with a PR, open or merged.
    Watch { run: String },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Print the settings as TOML, defaults filled in; or the value of one dotted key.
    Get {
        key: Option<String>,
        /// As JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set a dotted key (lane.max_runs, 'projects."owner/repo".checks') to a TOML
    /// value (3, true, '["a"]'); text that isn't one is taken as a string.
    Set { key: String, value: String },
    /// Remove a dotted key from settings.toml, so its default applies again; prints
    /// the value now in effect, if the key has one.
    Unset { key: String },
    /// Every key, with its default and what it does.
    Keys,
    /// Edit settings.toml in $EDITOR: saved only if it validates (else edit again or
    /// drop it), then applied by the running actuo.
    Edit,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LevelArg {
    Ask,
    Edits,
}

/// `actuo serve` can't start and starting again won't help (settings that don't
/// validate, another actuo on the socket): the service stays stopped instead of
/// restarting every few seconds.
#[derive(Debug)]
struct WontStart(String);

impl std::fmt::Display for WontStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; not restarting until it's fixed (then start the service again)",
            self.0
        )
    }
}

impl std::error::Error for WontStart {}

/// The exit code for [`WontStart`]: 3, which the systemd unit names in
/// `RestartPreventExitStatus`; 0 under the launch agent (launchd sets
/// `XPC_SERVICE_NAME` to its label), the only code its `KeepAlive` (`SuccessfulExit`
/// false) doesn't restart on.
fn wont_start_exit() -> i32 {
    if std::env::var("XPC_SERVICE_NAME").as_deref() == Ok(service::LABEL) {
        0
    } else {
        3
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        if e.downcast_ref::<WontStart>().is_some() {
            std::process::exit(wont_start_exit());
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::from_env();
    if let Cmd::Serve = cli.command {
        return serve(paths).await;
    }
    if let Cmd::Stdio = cli.command {
        return stdio(&paths).await;
    }
    if let Cmd::Service { command } = &cli.command {
        return match command {
            ServiceCmd::Install { now } => service::install(&paths, *now).await,
            ServiceCmd::Uninstall => service::uninstall(&paths),
        };
    }
    if let Cmd::Settings {
        command: SettingsCmd::Keys,
    } = cli.command
    {
        out!(no_newline; "{}", settings_keys::keys());
        return Ok(());
    }
    if let Cmd::Settings {
        command: SettingsCmd::Edit,
    } = cli.command
    {
        return settings_edit(&paths).await;
    }
    if let Cmd::Restart { now } = cli.command {
        return service::restart_installed(&paths, now).await;
    }
    if let Cmd::Update { now } = cli.command {
        return update::update(&paths, now).await;
    }
    if let Cmd::Agents {
        command: Some(AgentsCmd::Install),
        ..
    } = cli.command
    {
        return install(&paths).await;
    }
    if let Cmd::Login { agent } = &cli.command {
        return login(&paths, agent).await;
    }
    if let Cmd::Settings { command } = &cli.command
        && no_actuo(&paths.socket()).await
    {
        return settings_on_file(&paths, command);
    }
    let (client, events) = Client::connect(&paths.socket()).await?;
    match cli.command {
        Cmd::Serve
        | Cmd::Stdio
        | Cmd::Service { .. }
        | Cmd::Restart { .. }
        | Cmd::Update { .. }
        | Cmd::Login { .. } => {
            unreachable!()
        }
        Cmd::Agents {
            command: Some(AgentsCmd::Probe { agent }),
            ..
        } => {
            let a = client
                .call::<AgentProbe>(AgentProbeParams { agent })
                .await?
                .agent;
            out!(
                "{}: {} models, probed {}",
                a.name,
                a.options_by_model.len(),
                a.probed_at.unwrap_or_default()
            );
        }
        Cmd::Agents { json, .. } => {
            let r = client.call::<AgentList>(Empty {}).await?;
            if json {
                return print_json(&r);
            }
            // The Node actuo runs the adapters and srt on (its own, pinned).
            out!(
                "node\t{}\t{}",
                actuo_core::node::VERSION,
                if actuo_core::node::bin(&paths).exists() {
                    "installed"
                } else {
                    "not installed yet: the first session installs it (about 30 MB), or actuo agents install"
                }
            );
            for a in r.agents {
                // What it gets from actuo's environment, names only.
                let passes = if a.pass_env.is_empty() {
                    String::new()
                } else {
                    format!("\tpasses {}", a.pass_env.join(", "))
                };
                out!(
                    "{}\t{}\t{} {}{passes}",
                    a.name,
                    a.version.unwrap_or_else(|| "-".into()),
                    a.command,
                    a.args.join(" ")
                );
                // What its defaults can be set to: with its own model, then with each
                // model (from its last probe, else its last session here).
                if a.config_options.is_empty() {
                    out!("  options: none known yet (actuo agents probe {})", a.name);
                }
                let choices = |o: &AgentConfigOption| -> String {
                    if o.values.is_empty() {
                        "true, false".into()
                    } else {
                        o.values
                            .iter()
                            .map(|v| v.value.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                for o in &a.config_options {
                    out!("  option {}: {}", o.id, choices(o));
                }
                for (model, options) in &a.options_by_model {
                    let rest: Vec<String> = options
                        .iter()
                        .filter(|o| o.id != "model")
                        .map(|o| format!("{} {}", o.id, choices(o)))
                        .collect();
                    out!("  with model {model}: {}", rest.join("; "));
                }
                if let Some(at) = &a.probed_at {
                    out!("  probed {at}");
                }
                for r in &a.builtin_rules {
                    out!(
                        "  built-in lane rule: {}",
                        actuo_core::lane::rules::describe(r)
                    );
                }
            }
        }
        Cmd::Start {
            path,
            agent,
            no_worktree,
            base,
            branch,
            level,
            clean,
            config,
            prompt,
            attach,
        } => {
            let path = path.canonicalize().context("the project path")?;
            let project = client
                .call::<ProjectOpen>(ProjectOpenParams {
                    path: path.display().to_string(),
                })
                .await?
                .project;
            let started = client
                .call::<SessionStart>(SessionStartParams {
                    project_id: project.id,
                    agent,
                    worktree: !no_worktree,
                    base,
                    branch,
                    sandbox: None,
                    web_tools: true,
                    telemetry: true,
                    level: match level {
                        LevelArg::Ask => Level::Ask,
                        LevelArg::Edits => Level::Edits,
                    },
                    clean,
                    config: parse_config(&config)?,
                    prompt,
                })
                .await?;
            out!("{}", started.session_id);
            if attach {
                attach_to(&client, events, started.session_id, 0).await?;
            } else {
                wait_ready(&client, events, started.session_id).await?;
            }
        }
        Cmd::Sessions { json } => {
            let r = client
                .call::<SessionList>(SessionListParams::default())
                .await?;
            if json {
                return print_json(&r);
            }
            for s in r.sessions {
                out!(
                    "{}\t{}\t{}\t{} turns\t{}",
                    s.id,
                    s.agent,
                    serde_json::to_value(s.state)?.as_str().unwrap_or_default(),
                    s.turn_count,
                    s.title.unwrap_or_else(|| s.cwd.clone())
                );
            }
        }
        Cmd::Show { session } => match resolve(&client, &session).await {
            Ok(id) => {
                let detail = client
                    .call::<SessionGet>(SessionParams { session_id: id })
                    .await?;
                out!("{}", serde_json::to_string_pretty(&detail.session)?);
            }
            Err(not_a_session) => {
                let Ok(run_id) = resolve_run(&client, &session).await else {
                    return Err(not_a_session.context(format!("and no run {session}")));
                };
                let detail = client.call::<RunGet>(RunParams { run_id }).await?;
                out!("{}", serde_json::to_string_pretty(&detail.run)?);
            }
        },
        Cmd::Attach { session, from } => {
            let id = resolve(&client, &session).await?;
            attach_to(&client, events, id, from).await?;
        }
        Cmd::Send {
            session,
            text,
            interrupt,
        } => {
            let id = resolve(&client, &session).await?;
            let r = client
                .call::<SessionSend>(SessionSendParams {
                    session_id: id,
                    text,
                    interrupt,
                })
                .await?;
            out!("turn {}", r.turn);
        }
        Cmd::Respond {
            session,
            request,
            option,
            cancel,
        } => {
            let id = resolve(&client, &session).await?;
            if option.is_some() == cancel {
                bail!("give an option or --cancel");
            }
            client
                .call::<SessionRespond>(SessionRespondParams {
                    session_id: id,
                    request_id: RequestId(request),
                    option_id: option.map(PermissionOptionId),
                    cancel,
                })
                .await?;
        }
        Cmd::Interrupt { session } => {
            let id = resolve(&client, &session).await?;
            client
                .call::<SessionInterrupt>(SessionParams { session_id: id })
                .await?;
        }
        Cmd::Revert {
            session,
            turn,
            force,
        } => {
            let id = resolve(&client, &session).await?;
            let r = client
                .call::<CheckpointRevert>(CheckpointRevertParams {
                    session_id: id,
                    turn,
                    force,
                })
                .await?;
            out!(
                "restored to checkpoint {}: restored {:?}, deleted {:?}",
                r.to_n,
                r.restored,
                r.deleted
            );
        }
        Cmd::Stop { session } => {
            let id = resolve(&client, &session).await?;
            client
                .call::<SessionStop>(SessionParams { session_id: id })
                .await?;
        }
        Cmd::Diff { session } => {
            let id = session_or_run(&client, &session).await?;
            let d = client
                .call::<SessionDiff>(SessionParams { session_id: id })
                .await?;
            out!(no_newline; "{}", d.patch);
            if d.truncated {
                eprintln!("the diff is cut at 2 MB; {} files changed", d.files.len());
            }
        }
        Cmd::Checkpoints { session, json } => {
            let id = session_or_run(&client, &session).await?;
            let r = client
                .call::<CheckpointList>(SessionParams { session_id: id })
                .await?;
            if json {
                return print_json(&r);
            }
            for c in r.checkpoints {
                let files: Vec<_> = c
                    .files
                    .iter()
                    .map(|f| format!("{} {}", show::name(&f.status), f.path))
                    .collect();
                out!(
                    "{}\t{}\t{}\t{}",
                    c.n,
                    &c.commit[..c.commit.len().min(8)],
                    c.created_at,
                    files.join(", ")
                );
            }
        }
        Cmd::Run(args) => match (args.command, args.issue_url) {
            (Some(RunCmd::Resume { run }), _) => {
                let id = resolve_run(&client, &run).await?;
                client.call::<RunResume>(RunParams { run_id: id }).await?;
            }
            (Some(RunCmd::Cancel { run }), _) => {
                let id = resolve_run(&client, &run).await?;
                client.call::<RunCancel>(RunParams { run_id: id }).await?;
            }
            (Some(RunCmd::Watch { run }), _) => watch_run(&client, events, &run).await?,
            (None, Some(issue_url)) => {
                let r = client
                    .call::<RunStart>(RunStartParams {
                        issue_url,
                        agent: args.agent,
                        level: args.level.map(|l| match l {
                            LevelArg::Ask => Level::Ask,
                            LevelArg::Edits => Level::Edits,
                        }),
                    })
                    .await?;
                out!("{}", r.run_id);
            }
            (None, None) => bail!("give an issue URL, or watch, resume or cancel with a run"),
        },
        Cmd::Runs { json } => {
            let list = client.call::<RunList>(RunListParams::default()).await?;
            if json {
                return print_json(&list);
            }
            for r in list.runs {
                let state = serde_json::to_value(r.state)?;
                let step = serde_json::to_value(r.step)?;
                out!(
                    "{}\t{}#{}\t{}{}\t{}\t{}\t{}",
                    r.id,
                    r.repo,
                    r.issue,
                    state.as_str().unwrap_or_default(),
                    r.reason.map(|x| format!(" ({x})")).unwrap_or_default(),
                    step.as_str().unwrap_or_default(),
                    r.session_id.map(|s| s.0).unwrap_or_else(|| "-".into()),
                    r.pr_url.unwrap_or_else(|| "-".into()),
                );
            }
        }
        Cmd::Status { json } => {
            let s = client.call::<ActuoStatus>(Empty {}).await?;
            if json {
                // The status with actuo's build from `initialize`.
                let mut v = serde_json::to_value(&s)?;
                v["actuo"] = serde_json::to_value(&client.actuo)?;
                return print_json(&v);
            }
            if let Some(r) = &client.actuo {
                out!(
                    "actuo\t{} {}{}",
                    r.name,
                    r.version,
                    r.commit
                        .as_ref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default()
                );
            }
            let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
            let space = |s: Option<Space>| match s {
                Some(s) => format!("{} free of {}", gb(s.free), gb(s.total)),
                None => "unknown".into(),
            };
            if let Some(login) = &s.gh_login {
                out!("github\t{login}");
            }
            out!("disk\t{}", space(s.disk));
            out!("memory\t{}", space(s.memory));
            out!(
                "lane\t{}",
                match &s.last_poll {
                    None => "no poll yet, or the lane is off".into(),
                    Some(p) => match &p.error {
                        None => format!("polled {}", p.at),
                        Some(e) => format!("poll failed {}: {e}", p.at),
                    },
                }
            );
            if let Some(commit) = &s.update {
                out!("update\trelease {commit} is out; actuo update installs it");
            }
            if s.problems.is_empty() {
                out!("problems\tnone");
            }
            for p in &s.problems {
                out!(
                    "problem\t{}{}",
                    if p.serious { "serious: " } else { "" },
                    p.message
                );
            }
        }
        Cmd::Project { command } => project(&client, command).await?,
        Cmd::Settings { command } => {
            let s = match settings_patch(&command)? {
                None => client.call::<SettingsGet>(Empty {}).await?.settings,
                Some(patch) => {
                    client
                        .call::<SettingsUpdate>(SettingsUpdateParams { patch })
                        .await?
                        .settings
                }
            };
            print_settings(&command, &s)?;
        }
    }
    Ok(())
}

/// `actuo project …`.
async fn project(client: &Client, command: ProjectCmd) -> Result<()> {
    match command {
        ProjectCmd::Add { repo, yes } => {
            let found = client
                .call::<ProjectInspect>(ProjectRepoParams { repo: repo.clone() })
                .await?;
            if found.accepted_invitation {
                out!("accepted actuo's invitation to {repo}");
            }
            let a = &found.access;
            out!(
                "{repo}: {} access, {} repo, default branch {} ({})",
                a.permission,
                a.visibility.to_lowercase(),
                a.default_branch,
                match a.protected {
                    Some(true) => "protected",
                    Some(false) => "not protected",
                    None => "no branch protection available",
                }
            );
            let d = &found.detected;
            let count = |s: &str| {
                d.workspaces
                    .iter()
                    .filter(|w| w.scripts.iter().any(|x| x == s))
                    .count()
            };
            out!(
                "detected: {} {}, {}, Node {}; {} package.json files (lint {}, typecheck {}, test {})",
                d.package_manager.as_deref().unwrap_or("no package manager"),
                d.package_manager_version.as_deref().unwrap_or(""),
                d.workspace_tool.as_deref().unwrap_or("no workspace tool"),
                d.node.as_deref().unwrap_or("not set"),
                d.workspaces.len(),
                count("lint"),
                count("typecheck"),
                count("test"),
            );
            out!("proposal:\n{}", project_toml(&found.proposal)?);
            if let Some(existing) = &found.existing {
                out!(
                    "{repo} is a project here already; its settings now:\n{}",
                    project_toml(existing)?
                );
                anyhow::bail!(
                    "project_exists: change it with `actuo project set {repo} <key> <value>`"
                );
            }
            let mut settings = if yes {
                found.proposal.clone()
            } else {
                edit_proposal(&repo, &found.proposal)?
            };
            // Left as proposed: detected from the repo at each run instead, so they
            // follow it as it changes (an empty repo the agent scaffolds).
            let p = &found.proposal;
            if settings.setup == p.setup {
                settings.setup = None;
            }
            if settings.checks == p.checks {
                settings.checks = None;
            }
            // `network` is kept as reviewed, even when it equals the proposal.
            let added = client
                .call::<ProjectAdd>(ProjectAddParams {
                    repo: repo.clone(),
                    settings,
                })
                .await?
                .project;
            out!(
                "added {repo} (clone {}); its lane is off: set it up, then turn it on with \
                 `actuo project set {repo} lane true`",
                added.root
            );
        }
        ProjectCmd::List { json } => {
            let s = client.call::<SettingsGet>(Empty {}).await?.settings;
            if json {
                return print_json(&s.projects);
            }
            for (repo, p) in &s.projects {
                out!(
                    "{repo}\tlane {}\tlabel {}\tagent {}",
                    if p.lane { "on" } else { "off" },
                    p.label.as_deref().unwrap_or(&s.lane.labels.trigger),
                    p.agent.as_deref().unwrap_or(&s.sessions.agent),
                );
            }
        }
        ProjectCmd::Show { repo } => {
            let s = client.call::<SettingsGet>(Empty {}).await?.settings;
            let p = s
                .projects
                .get(&repo)
                .ok_or_else(|| anyhow::anyhow!("project_not_found: {repo} isn't a project here"))?;
            out!(no_newline; "{}", project_toml(p)?);
            if let Some(check) = last_check(client, &repo).await? {
                out!("\nlast check run ({}):", check.started_at);
                for step in &check.steps {
                    out!("{}", check_step_line(step));
                }
                out!("{}", check_totals(&check));
            }
        }
        ProjectCmd::Check { repo } => project_check(client, &repo).await?,
        ProjectCmd::AllowHost { repo, host, check } => {
            client
                .call::<ProjectAllowHost>(ProjectAllowHostParams {
                    repo: repo.clone(),
                    host: host.clone(),
                })
                .await?;
            out!("allowed {host} for {repo}'s setup; saved on this actuo");
            if check {
                project_check(client, &repo).await.map_err(|e| {
                    anyhow::anyhow!("the host is saved, but the check run failed: {e}")
                })?;
            }
        }
        ProjectCmd::Set { repo, key, value } => {
            let command = SettingsCmd::Set {
                key: format!("projects.\"{repo}\".{key}"),
                value,
            };
            let patch = settings_patch(&command)?.expect("set patches");
            let s = client
                .call::<SettingsUpdate>(SettingsUpdateParams { patch })
                .await?
                .settings;
            print_settings(&command, &s)?;
        }
        ProjectCmd::Remove { repo, keep_clone } => {
            let delete_clone = !keep_clone;
            client
                .call::<ProjectRemove>(ProjectRemoveParams {
                    repo: repo.clone(),
                    delete_clone,
                })
                .await?;
            out!(
                "removed {repo}; its runs and sessions stay{}",
                if delete_clone {
                    ", its clone is deleted"
                } else {
                    ", and its clone"
                }
            );
        }
    }
    Ok(())
}

/// A project's last check run, from `project/list`.
async fn last_check(client: &Client, repo: &str) -> Result<Option<actuo_protocol::runs::CheckRun>> {
    Ok(client
        .call::<ProjectList>(Empty {})
        .await?
        .projects
        .into_iter()
        .find(|p| p.repo.as_deref() == Some(repo))
        .and_then(|p| p.last_check_run))
}

/// For example: 2. check `pnpm turbo test --affected`: failed (exit 1) in 4 min 3 s,
/// peak 1.2 GB (largest process 900 MB).
fn check_step_line(step: &actuo_protocol::runs::CheckStep) -> String {
    let kind = if step.kind == actuo_protocol::runs::RunStepKind::Setup {
        "setup"
    } else {
        "check"
    };
    let result = match (step.timed_out, step.exit_code) {
        (true, _) => "timed out".to_string(),
        (_, Some(0)) => "ok".to_string(),
        (_, Some(code)) => format!("failed (exit {code})"),
        (_, None) => "failed (no exit code)".to_string(),
    };
    let memory = match (step.peak_kb, step.peak_process_kb) {
        (Some(total), Some(largest)) => {
            format!(", peak {} (largest process {})", size(total), size(largest))
        }
        _ => String::new(),
    };
    let mut line = format!(
        "{}. {kind} `{}`: {result} in {}{memory}",
        step.n,
        step.command,
        duration(step.duration_ms)
    );
    if step.exit_code != Some(0)
        && let Some(tail) = &step.output_tail
    {
        let last: Vec<&str> = tail.lines().rev().take(8).collect();
        for l in last.into_iter().rev() {
            line.push_str(&format!("\n   | {l}"));
        }
    }
    if let Some(d) = &step.diagnosis {
        line.push_str(&why(d));
    }
    line
}

/// A step's diagnosis, as lines under it.
fn why(d: &actuo_protocol::runs::Diagnosis) -> String {
    let mut s = format!("\n   why: {}", d.detail);
    for fix in &d.fixes {
        s.push_str(&format!("\n   - {fix}"));
    }
    s
}

/// The end of a check run: its state, total time, the install's disk use.
fn check_totals(check: &actuo_protocol::runs::CheckRun) -> String {
    let total: u64 = check.steps.iter().map(|s| s.duration_ms).sum();
    let mut line = format!(
        "{:?}: {} steps in {}",
        check.state,
        check.steps.len(),
        duration(total)
    )
    .to_lowercase();
    if let Some(e) = &check.error {
        line.push_str(&format!(": {e}"));
    }
    if let Some(nm) = check.node_modules_kb {
        line.push_str(&format!("; node_modules {}", size(nm)));
    }
    if let (Some(before), Some(after)) = (check.store_kb_before, check.store_kb_after) {
        line.push_str(&format!(
            "; package store {} → {}",
            size(before),
            size(after)
        ));
    }
    line
}

fn size(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GB", kb as f64 / 1024.0 / 1024.0)
    } else {
        format!("{} MB", kb / 1024)
    }
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Start and follow one standalone check run.
async fn project_check(client: &Client, repo: &str) -> Result<()> {
    let mut notices = client.run_notices();
    client
        .call::<RunSubscribe>(RunSubscribeParams::default())
        .await?;
    let started = client
        .call::<ProjectCheck>(ProjectRepoParams {
            repo: repo.to_string(),
        })
        .await?
        .check_run;
    out!(
        "check run {} of {repo}: setup, then each check, in the sandbox",
        started.id
    );
    // Each step shown, and whether with its diagnosis: a failing check's retry
    // adds one to a step already shown.
    let mut shown: Vec<bool> = Vec::new();
    while let Some(notice) = notices.recv().await {
        let RunNotice::Check(check) = notice else {
            continue;
        };
        if check.id != started.id {
            continue;
        }
        for (i, step) in check.steps.iter().enumerate() {
            if i == shown.len() {
                out!("{}", check_step_line(step));
                shown.push(step.diagnosis.is_some());
            } else if !shown[i]
                && let Some(d) = &step.diagnosis
            {
                out!("{}. {}", step.n, why(d).trim_start());
                shown[i] = true;
            }
        }
        if check.state != actuo_protocol::runs::CheckRunState::Running {
            out!("{}", check_totals(&check));
            if check.state != actuo_protocol::runs::CheckRunState::Passed {
                std::process::exit(1);
            }
            return Ok(());
        }
    }
    bail!("actuo closed the connection")
}

/// Follow a run until it ends: its state changes and finished steps, read with
/// `run/get` whenever `run/subscribe` or its session's `run.step` events say it
/// changed.
async fn watch_run(
    client: &Client,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    given: &str,
) -> Result<()> {
    let id = resolve_run(client, given).await?;
    let mut notices = client.run_notices();
    client
        .call::<RunSubscribe>(RunSubscribeParams::default())
        .await?;
    let mut states = 0u32;
    // Each finished step shown, and whether with its diagnosis: actuo saves
    // one just after the step's end.
    let mut steps: Vec<bool> = Vec::new();
    // The session's events up to here are its history.
    let mut session_head = None;
    loop {
        let detail = client
            .call::<RunGet>(RunParams { run_id: id.clone() })
            .await?
            .run;
        let run = &detail.run;
        if states == 0 && steps.is_empty() {
            out!(
                "run {}: {}#{}{}",
                run.id,
                run.repo,
                run.issue,
                run.title
                    .as_deref()
                    .map(|t| format!(" {t}"))
                    .unwrap_or_default()
            );
        }
        let shown = states;
        for s in detail.states.iter().filter(|s| s.n > shown) {
            out!(
                "{} {}{}{}",
                s.at,
                show::name(&s.state),
                s.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                s.by.as_deref()
                    .map(|b| format!(" by {b}"))
                    .unwrap_or_default()
            );
            states = s.n;
            if s.n == detail.states.len() as u32
                && run.state == actuo_protocol::runs::RunState::Waiting
                && let Some(why) = &run.reason_text
            {
                out!("   {why}");
            }
        }
        for (i, step) in detail.steps.iter().enumerate() {
            if i == steps.len() {
                if step.ended_at.is_none() {
                    break;
                }
                out!("{}", run_step_line(step));
                steps.push(step.diagnosis.is_some());
            } else if !steps[i]
                && let Some(d) = &step.diagnosis
            {
                out!("{}. {}", step.n, why(d).trim_start());
                steps[i] = true;
            }
        }
        if run.state.ended() {
            if let Some(pr) = &run.pr_url {
                out!("{pr}");
            }
            if !matches!(
                run.state,
                actuo_protocol::runs::RunState::PrOpened | actuo_protocol::runs::RunState::Merged
            ) {
                std::process::exit(1);
            }
            return Ok(());
        }
        if session_head.is_none()
            && let Some(session_id) = &run.session_id
        {
            let r = client
                .call::<SessionSubscribe>(SessionSubscribeParams {
                    session_id: session_id.clone(),
                    from_seq: 0,
                    include_raw: false,
                })
                .await?;
            session_head = Some(r.head_seq);
        }
        // Until something of this run changes.
        loop {
            tokio::select! {
                Some(notice) = notices.recv() => {
                    if let RunNotice::Run(r) = notice && r.id == id {
                        break;
                    }
                }
                Some(event) = events.recv() => {
                    if let EventBody::RunStep(d) = &event.body
                        && d.run_id == id
                        && Some(event.seq) > session_head
                    {
                        break;
                    }
                }
                else => bail!("actuo closed the connection"),
            }
        }
    }
}

/// For example: 3. check `npm test`: failed (exit 1), with the output's last lines and why.
fn run_step_line(step: &actuo_protocol::runs::RunStepRecord) -> String {
    let result = match step.exit_code {
        Some(0) => "ok".to_string(),
        Some(code) => format!("failed (exit {code})"),
        None => "stopped".to_string(),
    };
    let command = step
        .command
        .as_deref()
        .map(|c| format!(" `{c}`"))
        .unwrap_or_default();
    let mut line = format!("{}. {}{command}: {result}", step.n, show::name(&step.kind));
    if step.exit_code != Some(0)
        && let Some(tail) = &step.output_tail
    {
        let last: Vec<&str> = tail.lines().rev().take(8).collect();
        for l in last.into_iter().rev() {
            line.push_str(&format!("\n   | {l}"));
        }
    }
    if let Some(d) = &step.diagnosis {
        line.push_str(&why(d));
    }
    line
}

/// A project's settings as TOML: `lane` and what differs from the defaults.
fn project_toml(p: &actuo_protocol::settings::ProjectSettings) -> Result<String> {
    let mut value = serde_json::to_value(p)?;
    let defaults = serde_json::to_value(actuo_protocol::settings::ProjectSettings::default())?;
    if let (serde_json::Value::Object(v), serde_json::Value::Object(d)) = (&mut value, &defaults) {
        v.retain(|k, x| k == "lane" || d.get(k) != Some(x));
    }
    let mut text = toml::to_string(&value)?;
    let unset: Vec<&str> = [("setup", p.setup.is_none()), ("checks", p.checks.is_none())]
        .into_iter()
        .filter(|(_, u)| *u)
        .map(|(k, _)| k)
        .collect();
    if !unset.is_empty() {
        text.push_str(&format!(
            "# {}: detected from the repo at each run\n",
            unset.join(", ")
        ));
    }
    if p.network.is_none() {
        text.push_str(
            "# network: default package download hosts (additional lockfile hosts need approval)\n",
        );
    }
    Ok(text)
}

/// The proposal in `$EDITOR` (else `vi`) as TOML; what's saved when it closes.
fn edit_proposal(
    repo: &str,
    proposal: &actuo_protocol::settings::ProjectSettings,
) -> Result<actuo_protocol::settings::ProjectSettings> {
    let path = std::env::temp_dir().join(format!("actuo-project-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        format!(
            "# {repo}: edit, save and close to add it. Keys as in [projects.\"{repo}\"].\n{}",
            project_toml(proposal)?
        ),
    )?;
    let edited = run_editor(&path);
    let text = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    edited.context("nothing added")?;
    toml::from_str(&text).map_err(|e| anyhow::anyhow!("the edited proposal isn't valid: {e}"))
}

/// Opens `path` in `$EDITOR` (default `vi`) and waits for it.
fn run_editor(path: &std::path::Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    // `$EDITOR` may carry arguments (e.g. `code --wait`): run it through the shell,
    // the file's path as `$1`.
    let mut child = actuo_core::process::spawn_std(
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path),
    )?;
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "{editor} exited with {status}");
    Ok(())
}

/// `actuo settings edit`: a copy of settings.toml in `$EDITOR`; saved over the
/// file only if it validates as actuo validates it, and only if the file
/// didn't change meanwhile; then a running actuo re-reads it (an empty
/// `settings/update`, which applies a hand edit).
async fn settings_edit(paths: &Paths) -> Result<()> {
    let agents = AgentsConfig::load(&paths.agents_toml()).map_err(anyhow::Error::msg)?;
    let file = paths.settings_toml();
    let before = settings::text(&file).map_err(|e| anyhow::anyhow!("{e}"))?;
    let copy = paths.home.join("tmp/settings-edit.toml");
    std::fs::create_dir_all(copy.parent().expect("a parent"))?;
    std::fs::write(&copy, &before)?;
    loop {
        run_editor(&copy).context("nothing saved")?;
        let text = std::fs::read_to_string(&copy)?;
        if text == before {
            let _ = std::fs::remove_file(&copy);
            out!("no change");
            return Ok(());
        }
        if settings::text(&file).map_err(|e| anyhow::anyhow!("{e}"))? != before {
            bail!(
                "{} changed while you edited it; nothing saved. Your version is in {}",
                file.display(),
                copy.display()
            );
        }
        match settings::replace(&file, &text, &agents) {
            Ok(_) => break,
            Err(e) => {
                eprintln!("not valid, nothing saved: {e}");
                eprint!("edit again? [Y/n] ");
                let mut answer = String::new();
                // No input left (not a terminal): no.
                let read = std::io::stdin().read_line(&mut answer)?;
                if read == 0 || !matches!(answer.trim(), "" | "y" | "Y" | "yes") {
                    let _ = std::fs::remove_file(&copy);
                    bail!("nothing saved");
                }
            }
        }
    }
    let _ = std::fs::remove_file(&copy);
    if no_actuo(&paths.socket()).await {
        out!("saved {}; actuo reads it when it starts", file.display());
        return Ok(());
    }
    let (client, _) = Client::connect(&paths.socket()).await?;
    client
        .call::<SettingsUpdate>(SettingsUpdateParams {
            patch: serde_json::json!({}),
        })
        .await?;
    out!("saved {} and applied", file.display());
    Ok(())
}

/// Whether no actuo is running here at all: no socket file, or nothing listening
/// on it (connection refused). An actuo that is running but slow to answer doesn't
/// count: `actuo settings` then goes through it, as always.
async fn no_actuo(socket: &std::path::Path) -> bool {
    match tokio::net::UnixStream::connect(socket).await {
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
    }
}

/// `actuo settings` with no actuo running: the same re-read, patch, validation
/// and atomic write as `settings/update`, on `settings.toml` itself (comments and
/// order kept). Says so on stderr.
fn settings_on_file(paths: &Paths, command: &SettingsCmd) -> Result<()> {
    let agents = AgentsConfig::load(&paths.agents_toml()).map_err(anyhow::Error::msg)?;
    let file = paths.settings_toml();
    let s = match settings_patch(command)? {
        None => {
            let s = settings::load(&file, &agents).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!("no actuo is running: read {} directly", file.display());
            s
        }
        Some(patch) => {
            let s = settings::update(&file, &patch, &agents).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!("no actuo is running: edited {} directly", file.display());
            s
        }
    };
    print_settings(command, &s)
}

/// The merge patch a settings command makes; `None` for `get`.
fn settings_patch(command: &SettingsCmd) -> Result<Option<serde_json::Value>> {
    Ok(match command {
        SettingsCmd::Get { .. } => None,
        SettingsCmd::Keys | SettingsCmd::Edit => unreachable!(),
        SettingsCmd::Set { key, value } => Some(settings::patch_for(
            &parse_key(key)?,
            settings::parse_value(value),
        )),
        SettingsCmd::Unset { key } => Some(settings::patch_for(
            &parse_key(key)?,
            serde_json::Value::Null,
        )),
    })
}

/// What a settings command prints: every setting, or the value at its key (after
/// `unset`, only if the key has a default).
fn print_settings(command: &SettingsCmd, s: &actuo_protocol::settings::Settings) -> Result<()> {
    let value = serde_json::to_value(s)?;
    match command {
        SettingsCmd::Get { key, json } => {
            let keys = key.as_deref().map(parse_key).transpose()?;
            if *json {
                let mut v = &value;
                for k in keys.as_deref().unwrap_or_default() {
                    v = v.get(k).with_context(|| {
                        format!("no setting {}", key.as_deref().unwrap_or_default())
                    })?;
                }
                return print_json(v);
            }
            out!(no_newline; "{}", show_setting(&value, keys.as_deref())?);
        }
        SettingsCmd::Set { key, .. } => {
            out!(no_newline; "{}", show_setting(&value, Some(&parse_key(key)?))?);
        }
        SettingsCmd::Keys | SettingsCmd::Edit => unreachable!(),
        SettingsCmd::Unset { key } => {
            if let Ok(text) = show_setting(&value, Some(&parse_key(key)?)) {
                out!(no_newline; "{text}");
            }
        }
    }
    Ok(())
}

fn parse_key(key: &str) -> Result<Vec<String>> {
    settings::parse_key(key).map_err(anyhow::Error::msg)
}

/// The settings as TOML, or the value at `keys`: a table as TOML, anything else as a
/// TOML value on one line.
fn show_setting(settings: &serde_json::Value, keys: Option<&[String]>) -> Result<String> {
    let mut value = settings;
    for key in keys.unwrap_or_default() {
        value = value
            .get(key)
            .with_context(|| format!("no setting {}", keys.unwrap_or_default().join(".")))?;
    }
    Ok(match value {
        serde_json::Value::Object(_) => toml::to_string(value)?,
        v => format!("{}\n", toml::Value::try_from(v)?),
    })
}

/// `actuo agents install`: runs locally, without actuo.
async fn install(paths: &Paths) -> Result<()> {
    use actuo_core::sandbox;
    let agents = AgentsConfig::load(&paths.agents_toml()).map_err(anyhow::Error::msg)?;
    actuo_core::node::ensure(paths)
        .await
        .map_err(anyhow::Error::msg)?;
    out!(
        "node {}\t{}",
        actuo_core::node::VERSION,
        actuo_core::node::dir(paths).display()
    );
    let dir = sandbox::install_dir(paths, sandbox::SRT_PACKAGE, sandbox::SRT_VERSION);
    sandbox::install(
        paths,
        &dir,
        sandbox::SRT_PACKAGE,
        sandbox::SRT_VERSION,
        "srt",
    )
    .await
    .map_err(anyhow::Error::msg)?;
    out!(
        "{}@{}\t{}",
        sandbox::SRT_PACKAGE,
        sandbox::SRT_VERSION,
        dir.display()
    );
    for (name, a) in &agents.agents {
        if let (Some(dir), Some(p), Some(v), Some(b)) =
            (a.install_dir(paths), &a.package, &a.version, &a.bin)
        {
            sandbox::install(paths, &dir, p, v, b)
                .await
                .map_err(anyhow::Error::msg)?;
            out!("{name}\t{p}@{v}\t{}", dir.display());
        }
    }
    // The probe sessions get: `true` inside srt with `~` denied, as every sandboxed
    // process has it.
    sandbox::probe(paths)
        .await
        .map_err(|e| anyhow::anyhow!("srt probe: {e}"))?;
    out!("srt probe\tok");
    Ok(())
}

async fn login(paths: &Paths, agent: &str) -> Result<()> {
    let agents = AgentsConfig::load(&paths.agents_toml()).map_err(anyhow::Error::msg)?;
    let status = actuo_core::login::login(paths, &agents, agent, |url| {
        // The browser opens from outside the sandbox. Elsewhere (a server) there's
        // none, and this URL's localhost callback wouldn't reach the browser you use:
        // the agent's own paste-the-code URL is the one to open.
        if cfg!(target_os = "macos") {
            eprintln!("actuo: opening the login page in your browser: {url}");
            let _ = actuo_core::process::output_std(
                std::process::Command::new("/usr/bin/open").arg(url),
            );
        }
    })
    .await
    .map_err(anyhow::Error::msg)?;
    out!("{status}");
    Ok(())
}

/// Copies stdin to actuo's socket and the socket to stdout. When stdin ends
/// (the other end went away, or a script sent all it had), only the socket's write half
/// is shut down, and actuo's answers keep coming until it closes the
/// connection. When actuo closes it (it stopped), that's the end even with
/// stdin still open. Exits the process then, since a blocking stdin read would
/// otherwise keep it alive.
async fn stdio(paths: &Paths) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let socket = paths.socket();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|_| anyhow::anyhow!("no actuo is running at {}", socket.display()))?;
    let (mut from_actuo, mut to_actuo) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        tokio::io::copy(&mut stdin, &mut to_actuo).await?;
        to_actuo.shutdown().await
    };
    let down = async {
        tokio::io::copy(&mut from_actuo, &mut stdout).await?;
        stdout.flush().await
    };
    tokio::pin!(up, down);
    let result = tokio::select! {
        r = &mut down => r,
        r = &mut up => match r {
            Ok(()) => down.await,
            Err(e) => Err(e),
        },
    };
    let _ = tokio::io::stdout().flush().await;
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => Err(e).context("actuo stdio"),
    }
}

async fn serve(paths: Paths) -> Result<()> {
    paths.create().context("creating actuo's home")?;
    // Before anything touches the store: another actuo may be using it.
    let socket = paths.socket();
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        return Err(WontStart(format!(
            "an actuo is already listening on {}",
            socket.display()
        ))
        .into());
    }
    let agents = AgentsConfig::load(&paths.agents_toml())
        .map_err(|e| WontStart(format!("{}: {e}", paths.agents_toml().display())))?;
    // Tokens and API keys for the agents whose pass_env names them, from the
    // environment or ~/.config/actuo/agents.env; from here on no process
    // actuo starts inherits them, only those agents get them.
    let env_file = actuo_core::pass_env::file();
    actuo_core::pass_env::keep_from_env_and_file(
        agents.agents.values().flat_map(|a| a.pass_env.clone()),
        &env_file,
    )
    .map_err(WontStart)?;
    // Names only, never values: a key left in the environment shows here.
    for (name, a) in &agents.agents {
        let passed = actuo_core::pass_env::names_for(&a.pass_env);
        if !passed.is_empty() {
            actuo_core::log!(
                "passing {} to {name} (from actuo's environment or {})",
                passed.join(", "),
                env_file.display()
            );
        }
    }
    let settings = settings::load(&paths.settings_toml(), &agents)
        .map_err(|e| WontStart(format!("{}: {e}", paths.settings_toml().display())))?;
    let store = Arc::new(Store::open(&paths.db()).context("opening the database")?);
    let host = Host::new(store, paths.clone(), agents, settings, Arc::new(AcpBackend));
    // Sessions left behind by an actuo that didn't stop cleanly.
    host.recover().await;
    let lane = Lane::start(host.clone(), Gh::default());
    // What each agent in use offers, model by model, if not known at its version.
    tokio::spawn(actuo_core::probe::in_use(host.clone()));
    // A newer release, at start and once a day (`actuo status`).
    tokio::spawn(actuo_core::release::check(host.clone()));
    actuo_core::log!("actuo listening on {}", socket.display());
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    };
    actuo_core::server::serve_unix(host.clone(), &socket, shutdown).await?;
    actuo_core::log!("stopping: letting go of the agents");
    // Runs stay where they are; the next start picks them up.
    lane.stop();
    host.shutdown().await;
    Ok(())
}

/// A run id, or any unique prefix of one.
async fn resolve_run(client: &Client, given: &str) -> Result<actuo_protocol::ids::RunId> {
    let runs = client.call::<RunList>(RunListParams::default()).await?.runs;
    let matches: Vec<_> = runs
        .into_iter()
        .filter(|r| r.id.0.starts_with(given))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!("no run {given}"),
        _ => bail!("{given} matches {} runs", matches.len()),
    }
}

/// A session, or a run's session, by id or any unique prefix of one.
async fn session_or_run(client: &Client, given: &str) -> Result<SessionId> {
    match resolve(client, given).await {
        Ok(id) => Ok(id),
        Err(not_a_session) => {
            let Ok(run_id) = resolve_run(client, given).await else {
                return Err(not_a_session.context(format!("and no run {given}")));
            };
            let run = client.call::<RunGet>(RunParams { run_id }).await?.run.run;
            run.session_id
                .ok_or_else(|| anyhow::anyhow!("run {given} has no session yet"))
        }
    }
}

/// `--json`: a reply as the protocol has it, indented.
fn print_json(v: &impl serde::Serialize) -> Result<()> {
    out!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// A session id, or any unique prefix of one.
async fn resolve(client: &Client, given: &str) -> Result<SessionId> {
    let sessions: Vec<SessionSummary> = client
        .call::<SessionList>(SessionListParams::default())
        .await?
        .sessions;
    let matches: Vec<_> = sessions
        .into_iter()
        .filter(|s| s.id.0.starts_with(given))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!("no session {given}"),
        _ => bail!("{given} matches {} sessions", matches.len()),
    }
}

fn parse_config(items: &[String]) -> Result<BTreeMap<String, ConfigValue>> {
    items
        .iter()
        .map(|item| {
            let (k, v) = item
                .split_once('=')
                .with_context(|| format!("--config {item}: expected ID=VALUE"))?;
            let v = match v {
                "true" => ConfigValue::Bool(true),
                "false" => ConfigValue::Bool(false),
                v => ConfigValue::String(v.to_string()),
            };
            Ok((k.to_string(), v))
        })
        .collect()
}

/// Waits until a new session's agent is ready (idle, or its first turn running),
/// saying on stderr what setup is doing (installing Node or the adapter); fails
/// with the reason when setup fails.
async fn wait_ready(
    client: &Client,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    id: SessionId,
) -> Result<()> {
    client
        .call::<SessionSubscribe>(SessionSubscribeParams {
            session_id: id.clone(),
            from_seq: 0,
            include_raw: false,
        })
        .await?;
    while let Some(event) = events.recv().await {
        let EventBody::SessionState(d) = &event.body else {
            continue;
        };
        if event.session_id != id {
            continue;
        }
        match d.state {
            SessionState::Starting => {
                if let Some(r) = &d.reason {
                    eprintln!("starting: {r}");
                }
            }
            SessionState::Idle | SessionState::Running => return Ok(()),
            SessionState::Ended | SessionState::Detached => bail!(
                "the session didn't start: {}",
                d.reason.as_deref().unwrap_or("its agent stopped")
            ),
        }
    }
    bail!("actuo closed the connection")
}

async fn attach_to(
    client: &Client,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    id: SessionId,
    from: u64,
) -> Result<()> {
    // Events up to `head` are the replay; requests they open may be long resolved.
    let head = client
        .call::<SessionSubscribe>(SessionSubscribeParams {
            session_id: id.clone(),
            from_seq: from,
            include_raw: false,
        })
        .await?
        .head_seq;

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut printer = show::Printer::default();
    let mut out = std::io::stdout();
    // Open permission requests, oldest first.
    let mut open: Vec<(RequestId, Vec<PermissionOption>)> = Vec::new();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    bail!("actuo closed the connection");
                };
                printer.print(&event, &mut out);
                match &event.body {
                    EventBody::RequestOpened(d) => {
                        open.push((d.request_id.clone(), d.options.clone()));
                        if event.seq > head {
                            printer.ask(&d.request_id, &mut out);
                        }
                    }
                    EventBody::RequestResolved(d) => open.retain(|(r, _)| r != &d.request_id),
                    _ => {}
                }
                // End of the replay: ask about requests that are still open.
                if event.seq == head {
                    for (request, _) in &open {
                        printer.ask(request, &mut out);
                    }
                }
            }
            line = lines.recv(), if stdin_open => {
                let Some(line) = line else {
                    stdin_open = false;
                    continue;
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let result = match line {
                    "/quit" => return Ok(()),
                    "/interrupt" => client
                        .call::<SessionInterrupt>(SessionParams { session_id: id.clone() })
                        .await
                        .map(|_| ()),
                    "/stop" => client
                        .call::<SessionStop>(SessionParams { session_id: id.clone() })
                        .await
                        .map(|_| ()),
                    _ => match open.first() {
                        Some((request, options)) => answer(client, &id, request, options, line).await,
                        None => client
                            .call::<SessionSend>(SessionSendParams {
                                session_id: id.clone(),
                                text: line.to_string(),
                                interrupt: false,
                            })
                            .await
                            .map(|_| ()),
                    },
                };
                if let Err(e) = result {
                    eprintln!("error: {e:#}");
                }
            }
        }
    }
}

async fn answer(
    client: &Client,
    session: &SessionId,
    request: &RequestId,
    options: &[PermissionOption],
    line: &str,
) -> Result<()> {
    let by_number: HashMap<String, &PermissionOption> = options
        .iter()
        .enumerate()
        .map(|(i, o)| ((i + 1).to_string(), o))
        .collect();
    let (option, cancel) = if line == "c" || line == "cancel" {
        (None, true)
    } else if let Some(o) = by_number.get(line) {
        (Some(o.option_id.clone()), false)
    } else {
        (Some(PermissionOptionId(line.to_string())), false)
    };
    client
        .call::<SessionRespond>(SessionRespondParams {
            session_id: session.clone(),
            request_id: request.clone(),
            option_id: option,
            cancel,
        })
        .await?;
    Ok(())
}

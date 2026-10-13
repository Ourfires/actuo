//! Where actuo keeps things (`~/.actuo`, or `ACTUO_HOME`) and the agents
//! config: built-in agents, overridden or extended by `~/.actuo/agents.toml`.

use actuo_protocol::ids::{ProjectId, SessionId};
use actuo_protocol::types::{AgentEntry, ConfigValue, Level};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// macOS limits a Unix socket path to 104 bytes including the terminating NUL.
pub const MAX_SOCKET_PATH: usize = 103;

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    /// `ACTUO_HOME`, or `~/.actuo`.
    pub fn from_env() -> Self {
        let home = match std::env::var_os("ACTUO_HOME") {
            Some(h) => PathBuf::from(h),
            None => home_dir().join(".actuo"),
        };
        Self { home }
    }

    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Creates the home (mode 0700) and its log directory.
    pub fn create(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&self.home)?;
        std::fs::set_permissions(&self.home, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir_all(self.logs())?;
        Ok(())
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("actuo.sock")
    }

    pub fn db(&self) -> PathBuf {
        self.home.join("actuo.db")
    }

    pub fn agents_toml(&self) -> PathBuf {
        self.home.join("agents.toml")
    }

    pub fn settings_toml(&self) -> PathBuf {
        self.home.join("settings.toml")
    }

    pub fn worktree(&self, project: &ProjectId, session: &SessionId) -> PathBuf {
        self.home
            .join("worktrees")
            .join(&project.0)
            .join(&session.0)
    }

    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }

    /// The lane's own clone of a watched repo (`owner/repo`).
    pub fn lane_clone(&self, repo: &str) -> PathBuf {
        self.home.join("repos").join(repo)
    }

    /// A lane run's command output, one file per step.
    pub fn run_logs(&self, run: &actuo_protocol::ids::RunId) -> PathBuf {
        self.logs().join("runs").join(&run.0)
    }

    pub fn agent_log(&self, session: &SessionId) -> PathBuf {
        self.logs().join(format!("{session}.agent.log"))
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Paths under your home folder that hold credentials: no sandboxed process reads
/// them, and no `read` entry may be or contain one.
pub const CREDENTIAL_PATHS: &[&str] = &[
    "~/.ssh",
    "~/.config/gh",
    "~/.git-credentials",
    "~/.netrc",
    "~/.npmrc",
    "~/.aws",
    "~/.docker",
    "~/.claude",
    "~/.codex",
    "~/.agents",
    // Where the user keeps the tokens actuo's service passes to agents.
    "~/.config/actuo",
];

/// A `read` entry (agents config `sandbox.read`, a repo's `read` in the settings): an
/// absolute path or one starting with `~/`, not your whole home folder, and neither
/// a credential path or actuo's home, nor inside or around one.
pub fn check_read_entry(entry: &str) -> Result<(), String> {
    let p = PathBuf::from(expand_home(entry));
    if !p.is_absolute() {
        return Err(format!("{entry} must be absolute or start with ~/"));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("{entry} can't contain .."));
    }
    let home = home_dir();
    if p == home || home.starts_with(&p) {
        return Err(format!("{entry} is your whole home folder"));
    }
    let actuo_home = Paths::from_env().home;
    let closed = CREDENTIAL_PATHS
        .iter()
        .map(|c| PathBuf::from(expand_home(c)))
        .chain([actuo_home]);
    for c in closed {
        if p.starts_with(&c) || c.starts_with(&p) {
            return Err(format!("{entry} is, is in or contains {}", c.display()));
        }
    }
    Ok(())
}

/// `~/` at the start of a value means the user's home directory.
pub fn expand_home(value: &str) -> String {
    match value.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest).display().to_string(),
        None => value.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The program, for an agent without `package`.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The adapter's npm package, installed once into `~/.actuo/agents/<package>@<version>`
    /// and run from there (`node_modules/.bin/<bin>`) instead of `command`.
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub bin: Option<String>,
    /// Pinned adapter version, for display and records (and the install, with `package`).
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables whose value is JSON, given as TOML tables and deep-merged across the
    /// layers that apply (base, `clean`, `sandbox`, `web_tools_off`, `telemetry_off`),
    /// e.g. Codex's `CODEX_CONFIG`.
    #[serde(default)]
    pub env_json: BTreeMap<String, toml::Value>,
    /// Actuo level to the agent's mode id.
    #[serde(default)]
    pub modes: BTreeMap<Level, String>,
    /// Sent as `_meta` on `session/new`, `session/load` and `session/resume`.
    #[serde(default)]
    pub session_meta: Option<toml::Value>,
    #[serde(default)]
    pub clean: CleanConfig,
    /// What a session inside the sandbox needs.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Turns the agent's web search and fetch off (`webTools = false`).
    #[serde(default)]
    pub web_tools_off: Layer,
    /// Turns the agent's own telemetry off (`telemetry = false`).
    #[serde(default)]
    pub telemetry_off: Layer,
    /// How `actuo login` logs the agent in to its sandbox config dir.
    #[serde(default)]
    pub login: Option<LoginConfig>,
    /// Variables actuo passes from its own environment to this agent's
    /// processes, and to nothing else: a token or API key the user put in the
    /// service's environment or in `~/.config/actuo/agents.env` (`pass_env.rs`).
    #[serde(default)]
    pub pass_env: Vec<String>,
}

/// `actuo login <agent>`: the agent's login command, run inside the sandbox with the
/// sandbox env and dirs (its own config dir), and the command that checks it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginConfig {
    /// The program: absolute, or relative to the adapter's install dir.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments for `command` that exit 0 when the agent is logged in, run with a
    /// session's sandbox settings afterwards.
    #[serde(default)]
    pub status: Vec<String>,
    /// Hosts the login reaches.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Lets the login listen on localhost (its OAuth callback).
    #[serde(default)]
    pub local_binding: bool,
}

/// Extra env and `_meta` for a session option.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_json: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub session_meta: Option<toml::Value>,
}

/// How an agent runs inside the sandbox (`srt`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Hosts the agent reaches (its model API).
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_json: BTreeMap<String, toml::Value>,
    /// Env variables naming directories the agent keeps its state in: created, and
    /// readable and writable inside the sandbox.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Env variables set to the session's temp dir too (besides `TMPDIR`).
    #[serde(default)]
    pub tmp_env: Vec<String>,
    /// Paths under your home folder the agent needs to read (its toolchain), e.g.
    /// `~/.nvm`. Credential paths can't be listed.
    #[serde(default)]
    pub read: Vec<String>,
}

/// How a `clean` session differs: merged over the normal settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanConfig {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_json: BTreeMap<String, toml::Value>,
    /// Names of `clean.env` variables whose values are directories to create before
    /// the agent starts (e.g. `CODEX_HOME`).
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Files written (overwritten) at each clean session start, into a `clean.dirs`
    /// directory, e.g. Codex's `config.toml` in `CODEX_HOME`.
    #[serde(default)]
    pub files: Vec<CleanFile>,
    #[serde(default)]
    pub session_meta: Option<toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanFile {
    /// A `clean.dirs` variable naming the directory.
    pub dir: String,
    pub name: String,
    pub content: String,
}

/// The session options that pick an agent's config layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionOptions {
    pub clean: bool,
    pub sandboxed: bool,
    pub web_tools: bool,
    pub telemetry: bool,
}

impl SessionOptions {
    /// A session with the agent's own defaults: not clean, no sandbox, web tools and
    /// telemetry on.
    pub fn plain() -> Self {
        Self {
            clean: false,
            sandboxed: false,
            web_tools: true,
            telemetry: true,
        }
    }

    pub fn clean(clean: bool) -> Self {
        Self {
            clean,
            ..Self::plain()
        }
    }
}

impl AgentConfig {
    /// The layers that apply, in order: (env, env_json, session_meta).
    #[allow(clippy::type_complexity)]
    fn layers(
        &self,
        o: SessionOptions,
    ) -> Vec<(
        &BTreeMap<String, String>,
        &BTreeMap<String, toml::Value>,
        Option<&toml::Value>,
    )> {
        let mut l = vec![(&self.env, &self.env_json, self.session_meta.as_ref())];
        if o.clean {
            l.push((
                &self.clean.env,
                &self.clean.env_json,
                self.clean.session_meta.as_ref(),
            ));
        }
        if o.sandboxed {
            l.push((&self.sandbox.env, &self.sandbox.env_json, None));
        }
        if !o.web_tools {
            let w = &self.web_tools_off;
            l.push((&w.env, &w.env_json, w.session_meta.as_ref()));
        }
        if !o.telemetry {
            let t = &self.telemetry_off;
            l.push((&t.env, &t.env_json, t.session_meta.as_ref()));
        }
        l
    }

    /// The environment for a session: each layer's `env` over the last, then the
    /// `env_json` variables (deep-merged tables, as JSON), with `~/` expanded.
    pub fn env_for(&self, o: SessionOptions) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        let mut json: BTreeMap<String, Value> = BTreeMap::new();
        for (e, j, _) in self.layers(o) {
            env.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
            for (k, v) in j {
                let slot = json
                    .entry(k.clone())
                    .or_insert(Value::Object(Default::default()));
                merge(slot, to_json(v));
            }
        }
        let mut env: BTreeMap<String, String> =
            env.into_iter().map(|(k, v)| (k, expand_home(&v))).collect();
        for (k, v) in json {
            env.insert(k, v.to_string());
        }
        env
    }

    /// The directories to create for a session: the `clean.dirs` (and, inside the
    /// sandbox, `sandbox.dirs`) variables' values.
    pub fn dirs_for(&self, o: SessionOptions) -> Vec<PathBuf> {
        let env = self.env_for(o);
        let mut names: Vec<&String> = Vec::new();
        if o.clean {
            names.extend(&self.clean.dirs);
        }
        if o.sandboxed {
            names.extend(&self.sandbox.dirs);
        }
        let mut dirs: Vec<PathBuf> = names
            .into_iter()
            .filter_map(|name| env.get(name).map(PathBuf::from))
            .collect();
        dirs.dedup();
        dirs
    }

    /// The agent's state directories inside the sandbox (`sandbox.dirs`).
    pub fn sandbox_dirs(&self, o: SessionOptions) -> Vec<PathBuf> {
        let env = self.env_for(o);
        self.sandbox
            .dirs
            .iter()
            .filter_map(|name| env.get(name).map(PathBuf::from))
            .collect()
    }

    /// The files to write for a session: `(path, content)` for each `clean.files`.
    pub fn files_for(&self, o: SessionOptions) -> Vec<(PathBuf, String)> {
        if !o.clean {
            return Vec::new();
        }
        let env = self.env_for(o);
        self.clean
            .files
            .iter()
            .filter_map(|f| {
                let dir = env.get(&f.dir)?;
                Some((PathBuf::from(dir).join(&f.name), f.content.clone()))
            })
            .collect()
    }

    /// The `_meta` for session requests: each layer's `session_meta` deep-merged
    /// over the last, with `~/` expanded in strings.
    pub fn meta_for(&self, o: SessionOptions) -> Option<Value> {
        let mut meta: Option<Value> = None;
        for (_, _, m) in self.layers(o) {
            if let Some(m) = m {
                match meta.as_mut() {
                    Some(b) => merge(b, to_json(m)),
                    None => meta = Some(to_json(m)),
                }
            }
        }
        if let Some(m) = meta.as_mut() {
            expand_home_in(m);
        }
        meta
    }

    /// The login program: `login.command`, under the install dir unless absolute.
    pub fn login_program(&self, paths: &Paths) -> Option<PathBuf> {
        let l = self.login.as_ref()?;
        let c = PathBuf::from(expand_home(&l.command));
        Some(match self.install_dir(paths) {
            Some(dir) if c.is_relative() => dir.join(c),
            _ => c,
        })
    }

    /// Where an agent with `package` is installed.
    pub fn install_dir(&self, paths: &Paths) -> Option<PathBuf> {
        let (p, v) = (self.package.as_ref()?, self.version.as_ref()?);
        Some(paths.home.join("agents").join(format!("{p}@{v}")))
    }

    /// The installed adapter script of an agent with `package`.
    pub fn adapter(&self, paths: &Paths) -> Option<PathBuf> {
        Some(
            self.install_dir(paths)?
                .join("node_modules/.bin")
                .join(self.bin.as_ref()?),
        )
    }

    /// The program and arguments that start the agent: for an agent with
    /// `package`, actuo's pinned Node ([`crate::node`]) running its adapter; else
    /// `command`.
    pub fn program(&self, paths: &Paths) -> (String, Vec<String>) {
        match self.adapter(paths) {
            Some(script) => {
                let mut args = vec![script.display().to_string()];
                args.extend(self.args.iter().cloned());
                (crate::node::bin(paths).display().to_string(), args)
            }
            None => (self.command.clone(), self.args.clone()),
        }
    }

    /// Whether a layer of this agent's config sets `name` (`env`, `env_json` or
    /// `tmp_env`, in any layer).
    fn sets(&self, name: &str) -> bool {
        let layers = [
            (&self.env, &self.env_json),
            (&self.clean.env, &self.clean.env_json),
            (&self.sandbox.env, &self.sandbox.env_json),
            (&self.web_tools_off.env, &self.web_tools_off.env_json),
            (&self.telemetry_off.env, &self.telemetry_off.env_json),
        ];
        layers
            .iter()
            .any(|(e, j)| e.contains_key(name) || j.contains_key(name))
            || self.sandbox.tmp_env.iter().any(|t| t == name)
    }

    fn check(&self) -> Result<(), String> {
        if self.package.is_some() && (self.bin.is_none() || self.version.is_none()) {
            return Err("an agent with package needs bin and version".into());
        }
        if self.package.is_none() && self.command.is_empty() {
            return Err("an agent needs command, or package, bin and version".into());
        }
        for dir in &self.clean.dirs {
            if !self.clean.env.contains_key(dir) {
                return Err(format!("clean.dirs names {dir}, which isn't in clean.env"));
            }
        }
        for dir in &self.sandbox.dirs {
            let known = self.env.contains_key(dir)
                || self.clean.env.contains_key(dir)
                || self.sandbox.env.contains_key(dir);
            if !known {
                return Err(format!(
                    "sandbox.dirs names {dir}, which isn't in env, clean.env or sandbox.env"
                ));
            }
        }
        for f in &self.clean.files {
            if !self.clean.dirs.contains(&f.dir) {
                return Err(format!(
                    "clean.files writes into {}, which isn't in clean.dirs",
                    f.dir
                ));
            }
            if f.name.contains('/') || f.name == ".." {
                return Err(format!(
                    "clean.files name {} must be a plain file name",
                    f.name
                ));
            }
        }
        for entry in &self.sandbox.read {
            check_read_entry(entry).map_err(|e| format!("sandbox.read: {e}"))?;
        }
        // Removed from every other process actuo starts: never one they need.
        for name in &self.pass_env {
            let plain = !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !plain {
                return Err(format!(
                    "pass_env: {name:?} isn't a variable name (A-Z, 0-9, _)"
                ));
            }
            if crate::sandbox::ENV_ALLOWLIST.contains(&name.as_str())
                || crate::sandbox::ACTUO_SET.contains(&name.as_str())
                || ["SHELL", "XDG_RUNTIME_DIR", "NODE_OPTIONS", "SRT_DEBUG"]
                    .contains(&name.as_str())
                || ["ACTUO_", "LD_", "DYLD_"]
                    .iter()
                    .any(|p| name.starts_with(p))
            {
                return Err(format!("pass_env: {name} is one actuo sets or needs"));
            }
            if self.sets(name) {
                return Err(format!(
                    "pass_env: {name} is set by this agent's own config"
                ));
            }
        }
        if let Some(l) = &self.login {
            let c = PathBuf::from(expand_home(&l.command));
            if c.is_relative() && self.package.is_none() {
                return Err("login.command must be absolute for an agent without package".into());
            }
            if c.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("login.command can't contain ..".into());
            }
        }
        Ok(())
    }

    /// The agent as `agent/list` shows it, with its config defaults from the settings.
    pub fn entry(&self, name: &str, config: BTreeMap<String, ConfigValue>) -> AgentEntry {
        AgentEntry {
            name: name.to_string(),
            version: self.version.clone(),
            command: self
                .adapter(&Paths::from_env())
                .map(|a| a.display().to_string())
                .unwrap_or_else(|| self.command.clone()),
            args: self.args.clone(),
            levels: self.modes.keys().copied().collect(),
            config,
            pass_env: crate::pass_env::names_for(&self.pass_env),
            config_options: Vec::new(),
            options_by_model: BTreeMap::new(),
            probed_at: None,
            builtin_rules: crate::lane::rules::builtin(name),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
}

impl AgentsConfig {
    /// The built-in agents, with their pinned versions.
    pub fn builtin() -> Self {
        Self::builtin_in(&Paths::from_env().home)
    }

    /// The built-in agents for an actuo whose home is `home` (their own config dirs
    /// are under it).
    fn builtin_in(home: &Path) -> Self {
        Self::from_table(builtin_table(home)).expect("built-in agents config parses")
    }

    /// The built-in agents, with each agent in `path` (if it exists) merged over the
    /// built-in one of the same name, or added. Tables merge key by key; other values,
    /// arrays included, replace the built-in ones. An agent given `command` without
    /// `package` runs that program: the built-in `package`, `bin` and `version` go, and
    /// so does the built-in `login` (its command is in the package) unless the file
    /// gives one.
    pub fn load(path: &Path) -> Result<Self, String> {
        // Actuo's home: the folder agents.toml is in.
        let home = path.parent().unwrap_or(Path::new("."));
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::builtin_in(home));
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let at = |e: String| format!("{}: {e}", path.display());
        // Parsed on its own first, so a mistake is reported against the file.
        let file: Self = toml::from_str(&text).map_err(|e| at(e.to_string()))?;
        let file_table: toml::Table = toml::from_str(&text).map_err(|e| at(e.to_string()))?;
        let mut table = builtin_table(home);
        let agents = table
            .entry("agents")
            .or_insert_with(|| toml::Table::new().into());
        let agents = agents.as_table_mut().expect("built-in agents is a table");
        let file_agents = file_table.get("agents").and_then(|a| a.as_table());
        for (name, agent) in file_agents.into_iter().flatten() {
            let base = agents
                .entry(name.clone())
                .or_insert_with(|| toml::Table::new().into());
            if let (Some(b), Some(a)) = (base.as_table_mut(), agent.as_table())
                && a.contains_key("command")
                && !a.contains_key("package")
            {
                for key in ["package", "bin", "version"] {
                    b.remove(key);
                }
                if !a.contains_key("login") {
                    b.remove("login");
                }
            }
            merge_toml(base, agent.clone());
        }
        let config = Self::from_table(table).map_err(at)?;
        for name in file.agents.keys() {
            config.agents[name]
                .check()
                .map_err(|e| at(format!("agents.{name}: {e}")))?;
        }
        Ok(config)
    }

    fn from_table(table: toml::Table) -> Result<Self, String> {
        Self::deserialize(table).map_err(|e| e.to_string())
    }
}

fn builtin_table(home: &Path) -> toml::Table {
    let mut t: toml::Table = toml::from_str(BUILTIN).expect("built-in agents config parses");
    // Each agent's own config dir (its own login) is under actuo's home.
    let agent_home = |agent: &str| home.join("agent-homes").join(agent).display().to_string();
    let homes: toml::Table = toml::from_str(&format!(
        "[claude.sandbox.env]\nCLAUDE_CONFIG_DIR = {:?}\n[codex.clean.env]\nCODEX_HOME = {:?}\n",
        agent_home("claude"),
        agent_home("codex")
    ))
    .expect("parses");
    merge_toml(t.get_mut("agents").expect("built-in agents"), homes.into());
    // Codex checks certificates through macOS's trust service, which the sandbox
    // blocks; it reads its roots from this file instead.
    if cfg!(target_os = "macos") {
        let codex = t
            .get_mut("agents")
            .and_then(|a| a.get_mut("codex"))
            .expect("built-in codex");
        let extra: toml::Table =
            toml::from_str("[sandbox.env]\nCODEX_CA_CERTIFICATE = \"/etc/ssl/cert.pem\"\n")
                .expect("parses");
        merge_toml(codex, extra.into());
    }
    // Claude's login runs the Claude Code binary the adapter's SDK bundles for this
    // platform (npm installs only the matching one).
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let claude = t
        .get_mut("agents")
        .and_then(|a| a.get_mut("claude"))
        .and_then(|c| c.get_mut("login"))
        .and_then(|l| l.as_table_mut())
        .expect("built-in claude login");
    claude.insert(
        "command".into(),
        format!("node_modules/@anthropic-ai/claude-agent-sdk-{os}-{arch}/claude").into(),
    );
    // Codex's: the binary codex-acp's `@openai/codex` bundles for this platform.
    let triple = match (os, arch) {
        ("darwin", "arm64") => "aarch64-apple-darwin",
        ("darwin", _) => "x86_64-apple-darwin",
        (_, "arm64") => "aarch64-unknown-linux-musl",
        _ => "x86_64-unknown-linux-musl",
    };
    let codex = t
        .get_mut("agents")
        .and_then(|a| a.get_mut("codex"))
        .and_then(|c| c.get_mut("login"))
        .and_then(|l| l.as_table_mut())
        .expect("built-in codex login");
    codex.insert(
        "command".into(),
        format!("node_modules/@openai/codex-{os}-{arch}/vendor/{triple}/bin/codex").into(),
    );
    t
}

/// Deep merge for TOML: tables merge key by key, anything else is replaced.
fn merge_toml(base: &mut toml::Value, extra: toml::Value) {
    match (base, extra) {
        (toml::Value::Table(b), toml::Value::Table(e)) => {
            for (k, v) in e {
                match b.get_mut(&k) {
                    Some(slot) => merge_toml(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, e) => *b = e,
    }
}

const BUILTIN: &str = r#"
[agents.claude]
package = "@agentclientprotocol/claude-agent-acp"
bin = "claude-agent-acp"
version = "0.84.0"
modes = { ask = "default", edits = "acceptEdits" }
session_meta = { claudeCode = { options = { allowDangerouslySkipPermissions = false } } }
# Clean: only the project's settings and CLAUDE.md files. Claude looks for CLAUDE.md in
# every parent of the cwd, and worktrees live under ~/.actuo, so the home-level files
# are excluded by path. Auto memory is off by setting and by env. Nothing from the
# claude.ai account the login belongs to: no synced skills or plugins, no connectors
# (strictMcpConfig keeps them out; disableClaudeAiConnectors says so too), and no
# skills bundled with Claude Code (skills = [] hides them from the model; this
# removes them).
clean.session_meta = { claudeCode = { options = { settingSources = ["project"], strictMcpConfig = true, skills = [], settings = { autoMemoryEnabled = false, claudeMdExcludes = ["~/.claude/CLAUDE.md", "~/.claude/rules/**", "~/CLAUDE.md"], syncClaudeAiSkills = false, syncClaudeAiPlugins = false, disableClaudeAiConnectors = true, disableBundledSkills = true } } } }
clean.env = { CLAUDE_CODE_DISABLE_AUTO_MEMORY = "1" }
web_tools_off.session_meta = { claudeCode = { options = { disallowedTools = ["WebSearch", "WebFetch"] } } }
telemetry_off.env = { CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1" }
# From actuo's environment (its service's), if set there: a `claude setup-token`
# token or an API key.
pass_env = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]

# Inside the sandbox: its own config dir (its own login, never your ~/.claude), and
# its shell's temp dir in the session's (Claude's default is shared with yours).
# Hosts: the model API, and where Claude Code refreshes its login token
# (https://platform.claude.com/v1/oauth/token).
[agents.claude.sandbox]
hosts = ["api.anthropic.com", "platform.claude.com"]
# env.CLAUDE_CONFIG_DIR: <actuo home>/agent-homes/claude, set in `builtin_table`.
dirs = ["CLAUDE_CONFIG_DIR"]
tmp_env = ["CLAUDE_CODE_TMPDIR"]

# `actuo login claude`: Claude Code's own login, inside the sandbox, into the config
# dir above. Its OAuth callback listens on localhost; the token exchange and profile go
# to these hosts (the authorize page opens in your browser, outside the sandbox).
# Claude saves the login to .credentials.json because the keychain is denied.
[agents.claude.login]
args = ["auth", "login"]
status = ["auth", "status"]
hosts = ["platform.claude.com", "api.anthropic.com"]
local_binding = true

[agents.codex]
package = "@agentclientprotocol/codex-acp"
bin = "codex-acp"
version = "2.0.1"
modes = { ask = "read-only", edits = "workspace-write" }
# Clean: its own home (own login), with the account's connectors (codex_apps) and
# curated and remote plugins turned off. The same keys go in config.toml, which
# Codex reads at startup (plugin sync runs then), and in CODEX_CONFIG, which
# applies to each session and ranks above a repo's .codex/config.toml (codex-acp trusts
# every cwd). Outside the sandbox, ~/.agents/skills still loads.
clean.dirs = ["CODEX_HOME"]
# clean.env.CODEX_HOME: <actuo home>/agent-homes/codex, set in `builtin_table`.
clean.env_json.CODEX_CONFIG = { features = { apps = false, plugins = false, remote_plugin = false, plugin_sharing = false, tool_suggest = false }, apps = { _default = { enabled = false } } }
web_tools_off.env_json.CODEX_CONFIG = { web_search = "disabled" }
telemetry_off.env_json.CODEX_CONFIG = { analytics = { enabled = false }, feedback = { enabled = false } }

# Hosts: the model API, and where Codex refreshes its login token
# (https://auth.openai.com/oauth/token).
[agents.codex.sandbox]
hosts = ["chatgpt.com", "auth.openai.com"]
dirs = ["CODEX_HOME"]

# `actuo login codex`: Codex's device-code login (a URL and a code to enter in any
# browser; no callback), inside the sandbox, into CODEX_HOME. Hosts: the auth server
# and the API.
[agents.codex.login]
args = ["login", "--device-auth"]
status = ["login", "status"]
hosts = ["auth.openai.com", "chatgpt.com"]

[[agents.codex.clean.files]]
dir = "CODEX_HOME"
name = "config.toml"
content = """
# Written by actuo at each clean session start; changes here are overwritten.
[features]
apps = false
plugins = false
remote_plugin = false
plugin_sharing = false
tool_suggest = false

[apps._default]
enabled = false
"""
"#;

fn to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn expand_home_in(v: &mut Value) {
    match v {
        Value::String(s) => *s = expand_home(s),
        Value::Array(items) => items.iter_mut().for_each(expand_home_in),
        Value::Object(map) => map.values_mut().for_each(expand_home_in),
        _ => {}
    }
}

/// Deep merge: objects merge key by key, anything else is replaced.
pub fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(b), Value::Object(e)) => {
            for (k, v) in e {
                merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, e) => *b = e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_agents_pass_their_own_checks() {
        for (name, agent) in AgentsConfig::builtin().agents {
            agent.check().unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn claude_login_runs_the_bundled_claude() {
        let paths = Paths::new("/r");
        let claude = &AgentsConfig::builtin().agents["claude"];
        let program = claude.login_program(&paths).unwrap();
        let p = program.display().to_string();
        assert!(
            p.starts_with("/r/agents/@agentclientprotocol/claude-agent-acp@0.84.0/node_modules/@anthropic-ai/claude-agent-sdk-"),
            "{p}"
        );
        assert!(p.ends_with("/claude"), "{p}");
        let l = claude.login.as_ref().unwrap();
        assert_eq!(l.args, ["auth", "login"]);
        assert!(l.local_binding);
        assert!(l.hosts.contains(&"platform.claude.com".to_string()));
        let codex = &AgentsConfig::builtin().agents["codex"];
        let p = codex.login_program(&paths).unwrap().display().to_string();
        assert!(
            p.starts_with(
                "/r/agents/@agentclientprotocol/codex-acp@2.0.1/node_modules/@openai/codex-"
            ) && p.ends_with("/bin/codex")
                && p.contains("/vendor/"),
            "{p}"
        );
        let l = codex.login.as_ref().unwrap();
        assert_eq!(l.args, ["login", "--device-auth"]);
        assert!(!l.local_binding);
    }

    #[test]
    fn clean_codex_file_and_env_say_the_same() {
        let codex = &AgentsConfig::builtin().agents["codex"];
        let env = codex.env_for(SessionOptions::clean(true));
        let files = codex.files_for(SessionOptions::clean(true));
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].0,
            PathBuf::from(&env["CODEX_HOME"]).join("config.toml")
        );
        let from_file: Value =
            serde_json::to_value(toml::from_str::<toml::Value>(&files[0].1).unwrap()).unwrap();
        let from_env: Value = serde_json::from_str(&env["CODEX_CONFIG"]).unwrap();
        assert_eq!(from_file, from_env);
        assert_eq!(from_env["features"]["apps"], false);
        assert!(codex.files_for(SessionOptions::clean(false)).is_empty());
        assert!(
            !codex
                .env_for(SessionOptions::clean(false))
                .contains_key("CODEX_CONFIG")
        );
    }

    #[test]
    fn builtin_agents() {
        let c = AgentsConfig::builtin();
        let claude = &c.agents["claude"];
        assert_eq!(claude.modes[&Level::Ask], "default");
        assert_eq!(claude.modes[&Level::Edits], "acceptEdits");
        assert_eq!(
            claude.meta_for(SessionOptions::clean(false)),
            Some(json!({"claudeCode": {"options": {"allowDangerouslySkipPermissions": false}}}))
        );
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            claude.meta_for(SessionOptions::clean(true)),
            Some(json!({"claudeCode": {"options": {
                "allowDangerouslySkipPermissions": false,
                "settingSources": ["project"],
                "strictMcpConfig": true,
                "skills": [],
                "settings": {
                    "autoMemoryEnabled": false,
                    "claudeMdExcludes": [
                        format!("{home}/.claude/CLAUDE.md"),
                        format!("{home}/.claude/rules/**"),
                        format!("{home}/CLAUDE.md"),
                    ],
                    "syncClaudeAiSkills": false,
                    "syncClaudeAiPlugins": false,
                    "disableClaudeAiConnectors": true,
                    "disableBundledSkills": true,
                },
            }}}))
        );
        assert_eq!(
            claude.env_for(SessionOptions::clean(true))["CLAUDE_CODE_DISABLE_AUTO_MEMORY"],
            "1"
        );
        assert!(claude.env_for(SessionOptions::clean(false)).is_empty());
        assert_eq!(
            claude.sandbox.hosts,
            ["api.anthropic.com", "platform.claude.com"]
        );
        let codex = &c.agents["codex"];
        assert_eq!(codex.modes[&Level::Edits], "workspace-write");
        assert_eq!(codex.sandbox.hosts, ["chatgpt.com", "auth.openai.com"]);
        assert!(codex.env_for(SessionOptions::clean(false)).is_empty());
        let clean = codex.env_for(SessionOptions::clean(true));
        assert!(clean["CODEX_HOME"].ends_with("/.actuo/agent-homes/codex"));
        assert!(!clean["CODEX_HOME"].starts_with('~'));
        assert_eq!(
            codex.dirs_for(SessionOptions::clean(true)),
            vec![PathBuf::from(&clean["CODEX_HOME"])]
        );
        assert!(codex.dirs_for(SessionOptions::clean(false)).is_empty());
        assert!(claude.dirs_for(SessionOptions::clean(true)).is_empty());
        assert_eq!(codex.meta_for(SessionOptions::clean(true)), None);
    }

    #[test]
    fn file_merges_over_and_adds_agents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.toml");
        std::fs::write(
            &path,
            r#"
            [agents.claude.sandbox]
            read = ["~/.nvm/versions/node/v24.14.0"]
            hosts = ["api.anthropic.com", "statsig.anthropic.com"]

            [agents.codex]
            command = "codex-acp"
            modes = { ask = "never" }

            [agents.fake]
            command = "/bin/fake-agent"
            args = ["--script", "s.json"]
            "#,
        )
        .unwrap();
        let c = AgentsConfig::load(&path).unwrap();
        // Its own command: the built-in package and login (a path in it) go.
        assert_eq!(c.agents["codex"].package, None);
        assert_eq!(c.agents["codex"].login, None);
        let (builtin, claude) = (AgentsConfig::builtin_in(dir.path()), &c.agents["claude"]);
        // Tables merge: the built-in claude keeps everything the file doesn't set.
        // Its own config dir is under the home agents.toml is in, not ~/.actuo.
        assert_eq!(
            claude.sandbox.env["CLAUDE_CONFIG_DIR"],
            dir.path().join("agent-homes/claude").display().to_string()
        );
        assert_eq!(claude.package, builtin.agents["claude"].package);
        assert_eq!(claude.clean, builtin.agents["claude"].clean);
        assert_eq!(claude.sandbox.env, builtin.agents["claude"].sandbox.env);
        assert_eq!(claude.sandbox.read, vec!["~/.nvm/versions/node/v24.14.0"]);
        // Arrays replace.
        assert_eq!(
            claude.sandbox.hosts,
            vec!["api.anthropic.com", "statsig.anthropic.com"]
        );
        // `command` without `package` runs that program instead of the adapter.
        let codex = &c.agents["codex"];
        assert_eq!(codex.command, "codex-acp");
        assert_eq!(
            (&codex.package, &codex.bin, &codex.version),
            (&None, &None, &None)
        );
        assert_eq!(codex.modes[&Level::Ask], "never");
        assert_eq!(codex.modes[&Level::Edits], "workspace-write");
        assert_eq!(codex.clean, builtin.agents["codex"].clean);
        assert_eq!(codex.sandbox, builtin.agents["codex"].sandbox);
        assert!(c.agents["fake"].modes.is_empty());
        assert_eq!(
            AgentsConfig::load(&dir.path().join("missing.toml")).unwrap(),
            AgentsConfig::builtin_in(dir.path())
        );
        std::fs::write(&path, "[agents.x]\ncomand = \"typo\"\n").unwrap();
        assert!(AgentsConfig::load(&path).is_err());
        std::fs::write(
            &path,
            "[agents.x]\ncommand = \"x\"\nclean.dirs = [\"NOT_IN_ENV\"]\n",
        )
        .unwrap();
        assert!(
            AgentsConfig::load(&path)
                .unwrap_err()
                .contains("NOT_IN_ENV")
        );
        std::fs::write(
            &path,
            "[agents.x]\ncommand = \"x\"\n[[agents.x.clean.files]]\ndir = \"D\"\nname = \"f\"\ncontent = \"\"\n",
        )
        .unwrap();
        assert!(
            AgentsConfig::load(&path)
                .unwrap_err()
                .contains("isn't in clean.dirs")
        );
    }

    #[test]
    fn layers_for_the_sandbox_web_tools_and_telemetry() {
        let c = AgentsConfig::builtin();
        let claude = &c.agents["claude"];
        let lane = SessionOptions {
            clean: true,
            sandboxed: true,
            web_tools: false,
            telemetry: false,
        };
        let env = claude.env_for(lane);
        assert!(env["CLAUDE_CONFIG_DIR"].ends_with("/.actuo/agent-homes/claude"));
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
        assert_eq!(env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"], "1");
        assert!(
            !claude
                .env_for(SessionOptions::clean(true))
                .contains_key("CLAUDE_CONFIG_DIR")
        );
        let meta = claude.meta_for(lane).unwrap();
        let options = &meta["claudeCode"]["options"];
        assert_eq!(options["disallowedTools"], json!(["WebSearch", "WebFetch"]));
        assert_eq!(options["settingSources"], json!(["project"]));
        assert!(
            claude.meta_for(SessionOptions::clean(true)).unwrap()["claudeCode"]["options"]
                .get("disallowedTools")
                .is_none()
        );
        assert_eq!(
            claude.sandbox_dirs(lane),
            vec![PathBuf::from(&env["CLAUDE_CONFIG_DIR"])]
        );
        assert_eq!(claude.sandbox.tmp_env, vec!["CLAUDE_CODE_TMPDIR"]);

        // CODEX_CONFIG: the clean, web-tools-off and telemetry-off tables merged.
        let codex = &c.agents["codex"];
        let env = codex.env_for(lane);
        let cfg: Value = serde_json::from_str(&env["CODEX_CONFIG"]).unwrap();
        assert_eq!(cfg["web_search"], "disabled");
        assert_eq!(cfg["analytics"]["enabled"], false);
        assert_eq!(cfg["features"]["apps"], false);
        assert_eq!(cfg["apps"]["_default"]["enabled"], false);
        assert_eq!(
            env.contains_key("CODEX_CA_CERTIFICATE"),
            cfg!(target_os = "macos")
        );
        let plain: Value =
            serde_json::from_str(&codex.env_for(SessionOptions::clean(true))["CODEX_CONFIG"])
                .unwrap();
        assert!(plain.get("web_search").is_none());
    }

    #[test]
    fn installed_adapters() {
        let c = AgentsConfig::builtin();
        let paths = Paths::new("/h");
        // Actuo's pinned Node runs the adapter script.
        let (program, args) = c.agents["claude"].program(&paths);
        assert_eq!(
            program,
            format!("/h/node/{}/bin/node", crate::node::VERSION)
        );
        assert_eq!(
            args,
            [
                "/h/agents/@agentclientprotocol/claude-agent-acp@0.84.0/node_modules/.bin/claude-agent-acp"
            ]
        );
        // agent/list shows the adapter.
        assert!(
            c.agents["claude"]
                .adapter(&paths)
                .unwrap()
                .ends_with(".bin/claude-agent-acp")
        );
        let mut a = c.agents["claude"].clone();
        a.bin = None;
        assert!(a.check().is_err());
        let mut a = c.agents["claude"].clone();
        a.package = None;
        assert!(a.check().unwrap_err().contains("needs command"));
        let mut a = c.agents["claude"].clone();
        a.sandbox.read = vec!["~/.ssh".into()];
        assert!(a.check().unwrap_err().contains("sandbox.read"));
        a.sandbox.read = vec!["~/.nvm".into()];
        a.check().unwrap();
        a.sandbox.dirs = vec!["NOPE".into()];
        assert!(a.check().unwrap_err().contains("sandbox.dirs"));
    }

    #[test]
    fn read_entries() {
        for ok in ["~/.nvm", "~/.bun", "/opt/homebrew", "~/Library/pnpm"] {
            check_read_entry(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "~",
            "~/",
            "/",
            "~/.ssh",
            "~/.ssh/id",
            "~/.config",
            "~/.config/gh/x",
            "~/.actuo",
            "~/.actuo/agents",
            "x/y",
            "~/a/../b",
            "~/.npmrc",
        ] {
            assert!(check_read_entry(bad).is_err(), "{bad}");
        }
        assert!(check_read_entry("~/.config/actuo").is_err());
    }

    #[test]
    fn pass_env_names() {
        assert_eq!(
            AgentsConfig::builtin().agents["claude"].pass_env,
            ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]
        );
        assert!(AgentsConfig::builtin().agents["codex"].pass_env.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.toml");
        let load = |names: &str| {
            std::fs::write(
                &path,
                format!("[agents.x]\ncommand = \"/bin/true\"\npass_env = {names}\n"),
            )
            .unwrap();
            AgentsConfig::load(&path)
        };
        let ok = load(r#"["MY_TOKEN_2"]"#).unwrap();
        assert_eq!(ok.agents["x"].pass_env, ["MY_TOKEN_2"]);
        for bad in [
            "PATH",
            "HOME",
            "TMPDIR",
            "TMPPREFIX",
            "CLAUDE_CODE_TMPDIR",
            "GIT_CONFIG_GLOBAL",
            "ACTUO_HOME",
            "ACTUO_SRT_RECORDS_FD",
            "NODE_OPTIONS",
            "SRT_DEBUG",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "my_token",
            "2X",
            "",
            "A-B",
        ] {
            let e = load(&format!("[{bad:?}]")).unwrap_err();
            assert!(e.contains("pass_env"), "{bad}: {e}");
        }
        // Never over what the agent's own config sets, in any layer.
        let own = |table: &str| {
            std::fs::write(
                &path,
                format!("[agents.x]\ncommand = \"/bin/true\"\npass_env = [\"MINE\"]\n{table}\n"),
            )
            .unwrap();
            AgentsConfig::load(&path)
        };
        for table in [
            "env = { MINE = \"1\" }",
            "env_json.MINE = { a = 1 }",
            "clean.env = { MINE = \"1\" }",
            "clean.env_json.MINE = { a = 1 }",
            "sandbox.env = { MINE = \"1\" }",
            "sandbox.env_json.MINE = { a = 1 }",
            "sandbox.tmp_env = [\"MINE\"]",
            "web_tools_off.env = { MINE = \"1\" }",
            "telemetry_off.env_json.MINE = { a = 1 }",
        ] {
            let e = own(table).unwrap_err();
            assert!(e.contains("set by this agent's own config"), "{table}: {e}");
        }
        // The built-in claude's names are set nowhere in its config.
        assert!(own("env = { OTHER = \"1\" }").is_ok());
        // A file's pass_env over the built-in claude's is checked against its layers.
        std::fs::write(
            &path,
            "[agents.claude]\npass_env = [\"CLAUDE_CODE_DISABLE_AUTO_MEMORY\"]\n",
        )
        .unwrap();
        let e = AgentsConfig::load(&path).unwrap_err();
        assert!(e.contains("set by this agent's own config"), "{e}");
    }

    #[test]
    fn socket_path_fits() {
        let p = Paths::new("/Users/someone/.actuo");
        assert!(p.socket().as_os_str().len() <= MAX_SOCKET_PATH);
        assert!(
            p.worktree(&ProjectId::from("p"), &SessionId::from("s"))
                .ends_with("worktrees/p/s")
        );
    }
}

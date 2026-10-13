//! `~/.actuo/settings.toml`: loading, validating and patching actuo's settings.
//!
//! An update re-reads the file from disk, applies the patch to it with `toml_edit`
//! (comments and order stay), validates the result and only then writes it (temp
//! file, then rename). So a hand edit made while actuo runs isn't lost.

use crate::config::AgentsConfig;
use actuo_protocol::errors::{ErrorKind, RpcError};
use actuo_protocol::settings::Settings;
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Duration;
use toml_edit::{DocumentMut, Item, TableLike};

/// Why settings don't validate: the dotted key, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    pub field: String,
    pub reason: String,
}

impl SettingsError {
    fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.field, self.reason)
        }
    }
}

impl From<SettingsError> for RpcError {
    fn from(e: SettingsError) -> Self {
        RpcError::new(ErrorKind::InvalidSettings, e.to_string())
            .with_detail("field", e.field)
            .with_detail("reason", e.reason)
    }
}

pub type Result<T> = std::result::Result<T, SettingsError>;

/// Reads and validates the file; a missing file is the defaults.
pub fn load(path: &Path, agents: &AgentsConfig) -> Result<Settings> {
    parse(&read(path)?, agents)
}

/// Re-reads the file, applies `patch` (a JSON merge patch by the file's keys),
/// validates the result and writes it. Nothing is written if it doesn't validate.
pub fn update(path: &Path, patch: &Value, agents: &AgentsConfig) -> Result<Settings> {
    let Value::Object(patch) = patch else {
        return Err(SettingsError::new("", "the patch must be an object"));
    };
    let text = read(path)?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| SettingsError::new("", format!("{}: {e}", path.display())))?;
    apply(doc.as_table_mut(), patch, "")?;
    let text = doc.to_string();
    let settings = parse(&text, agents)?;
    write(path, &text).map_err(|e| SettingsError::new("", format!("{}: {e}", path.display())))?;
    Ok(settings)
}

/// The file's text, as an update would read it (empty when missing).
pub fn text(path: &Path) -> Result<String> {
    read(path)
}

/// Validates `text` and writes it as the whole file (`actuo settings edit`).
/// Nothing is written if it doesn't validate.
pub fn replace(path: &Path, text: &str, agents: &AgentsConfig) -> Result<Settings> {
    let settings = parse(text, agents)?;
    write(path, text).map_err(|e| SettingsError::new("", format!("{}: {e}", path.display())))?;
    Ok(settings)
}

/// Parses settings from TOML text and validates them.
pub fn parse(text: &str, agents: &AgentsConfig) -> Result<Settings> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| SettingsError::new("", e.to_string()))?;
    let json = serde_json::to_value(table).map_err(|e| SettingsError::new("", e.to_string()))?;
    let settings: Settings = serde_path_to_error::deserialize(json).map_err(|e| {
        let field = e.path().to_string();
        SettingsError::new(
            if field == "." { String::new() } else { field },
            e.into_inner().to_string(),
        )
    })?;
    validate(&settings, agents)?;
    Ok(settings)
}

/// Reads the file; empty when it's missing.
fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(SettingsError::new("", format!("{}: {e}", path.display()))),
    }
}

/// Writes `text` to a temp file (0600) next to `path`, then renames it over `path`.
fn write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Merge-patches a TOML table in place: objects merge, `null` removes, anything
/// else replaces (arrays whole). New tables are implicit, so a table holding only
/// other tables gets no header of its own; a table a removal leaves empty goes too
/// (no stray `[lane.rules]`). Returns whether anything was removed.
fn apply(table: &mut dyn TableLike, patch: &Map<String, Value>, at: &str) -> Result<bool> {
    let mut removed = false;
    for (key, value) in patch {
        let field = if at.is_empty() {
            key.clone()
        } else {
            format!("{at}.{key}")
        };
        match value {
            Value::Null => {
                removed |= table.remove(key).is_some();
            }
            Value::Object(inner) => {
                if !table.get(key).is_some_and(Item::is_table_like) {
                    let mut t = toml_edit::Table::new();
                    t.set_implicit(true);
                    table.insert(key, Item::Table(t));
                }
                let item = table.get_mut(key).expect("just inserted");
                let inner_table = item.as_table_like_mut().expect("a table");
                if apply(inner_table, inner, &field)? {
                    removed = true;
                    if inner_table.is_empty() {
                        table.remove(key);
                    }
                }
            }
            v => {
                table.insert(key, Item::Value(to_toml(v, &field)?));
            }
        }
    }
    Ok(removed)
}

fn to_toml(v: &Value, field: &str) -> Result<toml_edit::Value> {
    Ok(match v {
        Value::Null => {
            return Err(SettingsError::new(
                field,
                "null is only allowed to remove a key",
            ));
        }
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n
                .as_f64()
                .ok_or_else(|| SettingsError::new(field, "number out of range"))?
                .into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => {
            let mut a = toml_edit::Array::new();
            for item in items {
                a.push(to_toml(item, field)?);
            }
            toml_edit::Value::Array(a)
        }
        Value::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, item) in map {
                t.insert(k, to_toml(item, &format!("{field}.{k}"))?);
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}

/// Checks what the types can't: names that must exist, durations, numbers, repo names.
pub fn validate(s: &Settings, agents: &AgentsConfig) -> Result<()> {
    let agent = |field: &str, name: &str| {
        if agents.agents.contains_key(name) {
            Ok(())
        } else {
            Err(SettingsError::new(
                field,
                format!("no agent named {name} in the agents config"),
            ))
        }
    };
    agent("sessions.agent", &s.sessions.agent)?;
    if s.sessions.idle_agent_timeout != "off" {
        duration(
            "sessions.idle_agent_timeout",
            &s.sessions.idle_agent_timeout,
        )?;
    }
    for (name, values) in &s.defaults {
        agent(&format!("defaults.{name}"), name)?;
        no_mode(&format!("defaults.{name}"), values)?;
    }

    let lane = &s.lane;
    for name in lane.rules.keys() {
        agent(&format!("lane.rules.{name}"), name)?;
    }
    duration("lane.poll_interval", &lane.poll_interval)?;
    duration("lane.turn_time_limit", &lane.turn_time_limit)?;
    duration("lane.run_time_limit", &lane.run_time_limit)?;
    for (field, n) in [
        ("lane.max_runs", lane.max_runs),
        ("lane.output_tail_lines", lane.output_tail_lines),
        ("lane.output_tail_bytes", lane.output_tail_bytes),
        ("lane.max_prompt_bytes", lane.max_prompt_bytes),
    ] {
        positive(field, n)?;
    }
    protected_paths("lane.protected_paths", &lane.protected_paths)?;
    not_blank("lane.trusted_users", &lane.trusted_users)?;
    let l = &lane.labels;
    not_blank(
        "lane.labels",
        &[
            &l.trigger,
            &l.running,
            &l.waiting,
            &l.pr,
            &l.failed,
            &l.agent_prefix,
        ],
    )?;

    for (name, repo) in &s.projects {
        let at = format!("projects.{name}");
        if !is_repo_name(name) {
            return Err(SettingsError::new(&at, "expected owner/repo"));
        }
        // The agent its lane runs get when no label picks one: its own, else
        // actuo's; any agent actuo has.
        if let Some(own) = &repo.agent {
            agent(&format!("{at}.agent"), own)?;
        }
        if let Some(label) = &repo.label {
            not_blank(&format!("{at}.label"), std::slice::from_ref(label))?;
        }
        if let Some(users) = &repo.trusted_users {
            not_blank(&format!("{at}.trusted_users"), users)?;
        }
        for a in repo.rules.keys() {
            agent(&format!("{at}.rules.{a}"), a)?;
        }
        no_mode(&format!("{at}.config"), &repo.config)?;
        duration(&format!("{at}.setup_time_limit"), &repo.setup_time_limit)?;
        duration(&format!("{at}.check_time_limit"), &repo.check_time_limit)?;
        for (key, d) in [
            ("turn_time_limit", &repo.turn_time_limit),
            ("run_time_limit", &repo.run_time_limit),
        ] {
            if let Some(d) = d {
                duration(&format!("{at}.{key}"), d)?;
            }
        }
        for (key, n) in [
            ("output_tail_lines", repo.output_tail_lines),
            ("output_tail_bytes", repo.output_tail_bytes),
            ("max_prompt_bytes", repo.max_prompt_bytes),
        ] {
            if let Some(n) = n {
                positive(&format!("{at}.{key}"), n)?;
            }
        }
        not_blank(
            &format!("{at}.setup"),
            repo.setup.as_deref().unwrap_or_default(),
        )?;
        not_blank(
            &format!("{at}.checks"),
            repo.checks.as_deref().unwrap_or_default(),
        )?;
        not_blank(
            &format!("{at}.network"),
            repo.network.as_deref().unwrap_or_default(),
        )?;
        for entry in &repo.read {
            crate::config::check_read_entry(entry)
                .map_err(|e| SettingsError::new(format!("{at}.read"), e))?;
        }
        for (field, ports) in [
            ("checks_local_ports", &repo.checks_local_ports),
            ("checks_local_listen_ports", &repo.checks_local_listen_ports),
        ] {
            if ports.contains(&0) || ports.len() > 64 {
                return Err(SettingsError::new(
                    format!("{at}.{field}"),
                    "at most 64 ports, each from 1 to 65535",
                ));
            }
            if !cfg!(target_os = "macos") && !ports.is_empty() {
                return Err(SettingsError::new(format!("{at}.{field}"), "macOS only"));
            }
        }
        if let Some(p) = &repo.protected_paths {
            protected_paths(&format!("{at}.protected_paths"), p)?;
        }
        if repo
            .commit_check
            .as_ref()
            .is_some_and(|c| c.trim().is_empty())
        {
            return Err(SettingsError::new(
                format!("{at}.commit_check"),
                "can't be empty",
            ));
        }
    }
    Ok(())
}

/// A duration with a unit: `30s`, `15m`, `2h`; more than zero.
pub fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let bad = || format!("{s:?} isn't a duration like 30s, 15m or 2h");
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?);
    let n: u64 = n.parse().map_err(|_| bad())?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(bad()),
    };
    if secs == 0 {
        return Err(format!("{s:?} must be more than zero"));
    }
    Ok(Duration::from_secs(secs))
}

/// `sessions.idle_agent_timeout`: `None` when `off`.
pub fn idle_agent_timeout(s: &Settings) -> Option<Duration> {
    match s.sessions.idle_agent_timeout.as_str() {
        "off" => None,
        d => parse_duration(d).ok(),
    }
}

/// `mode` isn't a default: the level sets it, as in `session/start`.
fn no_mode<V>(field: &str, values: &std::collections::BTreeMap<String, V>) -> Result<()> {
    if values.contains_key("mode") {
        return Err(SettingsError::new(
            format!("{field}.mode"),
            "`mode` isn't set through config; the level sets it",
        ));
    }
    Ok(())
}

fn duration(field: &str, s: &str) -> Result<()> {
    parse_duration(s)
        .map(|_| ())
        .map_err(|r| SettingsError::new(field, r))
}

fn positive(field: &str, n: u32) -> Result<()> {
    if n == 0 {
        return Err(SettingsError::new(field, "must be more than zero"));
    }
    Ok(())
}

/// Not blank, and each entry with a `*` a glob that compiles.
fn protected_paths(field: &str, items: &[String]) -> Result<()> {
    not_blank(field, items)?;
    for p in items.iter().filter(|p| p.contains('*')) {
        crate::lane::git::protected_glob(p)
            .map_err(|e| SettingsError::new(field, format!("{p:?} isn't a valid glob: {e}")))?;
    }
    Ok(())
}

fn not_blank(field: &str, items: &[impl AsRef<str>]) -> Result<()> {
    if items.iter().any(|i| i.as_ref().trim().is_empty()) {
        return Err(SettingsError::new(field, "an entry is empty"));
    }
    Ok(())
}

pub(crate) fn is_repo_name(s: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!(s.split_once('/'), Some((owner, repo)) if ok(owner) && ok(repo))
}

/// A dotted key as the CLI takes it (`projects."o/r".checks`), in TOML key syntax.
pub fn parse_key(key: &str) -> std::result::Result<Vec<String>, String> {
    toml_edit::Key::parse(key)
        .map(|keys| keys.into_iter().map(|k| k.get().to_string()).collect())
        .map_err(|e| format!("{key:?} isn't a key: {e}"))
}

/// A value as the CLI takes it: a TOML value (`3`, `true`, `"15m"`, `["a"]`), else
/// the text as a string.
pub fn parse_value(text: &str) -> Value {
    match format!("v = {text}").parse::<toml::Table>() {
        Ok(mut t) => match t.remove("v") {
            Some(toml::Value::Datetime(d)) => Value::String(d.to_string()),
            Some(v) => serde_json::to_value(v).unwrap_or_else(|_| Value::String(text.into())),
            None => Value::String(text.into()),
        },
        Err(_) => Value::String(text.into()),
    }
}

/// The merge patch that sets `keys` to `value`.
pub fn patch_for(keys: &[String], value: Value) -> Value {
    keys.iter().rev().fold(value, |inner, key| {
        Value::Object(Map::from_iter([(key.clone(), inner)]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actuo_protocol::types::ConfigValue;
    use serde_json::json;

    fn agents() -> AgentsConfig {
        AgentsConfig::builtin()
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir.path().join("settings.toml"), &agents()).unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(idle_agent_timeout(&s), Some(Duration::from_secs(900)));
    }

    #[test]
    fn local_check_ports_are_explicit_bounded_and_platform_specific() {
        let a = agents();
        for field in ["checks_local_ports", "checks_local_listen_ports"] {
            assert!(parse(&format!("[projects.\"o/r\"]\n{field} = []\n"), &a).is_ok());
            for ports in [
                "[0]".to_string(),
                "[65536]".to_string(),
                format!("[{}]", vec!["80"; 65].join(",")),
            ] {
                assert!(parse(&format!("[projects.\"o/r\"]\n{field} = {ports}\n"), &a).is_err());
            }
            let result = parse(
                &format!("[projects.\"o/r\"]\n{field} = [55421, 55422]\n"),
                &a,
            );
            assert_eq!(result.is_ok(), cfg!(target_os = "macos"));
        }
    }

    #[test]
    fn validation_names_the_field() {
        let a = agents();
        let fails = |text: &str| parse(text, &a).unwrap_err();
        let e = fails("[lane]\nmax_run = 3\n");
        assert_eq!(e.field, "lane.max_run");
        assert!(e.reason.contains("unknown field `max_run`"), "{e}");
        let e = fails("[lane]\nmax_runs = \"two\"\n");
        assert_eq!(e.field, "lane.max_runs");
        assert_eq!(fails("[lane]\nmax_runs = 0\n").field, "lane.max_runs");
        assert_eq!(
            fails("[lane]\npoll_interval = \"60\"\n").field,
            "lane.poll_interval"
        );
        assert_eq!(
            fails("[lane]\npoll_interval = \"0s\"\n").field,
            "lane.poll_interval"
        );
        assert_eq!(
            fails("[sessions]\nagent = \"nope\"\n").field,
            "sessions.agent"
        );
        assert_eq!(
            fails("[defaults]\nnope = { model = \"x\" }\n").field,
            "defaults.nope"
        );
        assert_eq!(
            fails("[defaults]\nclaude = { mode = \"plan\" }\n").field,
            "defaults.claude.mode"
        );
        assert_eq!(
            fails("[projects.\"o/r\"]\nconfig = { mode = \"plan\" }\n").field,
            "projects.o/r.config.mode"
        );
        // 0 change-size limit: no limit.
        assert!(
            parse(
                "[lane]\nmax_changed_lines = 0\n[projects.\"o/r\"]\nmax_changed_files = 0\n",
                &a
            )
            .is_ok()
        );
        // A project's agent may be any agent actuo has.
        assert!(parse("[projects.\"o/r\"]\nagent = \"codex\"\n", &a).is_ok());
        assert!(parse("[sessions]\nagent = \"codex\"\n[projects.\"o/r\"]\n", &a).is_ok());
        assert_eq!(
            fails("[projects.\"o/r\"]\nagent = \"nope\"\n").field,
            "projects.o/r.agent"
        );
        assert_eq!(
            fails("[projects.\"not-a-repo\"]\n").field,
            "projects.not-a-repo"
        );
        assert_eq!(
            fails("[projects.\"o/r\"]\nsetup_time_limit = \"soon\"\n").field,
            "projects.o/r.setup_time_limit"
        );
        for bad in [
            "~",
            "~/.ssh/x",
            "~/.config",
            "relative",
            "~/.actuo",
            "/opt/../etc",
        ] {
            let e = fails(&format!("[projects.\"o/r\"]\nread = [\"{bad}\"]\n"));
            assert_eq!(e.field, "projects.o/r.read", "{bad}");
        }
        assert_eq!(
            fails("[projects.\"o/r\"]\ncommit_check = \" \"\n").field,
            "projects.o/r.commit_check"
        );
        let e = fails("[projects.\"o/r\"]\nprotected_paths = [\"src/[*.ts\"]\n");
        assert_eq!(e.field, "projects.o/r.protected_paths");
        assert!(e.reason.contains("\"src/[*.ts\" isn't a valid glob"), "{e}");
        assert_eq!(
            fails("[lane]\nprotected_paths = [\"**/[x*\"]\n").field,
            "lane.protected_paths"
        );
        parse(
            "[projects.\"o/r\"]\nprotected_paths = [\"**/*-snapshots/\", \"packages/*/data/generated/\"]\n",
            &a,
        )
        .unwrap();
        // `pattern` isn't a key: refused.
        fails("[projects.\"o/r\"]\ncommit_subject = { pattern = \"x\" }\n");
        let e =
            fails("[[lane.rules.claude]]\nkind = \"execute\"\nanswer = [\"allow_sometimes\"]\n");
        assert!(e.field.starts_with("lane.rules.claude"), "{e:?}");
        assert!(fails("not toml =").field.is_empty());

        let ok = parse(
            "[sessions]\nidle_agent_timeout = \"off\"\n[[lane.rules.claude]]\nkind = \"edit\"\ninside_worktree = true\nanswer = [\"allow_once\"]\n[projects.\"o/r\"]\nagent = \"codex\"\nconfig = { model = \"x\" }\nchecks = [\"pnpm test\"]\nmax_check_retries = 0\n",
            &a,
        )
        .unwrap();
        assert_eq!(idle_agent_timeout(&ok), None);
        assert_eq!(ok.lane.rules["claude"][0].inside_worktree, Some(true));
        assert_eq!(ok.projects["o/r"].max_check_retries, Some(0));
        assert_eq!(ok.projects["o/r"].agent.as_deref(), Some("codex"));
        assert_eq!(
            ok.projects["o/r"].config["model"],
            ConfigValue::String("x".into())
        );
        let repo = &ok.projects["o/r"];
        assert_eq!((&repo.commit_check, &repo.commit_subject), (&None, &None));
        assert!(repo.co_authored_by);
        let rules = parse(
            "[projects.\"o/r\"]\ncommit_check = \"pnpm exec commitlint --edit\"\ncommit_subject = { prompt = \"Title: x.\" }\nco_authored_by = false\n",
            &a,
        )
        .unwrap();
        let repo = &rules.projects["o/r"];
        assert_eq!(
            repo.commit_check.as_deref(),
            Some("pnpm exec commitlint --edit")
        );
        assert_eq!(
            repo.commit_subject
                .as_ref()
                .and_then(|c| c.prompt.as_deref()),
            Some("Title: x.")
        );
        assert!(!repo.co_authored_by);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["", "m", "15", "1.5h", "-1m", "15 m", "1d", "0m"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn update_keeps_comments_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "# my settings\n[lane]\nmax_runs = 3 # busy\n\n[defaults]\nclaude = { model = \"opus\", effort = \"high\" }\n",
        )
        .unwrap();
        let s = update(
            &path,
            &json!({
                "lane": {"poll_interval": "30s"},
                "projects": {"o/r": {"checks": ["pnpm test"], "agent": "claude"}},
                "defaults": {"claude": {"effort": "low"}},
                "sessions": {"agent": "codex"}
            }),
            &agents(),
        )
        .unwrap();
        assert_eq!(s.lane.max_runs, 3);
        assert_eq!(s.lane.poll_interval, "30s");
        assert_eq!(
            s.projects["o/r"].checks,
            Some(vec!["pnpm test".to_string()])
        );
        assert_eq!(s.sessions.agent, "codex");
        let claude = &s.defaults["claude"];
        assert_eq!(
            serde_json::to_value(claude).unwrap(),
            json!({"model": "opus", "effort": "low"})
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# my settings\n"), "{text}");
        assert!(text.contains("max_runs = 3 # busy"), "{text}");
        assert!(text.contains("[projects.\"o/r\"]"), "{text}");
        assert!(!text.contains("[projects]\n"), "{text}");
        assert_eq!(load(&path, &agents()).unwrap(), s);

        // null removes a key: its default applies again.
        let s = update(&path, &json!({"lane": {"max_runs": null}}), &agents()).unwrap();
        assert_eq!(s.lane.max_runs, 2);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("max_runs"));

        // Arrays are replaced whole, including lists of tables.
        let s = update(
            &path,
            &json!({"lane": {"rules": {"claude": [{"kind": "execute", "answer": ["allow_once"]}]}}}),
            &agents(),
        )
        .unwrap();
        assert_eq!(s.lane.rules["claude"].len(), 1);
    }

    #[test]
    fn invalid_update_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[lane]\nmax_runs = 3\n").unwrap();
        let e = update(&path, &json!({"lane": {"max_runs": 0}}), &agents()).unwrap_err();
        assert_eq!(e.field, "lane.max_runs");
        let e = update(&path, &json!({"lane": {"agents": ["nope"]}}), &agents()).unwrap_err();
        assert_eq!(e.field, "lane.agents");
        assert!(update(&path, &json!([1]), &agents()).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[lane]\nmax_runs = 3\n"
        );
        let rpc: RpcError = e.into();
        assert_eq!(rpc.kind(), Some(ErrorKind::InvalidSettings));
        assert_eq!(rpc.data.unwrap().details["field"], "lane.agents");
    }

    #[test]
    fn update_rereads_a_hand_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let a = agents();
        update(&path, &json!({"lane": {"max_runs": 3}}), &a).unwrap();
        // Edited by hand while actuo runs.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("trusted_users = [\"someone\"]\n");
        std::fs::write(&path, text).unwrap();
        let s = update(&path, &json!({"lane": {"poll_interval": "2m"}}), &a).unwrap();
        assert_eq!(s.lane.trusted_users, vec!["someone"]);
        assert_eq!(s.lane.max_runs, 3);
        assert_eq!(s.lane.poll_interval, "2m");
        // A hand edit that doesn't validate fails the next update, naming the field.
        std::fs::write(&path, "[lane]\nmax_runs = -1\n").unwrap();
        let e = update(&path, &json!({"lane": {"poll_interval": "2m"}}), &a).unwrap_err();
        assert_eq!(e.field, "lane.max_runs");
    }

    #[test]
    #[cfg(unix)]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        update(&path, &json!({"lane": {"max_runs": 3}}), &agents()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn cli_keys_and_values() {
        assert_eq!(
            parse_key("lane.max_runs").unwrap(),
            vec!["lane", "max_runs"]
        );
        assert_eq!(
            parse_key("projects.\"o/r\".checks").unwrap(),
            vec!["projects", "o/r", "checks"]
        );
        assert!(parse_key("lane..x").is_err());
        assert_eq!(parse_value("3"), json!(3));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value("\"15m\""), json!("15m"));
        assert_eq!(parse_value("15m"), json!("15m"));
        assert_eq!(parse_value("[\"pnpm test\"]"), json!(["pnpm test"]));
        assert_eq!(parse_value("codex"), json!("codex"));
        assert_eq!(
            patch_for(&["lane".into(), "max_runs".into()], json!(3)),
            json!({"lane": {"max_runs": 3}})
        );
    }
}

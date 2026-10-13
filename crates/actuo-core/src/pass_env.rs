//! Tokens and API keys the user put in actuo's environment (its service's), or in
//! `~/.config/actuo/agents.env` ([`file`]), for the agents whose `pass_env` names
//! them, e.g. `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`. The user writes
//! them; actuo never asks for or stores one. `actuo serve` keeps their values when
//! it starts; from then on every process it starts has them removed
//! ([`crate::process`]) unless the command sets them itself, which only an agent's
//! spawn does, with the ones its `pass_env` names. So git, gh, setup, checks and
//! logins never get them. The values are never logged or shown; the names are
//! (`actuo agents`, the log).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static KEPT: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// `~/.config/actuo/agents.env`: `NAME=value` lines the user writes, mode 0600.
/// The same on macOS and Linux, and kept across updates (the service files aren't).
pub fn file() -> PathBuf {
    // Tests point it elsewhere, never at the real one (debug builds only).
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("ACTUO_AGENTS_ENV") {
        return PathBuf::from(p);
    }
    PathBuf::from(crate::config::expand_home("~/.config/actuo/agents.env"))
}

/// Keeps the values of `names`: each from this process's environment when set
/// there (and not empty), else from `file`. The file is read only when a name is
/// still missing, only if it exists, and refused unless it's a regular file of
/// this user that no one else may read or write. Returns the names kept. Only the
/// first call keeps anything.
pub fn keep_from_env_and_file<I: IntoIterator<Item = String>>(
    names: I,
    file: &Path,
) -> Result<Vec<String>, String> {
    let names: Vec<String> = names.into_iter().collect();
    let mut values = BTreeMap::new();
    for n in &names {
        if let Ok(v) = std::env::var(n)
            && !v.is_empty()
        {
            values.insert(n.clone(), v);
        }
    }
    if names.iter().any(|n| !values.contains_key(n)) {
        for (n, v) in read(file)? {
            if names.contains(&n) && !values.contains_key(&n) && !v.is_empty() {
                values.insert(n, v);
            }
        }
    }
    Ok(keep(values))
}

/// The file's `NAME=value` pairs; none when it doesn't exist. Errors name the file
/// and the line, never a value.
fn read(file: &Path) -> Result<Vec<(String, String)>, String> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    if !meta.is_file() {
        return Err(format!("{} isn't a file", file.display()));
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(format!("{} belongs to another user", file.display()));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} can be read or written by others (mode {:o}); make it yours only: chmod 600 {}",
            file.display(),
            meta.mode() & 0o777,
            file.display()
        ));
    }
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", file.display()))
}

/// `NAME=value` lines, as a systemd `EnvironmentFile` has them: blank lines and
/// `#` comments skipped, one pair of matching quotes around a value removed.
fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected NAME=value", i + 1));
        };
        let name = name.trim();
        let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!("line {}: not a variable name", i + 1));
        }
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
            .unwrap_or(value);
        pairs.push((name.to_string(), value.to_string()));
    }
    Ok(pairs)
}

/// Keeps `values` (what [`keep_from_env_and_file`] does after reading the
/// environment; tests call it directly). Returns the names kept. Only the first call keeps anything.
pub fn keep(values: BTreeMap<String, String>) -> Vec<String> {
    let names = values.keys().cloned().collect();
    let _ = KEPT.set(values);
    names
}

fn kept() -> &'static BTreeMap<String, String> {
    static NONE: BTreeMap<String, String> = BTreeMap::new();
    KEPT.get().unwrap_or(&NONE)
}

/// Every kept name: removed from each process actuo starts that doesn't set it.
pub fn names() -> impl Iterator<Item = &'static str> {
    kept().keys().map(String::as_str)
}

/// The names in an agent's `pass_env` that actuo kept: what it passes.
pub fn names_for(pass_env: &[String]) -> Vec<String> {
    pass_env
        .iter()
        .filter(|n| kept().contains_key(*n))
        .cloned()
        .collect()
}

/// The variables an agent's processes get: those its `pass_env` names.
pub fn env_for(pass_env: &[String]) -> BTreeMap<String, String> {
    pass_env
        .iter()
        .filter_map(|n| kept().get(n).map(|v| (n.clone(), v.clone())))
        .collect()
}

/// Every kept value, for the credential check before anything is published.
pub fn values() -> impl Iterator<Item = &'static str> {
    kept().values().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn lines_as_systemd_reads_them() {
        let pairs = parse(
            "# Claude, from `claude setup-token`\n\nCLAUDE_CODE_OAUTH_TOKEN=\"a b\"\n  OTHER = 'x' \nPLAIN=y=z\n",
        )
        .unwrap();
        assert_eq!(
            pairs,
            [
                ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "a b".to_string()),
                ("OTHER".to_string(), "x".to_string()),
                ("PLAIN".to_string(), "y=z".to_string()),
            ]
        );
        // The error names the line, never what's on it.
        let e = parse("A=1\nsecret-without-a-name\n").unwrap_err();
        assert_eq!(e, "line 2: expected NAME=value");
        assert_eq!(parse("1A=x\n").unwrap_err(), "line 1: not a variable name");
    }

    #[test]
    fn only_a_file_no_one_else_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agents.env");
        assert!(read(&file).unwrap().is_empty(), "no file: nothing");
        std::fs::write(&file, "T=v\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read(&file).unwrap_err();
        assert!(e.contains("mode 644") && e.contains("chmod 600"), "{e}");
        assert!(!e.contains("T=v"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&file).unwrap(), [("T".to_string(), "v".to_string())]);
        assert!(read(dir.path()).unwrap_err().contains("isn't a file"));
    }
}

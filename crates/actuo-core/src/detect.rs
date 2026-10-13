//! What a repo needs, from its files alone (nothing installed or run): its package
//! manager, workspace tool, workspaces and their scripts, Node version; and the
//! setup and checks proposed from that (`project/inspect`).

use actuo_protocol::methods::{Detected, Workspace};
use actuo_protocol::settings::ProjectSettings;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The scripts a check can run.
const SCRIPTS: [&str; 3] = ["lint", "typecheck", "test"];

/// A repo's files at one revision.
pub trait Files {
    /// A file's text, if it exists.
    fn read(&self, path: &str) -> Option<String>;
    /// The folders directly in `dir` (`""` for the root), by path from the root.
    fn dirs(&self, dir: &str) -> Vec<String>;
}

/// A repo's files at a revision, read through git (the checkout isn't touched).
pub struct GitFiles {
    pub repo: PathBuf,
    pub rev: String,
}

impl Files for GitFiles {
    fn read(&self, path: &str) -> Option<String> {
        crate::git::git(&self.repo, ["show", &format!("{}:{path}", self.rev)], &[]).ok()
    }

    fn dirs(&self, dir: &str) -> Vec<String> {
        let tree = if dir.is_empty() {
            self.rev.clone()
        } else {
            format!("{}:{dir}", self.rev)
        };
        let Ok(out) = crate::git::git(&self.repo, ["ls-tree", "-d", "--name-only", &tree], &[])
        else {
            return Vec::new();
        };
        out.lines()
            .map(|name| {
                if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                }
            })
            .collect()
    }
}

/// A worktree's files as they are on disk (a run's, after the agent's turn).
/// `node_modules` and dot folders aren't looked into.
pub struct DirFiles {
    pub root: PathBuf,
}

impl Files for DirFiles {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }

    fn dirs(&self, dir: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.root.join(dir)) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n != "node_modules" && !n.starts_with('.'))
            .map(|n| {
                if dir.is_empty() {
                    n
                } else {
                    format!("{dir}/{n}")
                }
            })
            .collect();
        out.sort();
        out
    }
}

/// Package registries: the agent may always reach these when they're in a
/// project's network (to install and scaffold).
pub const REGISTRIES: [&str; 2] = ["registry.npmjs.org", "registry.yarnpkg.com"];

/// The registry a fresh repo uses before it has a package manager.
pub const DEFAULT_REGISTRY: &str = "registry.npmjs.org";

/// GitHub's archive host, which some dependencies download from (Electron's
/// node-gyp). Setup only; not a registry the agent reaches.
pub const ARCHIVE_HOST: &str = "codeload.github.com";

/// An exact DNS name, lowercased: no wildcard, URL, credentials, port, IP address
/// or local name.
pub fn download_host(value: &str) -> Option<String> {
    let host = value.to_ascii_lowercase();
    if host.len() > 253
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    Some(host)
}

/// Download hosts named in the textual lockfiles, as a proposal: nothing is run or
/// fetched. A host ends before the URL's path, query or fragment or the lockfile's
/// syntax; hosts with credentials or an unusual port are dropped.
fn lockfile_hosts(f: &impl Files) -> BTreeSet<String> {
    static URL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"(?i)https?://([^/\s\"'<>?#,}\]]+)"#).unwrap());
    let mut hosts = BTreeSet::new();
    for path in [
        "pnpm-lock.yaml",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "bun.lock",
    ] {
        if let Some(text) = f.read(path) {
            // npm's lockfile also has funding and homepage URLs: only its
            // `resolved` fields are downloads. The other formats keep resolution
            // URLs in text entries (Bun's binary lockfile isn't read).
            let text = if path.ends_with(".json") {
                fn resolved(value: &Value, out: &mut Vec<String>) {
                    match value {
                        Value::Object(object) => {
                            for (key, value) in object {
                                if key == "resolved" {
                                    if let Some(url) = value.as_str() {
                                        out.push(url.to_string());
                                    }
                                } else {
                                    resolved(value, out);
                                }
                            }
                        }
                        Value::Array(items) => items.iter().for_each(|v| resolved(v, out)),
                        _ => {}
                    }
                }
                let mut urls = Vec::new();
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    resolved(&value, &mut urls);
                }
                urls.join("\n")
            } else {
                text.lines()
                    .filter(|line| !line.trim_start().starts_with(['#', '/']))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            for capture in URL.captures_iter(&text) {
                let authority = &capture[1];
                let name = authority
                    .strip_suffix(":443")
                    .or_else(|| authority.strip_suffix(":80"))
                    .unwrap_or(authority);
                if let Some(host) = download_host(name) {
                    hosts.insert(host);
                }
            }
        }
    }
    hosts
}

/// A project's setup, checks and network as a run uses them: what's set, else
/// what the files say now (`files`: the run's worktree).
pub struct Resolved {
    pub setup: Vec<String>,
    pub checks: Vec<String>,
    pub network: Vec<String>,
}

pub fn resolve(
    setup: Option<&Vec<String>>,
    checks: Option<&Vec<String>>,
    network: Option<&Vec<String>>,
    files: &impl Files,
) -> Resolved {
    let detected = || propose(&detect(files), files);
    let p = if setup.is_none() || checks.is_none() || network.is_none() {
        Some(detected())
    } else {
        None
    };
    let pick = |set: Option<&Vec<String>>, found: Option<Vec<String>>| {
        set.cloned().or(found).unwrap_or_default()
    };
    let network = match network {
        Some(n) => n.clone(),
        // Unset: only the built-in defaults (npm's registry for a repo with no
        // package manager yet, so it can be scaffolded). Hosts read from the repo's
        // files are a proposal, never allowed by themselves; `project add` saves the
        // reviewed proposal, with any other lockfile hosts.
        None => default_network(&detect(files), files),
    };
    Resolved {
        setup: pick(setup, p.as_ref().and_then(|p| p.setup.clone())),
        checks: pick(checks, p.as_ref().and_then(|p| p.checks.clone())),
        network,
    }
}

pub fn default_network(d: &Detected, f: &impl Files) -> Vec<String> {
    let registry = if d.package_manager.as_deref() == Some("yarn") && yarn_classic(d, f) {
        "registry.yarnpkg.com"
    } else {
        DEFAULT_REGISTRY
    };
    let mut hosts = vec![registry.to_string()];
    if d.package_manager.is_some() {
        hosts.push(ARCHIVE_HOST.to_string());
    }
    hosts
}

/// The hosts the agent's session reaches besides its own: the registries in
/// `network`, or all of it with `agent_network`.
pub fn agent_hosts(network: &[String], agent_network: bool) -> Vec<String> {
    network
        .iter()
        .filter(|h| agent_network || REGISTRIES.contains(&h.as_str()))
        .cloned()
        .collect()
}

impl GitFiles {
    pub fn new(repo: &Path, rev: &str) -> Self {
        Self {
            repo: repo.to_path_buf(),
            rev: rev.to_string(),
        }
    }
}

pub fn detect(f: &impl Files) -> Detected {
    let root: Option<Value> = f
        .read("package.json")
        .and_then(|t| serde_json::from_str(&t).ok());
    let package_manager = [
        ("pnpm-lock.yaml", "pnpm"),
        ("package-lock.json", "npm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
    ]
    .iter()
    .find(|(lock, _)| f.read(lock).is_some())
    .map(|(_, pm)| pm.to_string());
    let package_manager_version = root
        .as_ref()
        .and_then(|r| r["packageManager"].as_str())
        .and_then(|pm| pm.split_once('@'))
        .map(|(_, v)| v.split('+').next().unwrap_or(v).to_string());
    let workspace_tool = if f.read("turbo.json").is_some() {
        Some("turbo".to_string())
    } else if f.read("nx.json").is_some() {
        Some("nx".to_string())
    } else {
        None
    };
    let node = root
        .as_ref()
        .and_then(|r| r["engines"]["node"].as_str())
        .map(str::to_string)
        .or_else(|| f.read(".nvmrc").map(|t| t.trim().to_string()))
        .filter(|n| !n.is_empty());

    let mut workspaces = Vec::new();
    if let Some(root) = &root {
        workspaces.push(workspace("", root));
    }
    for dir in workspace_dirs(f, root.as_ref()) {
        if let Some(pkg) = f
            .read(&format!("{dir}/package.json"))
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            workspaces.push(workspace(&dir, &pkg));
        }
    }
    Detected {
        package_manager,
        package_manager_version,
        workspace_tool,
        workspaces,
        node,
    }
}

fn workspace(path: &str, pkg: &Value) -> Workspace {
    Workspace {
        path: path.to_string(),
        name: pkg["name"].as_str().map(str::to_string),
        scripts: SCRIPTS
            .iter()
            .filter(|s| pkg["scripts"][**s].is_string())
            .map(|s| s.to_string())
            .collect(),
    }
}

/// The workspace folders: `pnpm-workspace.yaml`'s `packages`, else `package.json`'s
/// `workspaces`. A glob's last part may be `*` or `**` (one level is read).
fn workspace_dirs(f: &impl Files, root: Option<&Value>) -> Vec<String> {
    let globs: Vec<String> = match f.read("pnpm-workspace.yaml") {
        Some(yaml) => pnpm_packages(&yaml),
        None => {
            let w = root.map(|r| &r["workspaces"]);
            let list = w
                .and_then(|w| w.as_array().or_else(|| w["packages"].as_array()))
                .cloned()
                .unwrap_or_default();
            list.iter()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        }
    };
    let mut dirs = Vec::new();
    for glob in globs.iter().filter(|g| !g.starts_with('!')) {
        let glob = glob.trim_start_matches("./").trim_end_matches('/');
        match glob.rsplit_once('/') {
            Some((parent, "*" | "**")) => dirs.extend(f.dirs(parent)),
            None if glob == "*" || glob == "**" => dirs.extend(f.dirs("")),
            _ => dirs.push(glob.to_string()),
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The `packages:` list of a `pnpm-workspace.yaml` (its other keys aren't read).
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let mut in_packages = false;
    let mut out = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if !line.starts_with(' ') && !line.starts_with('-') && !trimmed.is_empty() {
            in_packages = trimmed == "packages:";
            continue;
        }
        if in_packages && let Some(item) = trimmed.strip_prefix('-') {
            let item = item.trim().trim_matches(|c| c == '\'' || c == '"');
            if !item.is_empty() {
                out.push(item.to_string());
            }
        }
    }
    out
}

/// Setup, network and checks from what was detected (`f`: the same files): the
/// package manager's frozen install; with turbo, its tasks for the changed packages
/// (`--affected`, against the run's base); else each script through the package
/// manager. Repo rules start empty; everything else is the lane's default.
pub fn propose(d: &Detected, f: &impl Files) -> ProjectSettings {
    let mut p = ProjectSettings::default();
    let Some(pm) = d.package_manager.as_deref() else {
        return p;
    };
    let classic = pm == "yarn" && yarn_classic(d, f);
    let (install, registry) = match pm {
        "pnpm" => ("pnpm install --frozen-lockfile", "registry.npmjs.org"),
        "npm" => ("npm ci", "registry.npmjs.org"),
        // yarn 1 ignores `--immutable` and has no `workspaces foreach`.
        "yarn" if classic => ("yarn install --frozen-lockfile", "registry.yarnpkg.com"),
        "yarn" => ("yarn install --immutable", "registry.yarnpkg.com"),
        "bun" => ("bun install --frozen-lockfile", "registry.npmjs.org"),
        _ => return p,
    };
    p.setup = Some(vec![install.to_string()]);
    let mut hosts = vec![registry.to_string(), ARCHIVE_HOST.to_string()];
    for host in lockfile_hosts(f) {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    p.network = Some(hosts);
    let mut checks = Vec::new();
    let has = |s: &str| {
        d.workspaces
            .iter()
            .any(|w| w.scripts.iter().any(|x| x == s))
    };
    let run = match pm {
        "npm" => "npx",
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        _ => "bunx",
    };
    if d.workspace_tool.as_deref() == Some("turbo") {
        let static_checks: Vec<&str> = ["lint", "typecheck"]
            .into_iter()
            .filter(|s| has(s))
            .collect();
        if !static_checks.is_empty() {
            checks.push(format!(
                "{run} turbo {} --affected",
                static_checks.join(" ")
            ));
        }
        if has("test") {
            checks.push(format!("{run} turbo test --affected"));
        }
        p.checks = Some(checks);
        return p;
    }
    let several = d.workspaces.iter().any(|w| !w.path.is_empty());
    for s in SCRIPTS.iter().filter(|s| has(s)) {
        if classic && several {
            checks.push(yarn_classic_run(d, s));
            continue;
        }
        checks.push(match (pm, several) {
            ("pnpm", true) => format!("pnpm -r --if-present run {s}"),
            ("npm", true) => format!("npm run {s} --workspaces --if-present"),
            ("yarn", true) => format!("yarn workspaces foreach -A run {s}"),
            ("bun", true) => format!("bun run --filter '*' {s}"),
            (pm, false) => format!("{pm} run {s}"),
            _ => unreachable!(),
        });
    }
    p.checks = Some(checks);
    p
}

/// yarn 1: `packageManager` says `yarn@1.…`, or, with none, the lockfile is a v1
/// one (yarn 2 and later write `__metadata:` instead).
fn yarn_classic(d: &Detected, f: &impl Files) -> bool {
    match &d.package_manager_version {
        Some(v) => v.starts_with("1."),
        None => f
            .read("yarn.lock")
            .is_some_and(|l| l.contains("# yarn lockfile v1")),
    }
}

/// yarn 1 has no "run where present": `script` in the root and each workspace that
/// has it, one after the other (`yarn workspaces run` fails on a workspace without
/// it).
fn yarn_classic_run(d: &Detected, script: &str) -> String {
    d.workspaces
        .iter()
        .filter(|w| w.scripts.iter().any(|x| x == script))
        .map(|w| match (w.path.as_str(), &w.name) {
            ("", _) => format!("yarn run {script}"),
            (_, Some(name)) => format!("yarn workspace {} run {script}", quoted(name)),
            (path, None) => format!("yarn --cwd {} run {script}", quoted(path)),
        })
        .collect::<Vec<_>>()
        .join(" && ")
}

/// A name or path from the repo's files, as one shell word.
fn quoted(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@/._-".contains(c))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fixture(BTreeMap<&'static str, &'static str>);

    impl Files for Fixture {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).map(|s| s.to_string())
        }

        fn dirs(&self, dir: &str) -> Vec<String> {
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let mut out: Vec<String> = self
                .0
                .keys()
                .filter_map(|k| k.strip_prefix(prefix.as_str()))
                .filter_map(|rest| rest.split_once('/').map(|(d, _)| format!("{prefix}{d}")))
                .collect();
            out.dedup();
            out
        }
    }

    /// pnpm with turbo: the frozen install, turbo's tasks.
    #[test]
    fn pnpm_with_turbo() {
        let f = Fixture(BTreeMap::from([
            (
                "package.json",
                r#"{"packageManager": "pnpm@10.33.0", "engines": {"node": ">=24"}}"#,
            ),
            ("pnpm-lock.yaml", "lockfileVersion: '9.0'"),
            (
                "pnpm-workspace.yaml",
                "packages:\n  - 'apps/*'\n  - packages/*\nonlyBuiltDependencies:\n  - esbuild\n",
            ),
            ("turbo.json", "{}"),
            (
                "apps/web/package.json",
                r#"{"name": "web", "scripts": {"lint": "x", "typecheck": "x", "test": "x"}}"#,
            ),
            (
                "packages/ui/package.json",
                r#"{"name": "@o/ui", "scripts": {"lint": "x"}}"#,
            ),
        ]));
        let d = detect(&f);
        assert_eq!(d.package_manager.as_deref(), Some("pnpm"));
        assert_eq!(d.package_manager_version.as_deref(), Some("10.33.0"));
        assert_eq!(d.workspace_tool.as_deref(), Some("turbo"));
        assert_eq!(d.node.as_deref(), Some(">=24"));
        let paths: Vec<&str> = d.workspaces.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["", "apps/web", "packages/ui"]);
        assert_eq!(d.workspaces[1].scripts, vec!["lint", "typecheck", "test"]);
        let p = propose(&d, &f);
        assert_eq!(p.setup, Some(strings(&["pnpm install --frozen-lockfile"])));
        assert_eq!(
            p.network,
            Some(strings(&["registry.npmjs.org", ARCHIVE_HOST]))
        );
        assert_eq!(
            p.checks,
            Some(strings(&[
                "pnpm turbo lint typecheck --affected",
                "pnpm turbo test --affected"
            ]))
        );
        // Proposed with its lane off: you turn it on once it's set up.
        assert!(!p.lane);
    }

    /// pnpm without turbo: each script across the workspaces.
    #[test]
    fn pnpm_without_turbo() {
        let f = Fixture(BTreeMap::from([
            ("package.json", r#"{"scripts": {"test": "vitest"}}"#),
            ("pnpm-lock.yaml", ""),
            ("pnpm-workspace.yaml", "packages:\n  - libs/*\n"),
            ("libs/a/package.json", r#"{"scripts": {"lint": "eslint"}}"#),
        ]));
        let p = propose(&detect(&f), &f);
        assert_eq!(
            p.checks,
            Some(strings(&[
                "pnpm -r --if-present run lint",
                "pnpm -r --if-present run test"
            ]))
        );
    }

    /// npm, one package, Node from `.nvmrc`.
    #[test]
    fn npm_single_package() {
        let f = Fixture(BTreeMap::from([
            (
                "package.json",
                r#"{"scripts": {"test": "jest", "build": "tsc"}}"#,
            ),
            ("package-lock.json", "{}"),
            (".nvmrc", "22\n"),
        ]));
        let d = detect(&f);
        assert_eq!(d.node.as_deref(), Some("22"));
        let p = propose(&d, &f);
        assert_eq!(p.setup, Some(strings(&["npm ci"])));
        assert_eq!(p.checks, Some(strings(&["npm run test"])));
    }

    /// Two workspaces: a root with `packages/*`, `a` with lint
    /// and test, `b` with test only; the lockfile named.
    fn two_workspaces(lock: &'static str, text: &'static str, root: &'static str) -> Fixture {
        Fixture(BTreeMap::from([
            ("package.json", root),
            (lock, text),
            (
                "packages/a/package.json",
                r#"{"name": "a", "scripts": {"lint": "x", "test": "x"}}"#,
            ),
            (
                "packages/b/package.json",
                r#"{"name": "@o/b", "scripts": {"test": "x"}}"#,
            ),
        ]))
    }

    const ROOT: &str = r#"{"private": true, "workspaces": ["packages/*"]}"#;

    /// npm with workspaces: each script where present.
    #[test]
    fn npm_workspaces() {
        let f = two_workspaces("package-lock.json", "{}", ROOT);
        let p = propose(&detect(&f), &f);
        assert_eq!(p.setup, Some(strings(&["npm ci"])));
        assert_eq!(
            p.network,
            Some(strings(&["registry.npmjs.org", ARCHIVE_HOST]))
        );
        assert_eq!(
            p.checks,
            Some(strings(&[
                "npm run lint --workspaces --if-present",
                "npm run test --workspaces --if-present"
            ]))
        );
    }

    /// yarn 1, from its lockfile: its own frozen flag, and each workspace that has
    /// the script by name (`foreach` is yarn 2's; `workspaces run` fails on `b`).
    #[test]
    fn yarn_1_workspaces() {
        let f = two_workspaces(
            "yarn.lock",
            "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n# yarn lockfile v1\n",
            r#"{"private": true, "workspaces": ["packages/*"], "scripts": {"test": "x"}}"#,
        );
        let d = detect(&f);
        assert_eq!(d.package_manager.as_deref(), Some("yarn"));
        let p = propose(&d, &f);
        assert_eq!(p.setup, Some(strings(&["yarn install --frozen-lockfile"])));
        assert_eq!(
            p.network,
            Some(strings(&["registry.yarnpkg.com", ARCHIVE_HOST]))
        );
        assert_eq!(
            p.checks,
            Some(strings(&[
                "yarn workspace a run lint",
                "yarn run test && yarn workspace a run test && yarn workspace @o/b run test"
            ]))
        );
        // `packageManager` wins over the lockfile.
        let f = two_workspaces(
            "yarn.lock",
            "__metadata:\n  version: 8\n",
            r#"{"packageManager": "yarn@1.22.22", "workspaces": ["packages/*"]}"#,
        );
        let p = propose(&detect(&f), &f);
        assert_eq!(p.setup, Some(strings(&["yarn install --frozen-lockfile"])));
    }

    /// yarn 2 and later: `--immutable` and `workspaces foreach`.
    #[test]
    fn yarn_4_workspaces() {
        let f = two_workspaces(
            "yarn.lock",
            "__metadata:\n  version: 8\n  cacheKey: 10c0\n",
            r#"{"packageManager": "yarn@4.5.0", "workspaces": ["packages/*"]}"#,
        );
        let p = propose(&detect(&f), &f);
        assert_eq!(p.setup, Some(strings(&["yarn install --immutable"])));
        assert_eq!(
            p.checks,
            Some(strings(&[
                "yarn workspaces foreach -A run lint",
                "yarn workspaces foreach -A run test"
            ]))
        );
        // No `packageManager`: the lockfile says yarn 2+.
        let f = two_workspaces("yarn.lock", "__metadata:\n  version: 8\n", ROOT);
        let p = propose(&detect(&f), &f);
        assert_eq!(p.setup, Some(strings(&["yarn install --immutable"])));
    }

    /// bun with workspaces, from `bun.lock`.
    #[test]
    fn bun_workspaces() {
        let f = two_workspaces("bun.lock", "{\"lockfileVersion\": 1}", ROOT);
        let p = propose(&detect(&f), &f);
        assert_eq!(p.setup, Some(strings(&["bun install --frozen-lockfile"])));
        assert_eq!(
            p.network,
            Some(strings(&["registry.npmjs.org", ARCHIVE_HOST]))
        );
        assert_eq!(
            p.checks,
            Some(strings(&[
                "bun run --filter '*' lint",
                "bun run --filter '*' test"
            ]))
        );
    }

    /// A workspace name that isn't a plain word is quoted.
    #[test]
    fn quoted_names() {
        assert_eq!(quoted("@o/ui"), "@o/ui");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
    }

    /// No lockfile: nothing proposed.
    #[test]
    fn no_lockfile() {
        let f = Fixture(BTreeMap::from([("README.md", "x")]));
        let d = detect(&f);
        assert_eq!(d.package_manager, None);
        assert!(d.workspaces.is_empty());
        let p = propose(&d, &f);
        assert!(p.setup.is_none() && p.checks.is_none() && p.network.is_none());
    }

    #[test]
    fn download_hosts_are_proposed_but_never_granted_from_changed_files() {
        for (path, text) in [
            (
                "pnpm-lock.yaml",
                "resolution: {tarball: https://codeload.github.com/electron/node-gyp/tar.gz/abc}\nresolution: {tarball: https://Downloads.example.com/pkg.tgz}",
            ),
            (
                "package-lock.json",
                r#"{"packages":{"node_modules/pkg":{"resolved":"https://Downloads.example.com/pkg.tgz","funding":{"url":"https://funding.example.com"}}}}"#,
            ),
            (
                "yarn.lock",
                "pkg:\n  resolved \"https://Downloads.example.com/pkg.tgz#hash\"",
            ),
            (
                "bun.lock",
                r#"{"packages":{"pkg":["pkg@https://Downloads.example.com/pkg.tgz"]}}"#,
            ),
        ] {
            let f = Fixture(BTreeMap::from([(path, text)]));
            let proposed = propose(&detect(&f), &f).network.unwrap();
            assert!(
                proposed.contains(&"downloads.example.com".to_string()),
                "{path}"
            );
            assert!(!proposed.contains(&"funding.example.com".to_string()));
            assert_eq!(
                proposed
                    .iter()
                    .filter(|h| h.as_str() == ARCHIVE_HOST)
                    .count(),
                1
            );
            let resolved = resolve(None, None, None, &f);
            assert!(
                !resolved
                    .network
                    .contains(&"downloads.example.com".to_string())
            );
            assert!(resolved.network.contains(&ARCHIVE_HOST.to_string()));
            assert!(!agent_hosts(&resolved.network, false).contains(&ARCHIVE_HOST.to_string()));
            let explicit = strings(&["custom.example.com"]);
            assert_eq!(resolve(None, None, Some(&explicit), &f).network, explicit);
        }
    }

    #[test]
    fn host_suggestions_reject_credentials_local_addresses_and_wildcards() {
        let f = Fixture(BTreeMap::from([(
            "pnpm-lock.yaml",
            r#"
            https://user:password@secret.example.com/pkg
            https://*.example.com/pkg https://127.0.0.1/pkg https://[::1]/pkg
            https://localhost/pkg https://machine.local/pkg https://x.localhost/pkg
            https://a.example.com:123/pkg https://good.example.com:443/pkg
            https://good.example.com/pkg https://bad_name.example.com/pkg
        "#,
        )]));
        assert_eq!(
            lockfile_hosts(&f).into_iter().collect::<Vec<_>>(),
            strings(&["good.example.com"])
        );
        for invalid in [
            "",
            "*.example.com",
            "example.com/path",
            "example.com:443",
            "a..com",
            "-a.com",
            "a-.com",
            "127.0.0.1",
            "localhost",
        ] {
            assert!(download_host(invalid).is_none(), "{invalid}");
        }
    }

    fn strings(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// At a run: nothing set and an empty repo gives no setup, no checks and npm's
    /// registry (to scaffold); once the repo has a lockfile and scripts they're
    /// found; what's set wins, `[]` included. The agent reaches the registries in
    /// the network, all of it with `agent_network`.
    #[test]
    fn resolved_at_each_run() {
        let empty = Fixture(BTreeMap::from([("README.md", "x")]));
        let r = resolve(None, None, None, &empty);
        assert!(r.setup.is_empty() && r.checks.is_empty());
        assert_eq!(r.network, strings(&["registry.npmjs.org"]));

        let next = Fixture(BTreeMap::from([
            (
                "package.json",
                r#"{"scripts": {"lint": "next lint", "build": "next build"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
        ]));
        let r = resolve(None, None, None, &next);
        assert_eq!(r.setup, strings(&["pnpm install --frozen-lockfile"]));
        assert_eq!(r.checks, strings(&["pnpm run lint"]));

        let set = strings(&["make check"]);
        let none = Vec::new();
        let r = resolve(Some(&none), Some(&set), Some(&none), &next);
        assert!(r.setup.is_empty() && r.network.is_empty());
        assert_eq!(r.checks, set);

        let network = strings(&["registry.npmjs.org", "api.example.com"]);
        assert_eq!(
            agent_hosts(&network, false),
            strings(&["registry.npmjs.org"])
        );
        assert_eq!(agent_hosts(&network, true), network);
    }
}

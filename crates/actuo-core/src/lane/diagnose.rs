//! Why a setup or check step failed, as far as actuo can tell from what srt
//! refused and from the step's whole output, and the fixes for that kind of
//! failure. Nothing is changed by itself.

use actuo_protocol::runs::{Diagnosis, DiagnosisKind};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

/// What a failed step left to read.
pub struct Seen<'a> {
    /// `owner/repo`, for the fixes' commands.
    pub repo: &'a str,
    /// Setup (else a check).
    pub setup: bool,
    pub output: &'a str,
    /// srt's refusals (`host:port (reason)`).
    pub refusals: &'a [String],
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub limit_s: u64,
    pub peak_kb: Option<u64>,
    /// Where a step may write: the worktree and its temp dir.
    pub worktree: &'a Path,
    pub tmp: &'a Path,
}

static UNIX_SOCKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:listen|connect|bind)\s+EPERM[^\n]*?(/[^\s'\x22]+\.(?:pipe|sock))").unwrap()
});
static FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:EACCES|EROFS|EPERM)[^\n,]*, (?:open|mkdir|unlink|rename|copyfile|symlink|rmdir|utime|chmod|scandir) '([^']+)'").unwrap()
});
static SERVICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"ECONNREFUSED (?:127\.0\.0\.1|::1|localhost|\[::1\]):(\d+)").unwrap()
});
static TURBO_FAILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*Failed:\s+(.+)$").unwrap());

/// Why the step failed, if actuo can tell.
pub fn diagnose(s: &Seen) -> Option<Diagnosis> {
    let output = strip_colors(s.output);
    let output = output.as_str();
    let limit_key = if s.setup {
        "setup_time_limit"
    } else {
        "check_time_limit"
    };
    if s.timed_out {
        return Some(Diagnosis {
            blocked_hosts: Vec::new(),
            kind: DiagnosisKind::Timeout,
            detail: format!("stopped at its time limit ({})", minutes(s.limit_s)),
            fixes: vec![format!(
                "Raise it if the step is just slow: `actuo project set {} {limit_key} <time>`.",
                s.repo
            )],
        });
    }
    if s.exit_code == Some(137)
        || output.contains("JavaScript heap out of memory")
        || output.contains("Out of memory: Killed")
    {
        let peak = s
            .peak_kb
            .map(|k| format!(" (peak {} MB)", k / 1024))
            .unwrap_or_default();
        return Some(Diagnosis {
            blocked_hosts: Vec::new(),
            kind: DiagnosisKind::Memory,
            detail: format!("killed for memory{peak}"),
            fixes: vec![
                "Run fewer tasks at once (with turbo, `--concurrency=1`).".into(),
                "Or give actuo's machine more memory.".into(),
            ],
        });
    }
    if let Some(m) = UNIX_SOCKET.captures(output) {
        return Some(unix_socket(
            s.repo,
            Some(&m[1]),
            &turbo_failed(output),
            output.contains("tsx"),
        ));
    }
    if let Some(path) = FILE
        .captures_iter(output)
        .map(|c| c[1].to_string())
        .find(|p| !Path::new(p).starts_with(s.worktree) && !Path::new(p).starts_with(s.tmp))
    {
        return Some(Diagnosis {
            blocked_hosts: Vec::new(),
            kind: DiagnosisKind::File,
            detail: format!("a write outside the worktree was refused: {path}"),
            fixes: vec![
                "Setup and checks may write only in the worktree and their temp dir ($TMPDIR); make the tool write there, or exclude what writes elsewhere.".into(),
            ],
        });
    }
    if let Some(m) = SERVICE.captures(output) {
        return Some(Diagnosis {
            blocked_hosts: Vec::new(),
            kind: DiagnosisKind::Service,
            detail: format!(
                "nothing answers on port {} on this machine (a database or server the checks expect)",
                &m[1]
            ),
            fixes: vec!["Exclude checks that need a running service, or on macOS allow its port with checks_local_ports.".into()],
        });
    }
    // A refused host counts only when nothing in the output explains the failure:
    // a tool checking for updates is refused in passing steps too.
    if !s.refusals.is_empty() {
        let mut hosts: Vec<&str> = s
            .refusals
            .iter()
            .map(|r| {
                r.split(' ')
                    .next()
                    .unwrap_or(r)
                    .rsplit_once(':')
                    .map_or(r.as_str(), |(h, _)| h)
            })
            .collect();
        hosts.sort();
        hosts.dedup();
        let list = hosts.join(", ");
        let blocked_hosts: Vec<String> = if s.setup {
            hosts
                .iter()
                .filter_map(|h| crate::detect::download_host(h))
                .collect()
        } else {
            Vec::new()
        };
        let fixes = if s.setup {
            blocked_hosts
                .iter()
                .map(|host| {
                    format!(
                        "Allow {host} and rerun: `actuo project allow-host {} {host} --check`.",
                        s.repo
                    )
                })
                .collect()
        } else {
            vec![
                "Checks reach no host; exclude the ones that need the network from the checks."
                    .into(),
            ]
        };
        return Some(Diagnosis {
            blocked_hosts,
            kind: DiagnosisKind::Network,
            detail: format!("the sandbox refused hosts the step tried to reach: {list}"),
            fixes,
        });
    }
    None
}

/// The `unix_socket` diagnosis: the socket seen, the packages that fail for it, the
/// fixes.
pub fn unix_socket(repo: &str, socket: Option<&str>, packages: &[String], tsx: bool) -> Diagnosis {
    let mut detail = match socket {
        Some(s) => format!("the sandbox refused a Unix socket ({s})"),
        None => "the sandbox refuses Unix sockets, and this needs one".to_string(),
    };
    if !packages.is_empty() {
        detail.push_str(&format!("; failing for it: {}", packages.join(", ")));
    }
    let mut fixes = Vec::new();
    if packages.is_empty() {
        fixes.push("Exclude what needs it from the checks.".into());
    } else {
        let filters: Vec<String> = packages.iter().map(|p| format!("--filter=!{p}")).collect();
        fixes.push(format!(
            "Exclude them from this check: add `{}` to it.",
            filters.join(" ")
        ));
    }
    if tsx {
        fixes.push("Or change how the repo runs `tsx`: `node --import tsx <file>` may not need the socket.".into());
    }
    fixes.push(format!(
        "Or let this project's checks use Unix sockets: `actuo project set {repo} checks_unix_sockets true` (less isolation: a check could also reach other sockets on actuo's machine)."
    ));
    Diagnosis {
        blocked_hosts: Vec::new(),
        kind: DiagnosisKind::UnixSocket,
        detail,
        fixes,
    }
}

/// The packages turbo's summary lists as failed (`Failed: @a/x#test, @b/y#test`).
pub fn turbo_failed(output: &str) -> Vec<String> {
    let output = strip_colors(output);
    TURBO_FAILED
        .captures_iter(&output)
        .last()
        .map(|c| {
            c[1].split(',')
                .map(|t| t.trim().split('#').next().unwrap_or_default().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Output without terminal color codes.
fn strip_colors(text: &str) -> String {
    static CODES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
    CODES.replace_all(text, "").into_owned()
}

fn minutes(s: u64) -> String {
    if s >= 60 && s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen<'a>(output: &'a str, refusals: &'a [String]) -> Seen<'a> {
        Seen {
            repo: "o/r",
            setup: false,
            output,
            refusals,
            exit_code: Some(1),
            timed_out: false,
            limit_s: 1800,
            peak_kb: Some(900 * 1024),
            worktree: Path::new("/w"),
            tmp: Path::new("/t"),
        }
    }

    /// A monorepo's check run: tsx's socket, turbo's summary of the failing packages.
    #[test]
    fn a_refused_unix_socket() {
        let out = "@acme/web:test: Error: listen EPERM: operation not permitted /tmp/tsx-501/3456.pipe\n\
                   \x1b[31m Failed:    @acme/audit#test, @acme/shared#test\x1b[39m\n";
        let d = diagnose(&seen(out, &[])).unwrap();
        assert_eq!(d.kind, DiagnosisKind::UnixSocket);
        assert_eq!(
            d.detail,
            "the sandbox refused a Unix socket (/tmp/tsx-501/3456.pipe); failing for it: @acme/audit, @acme/shared"
        );
        assert_eq!(
            d.fixes[0],
            "Exclude them from this check: add `--filter=!@acme/audit --filter=!@acme/shared` to it."
        );
        assert!(d.fixes[1].contains("node --import tsx"));
        assert!(d.fixes[2].contains("checks_unix_sockets true"));
    }

    #[test]
    fn each_kind() {
        let kind = |out: &str, refusals: &[String]| diagnose(&seen(out, refusals)).map(|d| d.kind);
        assert_eq!(
            kind("", &["api.example.com:443 (no matching rule)".into()]),
            Some(DiagnosisKind::Network)
        );
        assert_eq!(
            kind(
                "Error: EACCES: permission denied, open '/home/me/.cache/x'",
                &[]
            ),
            Some(DiagnosisKind::File)
        );
        // A write inside the worktree isn't the sandbox's doing.
        assert_eq!(
            kind("Error: EACCES: permission denied, open '/w/x'", &[]),
            None
        );
        assert_eq!(
            kind("connect ECONNREFUSED 127.0.0.1:5432", &[]),
            Some(DiagnosisKind::Service)
        );
        assert_eq!(
            kind("FATAL ERROR: JavaScript heap out of memory", &[]),
            Some(DiagnosisKind::Memory)
        );
        assert_eq!(kind("expected 1 to be 2", &[]), None);
        // What the output says wins over a host refused in passing (here a package
        // manager's update check, refused while tsx's socket was the cause).
        assert_eq!(
            kind(
                "Error: listen EPERM: operation not permitted /tmp/tsx-501/1.pipe",
                &["registry.npmjs.org:443 (not on the allow list)".into()]
            ),
            Some(DiagnosisKind::UnixSocket)
        );
        let mut s = seen("", &[]);
        s.timed_out = true;
        let d = diagnose(&s).unwrap();
        assert_eq!(d.detail, "stopped at its time limit (30 min)");
        assert!(d.fixes[0].contains("check_time_limit"));
    }

    #[test]
    fn only_setup_offers_exact_refused_hosts() {
        let refusals = vec![
            "codeload.github.com:443 (not on the allow list)".into(),
            "*.example.com:443 (denied)".into(),
        ];
        let mut s = seen("download failed", &refusals);
        assert!(diagnose(&s).unwrap().blocked_hosts.is_empty());
        s.setup = true;
        let d = diagnose(&s).unwrap();
        assert_eq!(d.blocked_hosts, vec!["codeload.github.com"]);
        assert!(d.fixes[0].contains("allow-host"));
        assert!(d.fixes[0].contains("--check"));
    }

    #[test]
    fn turbos_failed_line() {
        assert_eq!(
            turbo_failed("Tasks: 1\n Failed:    @a/x#test, b#lint\n"),
            vec!["@a/x", "b"]
        );
        assert!(turbo_failed("all good").is_empty());
    }
}

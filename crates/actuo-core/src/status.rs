//! `actuo/status`: the machine actuo runs on, the lane's last poll, and the
//! problems actuo sees in them.

use crate::session::Host;
use actuo_protocol::methods::{ActuoStatusResult, LanePoll, Problem, Space};
use std::path::Path;

const GB: u64 = 1_000_000_000;
/// Free disk under this is a problem.
const DISK_LOW: u64 = 10 * GB;
/// Free disk under this is a serious one.
const DISK_VERY_LOW: u64 = 5 * GB;

pub async fn status(host: &Host) -> ActuoStatusResult {
    let home = host.paths().home.clone();
    let (disk, memory) = tokio::task::spawn_blocking(move || (disk(&home), memory()))
        .await
        .unwrap_or((None, None));
    let last_poll = host.lane().ok().and_then(|l| l.last_poll());
    let gh_login = match host.lane() {
        Ok(lane) => lane.gh_login().await,
        Err(_) => None,
    };
    ActuoStatusResult {
        problems: problems(disk, last_poll.as_ref()),
        disk,
        memory,
        last_poll,
        gh_login,
        update: host.update(),
    }
}

fn problems(disk: Option<Space>, last_poll: Option<&LanePoll>) -> Vec<Problem> {
    let mut problems = Vec::new();
    if let Some(d) = disk
        && d.free < DISK_LOW
    {
        problems.push(Problem {
            message: format!("{:.1} GB of disk free", d.free as f64 / GB as f64),
            serious: d.free < DISK_VERY_LOW,
        });
    }
    if let Some(error) = last_poll.and_then(|p| p.error.as_ref()) {
        problems.push(Problem {
            message: format!("the lane's last poll failed: {error}"),
            serious: false,
        });
    }
    problems
}

/// The disk holding `path` (or its closest existing parent).
fn disk(path: &Path) -> Option<Space> {
    let path = path.ancestors().find(|p| p.exists())?;
    let s = rustix::fs::statvfs(path).ok()?;
    Some(Space {
        free: s.f_bavail * s.f_frsize,
        total: s.f_blocks * s.f_frsize,
    })
}

#[cfg(target_os = "linux")]
fn memory() -> Option<Space> {
    meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// `MemAvailable` and `MemTotal` from `/proc/meminfo`, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo(text: &str) -> Option<Space> {
    let kib = |key: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        line[key.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()
    };
    Some(Space {
        free: kib("MemAvailable:")? * 1024,
        total: kib("MemTotal:")? * 1024,
    })
}

/// Total from `sysctl hw.memsize`; available as free, inactive and speculative
/// pages from `vm_stat`.
#[cfg(target_os = "macos")]
fn memory() -> Option<Space> {
    let run = |cmd: &str, args: &[&str]| {
        let o = crate::process::output_std(std::process::Command::new(cmd).args(args)).ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let total = run("/usr/sbin/sysctl", &["-n", "hw.memsize"])?
        .trim()
        .parse()
        .ok()?;
    let free = vm_stat_available(&run("/usr/bin/vm_stat", &[])?)?;
    Some(Space { free, total })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn vm_stat_available(text: &str) -> Option<u64> {
    let page: u64 = text
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let pages = |key: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        line[key.len()..].trim().trim_end_matches('.').parse().ok()
    };
    Some((pages("Pages free:")? + pages("Pages inactive:")? + pages("Pages speculative:")?) * page)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory() -> Option<Space> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_problems_by_free_space() {
        let space = |gb: f64| {
            Some(Space {
                free: (gb * GB as f64) as u64,
                total: 100 * GB,
            })
        };
        assert!(problems(space(12.0), None).is_empty());
        let p = problems(space(7.5), None);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].message, "7.5 GB of disk free");
        assert!(!p[0].serious);
        assert!(problems(space(4.9), None)[0].serious);
        assert!(problems(None, None).is_empty());
    }

    #[test]
    fn a_failed_poll_is_a_problem() {
        let poll = |error: Option<&str>| LanePoll {
            at: "2026-10-05T12:00:00Z".into(),
            error: error.map(Into::into),
        };
        assert!(problems(None, Some(&poll(None))).is_empty());
        let p = problems(None, Some(&poll(Some("gh: not logged in"))));
        assert_eq!(
            p[0].message,
            "the lane's last poll failed: gh: not logged in"
        );
        assert!(!p[0].serious);
    }

    #[test]
    fn reads_meminfo() {
        let text = "MemTotal:        3884512 kB\nMemFree:          201324 kB\nMemAvailable:    2101236 kB\n";
        assert_eq!(
            meminfo(text),
            Some(Space {
                free: 2101236 * 1024,
                total: 3884512 * 1024
            })
        );
    }

    #[test]
    fn reads_vm_stat() {
        let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                                     4459.\nPages active:                                 120084.\nPages inactive:                               111385.\nPages speculative:                              7330.\n";
        assert_eq!(
            vm_stat_available(text),
            Some((4459 + 111385 + 7330) * 16384)
        );
    }

    #[test]
    fn disk_reads_the_closest_existing_folder() {
        let d = disk(Path::new("/tmp/does-not-exist/at-all")).unwrap();
        assert!(d.total > 0 && d.free <= d.total);
    }
}

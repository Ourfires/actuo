//! Starting processes, and agent process groups.
//!
//! Every process actuo starts is spawned through [`spawn`], [`output`],
//! [`spawn_std`] or [`output_std`], holding one lock, and without the tokens kept
//! for agents unless the command sets them ([`crate::pass_env`]); a pipe
//! actuo makes for a child comes from [`pipe`], under the same lock. Spawning makes
//! the child's stdio pipes, and on macOS a pipe is made close-on-exec only after it's
//! made: a fork in another thread in between hands that child the pipe's other end.
//! A long-lived child (an agent, `caffeinate`) holding the write end of git's output
//! pipe makes actuo wait for the end of git's output forever. With one lock no
//! spawn's pipes exist while another forks. Only the spawn holds it, never the wait.
//!
//! Agent process groups: every agent runs as the leader of its own group, so
//! actuo can kill the agent and everything it started (npx → node → tools).
//!
//! After a restart, a recorded group is only killed when it is provably ours:
//! the leader still exists with the recorded start time, or the leader is gone but
//! the group still has members. A pid can't be reused while a process group with that
//! id exists, so in the second case the members are the old agent's.

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The variables actuo keeps for agents ([`crate::pass_env`]) that `cmd`
/// doesn't set itself: removed, so the child doesn't inherit them.
fn unpassed(cmd: &Command) -> Vec<&'static str> {
    crate::pass_env::names()
        .filter(|n| !cmd.get_envs().any(|(k, v)| k == *n && v.is_some()))
        .collect()
}

/// Starts `cmd` holding the spawn lock.
pub fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    for n in unpassed(cmd.as_std()) {
        cmd.env_remove(n);
    }
    let _one = spawn_lock();
    cmd.spawn()
}

/// [`spawn`], passing `pass` to the child at its own descriptor number (made
/// inheritable only here, holding the spawn lock, so no other child gets it), then
/// closing it here.
pub fn spawn_passing(
    cmd: &mut tokio::process::Command,
    pass: std::os::fd::OwnedFd,
) -> std::io::Result<tokio::process::Child> {
    for n in unpassed(cmd.as_std()) {
        cmd.env_remove(n);
    }
    let _one = spawn_lock();
    rustix::io::fcntl_setfd(&pass, rustix::io::FdFlags::empty())?;
    let spawned = cmd.spawn();
    drop(pass);
    spawned
}

/// Runs `cmd` to its end with stdin closed and stdout and stderr collected, as
/// `Command::output` does, spawning it holding the spawn lock.
pub async fn output(cmd: &mut tokio::process::Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd)?.wait_with_output().await
}

/// A pipe, made holding the spawn lock (for a child's output actuo reads).
pub fn pipe() -> std::io::Result<(std::io::PipeReader, std::io::PipeWriter)> {
    let _one = spawn_lock();
    std::io::pipe()
}

/// [`spawn`] for a `std` command (blocking code).
pub fn spawn_std(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    for n in unpassed(cmd) {
        cmd.env_remove(n);
    }
    let _one = spawn_lock();
    cmd.spawn()
}

/// [`output`] for a `std` command (blocking code).
pub fn output_std(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn_std(cmd)?.wait_with_output()
}

/// How long SIGTERM gets before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(5);

/// The start time of `pid` as `ps` reports it, or `None` if there's no such process.
pub fn start_time(pid: u32) -> Option<String> {
    let out = output_std(
        Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("LC_ALL", "C"),
    )
    .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Whether any process that isn't a zombie is left in the group.
pub fn group_alive(pgid: u32) -> bool {
    let Some(p) = pid(pgid) else {
        return false;
    };
    if test_kill_process_group(p).is_err() {
        return false;
    }
    live_member_check(pgid)
}

/// Linux answers the signal test above for a group whose processes are all zombies
/// (exited, not yet reaped), so look for a member that isn't one.
#[cfg(target_os = "linux")]
fn live_member_check(pgid: u32) -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return true;
    };
    let pgid = pgid.to_string();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `pid (comm) state ppid pgrp ...`; comm may contain spaces and parentheses.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (state, _ppid, pgrp) = (fields.next(), fields.next(), fields.next());
        if pgrp == Some(pgid.as_str()) && state != Some("Z") {
            return true;
        }
    }
    false
}

/// macOS answers "no such process" for a group of zombies, so the signal test is enough.
#[cfg(not(target_os = "linux"))]
fn live_member_check(_pgid: u32) -> bool {
    true
}

/// SIGTERM to the group, SIGKILL after `grace`, then waits (up to 5 more seconds)
/// until no process is left in it. Returns whether the group is gone.
pub async fn kill_group(pgid: u32, grace: Duration) -> bool {
    let Some(p) = pid(pgid) else {
        return true;
    };
    if kill_process_group(p, Signal::TERM).is_err() {
        return !group_alive(pgid);
    }
    if wait_gone(pgid, grace).await {
        return true;
    }
    let _ = kill_process_group(p, Signal::KILL);
    wait_gone(pgid, Duration::from_secs(5)).await
}

/// Kills the group of a recorded agent process if it is still ours (see the module
/// docs). Returns whether anything was killed.
pub async fn kill_leftover(pid: u32, started_at: &str) -> bool {
    let ours = match start_time(pid) {
        Some(now) => now == started_at,
        None => group_alive(pid),
    };
    if ours && group_alive(pid) {
        kill_group(pid, TERM_GRACE).await;
        true
    } else {
        false
    }
}

/// SIGKILL to a process group when dropped, unless disarmed: a step whose future is
/// dropped (actuo stopping) takes everything it started with it, not only its
/// first process.
pub struct GroupGuard(Option<u32>);

impl GroupGuard {
    pub fn new(pgid: u32) -> Self {
        Self((pgid != 0).then_some(pgid))
    }

    /// The group is gone (killed and waited for): nothing to do on drop, and its id
    /// may be reused.
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.and_then(pid) {
            let _ = kill_process_group(p, Signal::KILL);
        }
    }
}

async fn wait_gone(pgid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !group_alive(pgid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    !group_alive(pgid)
}

fn pid(raw: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(raw).ok()?)
}

/// The resident memory of `root` and every process under it, in KB: the total and
/// the largest single process. `None` when `root` is gone. Read from `/proc` on
/// Linux, from `ps` on macOS; a sandbox's processes count (they're descendants).
pub fn tree_memory(root: u32) -> Option<(u64, u64)> {
    let procs = all_processes();
    if !procs.iter().any(|p| p.0 == root) {
        return None;
    }
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(procs.iter().filter(|p| p.1 == parent).map(|p| p.0));
        i += 1;
    }
    let rss: Vec<u64> = procs
        .iter()
        .filter(|p| tree.contains(&p.0))
        .map(|p| p.2)
        .collect();
    Some((rss.iter().sum(), rss.iter().copied().max().unwrap_or(0)))
}

/// Every process: (pid, parent pid, resident KB).
#[cfg(target_os = "linux")]
fn all_processes() -> Vec<(u32, u32, u64)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let field = |key: &str| -> Option<u64> {
                let line = status.lines().find(|l| l.starts_with(key))?;
                line[key.len()..].split_whitespace().next()?.parse().ok()
            };
            // Kernel threads have no VmRSS.
            Some((pid, field("PPid:")? as u32, field("VmRSS:").unwrap_or(0)))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn all_processes() -> Vec<(u32, u32, u64)> {
    let Ok(out) = output_std(Command::new("/bin/ps").args(["-A", "-o", "pid=,ppid=,rss="])) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(|x| x.parse::<u64>().ok());
            Some((f.next()?? as u32, f.next()?? as u32, f.next()??))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    /// A command and the process under it count; a process that isn't there is
    /// `None`.
    #[test]
    fn tree_memory_counts_descendants() {
        let mut child =
            spawn_std(Command::new("sh").args(["-c", "sleep 3 & sleep 3; wait"])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (total, largest) = tree_memory(child.id()).unwrap();
        assert!(
            total > 0 && largest > 0 && largest <= total,
            "{total} {largest}"
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(tree_memory(u32::MAX), None);
    }

    /// A shell that starts a grandchild, as `npx` does, in its own group.
    fn spawn_group() -> std::process::Child {
        Command::new("sh")
            .args(["-c", "sleep 60 & sleep 60"])
            .process_group(0)
            .spawn()
            .unwrap()
    }

    /// Many processes started at once from many threads, long-lived ones among them:
    /// every short one's output ends. A pipe leaked into a long-lived child would
    /// keep a short one's output open until that child exits (30 s here).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_spawns_dont_hang() {
        let mut tasks = Vec::new();
        for i in 0..400 {
            tasks.push(tokio::spawn(async move {
                match i % 4 {
                    // Long-lived, like an agent or caffeinate.
                    0 => {
                        let child = spawn(
                            tokio::process::Command::new("sleep")
                                .arg("30")
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .kill_on_drop(true),
                        )
                        .unwrap();
                        Some(child)
                    }
                    // Output through a pipe actuo made, like setup and checks.
                    1 => {
                        let (mut reader, writer) = pipe().unwrap();
                        let mut cmd = tokio::process::Command::new("echo");
                        cmd.arg("x").stdout(writer).stdin(Stdio::null());
                        let mut child = spawn(&mut cmd).unwrap();
                        drop(cmd);
                        let read = tokio::task::spawn_blocking(move || {
                            let mut s = String::new();
                            std::io::Read::read_to_string(&mut reader, &mut s).map(|_| s)
                        });
                        child.wait().await.unwrap();
                        assert_eq!(read.await.unwrap().unwrap(), "x\n");
                        None
                    }
                    // Collected output, like git and gh.
                    2 => {
                        let out = output(tokio::process::Command::new("echo").arg("x"))
                            .await
                            .unwrap();
                        assert_eq!(out.stdout, b"x\n");
                        None
                    }
                    _ => {
                        let out = tokio::task::spawn_blocking(|| {
                            output_std(Command::new("echo").arg("x"))
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(out.stdout, b"x\n");
                        None
                    }
                }
            }));
        }
        let all = async {
            let mut kept = Vec::new();
            for t in tasks {
                kept.push(t.await.unwrap());
            }
            kept
        };
        let kept = tokio::time::timeout(Duration::from_secs(20), all)
            .await
            .expect("a spawn hung: a pipe leaked into a long-lived child");
        drop(kept);
    }

    /// Nothing but this module starts a process or makes a pipe for one.
    #[test]
    fn every_spawn_goes_through_the_lock() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut found = Vec::new();
        let mut dirs = vec![crates.join("actuo-core/src"), crates.join("actuo/src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy();
                if !name.ends_with(".rs") || name == "tests.rs" || path.ends_with("src/process.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.split("#[cfg(test)]").next().unwrap();
                for (i, line) in code.lines().enumerate() {
                    let line = line.replace("self.status()", "");
                    if [".spawn()", ".output()", ".status()", "io::pipe("]
                        .iter()
                        .any(|p| line.contains(p))
                    {
                        found.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        assert!(found.is_empty(), "spawned without the lock: {found:#?}");
    }

    #[tokio::test]
    async fn kills_the_whole_group() {
        let mut child = spawn_group();
        let pgid = child.id();
        assert!(start_time(pgid).is_some());
        assert!(group_alive(pgid));
        assert!(kill_group(pgid, Duration::from_secs(2)).await);
        let _ = child.wait();
        assert!(!group_alive(pgid));
        assert!(start_time(pgid).is_none());
    }

    #[tokio::test]
    async fn leftover_needs_a_matching_start_time() {
        let mut child = spawn_group();
        let pgid = child.id();
        assert!(!kill_leftover(pgid, "not its start time").await);
        assert!(group_alive(pgid));
        let started = start_time(pgid).unwrap();
        assert!(kill_leftover(pgid, &started).await);
        let _ = child.wait();
        assert!(!group_alive(pgid));
    }

    #[tokio::test]
    async fn leftover_members_without_their_leader_are_killed() {
        // The leader exits at once; its background child stays in the group.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 &"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id();
        child.wait().unwrap();
        assert!(start_time(pgid).is_none());
        assert!(group_alive(pgid));
        assert!(kill_leftover(pgid, "whatever it was").await);
        assert!(!group_alive(pgid));
    }
}

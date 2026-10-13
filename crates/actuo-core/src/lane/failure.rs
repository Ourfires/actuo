//! What the agent sees of a failed check: its failing lines, not its last ones. A
//! monorepo's check output ends with the packages that passed and turbo's summary,
//! so its last lines can hold none of the failure.
//!
//! turbo output (lines `pkg:task: …`, a `Failed:` summary): each failed task's own
//! lines under a header, its `×` lines first, then its failure blocks, the budget
//! split evenly, turbo's summary last. Other output: the lines around each failure
//! marker, then the last lines. Nothing to tell (no marker): `None`, and the caller
//! sends the tail.
//!
//! Used for the agent's prompts and the PR; a step's stored tail stays the tail.

use regex::Regex;
use std::sync::LazyLock;

/// A line that says something failed, in the test runners and compilers checks use.
static MARK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"FAIL|AssertionError|✗|✖|×|ERR!|Error:|\berror\b|not ok\b|panicked|Traceback|^\s*ERROR\b",
    )
    .unwrap()
});
static FAILED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*Failed:\s+(.+)$").unwrap());
static SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(Tasks|Cached|Time|Failed):").unwrap());
static COLORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());

/// Lines kept from the end of output that isn't turbo's, after the failures.
const LAST_LINES: usize = 30;

/// The failing parts of a failed check's output, at most `max_bytes`; `None` when
/// it shows no failure marker.
pub(super) fn failure_lines(output: &str, max_bytes: usize) -> Option<String> {
    let text = COLORS.replace_all(output, "").into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let tasks = turbo_tasks(&text);
    if !tasks.is_empty() {
        return Some(turbo(&lines, &tasks, max_bytes));
    }
    other(&lines, max_bytes)
}

/// The tasks turbo's last `Failed:` line lists: (package, task).
fn turbo_tasks(text: &str) -> Vec<(String, String)> {
    let Some(c) = FAILED.captures_iter(text).last() else {
        return Vec::new();
    };
    c[1].split(',')
        .filter_map(|t| {
            let (pkg, task) = t.trim().split_once('#')?;
            Some((pkg.to_string(), task.to_string()))
        })
        .collect()
}

fn turbo(lines: &[&str], tasks: &[(String, String)], max_bytes: usize) -> String {
    let summary: Vec<&str> = lines
        .iter()
        .filter(|l| SUMMARY.is_match(l))
        .copied()
        .collect();
    let summary_bytes: usize = summary.iter().map(|l| l.len() + 1).sum();
    let per = max_bytes.saturating_sub(summary_bytes + 2) / tasks.len().max(1);
    let mut out = Vec::new();
    for (pkg, task) in tasks {
        let prefix = format!("{pkg}:{task}: ");
        // Its own lines, without the passing tests'.
        let own: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.strip_prefix(&prefix))
            .filter(|l| !l.trim_start().starts_with('✓'))
            .collect();
        let header = format!("── {pkg} ({task}) ──");
        let budget = per.saturating_sub(header.len() + 1);
        out.push(header);
        if own.is_empty() {
            out.push("(no output of its own)".into());
            continue;
        }
        // All of it when it fits; else the failing tests' names first (vitest's
        // ×), then the failure blocks.
        if own.iter().map(|l| l.len() + 1).sum::<usize>() <= budget {
            out.extend(own.iter().map(|l| l.to_string()));
            continue;
        }
        let mut picked: Vec<usize> = own
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                t.starts_with('×') || t.starts_with('✗') || t.starts_with('✖')
            })
            .map(|(i, _)| i)
            .collect();
        for i in around_marks(&own) {
            if !picked.contains(&i) {
                picked.push(i);
            }
        }
        if picked.is_empty() {
            picked = (own.len().saturating_sub(20)..own.len()).collect();
        }
        out.extend(within(&own, &picked, budget));
    }
    out.push(String::new());
    out.extend(summary.iter().map(|l| l.to_string()));
    out.join("\n")
}

fn other(lines: &[&str], max_bytes: usize) -> Option<String> {
    let marked = around_marks(lines);
    if marked.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(LAST_LINES);
    let last: Vec<&str> = lines[start..].to_vec();
    let last_bytes: usize = last.iter().map(|l| l.len() + 1).sum();
    let before: Vec<usize> = marked.into_iter().filter(|&i| i < start).collect();
    let mut out = within(lines, &before, max_bytes.saturating_sub(last_bytes + 2));
    if !out.is_empty() {
        out.push("…".into());
    }
    out.extend(last.iter().map(|l| l.to_string()));
    Some(out.join("\n"))
}

/// Each marker line with 1 line before it and 3 after, in order, no repeats.
fn around_marks(lines: &[&str]) -> Vec<usize> {
    let mut keep = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if MARK.is_match(l) {
            for j in i.saturating_sub(1)..(i + 4).min(lines.len()) {
                if keep.last().is_none_or(|&k| j > k) {
                    keep.push(j);
                }
            }
        }
    }
    keep
}

/// The picked lines, in the order picked, while they fit in `budget` bytes; then
/// how many didn't. A gap between picked lines that follow each other shows as `…`.
fn within(lines: &[&str], picked: &[usize], budget: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut used = 0;
    let mut prev: Option<usize> = None;
    for (n, &i) in picked.iter().enumerate() {
        let gap = prev.is_some_and(|p| i > p + 1);
        let line = lines[i];
        let cost = line.len() + 1 + if gap { 2 } else { 0 };
        if used + cost > budget {
            let more = picked.len() - n;
            out.push(format!(
                "… {more} more line{}",
                if more == 1 { "" } else { "s" }
            ));
            break;
        }
        if gap {
            out.push("…".into());
        }
        out.push(line.to_string());
        used += cost;
        prev = Some(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turbo run: a failing package in the middle, passing ones
    /// after it, turbo's summary at the end.
    fn turbo_output() -> String {
        let mut s = String::new();
        s.push_str("@o/ok:test: \x1b[32m✓\x1b[39m scripts/a.test.ts (3 tests) 5ms\n");
        s.push_str("@o/shared:test:  ❯ tests/parse.test.ts (3 tests | 1 failed) 881ms\n");
        s.push_str("@o/shared:test:      ✓ starts 2ms\n");
        s.push_str("@o/shared:test:      × parses empty input 870ms\n");
        s.push_str("@o/shared:test: ⎯⎯⎯ Failed Tests 1 ⎯⎯⎯\n");
        s.push_str("@o/shared:test:  FAIL  tests/parse.test.ts > parses empty input\n");
        s.push_str("@o/shared:test: AssertionError: expected 'x' not to match /FAIL/\n");
        s.push_str("@o/shared:test:   FAIL: case 2 (exit 4, expected 0)\n");
        s.push_str("@o/shared:test:  Test Files  1 failed | 6 passed (7)\n");
        for i in 0..300 {
            s.push_str(&format!(
                "@o/ok:test:  ✓ scripts/t{i}.test.ts (1 test) 1ms\n"
            ));
        }
        s.push_str("@o/shared#test:  ERROR  command pnpm run test exited (1)\n\n");
        s.push_str(" Tasks:    1 successful, 2 total\n");
        s.push_str("Failed:    @o/shared#test\n");
        s
    }

    #[test]
    fn turbo_shows_the_failed_tasks_lines_not_the_last_ones() {
        let out = failure_lines(&turbo_output(), 20480).unwrap();
        assert!(
            out.starts_with("── @o/shared (test) ──\n ❯ tests/parse.test.ts"),
            "{out}"
        );
        assert!(out.contains("     × parses empty input 870ms"), "{out}");
        assert!(!out.contains("✓"), "{out}");
        assert!(
            out.contains(" FAIL  tests/parse.test.ts > parses empty input"),
            "{out}"
        );
        assert!(
            out.contains("AssertionError: expected 'x' not to match /FAIL/"),
            "{out}"
        );
        assert!(out.ends_with("Failed:    @o/shared#test"), "{out}");
        // Passing packages, and the prefix, are left out.
        assert!(!out.contains("@o/ok"), "{out}");
        assert!(!out.contains("@o/shared:test:"), "{out}");
    }

    #[test]
    fn the_budget_is_split_and_what_doesnt_fit_is_counted() {
        let mut s = String::new();
        for p in ["a", "b"] {
            for i in 0..100 {
                s.push_str(&format!(
                    "@o/{p}:test:      × test {i} failed in a long name\n"
                ));
            }
        }
        s.push_str("Failed:    @o/a#test, @o/b#test\n");
        let out = failure_lines(&s, 2000).unwrap();
        assert!(out.len() <= 2000, "{}", out.len());
        assert!(
            out.contains("── @o/a (test) ──") && out.contains("── @o/b (test) ──"),
            "{out}"
        );
        assert!(out.contains(" more lines"), "{out}");
    }

    #[test]
    fn other_output_keeps_the_lines_around_failures_and_the_end() {
        let mut s = String::from("start\n");
        for i in 0..100 {
            s.push_str(&format!("noise {i}\n"));
        }
        s.push_str("src/a.ts(3,1): error TS2304: Cannot find name 'x'.\n");
        for i in 0..100 {
            s.push_str(&format!("more {i}\n"));
        }
        let out = failure_lines(&s, 20480).unwrap();
        assert!(
            out.contains("noise 99\nsrc/a.ts(3,1): error TS2304"),
            "{out}"
        );
        assert!(out.contains("more 0\nmore 1\nmore 2\n…\nmore 70"), "{out}");
        assert!(out.ends_with("more 99"), "{out}");
        assert!(!out.contains("noise 50"), "{out}");
    }

    #[test]
    fn nothing_to_tell_is_none() {
        assert_eq!(failure_lines("one\ntwo\nexit 1\n", 20480), None);
    }
}

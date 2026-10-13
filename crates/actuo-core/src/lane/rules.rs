//! Approval rules: how the lane answers a permission request, by the tool call's
//! kind and where it acts, choosing an option by kind (never by id or name).

use actuo_protocol::ids::PermissionOptionId;
use actuo_protocol::settings::Rule;
use actuo_protocol::types::{PermissionOption, PermissionOptionKind, ToolCall, ToolKind};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Answer with this option, by the rule at this index.
    Answer(PermissionOptionId, PermissionOptionKind, usize),
    /// No rule matches, or none of its kinds is offered: wait for a person.
    Pause,
}

/// The built-in rules for an agent when the settings have none: Claude and Codex may
/// run commands and edit files inside the worktree, once at a time; everything else
/// pauses. Other agents get none: every request pauses.
pub fn builtin(agent: &str) -> Vec<Rule> {
    match agent {
        "claude" | "codex" => vec![
            Rule {
                kind: ToolKind::Execute,
                inside_worktree: None,
                answer: vec![PermissionOptionKind::AllowOnce],
            },
            Rule {
                kind: ToolKind::Edit,
                inside_worktree: Some(true),
                answer: vec![PermissionOptionKind::AllowOnce],
            },
        ],
        _ => Vec::new(),
    }
}

fn name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// A rule as it reads in the settings, e.g. `kind = "edit", inside_worktree = false`.
pub fn describe(rule: &Rule) -> String {
    let mut s = format!("kind = \"{}\"", name(rule.kind));
    if let Some(inside) = rule.inside_worktree {
        s.push_str(&format!(", inside_worktree = {inside}"));
    }
    let answer: Vec<String> = rule
        .answer
        .iter()
        .map(|a| format!("\"{}\"", name(a)))
        .collect();
    s.push_str(&format!(", answer = [{}]", answer.join(", ")));
    s
}

/// The first rule that matches `call` answers it. In a session without the sandbox
/// a rule that would allow is skipped, so the request pauses: allowing is only safe
/// with the sandbox around the agent.
pub fn decide(
    rules: &[Rule],
    call: &ToolCall,
    options: &[PermissionOption],
    worktree: &Path,
    sandboxed: bool,
) -> Decision {
    for (i, rule) in rules.iter().enumerate() {
        if rule.kind != call.tool_kind {
            continue;
        }
        if let Some(want) = rule.inside_worktree
            && inside_worktree(call, worktree) != want
        {
            continue;
        }
        let allows = matches!(
            rule.answer.first(),
            Some(PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways)
        );
        if allows && !sandboxed {
            continue;
        }
        for kind in &rule.answer {
            if let Some(o) = options.iter().find(|o| o.kind == *kind) {
                return Decision::Answer(o.option_id.clone(), o.kind, i);
            }
        }
        return Decision::Pause;
    }
    Decision::Pause
}

/// Every location is inside the worktree, and there is at least one. Paths are
/// made absolute against the worktree and normalized without touching the disk.
pub fn inside_worktree(call: &ToolCall, worktree: &Path) -> bool {
    let root = normalize(worktree);
    !call.locations.is_empty()
        && call.locations.iter().all(|l| {
            let p = Path::new(&l.path);
            let p = if p.is_absolute() {
                p.to_path_buf()
            } else {
                worktree.join(p)
            };
            normalize(&p).starts_with(&root)
        })
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use actuo_protocol::types::{Location, ToolStatus};

    fn call(kind: ToolKind, paths: &[&str]) -> ToolCall {
        ToolCall {
            tool_kind: kind,
            title: "t".into(),
            status: ToolStatus::Pending,
            raw_input: None,
            raw_output: None,
            content: vec![],
            locations: paths
                .iter()
                .map(|p| Location {
                    path: p.to_string(),
                    line: None,
                })
                .collect(),
        }
    }

    fn options(kinds: &[(&str, PermissionOptionKind)]) -> Vec<PermissionOption> {
        kinds
            .iter()
            .map(|(id, kind)| PermissionOption {
                option_id: PermissionOptionId::from(*id),
                name: id.to_string(),
                kind: *kind,
            })
            .collect()
    }

    fn rule(kind: ToolKind, inside: Option<bool>, answer: &[PermissionOptionKind]) -> Rule {
        Rule {
            kind,
            inside_worktree: inside,
            answer: answer.to_vec(),
        }
    }

    use PermissionOptionKind::*;

    #[test]
    fn first_matching_rule_answers_by_kind() {
        let wt = Path::new("/w/t");
        let rules = vec![
            rule(ToolKind::Edit, Some(true), &[AllowOnce]),
            rule(ToolKind::Edit, Some(false), &[RejectOnce]),
            rule(ToolKind::Execute, None, &[AllowAlways, AllowOnce]),
        ];
        let opts = options(&[("yes", AllowOnce), ("cancel", RejectOnce)]);
        assert_eq!(
            decide(
                &rules,
                &call(ToolKind::Edit, &["a.txt", "/w/t/b/c.txt"]),
                &opts,
                wt,
                true
            ),
            Decision::Answer("yes".into(), AllowOnce, 0)
        );
        assert_eq!(
            decide(
                &rules,
                &call(ToolKind::Edit, &["../x.txt"]),
                &opts,
                wt,
                true
            ),
            Decision::Answer("cancel".into(), RejectOnce, 1)
        );
        // No allow_always offered: the next kind in the list.
        assert_eq!(
            decide(&rules, &call(ToolKind::Execute, &[]), &opts, wt, true),
            Decision::Answer("yes".into(), AllowOnce, 2)
        );
        // No rule for the kind; a kind the rule wants isn't offered.
        assert_eq!(
            decide(&rules, &call(ToolKind::Fetch, &[]), &opts, wt, true),
            Decision::Pause
        );
        let only_reject = options(&[("no", RejectOnce)]);
        assert_eq!(
            decide(
                &rules,
                &call(ToolKind::Execute, &[]),
                &only_reject,
                wt,
                true
            ),
            Decision::Pause
        );
        // An edit without locations isn't "inside".
        assert_eq!(
            decide(&rules[..1], &call(ToolKind::Edit, &[]), &opts, wt, true),
            Decision::Pause
        );
    }

    #[test]
    fn no_allow_without_the_sandbox() {
        let wt = Path::new("/w/t");
        let opts = options(&[("yes", AllowOnce), ("no", RejectOnce)]);
        let rules = vec![
            rule(ToolKind::Execute, None, &[AllowOnce]),
            rule(ToolKind::Execute, None, &[RejectOnce]),
        ];
        assert_eq!(
            decide(&rules, &call(ToolKind::Execute, &[]), &opts, wt, false),
            Decision::Answer("no".into(), RejectOnce, 1)
        );
        assert_eq!(
            decide(&rules[..1], &call(ToolKind::Execute, &[]), &opts, wt, false),
            Decision::Pause
        );
        // The built-in rules allow nothing without the sandbox.
        let claude = builtin("claude");
        for c in [
            call(ToolKind::Execute, &[]),
            call(ToolKind::Edit, &["/w/t/a.txt"]),
        ] {
            assert_eq!(decide(&claude, &c, &opts, wt, false), Decision::Pause);
        }
    }

    #[test]
    fn builtin_rules() {
        let wt = Path::new("/w/t");
        let opts = options(&[
            ("allow-always", AllowAlways),
            ("allow-once", AllowOnce),
            ("reject", RejectOnce),
        ]);
        let claude = builtin("claude");
        assert_eq!(claude, builtin("codex"));
        assert!(builtin("fake").is_empty());
        let once = |i| Decision::Answer("allow-once".into(), AllowOnce, i);
        let cases = [
            (call(ToolKind::Execute, &[]), once(0)),
            (call(ToolKind::Execute, &["/etc"]), once(0)),
            (call(ToolKind::Edit, &["src/a.ts", "/w/t/b.ts"]), once(1)),
            (
                call(ToolKind::Edit, &["/w/t/a.ts", "/w/other.ts"]),
                Decision::Pause,
            ),
            (call(ToolKind::Edit, &["../t2/a.ts"]), Decision::Pause),
            (call(ToolKind::Edit, &[]), Decision::Pause),
        ];
        for (c, want) in cases {
            assert_eq!(decide(&claude, &c, &opts, wt, true), want, "{c:?}");
        }
        for kind in [
            ToolKind::Read,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::SwitchMode,
            ToolKind::Other,
        ] {
            let c = call(kind, &["/w/t/a.ts"]);
            assert_eq!(
                decide(&claude, &c, &opts, wt, true),
                Decision::Pause,
                "{kind:?}"
            );
        }
        assert_eq!(
            describe(&claude[1]),
            "kind = \"edit\", inside_worktree = true, answer = [\"allow_once\"]"
        );
    }

    #[test]
    fn paths_are_normalized() {
        let wt = Path::new("/w/t");
        assert!(inside_worktree(
            &call(ToolKind::Edit, &["./a/../b.txt"]),
            wt
        ));
        assert!(!inside_worktree(
            &call(ToolKind::Edit, &["a/../../t2/x"]),
            wt
        ));
        assert!(!inside_worktree(&call(ToolKind::Edit, &["/w/t2/x"]), wt));
        assert!(!inside_worktree(
            &call(ToolKind::Edit, &["/w/t/../../etc/passwd"]),
            wt
        ));
    }
}

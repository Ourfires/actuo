//! Actuo's core end to end, from the CLI, against the scripted fake agent,
//! with `actuo serve` as a real process that is stopped and started again:
//!
//! 1. start a session on a project with an agent, in its own worktree;
//! 2. watch its events stream live;
//! 3. answer a permission request with one of the agent's options;
//! 4. interrupt a running turn;
//! 5. revert the files to the checkpoint before a given turn;
//! 6. stop actuo, start it again, and reattach to the session with its full
//!    history intact, then continue it (resume or load).

mod common;

use common::{Attach, Env, alive, last_agent_pid, last_spawned_pid};
use serde_json::{Value, json};
use std::path::PathBuf;

fn script(resume: bool) -> Value {
    json!({
        "loadSession": true,
        "resume": resume,
        "modes": [{"id": "ask", "name": "Ask"}, {"id": "edits", "name": "Edits"}],
        "turns": [
            {"match": "notes", "steps": [
                {"message": "Writing NOTES.md"},
                {"toolCall": {"id": "t1", "title": "Write NOTES.md", "kind": "edit",
                              "diff": {"path": "NOTES.md", "newText": "notes\n"}}},
                {"permission": {"toolCallId": "t1",
                    "options": [{"id": "allow-once", "name": "Yes", "kind": "allow_once"},
                                {"id": "reject", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "NOTES.md", "content": "notes\n"}},
                                {"toolUpdate": {"id": "t1", "status": "completed"}}]}}
            ]},
            {"match": "long", "steps": [
                {"message": "Writing the parts"},
                {"spawn": ["sleep", "300"]},
                {"toolCall": {"id": "t2", "title": "Run command", "kind": "execute",
                              "rawInput": {"command": "python3 write.py"}}},
                {"permission": {"toolCallId": "t2",
                    "options": [{"id": "allow_once", "name": "Yes", "kind": "allow_once"},
                                {"id": "cancel", "name": "No", "kind": "reject_once"}],
                    "allowed": [{"writeFile": {"path": "part1.txt", "content": "1\n"}}]}}
            ]}
        ]
    })
}

fn detail(env: &Env, id: &str) -> Value {
    serde_json::from_str(&env.ok(&["show", id])).unwrap()
}

fn methods(env: &Env, agent: &str) -> Vec<String> {
    env.agent_log(agent)
        .iter()
        .map(|v| v["method"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn end_to_end() {
    let mut env = Env::with_agents(&[("fake", script(true)), ("fakeload", script(false))]);
    env.start_actuo();

    // 1. A session in its own worktree.
    let id = env
        .ok(&["start", env.repo.to_str().unwrap(), "--agent", "fake"])
        .trim()
        .to_string();
    let d = detail(&env, &id);
    let worktree = PathBuf::from(d["worktreePath"].as_str().unwrap());
    assert_eq!(d["cwd"], d["worktreePath"]);
    assert!(worktree.starts_with(env.home.join("worktrees")));
    assert!(worktree.join("README.md").exists());
    assert_eq!(d["branch"], format!("actuo/{id}"));

    // 2. Events stream live.
    let mut attach = Attach::start(&env, &id);
    attach.wait_for("state idle");
    assert_eq!(env.ok(&["send", &id, "write notes"]).trim(), "turn 1");
    attach.wait_for("t1 > write notes");
    attach.wait_for("t1 agent: Writing NOTES.md");

    // 3. A permission request answered with one of the agent's options.
    attach.wait_for("t1 PERMISSION 1.r1 for edit pending \"Write NOTES.md\"");
    attach.wait_for("1. allow-once [allow_once] Yes");
    env.ok(&["respond", &id, "1.r1", "allow-once"]);
    attach.wait_for("t1 request 1.r1 allowed once (allow-once) by client");
    attach.wait_for("t1 turn ended: end_turn, files: added NOTES.md");
    assert_eq!(
        std::fs::read_to_string(worktree.join("NOTES.md")).unwrap(),
        "notes\n"
    );
    assert!(!env.repo.join("NOTES.md").exists());

    // 4. Interrupt a running turn (with its permission request open).
    env.ok(&["send", &id, "long task"]);
    attach.wait_for("t2 PERMISSION 2.r1");
    env.ok(&["interrupt", &id]);
    attach.wait_for("t2 request 2.r1 cancelled by interrupt");
    attach.wait_for("t2 turn ended: cancelled");
    assert!(!worktree.join("part1.txt").exists());

    // 5. Revert the files to the checkpoint before turn 1.
    let out = env.ok(&["revert", &id, "1"]);
    assert!(out.contains("deleted [\"NOTES.md\"]"), "{out}");
    attach.wait_for("reverted to checkpoint 0");
    assert!(!worktree.join("NOTES.md").exists());
    drop(attach);

    // 6a. Stop actuo cleanly and start it again: the history is intact and the
    // session continues through session/resume.
    let first_agent = last_agent_pid(&env, "fake");
    env.stop_actuo(false);
    assert!(!alive(first_agent), "a clean stop kills the agent");
    env.start_actuo();
    assert!(env.ok(&["sessions"]).contains("detached"));
    let mut attach = Attach::start(&env, &id);
    attach.wait_for("t1 turn ended: end_turn, files: added NOTES.md");
    attach.wait_for("t2 turn ended: cancelled");
    attach.wait_for("reverted to checkpoint 0");
    attach.wait_for("state detached (actuo stopped)");
    assert_eq!(env.ok(&["send", &id, "hello"]).trim(), "turn 3");
    attach.wait_for("state starting (reattaching)");
    attach.wait_for("state idle (reattached with session/resume)");
    let started = attach.wait_for("t3 > ");
    assert!(started.contains("hello"));
    attach.wait_for("t3 agent: ok");
    // Turn 3 changed nothing: the revert before it isn't counted as its change.
    let ended = attach.wait_for("t3 turn ended");
    assert_eq!(ended, "t3 turn ended: end_turn");
    let m = methods(&env, "fake");
    assert!(m.contains(&"session/resume".to_string()));
    assert!(!m.contains(&"session/load".to_string()));
    assert_eq!(detail(&env, &id)["turnCount"], 3);

    // 6b. Kill actuo with a turn running and a request open. The agent exits
    // when its stdin closes, but a command it started (here `sleep 300`) is left
    // running in its process group until the next actuo kills the group; then the
    // turn ends actuo_stopped and the session continues.
    env.ok(&["send", &id, "long task"]);
    attach.wait_for("t4 PERMISSION 4.r1");
    drop(attach);
    let orphan = last_spawned_pid(&env, "fake");
    env.stop_actuo(true);
    assert!(alive(orphan), "SIGKILL leaves the agent's command running");
    env.start_actuo();
    assert!(!alive(orphan), "the new actuo killed the leftover command");
    let mut attach = Attach::start(&env, &id);
    attach.wait_for("t4 request 4.r1 cancelled by actuo_restart");
    attach.wait_for("t4 tool execute cancelled \"Run command\"");
    attach.wait_for("t4 checkpoint 4");
    attach.wait_for("t4 turn ended: actuo_stopped");
    attach.wait_for("state detached (actuo restarted)");
    env.ok(&["send", &id, "hello again"]);
    attach.wait_for("t5 turn ended: end_turn");
    let d = detail(&env, &id);
    assert_eq!(d["turnCount"], 5);
    assert_eq!(d["turns"][3]["stopReason"], "actuo_stopped");
    assert_eq!(d["openRequests"], json!([]));
    drop(attach);

    // 6c. An agent that can load but not resume: the replay is dropped and the
    // history still comes from actuo.
    let lid = env
        .ok(&[
            "start",
            env.repo.to_str().unwrap(),
            "--agent",
            "fakeload",
            "--prompt",
            "first",
        ])
        .trim()
        .to_string();
    let mut attach = Attach::start(&env, &lid);
    attach.wait_for("t1 turn ended: end_turn");
    drop(attach);
    env.stop_actuo(false);
    env.start_actuo();
    env.ok(&["send", &lid, "second"]);
    let mut attach = Attach::start(&env, &lid);
    attach.wait_for("state idle (reattached with session/load)");
    attach.wait_for("t2 turn ended: end_turn");
    assert!(methods(&env, "fakeload").contains(&"session/load".to_string()));
    // The agent replayed "first" and "ok" during load; neither became an event.
    let turns = attach.seen.iter().filter(|l| l.contains("> first")).count();
    let answers = attach
        .seen
        .iter()
        .filter(|l| l.contains("agent: ok"))
        .count();
    assert_eq!((turns, answers), (1, 2), "{:#?}", attach.seen);
}

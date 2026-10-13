//! `actuo stdio`: joins stdin and stdout to the running actuo's socket.

mod common;

use common::Env;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn stdio(env: &Env) -> Child {
    Command::new(env!("CARGO_BIN_EXE_actuo"))
        .arg("stdio")
        .env("ACTUO_HOME", &env.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits up to 10 s for `child` to exit; returns its exit code.
fn exit_code(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        assert!(Instant::now() < deadline, "actuo stdio didn't exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn joins_stdin_and_stdout_to_the_socket() {
    let mut env = Env::new(&json!({"turns": []}));

    let out = env.run(&["stdio"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "no actuo is running at {}",
            env.socket().display()
        )),
        "{stderr}"
    );

    env.start_actuo();

    // A JSON-RPC call through stdio gets actuo's answer; stdin's EOF ends it.
    let mut child = stdio(&env);
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "client": {"name": "test", "version": "0"}}});
    writeln!(stdin, "{call}").unwrap();
    let answer: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["protocolVersion"], 1);
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "agent/list", "params": {}});
    writeln!(stdin, "{call}").unwrap();
    let answer: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(answer["id"], 2);
    assert!(answer["result"]["agents"].is_array(), "{answer}");
    drop(stdin);
    assert_eq!(exit_code(&mut child), Some(0));

    // Stdin closed right after the calls: their answers still come, then it exits.
    let mut child = stdio(&env);
    let mut stdin = child.stdin.take().unwrap();
    let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "client": {"name": "test", "version": "0"}}});
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "run/list", "params": {}});
    writeln!(stdin, "{initialize}\n{list}").unwrap();
    drop(stdin);
    let answers: Vec<Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert_eq!(answers.len(), 2, "{answers:#?}");
    assert_eq!(answers[0]["result"]["protocolVersion"], 1);
    assert_eq!(answers[1]["id"], 2);
    assert_eq!(exit_code(&mut child), Some(0));

    // Actuo stopping ends it too, with stdin still open.
    let mut child = stdio(&env);
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "client": {"name": "test", "version": "0"}}});
    writeln!(stdin, "{call}").unwrap();
    assert!(
        lines
            .next()
            .unwrap()
            .unwrap()
            .contains("\"protocolVersion\":1")
    );
    env.stop_actuo(false);
    assert_eq!(exit_code(&mut child), Some(0));
    drop(stdin);
}

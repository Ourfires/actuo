//! `fake-srt`: stands in for sandbox-runtime's `srt` in tests. Takes
//! `[--debug] --settings <file> -- <program> <args>...` and refuses anything else
//! before `--` (the real srt would read it as its own options), appends what it was
//! given to `<file>.calls.jsonl` (`{"argv", "env", "settings"}`, the settings as
//! parsed JSON), then runs the program with the environment it got, without any
//! sandbox.
//!
//! The records preload, stood in for: with `ACTUO_SRT_RECORDS_FD` set, the lines of
//! `fake-srt-records.jsonl` next to the settings file (if there) go to that
//! descriptor, and the variable is removed from the program's environment. Unlike
//! the preload, the descriptor stays open in the program (closing it by number
//! would take `unsafe`).
//!
//! `FAKE_SRT_UNIX_SOCKETS=1` in the program's environment when the settings allow
//! every Unix socket (`network.allowAllUnixSockets`), so a test can stand in for a
//! command that needs one.

use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut settings = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--debug" | "-d" => i += 1,
            "--settings" | "-s" => {
                settings = args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            other => {
                eprintln!("fake-srt: {other:?} before `--`: srt would read it as its own");
                std::process::exit(2);
            }
        }
    }
    let Some(settings) = settings else {
        eprintln!("fake-srt: --settings is required");
        std::process::exit(2);
    };
    let command = &args[i..];
    if command.is_empty() {
        eprintln!("fake-srt: no command");
        std::process::exit(2);
    }
    let parsed: Value = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let env: serde_json::Map<String, Value> =
        std::env::vars().map(|(k, v)| (k, json!(v))).collect();
    let line = format!(
        "{}\n",
        json!({"argv": command, "env": env, "settings": parsed})
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{settings}.calls.jsonl"))
    {
        let _ = f.write_all(line.as_bytes());
    }
    if let Ok(fd) = std::env::var("ACTUO_SRT_RECORDS_FD") {
        let records = std::path::Path::new(&settings).with_file_name("fake-srt-records.jsonl");
        if let (Ok(text), Ok(mut out)) = (
            std::fs::read_to_string(records),
            std::fs::OpenOptions::new()
                .append(true)
                .open(format!("/dev/fd/{fd}")),
        ) {
            let _ = out.write_all(text.as_bytes());
        }
    }
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..]).env_remove("ACTUO_SRT_RECORDS_FD");
    if parsed["network"]["allowAllUnixSockets"] == json!(true) {
        cmd.env("FAKE_SRT_UNIX_SOCKETS", "1");
    }
    let err = cmd.exec();
    eprintln!("fake-srt: {}: {err}", command[0]);
    std::process::exit(127);
}

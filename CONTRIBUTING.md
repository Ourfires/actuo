# Contributing

## Build

You need:

- Rust. The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it, with `rustfmt` and `clippy`, the first time you build.
- `git` on `PATH`.
- Node.js, for one test file only (`crates/actuo-core/tests/srt_records.rs` runs the sandbox's records preload and the local-ports wrapper, `check-local-network.cjs`, with real Node). Actuo itself downloads its own pinned Node; the tests use a stand-in for it and never download it.

```bash
cargo build
```

The binary is `target/debug/actuo`. To run it in the foreground against a home of its own, not your `~/.actuo`:

```bash
ACTUO_HOME=/tmp/actuo-dev target/debug/actuo serve
```

Every other command talks to the running actuo; give it the same `ACTUO_HOME`.

## Checks

A change must pass these on macOS and Linux. CI runs them on both.

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

CI also checks every dependency's license against `deny.toml` (permissive licenses only). With [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) installed:

```bash
cargo deny check licenses
```

## How the tests work

The tests never start a real agent, call real GitHub or need a real sandbox. Three stand-ins in this workspace take their place:

- `fake-agent`: an ACP agent that follows a JSON script (messages, tool calls, permission requests, file writes, crashes, resume after a restart).
- `fake-gh`: answers the `gh` commands the lane uses from a JSON file that stands in for GitHub, with a local bare repo as `origin`, and records every call. A command it doesn't know exits 2, so a test notices a new call.
- `fake-srt`: takes `srt`'s arguments, records the sandbox settings it was given, and runs the command without a sandbox.

`crates/actuo/tests/end_to_end.rs` is actuo's end-to-end test. Try a change against a real agent or real GitHub by hand, with an `ACTUO_HOME` of its own.

## Everything works from the CLI

The command line is actuo's interface. Everything actuo does has an `actuo` command. `crates/actuo/src/coverage.rs` lists each request actuo answers with the command that makes it; a test fails when one is missing. When you add something actuo does, add its command and its line there.

A setting's help is its doc comment: `actuo settings keys` prints it.

## Conventions

- Keep changes small, one step at a time, each with a clear commit message.
- A change users will notice gets a line in [CHANGELOG.md](CHANGELOG.md) under "Unreleased".
- Agents run as child processes and speak ACP v1 over stdio, through the `agent-client-protocol` crate. Actuo doesn't contain an agent loop.
- Shell out to `git` and `gh`; no libgit2.
- A crashed agent ends its session with an error; it never takes actuo down.
- Events are written to the store before they are sent to anyone.
- Nothing secret goes into the code, logs or events.
- Pin the versions of agent adapters, sandbox-runtime and Node.js (`crates/actuo-core/src/node.rs`), and check their current releases before changing a pin.
- Packages are named `actuo-*` (and the test stand-ins `fake-*`). Never name one `core`: it shadows Rust's `core` crate.

## License

Actuo is licensed under the Apache License 2.0. By section 5 of that license, any contribution you submit for inclusion is licensed under it too, without further terms. There is no contributor license agreement.

If you copy code from another project, say so in the pull request and keep its copyright notice. For code under Apache-2.0, also add a line to `NOTICE`.

# Actuo

A headless service that hosts coding-agent sessions: it spawns agent CLIs, speaks ACP to them over stdio, and owns their git worktrees, checkpoints and the session store. Its lane turns labeled GitHub issues into draft PRs, as a client of sessions. User docs: `README.md`, `docs/`. Build and test details: `CONTRIBUTING.md`.

## Checks (from the repo root)

- Toolchain pinned in `rust-toolchain.toml` (the minimum is Rust 1.88, the ACP SDK's). Edition 2024, tokio.
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` must pass on macOS and Linux.
- Linux from macOS (Docker): `docker run --rm --init -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/tmp/target rust:1-bookworm bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq nodejs >/dev/null && cargo test --workspace'`
- Tests use the stand-ins `fake-agent`, `fake-gh` and `fake-srt`, never real agents, GitHub or a sandbox. The end-to-end test: `cargo test -p actuo --test end_to_end`.

## Rules

- Sessions are the core unit. The lane is a client of sessions that answers permission requests by rules; never run an agent with its permission prompts turned off.
- No agent loop: spawn agent CLIs and speak ACP v1 through the `agent-client-protocol` crate (no `unstable_protocol_v2`).
- Shell out to `git` and `gh`; no libgit2.
- Listen only on the local Unix socket (mode 0600); remote access is `ssh <host> actuo stdio`.
- Never collect, prompt for or store credentials. Nothing secret in code, logs or events.
- Lane runs always run in the sandbox; issue text and comments are untrusted input.
- Everything actuo does has a CLI command (`crates/actuo/src/coverage.rs`, checked by a test). A setting's doc comment is its help (`actuo settings keys`).
- Write events to the store before sending them. A crashed agent ends its session, never actuo.
- Pin agent adapter, sandbox and Node versions; check their current releases before changing a pin. Node's pin is `VERSION` and the three sha256s in `crates/actuo-core/src/node.rs`, from `https://nodejs.org/dist/<version>/SHASUMS256.txt`; bump it with Node's security releases.
- Packages are named `actuo-*` (test stand-ins `fake-*`); never `core`.
- Copied code must be under a compatible permissive license and keep its notice; Apache-2.0 code also gets a line in `NOTICE`.

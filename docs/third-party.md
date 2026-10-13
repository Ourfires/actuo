# Third-party software

Actuo is licensed under Apache-2.0. The agents it runs, their adapters, the sandbox and the Node.js they run on are not part of it. Actuo downloads them (the first time a session needs one, or all at once with `actuo agents install`), each under its own license and terms. You use your own agent accounts, under the agent provider's terms, and you pay for what the agents use.

The versions below are the pins of this source tree (`crates/actuo-core/src/config.rs`, `crates/actuo-core/src/sandbox.rs` and `crates/actuo-core/src/node.rs`). `actuo agents` shows the versions your actuo uses. Licenses are as each package's own metadata states them; read the package for its full terms.

## Node.js, from nodejs.org

| Download | Pinned version | License | Used for |
| --- | --- | --- | --- |
| Node.js, the official build for your platform (`.tar.xz`) | v24.21.0 (LTS) | MIT; its `LICENSE` file lists the licenses of what it bundles, npm's Artistic-2.0 among them | Running the adapters and the sandbox, and installing them with its npm |

It's checked against the sha256 pinned in actuo for each platform (from nodejs.org's `SHASUMS256.txt` for that version) and unpacked into `~/.actuo/node/<version>`; other versions there are removed. Actuo runs the adapters and `srt` as `<that node> <script>`: the `node` on your `PATH` isn't used for them, and your `PATH` is left as it is for everything else.

## Downloaded from npm, with that Node's npm

Each package is installed into its own folder, `~/.actuo/agents/<package>@<version>`.

| Package | Pinned version | License (package metadata) | Used for |
| --- | --- | --- | --- |
| `@agentclientprotocol/claude-agent-acp` | 0.84.0 | Apache-2.0 | The `claude` agent: an ACP adapter for Claude Code |
| `@agentclientprotocol/codex-acp` | 2.0.1 | Apache-2.0 | The `codex` agent: an ACP adapter for Codex |
| `@anthropic-ai/sandbox-runtime` | 0.0.78 | Apache-2.0 | The sandbox (`srt`) around lane runs |

npm also installs each package's dependencies, each under its own license.

### What the adapters bring in

- **Claude.** `claude-agent-acp` 0.84.0 depends on Anthropic's Claude Agent SDK (`@anthropic-ai/claude-agent-sdk`, 0.3.284), which includes the Claude Code program for your platform. These are not open source. Their package metadata says: "© Anthropic PBC. All rights reserved. Use is subject to the Legal Agreements outlined here: https://code.claude.com/docs/en/legal-and-compliance." Using them needs your own Claude account or API key, under Anthropic's terms.
- **Codex.** `codex-acp` 2.0.1 depends on OpenAI's Codex CLI (`@openai/codex`, any `0.159.x` from 0.159.1, as npm resolves it at install time), whose package metadata says Apache-2.0. Using it needs your own OpenAI account, under OpenAI's terms.
- **The sandbox.** sandbox-runtime uses the operating system's own sandboxing: Seatbelt on macOS, bubblewrap on Linux.

An agent you add yourself in `~/.actuo/agents.toml` runs the program you name there, under that program's terms.

## Programs you install

Actuo calls these programs; it doesn't ship them.

| Program | Used for |
| --- | --- |
| `git` | Worktrees, checkpoints, commits and pushes |
| `gh` (GitHub CLI) | The lane: issues, labels, comments, pull requests, cloning |
| `curl` | The install script, `actuo update`, and the release check |
| `bubblewrap`, `socat`, `ripgrep` | The sandbox on Linux |
| `launchctl` (macOS), `systemctl` and `journalctl` (Linux) | The user service |
| Your project's package manager (npm, pnpm, yarn, bun) | The lane's setup and checks |

## What actuo fetches by itself

- **Node.js**, from nodejs.org, and **the adapters and the sandbox**, from the npm registry: by `actuo agents install`, and when a session or `actuo login` needs one that isn't installed yet.
- **The latest release's version**, from actuo's GitHub releases (Ourfires/actuo): when `actuo serve` starts and once a day after. `actuo status` shows when a newer release is out. Nothing is installed until you run `actuo update`.
- **Agent options.** For each pinned agent in use, actuo starts the agent once per version, without a prompt, to learn which models and options it offers (`actuo agents probe`). No tokens are spent.

The agents themselves reach their providers' APIs, with your account. Lane runs turn off the agents' own telemetry (`lane.telemetry = false`) and their web tools (`lane.web_tools = false`). Sessions you start by hand run with the agent's own defaults.

## Compiled into actuo

Actuo's binary is built from Rust crates listed in `Cargo.lock`. Each dependency's license must be one of those allowed in `deny.toml` (Apache-2.0, Apache-2.0 WITH LLVM-exception, MIT, Unicode-3.0, Unlicense, Zlib); `cargo deny check licenses` checks this in CI. [NOTICE](../NOTICE) lists code adapted from other projects.

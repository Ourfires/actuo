<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/actuo-dark.png">
    <img src="assets/actuo.png" alt="actuo" width="128">
  </picture>
</p>

<h1 align="center">actuo</h1>

<p align="center">
  Run coding agents on your machine or a server, and turn labeled GitHub issues into draft PRs.
</p>

<p align="center">
  <a href="https://github.com/Ourfires/actuo/actions/workflows/ci.yml"><img src="https://github.com/Ourfires/actuo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License: Apache-2.0"></a>
</p>

`actuo` hosts live coding-agent sessions and lets you drive them from the terminal. It starts agent CLIs (Claude Code and Codex are built in; any agent that speaks the Agent Client Protocol can be added) and keeps each session going when you disconnect.

- **Isolated.** By default each session works in its own git worktree and branch; your checkout isn't touched.
- **Reversible.** Every turn ends with a checkpoint. See the diff, or revert to any turn.
- **You stay in control.** When the agent asks for permission, you allow, reject or cancel.
- **Durable.** Sessions are stored on disk and survive a restart; reattach and see the full history.
- **Issue to PR.** Label a GitHub issue and actuo works it into a draft PR in a sandbox, running your checks after each turn. When it needs you, it says why on the issue and waits.

Actuo is early (0.1). It runs on macOS (Apple Silicon) and Linux (x86_64, arm64).

## Install

```bash
curl -fsSL https://github.com/Ourfires/actuo/releases/latest/download/install.sh | sh
```

The script checks the download's sha256 and installs actuo for your user, never as root. It copies the binary to `~/.actuo/bin/actuo` and starts it as a service: a launch agent on macOS (logs in `~/.actuo/logs/serve.log`), a systemd user unit on Linux (logs with `journalctl --user -u actuo`).

It also adds `~/.actuo/bin` to your `PATH` in your shell's startup file (`~/.zshrc`; `~/.bashrc` on Linux or `~/.bash_profile` on macOS; fish's `conf.d`; else `~/.profile`) and says which. Open a new terminal to use `actuo`. To leave your startup file alone, run the script with `sh -s -- --no-modify-path` instead of `sh`.

On Linux, a user unit stops when you log out unless linger is on; the install prints the `loginctl enable-linger` command, which root runs.

### Requirements

- `git` and `curl`; on Linux also `tar` with `xz`, and systemd with a user session (`systemctl --user`).
- An account with each agent you use, logged in once with the agent's own CLI. Actuo doesn't sign you up or log you in to anything.

You don't need Node.js: the first time a session uses an agent, actuo downloads its own pinned Node (about 30 MB) and the agent's adapter. The Node on your `PATH`, if any, isn't used for them.

## Quick start

Start a session on a local git repository (with at least one commit) with a first prompt, and attach to it:

```bash
actuo start ~/code/my-app --prompt "Explain this project" --attach
```

The first session with an agent installs actuo's Node and the agent's adapter, which takes a minute or two. The agent works in a new worktree under `~/.actuo/worktrees`, on the branch `actuo/<session>`, starting from your checkout's `HEAD`.

While attached you see the agent's messages and tool calls as they come:

- When it asks for permission, its options are numbered: type a number (or `c` to cancel) and press Enter.
- When no request is open and no turn is running, any other line is sent as the next prompt.
- `/interrupt` cancels the running turn and `/stop` ends the session. `/quit` (or Ctrl-C) detaches and leaves the session running.

Without `--attach`, `actuo start` prints the session id, waits until the agent is ready, and exits; if the agent can't start (say it needs a login), it says why and exits non-zero.

Then, from any terminal:

```bash
actuo sessions                       # list sessions
actuo diff <session>                 # its changes as a unified diff
actuo checkpoints <session>          # checkpoint n is after turn n
actuo revert <session> 2             # files back to before turn 2
actuo send <session> "Also add tests"
actuo stop <session>                 # ends it; the worktree stays
```

Session and run ids can be shortened to any unique prefix. An idle agent is stopped after 15 minutes (`sessions.idle_agent_timeout`) and the session goes `detached`; a new prompt resumes it.

## The issue-to-PR lane

Label a GitHub issue and actuo works it into a draft pull request. It never merges. Lane runs work in a sandbox with an agent login of their own, so prepare once:

```bash
actuo agents install      # adapters + the sandbox, then a sandbox self-test
actuo login claude        # the lane's own login (or: actuo login codex)
```

On Linux the sandbox needs `bubblewrap`, `socat` and `ripgrep`, and user namespaces (Ubuntu 24.04 and later restrict them by default). `actuo agents install` says what's missing. The lane also needs the GitHub CLI `gh`, logged in as an account that can push to your repos.

Then add a project, check that its setup and checks run in the sandbox, and turn its lane on:

```bash
actuo project add owner/repo          # opens the proposal in $EDITOR; --yes saves it as is
actuo project check owner/repo
actuo project set owner/repo lane true
```

Add the `actuo` label to an issue. Only the account `gh` is logged in as, or a `trusted_users` entry, can start a run by label. Actuo picks it up at its next poll (every 60 seconds by default), works it, runs your checks, and opens a draft PR. Follow it:

```bash
actuo runs
actuo run watch <run>
```

See [docs/lane.md](docs/lane.md) for labels, limits, approval rules, and what to do when a run waits for you.

## Agent logins

Agents keep their own logins. Actuo never asks for, collects or stores a password, token or key.

- **Sessions you start** use the agent's own login on this machine. Log in once with its CLI: `claude auth login`, or `codex login`. (A Codex session started with `--clean` uses the lane's login instead: `actuo login codex`.)
- **Lane runs** are sandboxed and can't read your agent config. Each agent gets its own config dir under `~/.actuo/agent-homes`; log it in there with `actuo login <agent>`. The login runs in the sandbox; open the page or URL it gives you (on macOS, Claude's opens in your browser). It doesn't need actuo running.
- **A Claude token** instead: put a `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or an `ANTHROPIC_API_KEY` in `~/.config/actuo/agents.env` as a `NAME=value` line, then `chmod 600` it and `actuo restart`. Actuo reads it at start (a variable already in its environment wins) and refuses to start if anyone else can read or write the file, or a line isn't `NAME=value`. It passes the value to Claude agents, those you start included, and to nothing else. `actuo agents` shows the names it passes, never the values.

## Working on a server

Install actuo on the server as the user it should run as, then use the same commands in an SSH session. Sessions and lane runs keep going when you disconnect (on Linux, once linger is on: see Install). Actuo listens only on a Unix socket in its home folder and opens no network port; `ssh <host> .actuo/bin/actuo stdio` joins an SSH connection to that socket (the full path, since a non-interactive SSH command doesn't read your shell's startup file).

## Updating and uninstalling

```bash
actuo update
```

It waits for running work to finish (up to 30 minutes; `--now` doesn't wait), replaces the binary and restarts the service. If the new actuo doesn't answer within 30 seconds, the previous one is put back. `actuo update` replaces only a release build; on a local build it prints the `install.sh` line to use instead.

To uninstall, stop the service and remove actuo's home:

```bash
actuo service uninstall
rm -rf ~/.actuo
```

Then delete the `PATH` line the install added to your startup file (on fish, `~/.config/fish/conf.d/actuo.fish`), and `~/.config/actuo` if you made it. In repos you started sessions on, `git worktree prune` drops the removed worktrees; their `actuo/*` branches and `refs/actuo/` checkpoints stay until you delete them. On GitHub, the `actuo` labels and the lane's `actuo/issue-<n>` branches stay too.

## Reference

Run `actuo --help` and `actuo <command> --help` for every option.

| Command | What it does |
| --- | --- |
| `actuo start <path>` | Start a session. `--agent`, `--level ask\|edits`, `--prompt`, `--attach`, `--base`, `--branch`, `--no-worktree`, `--clean`, `--config id=value` |
| `actuo sessions` | List sessions |
| `actuo attach <session>` | Stream a session's events, history first; answer requests and send prompts. `--from <seq>` |
| `actuo send <session> <text>` | Send a prompt; fails while a turn runs unless `--interrupt` |
| `actuo respond <session> <request> [option]` | Answer a permission request, or `--cancel` it |
| `actuo interrupt <session>` | Cancel the running turn |
| `actuo diff <session\|run>` | The changes as a unified diff |
| `actuo checkpoints <session\|run>` | The checkpoints and the files each turn changed |
| `actuo revert <session> <turn>` | Restore the files to before that turn (`--force` for a `--no-worktree` session) |
| `actuo stop <session>` | End a session and its agent; the worktree stays |
| `actuo show <session\|run>` | A session's or a run's details, as JSON |
| `actuo agents` | The agents, their versions and options. `install`, `probe <agent>` |
| `actuo login <agent>` | Log an agent in for sandboxed lane runs |
| `actuo project add\|list\|show\|check\|set\|allow-host\|remove` | The repos the lane works on |
| `actuo run <issue-url>` | Start a lane run on an issue of a project whose lane is on, without the label. `--agent`, `--level`; `resume`, `cancel`, `watch` |
| `actuo runs` | The lane's runs, newest first |
| `actuo status` | Build, disk, memory, the lane's last poll, and problems |
| `actuo settings get\|set\|unset\|keys\|edit` | Read or change `~/.actuo/settings.toml` |
| `actuo service install\|uninstall` | Install or remove the user service |
| `actuo restart` | Restart after running work finishes (`--now` doesn't wait) |
| `actuo update` | Install the latest release |
| `actuo serve` | Run actuo in the foreground |
| `actuo stdio` | Join stdin and stdout to actuo's socket (for `ssh <host> actuo stdio`) |

`--json` on `sessions`, `runs`, `agents`, `status`, `checkpoints`, `project list` and `settings get` prints the output as JSON.

**Levels** set what the agent may do without asking: `ask` (it asks before changes; the default for `actuo start`) or `edits` (edits in its working directory are allowed; the default for lane runs).

**Settings** live in `~/.actuo/settings.toml`. `actuo settings keys` lists every key with its default and help; `actuo settings set lane.max_runs 3` changes one. `actuo settings edit` opens the file in `$EDITOR` and saves it only if it's valid; a running actuo applies it. Without actuo running, `settings get` reads the file and `set` and `unset` edit it directly. `ACTUO_HOME` moves actuo's home from `~/.actuo`.

**Other agents.** Claude Code and Codex are built in, each with a pinned adapter. Add an ACP agent, or change a built-in one, in `~/.actuo/agents.toml`:

```toml
[agents.my-agent]
command = "/usr/local/bin/my-agent"
args = ["--acp"]
```

An entry merges over the built-in one of the same name: tables merge key by key, other values (arrays included) replace. Giving `command` without `package` drops the built-in adapter and its login. `modes` maps actuo's levels to the agent's mode ids (`modes = { ask = "...", edits = "..." }`), and `[agents.my-agent.sandbox] hosts = [...]` lists the hosts it reaches in lane runs. An agent other than Claude and Codex has no built-in approval rules, so in lane runs each of its requests waits for you until you add rules.

## Documentation

- [docs/lane.md](docs/lane.md): the issue-to-PR lane in depth.
- [docs/security.md](docs/security.md): the security model.
- [docs/third-party.md](docs/third-party.md): what actuo downloads and runs, and under which terms.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The agents, their adapters, the sandbox and the Node.js they run on are not part of actuo. They're downloaded under their own licenses and terms; you use your own agent accounts and pay their costs. See [docs/third-party.md](docs/third-party.md).

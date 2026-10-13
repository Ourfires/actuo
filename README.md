<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/actuo-dark.png">
    <img src="assets/actuo.png" alt="actuo" width="128">
  </picture>
</p>

<h1 align="center">actuo</h1>

`actuo` hosts live coding-agent sessions and lets you drive them from the terminal. It starts agent CLIs (Claude Code and Codex are built in; any agent that speaks the Agent Client Protocol can be added) and talks to them over ACP.

It runs on your own machine or on a server you reach over SSH. A session keeps going when you disconnect: you attach again later and see its history.

- Each session works in its own git worktree, on its own branch.
- Every turn ends with a checkpoint. You can see the diff and revert to any turn.
- When the agent asks for permission, you answer: allow, reject, or cancel.
- Sessions and their events are stored on disk. After actuo restarts, a session whose agent supports resuming can be continued with a new prompt.

It also has an issue-to-PR lane. Add a label to a GitHub issue, and actuo works it into a draft pull request: in a sandbox, with your repo's checks run after each turn, under time and size limits and approval rules. When it needs you, it pauses, says why on the issue, and waits.

## Install

Platforms: macOS on Apple Silicon, Linux on x86_64 and arm64.

```bash
curl -fsSL https://github.com/Ourfires/actuo/releases/latest/download/install.sh | sh
```

The script checks the download's sha256 and installs actuo for your user, never as root. It copies the binary to `~/.actuo/bin/actuo` and starts it as a service of your user: a launch agent on macOS, a systemd user unit on Linux. On macOS the service logs to `~/.actuo/logs/serve.log`; on Linux, read them with `journalctl --user -u actuo`.

Put the binary on your `PATH`:

```bash
export PATH="$HOME/.actuo/bin:$PATH"
```

Later, update to the latest release with:

```bash
actuo update
```

It waits for running work to finish (up to 30 minutes; `--now` doesn't wait), replaces the binary, and restarts the service. If the new actuo doesn't answer within 30 seconds, the previous one is put back. When a release pins newer adapter versions, each is installed the first time a session needs it.

On Linux, a user unit stops when you log out unless linger is on for your user. The install says so and gives the `loginctl enable-linger` command, which root runs.

## Requirements

For sessions:

- `git`, and `curl` (install and updates).
- Node.js and npm. The first time a session uses an agent, actuo installs that agent's pinned adapter with npm into `~/.actuo/agents`. The Claude adapter needs Node 22 or later.
- An account with each agent you use (Claude Code, Codex), logged in once with the agent's own CLI. Actuo doesn't sign you up or log you in to anything; see "Agent logins" below.

For the issue-to-PR lane, also:

- The GitHub CLI `gh`, logged in as an account that can push to the repos you add.
- On Linux, the sandbox needs `bubblewrap`, `socat` and `ripgrep`, and user namespaces. Ubuntu 24.04 and later restrict user namespaces by default; the sandbox-runtime README explains the setting.

## Quick start

Start a session on a local repository, with a first prompt, and attach to it:

```bash
actuo start ~/code/my-app --prompt "Explain this project" --attach
```

The first session with an agent installs its adapter first, which takes a minute. `start` prints the session's id. The agent works in a new worktree under `~/.actuo/worktrees`, on the branch `actuo/<session id>`, starting from your checkout's `HEAD`. The files in your checkout aren't touched.

While attached, you see the agent's messages and tool calls as they come. When it asks for permission, its options are numbered: type the number (or `c` to cancel) and press Enter. Any other line is sent as the next prompt. `/interrupt` cancels the running turn, `/stop` ends the session, `/quit` detaches and leaves the session running.

In another terminal, or after you detach:

```bash
actuo sessions
```

```bash
actuo diff <session>
```

```bash
actuo checkpoints <session>
```

Checkpoint 0 is before turn 1; checkpoint n is after turn n. To put the files back to how they were before turn 2:

```bash
actuo revert <session> 2
```

Send another prompt without attaching:

```bash
actuo send <session> "Also document the flag in the README"
```

End the session (the worktree is kept):

```bash
actuo stop <session>
```

Session and run ids can be shortened to any unique prefix.

## Commands

Run `actuo --help` and `actuo <command> --help` for every option.

| Command | What it does |
| --- | --- |
| `actuo start <path>` | Start a session on a repository. `--agent`, `--level ask\|edits`, `--prompt`, `--attach`, `--base`, `--branch`, `--no-worktree`, `--clean`, `--config id=value` |
| `actuo sessions` | List sessions |
| `actuo attach <session>` | Stream a session's events, its history first; answer requests and send prompts |
| `actuo send <session> <text>` | Send a prompt. `--interrupt` cancels a running turn first |
| `actuo respond <session> <request> [option]` | Answer a permission request, or `--cancel` it |
| `actuo interrupt <session>` | Cancel the running turn |
| `actuo diff <session>` | The session's changes as a unified diff |
| `actuo checkpoints <session>` | The session's checkpoints and the files each turn changed |
| `actuo revert <session> <turn>` | Restore the files to how they were before that turn |
| `actuo stop <session>` | End a session and its agent; the worktree stays |
| `actuo show <session\|run>` | A session's or a run's details, as JSON |
| `actuo agents` | The configured agents, their versions and options. `install`, `probe <agent>` |
| `actuo login <agent>` | Log an agent in for sandboxed lane runs |
| `actuo project add\|list\|show\|check\|set\|allow-host\|remove` | GitHub repos the lane works on |
| `actuo run <issue-url>` | Start a lane run on an issue without the label. `resume`, `cancel`, `watch` |
| `actuo runs` | The lane's runs, newest first |
| `actuo status` | Actuo's build, disk, memory, the lane's last poll, and problems |
| `actuo settings get\|set\|unset\|keys\|edit` | Read or change `~/.actuo/settings.toml` |
| `actuo service install\|uninstall` | Install or remove the user service |
| `actuo restart` | Restart the service after running work finishes (`--now` doesn't wait) |
| `actuo update` | Install the latest release |
| `actuo serve` | Run actuo in the foreground |
| `actuo stdio` | Join stdin and stdout to actuo's socket (for `ssh <host> actuo stdio`) |

`--json` on `sessions`, `runs`, `agents`, `status`, `checkpoints`, `project list` and `settings get` prints actuo's full reply as JSON.

## Levels

A session's level sets what the agent may do without asking:

- `ask`: the agent asks before changes. The default for `actuo start`.
- `edits`: edits in its working directory are allowed without asking. The default for lane runs.

## Agent logins

The agents keep their own logins. Actuo never asks for, collects or stores a password, token or key.

- Sessions you start with `actuo start` use the agent's own login on this machine. Log in once with the agent's own CLI.
- Lane runs are sandboxed and can't read your agent config. Each agent gets a config dir of its own under `~/.actuo/agent-homes`. Log it in there with `actuo login claude` or `actuo login codex`. The login runs inside the sandbox. On macOS the login page opens in your browser; on a server, open the URL the agent prints. `actuo login` doesn't need actuo running.
- For Claude, you can instead give actuo a `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or an `ANTHROPIC_API_KEY`. Write it into `~/.config/actuo/agents.env` as a `NAME=value` line, make the file yours only, and restart:

  ```bash
  chmod 600 ~/.config/actuo/agents.env
  ```

  ```bash
  actuo restart
  ```

  Actuo reads the file when it starts (a variable already in its environment wins), and refuses to start while others can read it. It passes the value to the Claude agent and to no other process. `actuo agents` shows the names it passes, never the values. The file stays as you wrote it across `actuo update`.

## Working on a server

Install actuo on the server as above, as the user it should run as. Then work in an SSH session there with the same commands:

```bash
ssh my-server
```

```bash
actuo sessions
```

Sessions and lane runs keep going when you disconnect. Actuo listens only on a Unix socket in its home folder; it opens no network port. `ssh <host> actuo stdio` joins the SSH connection to that socket.

## The issue-to-PR lane

Lane runs work in a sandbox, with an agent login of their own. Prepare both once:

```bash
actuo agents install
```

```bash
actuo login claude
```

`actuo agents install` installs the pinned adapters and the sandbox (`srt`), then runs the sandbox once and says what's missing if it can't run here (on Linux: `bubblewrap`, `socat`, `ripgrep` or user namespaces). `actuo login codex` logs Codex in instead. Then add a project, check that its setup and checks run in the sandbox, and turn its lane on:

```bash
actuo project add owner/repo
```

```bash
actuo project check owner/repo
```

```bash
actuo project set owner/repo lane true
```

Then add the `actuo` label to an issue. Actuo picks it up at its next poll (every 60 seconds by default), works it in a sandbox, runs your checks, and opens a draft PR. Follow it with:

```bash
actuo runs
```

```bash
actuo run watch <run>
```

Read [docs/lane.md](docs/lane.md) for adding a project, labels, limits, approval rules, and what to do when a run waits for you.

## Settings

Settings live in `~/.actuo/settings.toml`. List every key with its default and what it does:

```bash
actuo settings keys
```

```bash
actuo settings set lane.max_runs 3
```

`actuo settings edit` opens the file in `$EDITOR`; it's saved only if it's valid, and a running actuo applies it. Without an actuo running, `settings get`, `set` and `unset` edit the file directly.

`ACTUO_HOME` moves actuo's home from `~/.actuo` to another folder.

## Other agents

Claude Code and Codex are built in, each with a pinned adapter version. Add another ACP agent, or change a built-in one, in `~/.actuo/agents.toml`. An entry there is merged over the built-in one of the same name. For example:

```toml
[agents.my-agent]
command = "/usr/local/bin/my-agent"
args = ["--acp"]
```

`modes` maps actuo's levels to the agent's mode ids (`modes = { ask = "...", edits = "..." }`). For lane runs, `[agents.my-agent.sandbox] hosts = [...]` lists the hosts the agent reaches inside the sandbox. An agent other than Claude and Codex has no built-in approval rules: in lane runs every one of its requests waits for you until you add rules.

## More

- [docs/lane.md](docs/lane.md): the issue-to-PR lane in depth.
- [docs/security.md](docs/security.md): the security model.
- [docs/third-party.md](docs/third-party.md): what actuo downloads and runs, and under which terms.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The agents, their adapters and the sandbox are not part of actuo. They are downloaded at install time under their own licenses and terms; you use your own agent accounts and pay their costs. See [docs/third-party.md](docs/third-party.md).

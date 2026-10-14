# Security model

Actuo starts coding agents and lets them change files and run commands. This page says what limits them, what actuo does with credentials, and what it publishes. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Who can talk to actuo

- Actuo listens only on a Unix socket, `~/.actuo/actuo.sock`, with mode `0600`. Its home folder, `~/.actuo`, has mode `0700`. Only your user can connect.
- It opens no network port.
- On another machine, actuo is reached over SSH: run `actuo` commands in an SSH session there, or `ssh <host> actuo stdio`, which joins the SSH connection's stdin and stdout to the socket. SSH does the authentication.
- Anyone who can connect to the socket can do everything actuo can. Treat access to actuo's user as access to actuo.

Actuo belongs to the user that runs it. The install script refuses to run as root, and installs a service of your user.

## Sessions you start by hand

A session from `actuo start` works in its own git worktree under `~/.actuo/worktrees`, so it doesn't change the files in your checkout (unless you pass `--no-worktree`, which runs it in your checkout). A worktree is not a sandbox: the agent runs with your user's permissions, your environment, its own login and its own config. What it may do without asking is the session's level (`ask` by default) and the agent's own permission prompts, which you answer.

Lane rules that allow a request apply only inside the sandbox. A session without the sandbox never has its requests allowed by a rule.

## Lane runs are always sandboxed

Every lane session, setup command, check and commit check runs inside Anthropic's sandbox-runtime (`srt`): Seatbelt on macOS, bubblewrap on Linux, and a proxy that lets through only listed hosts. Actuo writes a settings file for each process (`~/.actuo/sandbox/*.json`, mode `0600`).

For every sandboxed process:

- Reading is denied for your whole home folder, then allowed again only for the paths listed below. These paths are also denied by name: `~/.ssh`, `~/.config/gh`, `~/.git-credentials`, `~/.netrc`, `~/.npmrc`, `~/.aws`, `~/.docker`, `~/.claude`, `~/.codex`, `~/.agents`, `~/.config/actuo`.
- Writing is allowed only where listed.
- The network reaches only the listed hosts (a strict allowlist).
- The environment is cleared. Only `PATH`, `HOME`, `USER`, `LANG`, `LC_ALL` and `TERM` are kept, plus a temp folder and what actuo sets. No `SSH_AUTH_SOCK`, no `GH_TOKEN`, no `GIT_ASKPASS`.
- git reads a minimal config actuo writes, never your `~/.gitconfig`. Hooks are off.
- Unix sockets are off, unless a project sets `checks_unix_sockets` for its setup and checks. When a check fails for no reason actuo can tell, `actuo project check` runs it once more with Unix sockets allowed, to tell you whether that is the cause; it doesn't change the setting.

What each process may read, write and reach:

| | Reads | Writes | Reaches |
| --- | --- | --- | --- |
| Agent session | its worktree, the repo's git folder, the installed adapters and actuo's Node, its temp and config folders, the project's `read` list and the agent's `sandbox.read` | its worktree, its temp folder, its own config folder under `~/.actuo/agent-homes` | its model API and login-refresh hosts; the npm and yarn registries in the project's `network` (all of `network` with `agent_network`) |
| Setup | its worktree, the repo's git folder, the project's package store, the project's `read` list; `~/.npmrc` only with the project's `setup_reads_npmrc` | its worktree, its temp folder, the package store (`~/.actuo/stores/<owner>/<repo>`) | the project's `network` |
| Checks, commit check | its worktree, the repo's git folder, the package store, the project's `read` list | its worktree, its temp folder | nothing; on macOS, the loopback ports in `checks_local_ports` and `checks_local_listen_ports` (see below) |
| `actuo login <agent>` | the installed adapters and actuo's Node | the agent's config folder under `~/.actuo/agent-homes`, a temp folder | the agent's login hosts; it may listen on localhost for an OAuth callback |

On macOS, the sandbox can't keep `checks_local_listen_ports` to loopback: while a check listens on one of those ports, other machines on the network can reach it too. Set them only for tests you trust, on a network you trust.

The agent can read the repo's git folder but not write it: it can't commit or push. Actuo commits and pushes from outside the sandbox, after its checks. The agent can write its own worktree, including the small `.git` file that points git at the repo. Actuo never reads that file: it finds each worktree's git folder from the repo the worktree was made from (files the agent can't write) and runs all its own git there with `GIT_DIR` and `GIT_WORK_TREE` set, so a rewritten `.git` can't send actuo's git (the checkpoint after each turn, diffs, reverts, the commit and push) to a config or objects the agent controls. Git in one of actuo's worktrees that it hasn't resolved this way is refused.

A project's `read` list (and an agent's `sandbox.read` in `agents.toml`) must be absolute or start with `~/`, and can't contain `..`. It can't be your home folder or a folder above it, actuo's home, or one of the credential paths above, or contain or sit inside one.

By default, lane runs also turn off the agent's web search and fetch tools (`lane.web_tools = false`) and its own telemetry (`lane.telemetry = false`).

## Protected paths

Some files steer agents or CI: `.github/`, `.claude/`, `.codex/`, `.mcp.json`, `AGENTS.md`, `CLAUDE.md` by default (`lane.protected_paths`). A run whose change touches one of them pauses before its commit, and only you can let it go on (`actuo run resume`). The same pause comes from a change over `lane.max_changed_files` or `lane.max_changed_lines`.

## Credentials

- **Agents keep their own logins.** Actuo never collects, prompts for or stores a password, token or key.
- Lane sessions use a config folder of their own for each agent, under `~/.actuo/agent-homes`, never your `~/.claude` or `~/.codex`. `actuo login <agent>` runs the agent's own login inside the sandbox, into that folder.
- **Tokens you give actuo.** An agent's `pass_env` names variables actuo may take from its own environment or, for those not set there, from `~/.config/actuo/agents.env` (`NAME=value` lines you write): for Claude, `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_API_KEY`. Actuo reads that file only if it is a regular file of its user that no one else may read or write; otherwise `actuo serve` doesn't start, and says why without showing the file. When `actuo serve` starts, it keeps those values and removes them from every process it starts, except that agent's. git, gh, setup, checks and logins never get them. The values are never logged or shown; `actuo agents` and the log show the names.
- `pass_env` can't name a variable actuo sets or needs (`PATH`, `HOME`, `ACTUO_*`, `LD_*`, `DYLD_*` and others), or one the agent's own config sets.
- `gh` and git keep their own logins too. Actuo calls them from outside the sandbox; nothing inside the sandbox gets their credentials.

## Nothing secret is published

Before any commit, push, comment or PR, actuo scans the text for credential formats: Anthropic keys and tokens, OpenAI API keys, GitHub tokens, npm tokens, JWTs, Codex refresh tokens, private keys, and any value of 16 characters or more that it passes to an agent from its environment. For a commit it scans the added lines, the new file names, every commit message not yet on `origin`, and its own message.

If it finds one, nothing from that step is published. The run waits (`secret_detected`); a status comment with one is just not posted. The issue comment says where the text was and which format it matched, never the text.

## Agent text on GitHub

Text from the agent (the PR description, quoted messages) is made inert before it is posted:

- links show their target in a code span (a bare URL or `<url>` link already shows it and stays as it is), so no link hides where it goes;
- images become `[image: <alt>] <url>` in code spans, so nothing is fetched;
- raw HTML is shown as code, not rendered;
- `@user` and `@org/team` mentions go in code spans and notify no one;
- diagram and map blocks become plain code.

If a second look still finds any of these, the whole text is posted as one code block.

What the agent asked for in a pending request is posted as a fixed phrase and the command or paths in a code block; the agent's own title for it isn't posted.

## Issue text is untrusted

Anyone who can write on an issue can try to steer the agent. Actuo:

- puts GitHub text between two lines of a random marker, and tells the agent to treat it as a description of the task only;
- removes what GitHub's rendered view hides: HTML comments, zero-width and bidirectional control characters, `<details>` wrappers;
- caps its length (`lane.max_prompt_bytes`);
- runs setup and checks only from the project's settings, never from issue text;
- counts only trusted users' labels and comments: a trigger label added by anyone else is skipped, and comments by anyone else never reach the agent. The issue's own title and text go to the agent whoever wrote them: adding the label is what lets them through, so read the issue before you label it.

None of this stops prompt injection. What limits the damage is the sandbox, the missing credentials, the approval rules, the protected paths and size limits, the scan before publishing, and the draft PR, which a person reviews and merges. Actuo never merges; it pushes only its own `actuo/issue-<n>` branches, with git hooks skipped.

## What the sandbox doesn't cover

The sandbox limits lane runs; it isn't a complete boundary. Know these before you rely on it:

- **Reads outside your home folder aren't restricted.** The sandbox denies your home folder and the credential paths above; `/etc`, `/opt`, `/tmp`, mounted volumes and anything else world-readable stay readable. If `ACTUO_HOME` is outside your home folder, actuo's logs, database and settings there are readable too.
- **Tokens you give actuo reach the agent's sandbox.** A `pass_env` value (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`) is in the agent's environment, visible to the agent and every command it runs. The scan before publishing is what keeps it off GitHub.
- **Installs run outside the sandbox.** Actuo installs the adapters and `srt` with `npm install` under your user, with your environment and `~/.npmrc`; their dependencies resolve at install time. Actuo's own Node is checked against a sha256 pinned in actuo. Actuo's binary is checked against a `.sha256` from the same release: that proves the download is intact, not who made it.
- **Actuo starts agents unprompted** to learn their options (`actuo agents probe`): at `actuo serve` start and after a settings change, for an agent in use whose pinned version hasn't been probed, and when a session shows options the last probe didn't (at most every 10 minutes). Unless `lane.enabled` is on and a project whose lane is on uses that agent, that runs outside the sandbox with your own login.
- **Actuo's commit is yours.** It commits with your git identity and config; with `sign_commits` it signs with your key.

## Recommended setup for the lane

- Run the lane on a dedicated machine, VM or user account, not on the account you work in.
- Log `gh` in there with a GitHub token scoped to the lane's repos only.
- Protect each repo's default branch, so nothing reaches it without review. `actuo project add` tells you whether it is protected.
- Keep `lane.trusted_users` to people you trust to direct an agent on that machine.
- Leave `agent_network`, `checks_unix_sockets`, `setup_reads_npmrc` and `lane.web_tools` off unless a project needs them.
- If a project's setup needs a private registry (`setup_reads_npmrc`), keep only a read-only token in that machine's `~/.npmrc`: the dependencies' install scripts run in setup and can read it too.

# Changelog

Notable changes to actuo, newest first. Releases are named by their commit; `actuo --version` prints it, and `actuo update` installs the latest. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Unreleased

### Changed

- The README is restructured, and the README and docs were checked against the code.
- Issue templates and this changelog.

## e0a9414 (2026-10-14)

### Changed

- Actuo runs its own git in a session's worktree with the git folder it finds from the repo the worktree was made from, never through the worktree's `.git` file. Git in one of actuo's worktrees that it hasn't resolved this way is refused.

## aa74ca9 (2026-10-14)

### Changed

- The lane restores a worktree's `.git` link from its clone before running git on it, and reads the sandbox's git folder from the clone.

## fe85d7c (2026-10-14)

### Changed

- `actuo start` without `--attach` waits until the agent is ready, printing setup steps on stderr; if the agent can't start, it says why and exits 1.
- An agent that needs a login names the command: `claude auth login` or `codex login` for sessions, `actuo login <agent>` for lane runs.
- When actuo isn't running, the CLI says to start it with `actuo service install` (or `actuo serve`).

## dee7f4d (2026-10-13)

### Added

- Actuo downloads its own pinned Node.js (v24.21.0, sha256-checked) into `~/.actuo/node` the first time something needs it, and runs the agents' adapters, the sandbox and npm on it. Node on your `PATH` is no longer needed.
- `actuo agents` shows whether that Node is installed.

### Changed

- `actuo update` no longer asks you to run `actuo agents install`: newly pinned versions install when first needed.

## ca61037 (2026-10-13)

### Added

- `install.sh` adds `~/.actuo/bin` to your `PATH` in your shell's startup file; `--no-modify-path` skips it.

### Changed

- The README's first session takes two commands; the lane's preparation moved to the lane section.

## d1c4f40 (2026-10-13)

First public release.

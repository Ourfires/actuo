# The issue-to-PR lane

The lane turns labeled GitHub issues into draft pull requests. It polls the repos you add, starts a run for each issue a trusted user labeled, and drives an agent session for it: in a sandbox, with your checks after each turn, under limits and approval rules. When it needs you, the run waits and says why on the issue.

The lane drives the same sessions you start by hand. You can attach to a run's session at any time, answer its requests, or take it over.

## What a run does

1. **Claim.** Actuo swaps the trigger label for `actuo:running` and posts a status comment on the issue, which it edits as the run goes on.
2. **Prepare.** It clones the repo into `~/.actuo/repos/<owner>/<repo>` once, and fetches it on each run.
3. **Session.** It starts a session on a new worktree, on the branch `actuo/issue-<n>`, from the default branch on `origin`. The session is always sandboxed and clean: the agent runs without your global agent config.
4. **Setup.** The project's setup commands run in the worktree, in the sandbox. A setup command that fails fails the run.
5. **Turns.** The first prompt is the issue's title and text, with comments by trusted users. The agent works; its permission requests are answered by the approval rules, or the run waits for you.
6. **Checks.** After each turn, the project's checks run in the sandbox, offline. A failing check is sent back to the agent with its output, up to `max_check_retries` times.
7. **Review.** Once the checks pass, one more turn asks the agent to review its work against the issue and reply with a PR title and description; if it changes files, the checks run again. When the checks still fail, there's no review turn unless the repo has commit rules, and the PR takes the issue's title.
8. **Guards.** The change must not touch a protected path or be over the size limits.
9. **Commit and push.** Actuo applies the repo's commit rules, scans what it is about to publish for credential formats, commits, and pushes `actuo/issue-<n>`. The agent never commits or pushes.
10. **Draft PR.** It opens a draft PR (or, if one from the branch is open, replaces its description; its title stays), sets `actuo:pr` on the issue and comments with the PR's link.

If the checks still fail after the retries, the draft PR is opened anyway, with the failing check's output at the top of its description, and the run waits for you.

## Before you start

- `gh` is installed on actuo's machine and logged in as an account that can push to the repo.
- `actuo agents install` has run, and the agent you'll use is logged in for sandboxed runs (`actuo login claude` or `actuo login codex`; see the README's "Agent logins").
- Actuo is running (`actuo status`).

## Adding a project

A project is a GitHub repo on this actuo.

```bash
actuo project add owner/repo
```

`project add`:

- checks that the `gh` account can push to the repo and that it isn't archived (if the account has a pending invitation to that repo, it accepts it first);
- prints the account's access, the repo's visibility, its default branch and whether that branch is protected;
- clones the repo;
- reads the default branch's files and proposes a setup and checks.

Detection reads `package.json` files and lockfiles: the package manager (pnpm, npm, yarn, bun, by lockfile), turbo, the Node version, and each workspace's `lint`, `typecheck` and `test` scripts. The proposed setup is the package manager's frozen install. The proposed checks are those scripts (with turbo: `<x> turbo lint typecheck --affected` and `<x> turbo test --affected`, where `<x>` is `pnpm`, `npx`, `yarn` or `bunx`, each with only the tasks some workspace has). For a repo without a lockfile, nothing is proposed: write the setup and checks yourself.

The proposal opens in `$EDITOR` (or `vi`) as TOML, with the keys of `[projects."owner/repo"]`. Save and close to add the project. To save the proposal as it is, without the editor:

```bash
actuo project add owner/repo --yes
```

A `setup` or `checks` left as proposed isn't saved: it is detected again from the run's worktree at each run, so it follows the repo as it changes. `network` is saved as proposed: the registry, `codeload.github.com` and the download hosts named in the lockfile. A project starts with its lane off.

Other project commands:

```bash
actuo project list
```

```bash
actuo project show owner/repo
```

```bash
actuo project set owner/repo checks '["pnpm test"]'
```

```bash
actuo project remove owner/repo
```

`project set` sets one key of the project (a TOML value; text that isn't TOML is taken as a string). `project remove` takes the project out of the settings and deletes its clone; its runs and sessions stay. It is refused while a run of the project is working. Deleting the clone is also refused while a run hasn't ended or a session in the clone is open; `--keep-clone` keeps the clone and skips that check.

## Checking a project

```bash
actuo project check owner/repo
```

This runs the project's setup and each check once, in the sandbox, on a fresh worktree of the default branch, with no agent. It prints each step's time, exit code and peak memory as it ends, then the totals. When a step fails, actuo says why if it can tell: a host the sandbox refused, a write outside the worktree, a missing local service, a Unix socket, the time limit, memory.

If setup was refused a download host it needs, allow it and check again:

```bash
actuo project allow-host owner/repo example-cdn.com --check
```

`project show` prints the last check run's results under the project's settings.

## Turning the lane on

```bash
actuo project set owner/repo lane true
```

Actuo polls each project whose lane is on every `lane.poll_interval` (`60s` by default), listing up to 100 open issues with the trigger label per repo.

`lane.enabled` (on by default) pauses or resumes every project at once:

```bash
actuo settings set lane.enabled false
```

## Labels

Actuo creates the first five labels (the project's own trigger label in place of `actuo`, if set) in each repo whose lane is on.

| Setting | Default | Meaning |
| --- | --- | --- |
| `lane.labels.trigger` | `actuo` | Added by a trusted user, starts a run |
| `lane.labels.running` | `actuo:running` | The run works |
| `lane.labels.waiting` | `actuo:waiting` | The run waits for you |
| `lane.labels.pr` | `actuo:pr` | The run ended with its draft PR open and its checks passing |
| `lane.labels.failed` | `actuo:failed` | The run failed or was cancelled |
| `lane.labels.agent_prefix` | `agent:` | `agent:<name>` picks the run's agent |

A project can have its own trigger label (`projects."owner/repo".label`).

The run's agent is the one an `agent:<name>` label names; else the project's `agent`; else `sessions.agent` (`claude` by default). A project's `config` (its model and the like) applies only when the run's agent is the project's agent; an `agent:` label that picks another drops it.

If an issue already carries a status label that no run of this actuo set, the run is cancelled before it touches the issue. This keeps two actuo installs from working the same issue.

## Trusted users

Only labels and comments from trusted users count:

- the account `gh` is logged in as, always;
- the logins in `lane.trusted_users`, or in a project's own `trusted_users`, which replaces the lane's list.

```bash
actuo settings set lane.trusted_users '["teammate-login"]'
```

A trigger label added by anyone else is skipped (actuo logs it). Comments by anyone else never go into a prompt.

## Levels and approval rules

A run's level (`lane.level`, or a project's `level`; default `edits`) sets what the agent may do without asking: `ask`, or `edits` (edits in its worktree).

When the agent asks for permission, the lane answers by the agent's approval rules. The first rule that matches the request's tool kind (and, if given, whether every path it touches is inside the worktree) answers it with the first of its option kinds the request offers. If the request offers none of that rule's option kinds, the run waits; later rules aren't tried. A request no rule matches makes the run wait for you.

The built-in rules, for Claude and Codex:

```toml
[[lane.rules.claude]]
kind = "execute"
answer = ["allow_once"]

[[lane.rules.claude]]
kind = "edit"
inside_worktree = true
answer = ["allow_once"]
```

Other agents have none: each request waits for you. `actuo agents` prints each agent's built-in rules.

Rules in `lane.rules.<agent>` replace the built-in ones for that agent. A project's `rules.<agent>` replaces the lane's. Tool kinds: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `switch_mode`, `other`. Option kinds: `allow_once`, `allow_always`, `reject_once`, `reject_always`. `inside_worktree = true` matches a request whose paths (at least one) are all inside the worktree; `false` matches any other request, including one with no paths.

When a rule rejects a request, the agent gets a follow-up turn saying what was rejected and why, up to `max_reject_turns` times; then the run waits.

## Setup, checks and network

| Project key | Default | Meaning |
| --- | --- | --- |
| `setup` | detected | Commands run in the worktree before the first turn; `[]`: none |
| `setup_time_limit` | `10m` | A setup command running longer is stopped |
| `checks` | detected | Commands run after each turn; all must pass; `[]`: none |
| `check_time_limit` | `10m` | A check running longer is stopped (three times this in `project check`) |
| `network` | detected | Hosts setup may reach |
| `agent_network` | `false` | Let the agent's session reach all of `network` |
| `read` | `[]` | Paths under your home folder that setup, checks and the agent need to read, e.g. `~/.bun` |
| `checks_unix_sockets` | `false` | Let setup and checks use Unix sockets |
| `setup_reads_npmrc` | `false` | Let setup read `~/.npmrc`, for a private registry's token. The install scripts can read it too: use a read-only token |
| `checks_local_ports` | `[]` | macOS only: loopback TCP ports checks may connect to |
| `checks_local_listen_ports` | `[]` | macOS only: TCP ports checks may listen on and connect to. The sandbox can't keep them to loopback: other machines on the network can reach a check listening on one |

Setup and checks are the repo's commands from the settings, never text from GitHub. They run with `sh -c` in the worktree.

Network:

- **Setup** reaches only the hosts in `network`. Unset: `registry.npmjs.org` (`registry.yarnpkg.com` for yarn 1), plus `codeload.github.com` when a package manager is detected. `project add` sets it.
- **Checks** reach nothing.
- **The agent** reaches its own model API hosts, plus the package registries in `network` (`registry.npmjs.org`, `registry.yarnpkg.com`). With `agent_network = true`, it reaches all of `network`. Without a registry, the agent is told it can't add dependencies.
- The agent's web search and fetch tools are off (`lane.web_tools = false`). The agents' own telemetry is off (`lane.telemetry = false`).

## Limits

Each of these is a `lane.` setting; a project can set its own over each, except `max_runs`. A run keeps the settings it was created with: changes apply to new runs.

| Setting | Default | What happens |
| --- | --- | --- |
| `max_runs` | `2` | Runs working at once (lane only; paused runs don't count) |
| `turn_time_limit` | `30m` | A longer turn is stopped and the run waits |
| `run_time_limit` | `2h` | The agent's time across a run's turns since it last started or went on (a hand-back, a restart, or the label again on a run with no open request gives the full limit again); past it, the turn is stopped and the run waits. Paused time doesn't count |
| `max_check_retries` | `2` | Times failing checks go back to the agent |
| `max_changed_files` | `50` | A change touching more files pauses the run before its commit; `0`: no limit |
| `max_changed_lines` | `1500` | A change with more added and removed lines pauses the run before its commit; `0`: no limit |
| `max_reject_turns` | `2` | Follow-up turns after a rejected request |
| `max_restart_turns` | `1` | Automatic turns per run after a turn was cut off: "actuo restarted", or "continue" after the agent stopped mid-turn (one count for both) |
| `output_tail_lines` | `200` | Lines of a failing command's output kept as its tail; the agent gets its failing lines instead when actuo can find them |
| `output_tail_bytes` | `20480` | The same output's cap in bytes |
| `max_prompt_bytes` | `32768` | Cap on the issue text in a prompt |
| `protected_paths` | see below | Paths a run may not change without you |

Lockfiles (`package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `bun.lockb`) don't count toward the file and line limits.

`protected_paths` defaults to `.github/`, `.claude/`, `.codex/`, `.mcp.json`, `AGENTS.md`, `CLAUDE.md`. An entry ending in `/` with no other `/` is a folder of that name at any depth (`a/b/` is that folder from the root); an entry without `/` is a file of that name at any depth; an entry with a `/` inside is that exact path; an entry with `*` is a glob from the repo's root (`*` within one name, `**` across folders).

## Commits

- Actuo's commit message is a subject, `Closes #<n>` and an `Actuo-Session:` trailer. The first commit's subject is the PR title (else the issue's title); a later one's is the `Commit:` subject from the agent's review reply (else `Address comments on #<n>`).
- `commit_check`: a command that checks each commit message actuo writes. The message is in a file whose path is appended to the command. It runs in the sandbox like the checks. Any exit but 0 refuses the message: the agent gets one turn with the refusal; a second refusal pauses the run (`commit_format`).
- `commit_subject.prompt`: a sentence the review turn gives the agent, for example the repo's title format.
- `co_authored_by = false`: a `Co-Authored-By` line in the agent's title or commit subject is refused like a `commit_check` failure: one turn to correct it, then the run pauses (`commit_format`).
- `sign_commits` (lane or project; default `false`): `false` commits with `--no-gpg-sign`; `true` follows your git config.

## What actuo posts to GitHub

- Labels on the issue, as above.
- A status comment on the issue: the run's state, the agent, level and model, the session's id, each setup and check result, the PR's link, the run's id. It is edited as the run goes on.
- A comment when the run waits, fails or is cancelled, with the reason and how to go on.
- A comment with the draft PR's link.
- The branch `actuo/issue-<n>` and a draft PR. The PR's description is the agent's, followed by the agent, model and session, each setup and check result, the number of turns and tokens (and cost, when the agent reports one), and `Closes #<n>`.

Before any commit, push, comment or PR, actuo checks the text for credential formats (Anthropic, OpenAI, GitHub and npm tokens, JWTs, Codex refresh tokens, private keys) and for any value of 16 or more characters it passes to an agent from its environment. For a commit, it checks the added lines, new file names and commit messages that aren't on `origin` yet. If it finds one, nothing from that step is published, and the run waits (`secret_detected`); a status comment with one is just not posted. The comment says where, never what.

Agent text it posts is made inert first: links show their target, images become text, raw HTML is shown as code, `@` mentions can't notify anyone, and diagram blocks become plain code. See [security.md](security.md).

Actuo's own comments carry a hidden marker, so they never go into a prompt.

## When a run waits

The run's issue gets the `actuo:waiting` label and a comment that says why and how to go on. `actuo runs` shows the reason in parentheses.

| Reason | Why |
| --- | --- |
| `request` | The agent asked for something no rule answers |
| `no_changes` | After a turn, the branch has no changes against the default branch (the comment quotes the agent's last message) |
| `rejects` | Rules rejected requests after `max_reject_turns` follow-ups |
| `turn_time_limit`, `run_time_limit` | A time limit stopped the turn |
| `checks_failed` | Checks still fail after the retries; the draft PR is open |
| `protected_path` | The change touches a protected path |
| `diff_too_large` | The change is over the file or line limit |
| `cannot_fast_forward` | The branch on GitHub and the run's worktree both have commits the other lacks; actuo doesn't rebase |
| `commit_format` | The repo's commit rules refused the message twice |
| `secret_detected` | Text to publish had a credential format in it |
| `turn_failed` | The agent ended a turn with an error |
| `agent_stopped` | The agent stopped mid-turn again after one "continue" turn |
| `restarted` | An actuo restart cut a turn off again |
| `refusal`, `max_tokens` | The agent refused to go on, or ran out of output tokens |

Ways to go on:

- **Reply on the issue and add the trigger label again.** Your reply goes to the agent as its next turn. While the run waits on an open request, your reply cancels that request and goes in its place.
- **Hand the run back:** `actuo run resume <run>` (refused while it waits on an open request: answer that). After `protected_path` or `diff_too_large` it goes past the guards for this hand-back. After `checks_failed` the checks run once more, with no retries. After `turn_failed` or `secret_detected` the step runs again; after `agent_stopped`, one "continue" turn; after `commit_format`, the review turn again with the refusal; after `cannot_fast_forward`, the fast-forward again, setup, and a turn with the comments since the last prompt. Otherwise the lane goes on from the end of the last turn (no-changes guard, checks, review, PR) without a new agent turn.
- **Answer the open request:** `actuo attach <session>` shows it with numbered options; or use `actuo respond <session> <request> <option>`. The run goes on.
- **Take over:** send the session a prompt yourself, with `actuo send <session> <text>` or a line in `actuo attach`. The run becomes `taken_over` and the lane stays out of it. `actuo send --interrupt` first interrupts a running turn and cancels its open requests, so it takes over a run that waits on a request in one step. When you're done, hand it back with `actuo run resume <run>`; the lane goes on from the end of the turn (checks, review, PR).

## When a run fails

Some problems end a run instead of pausing it: the issue gets `actuo:failed` and a comment with the reason. `setup_failed` (a setup command failed), `auth_required` (the agent needs `actuo login <agent>`), `agent_not_found` (an `agent:<name>` label names an agent actuo doesn't have), `sandbox_unavailable`, `gh_error`, `git_error` and `agent_error`. A run is also cancelled quietly (`not_claimed`) if its trigger label was removed before it started, the issue couldn't be read, or another actuo's status label is on it.

## Following runs

```bash
actuo runs
```

Each line: the run's id, `owner/repo#issue`, its state (and reason), its step, its session, its PR.

```bash
actuo run watch <run>
```

`run watch` prints the run's state changes and finished steps so far, then follows it live until it ends. It exits 1 unless the run ends with a PR (open or merged).

```bash
actuo show <run>
```

```bash
actuo diff <run>
```

`show` prints the run with its steps and state changes as JSON. `diff` and `checkpoints` take a run's id too and show its session's.

To start a run on an issue without the label (the project's lane must be on):

```bash
actuo run https://github.com/owner/repo/issues/42
```

`--agent` and `--level` override the agent and level for that run.

To end a run:

```bash
actuo run cancel <run>
```

The run ends with `actuo:failed` on the issue. Its session and branch stay.

On macOS, actuo posts a notification when a run waits, opens a PR or fails, and keeps the Mac from idle-sleeping while a run works.

## Running an issue again

Adding the trigger label to an issue whose run has ended starts a new run on the same branch, and on the same PR while it is open (else a new draft PR). If the branch moved on `origin`, actuo fast-forwards to it; if both sides moved, the run waits (`cannot_fast_forward`); it never rebases. Setup runs again. The comments since the last run's prompt are the new run's turn; if its session has ended, a new session gets the issue as its first prompt (told to build on the earlier work when the branch has commits).

Adding the label while a run works sends the new comments into its next turn. While a run is taken over, the label is removed with a comment and nothing else happens.

## After the PR is merged or closed

At each poll, for projects whose lane is on, actuo looks at the PR of each issue's latest run that is `pr_opened` or waiting. When one was merged or closed, the run ends (`merged` or `closed`), the lane's labels leave the issue, and actuo removes what the run left on its machine: the session (stopped first), its worktree, the local branch, the session's checkpoint refs, and the run's temp folders and sandbox settings. The run's records and logs, the clone and the repo's package store stay.

## The baseline: failures already on the default branch

A check can fail for reasons that have nothing to do with the change. When a check fails, actuo looks at the same check at the commit the run started from, using a check run of that commit (`actuo project check` makes one for the default branch's head). If there isn't one with that check, it starts one and waits for it.

If the check failed there too, with nothing new, the failure isn't the change's: it isn't sent to the agent, the PR says so, and the next check runs. With turbo, this is decided package by package: when new packages fail, the check goes back to the agent with a note naming the packages that already failed at the starting commit, which aren't its to fix.

## When actuo restarts

Runs stay where they are. When actuo starts again, runs that were working go on from their step. A turn the restart cut off gets one "actuo restarted" turn (`max_restart_turns`). `actuo restart`, `actuo update` and `actuo service install` wait for running work to finish first (up to 30 minutes; `--now` doesn't wait).

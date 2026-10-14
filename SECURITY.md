# Security policy

## Reporting a vulnerability

Report it privately, through GitHub's private vulnerability reporting: [open a report](https://github.com/Ourfires/actuo/security/advisories/new), or on this repository open the **Security** tab and choose **Report a vulnerability**. Please don't open a public issue or pull request for it.

Include what you found, how to reproduce it, which release you used (`actuo --version`), and the platform (macOS or Linux, and the architecture).

## Supported versions

Only the latest release gets security fixes. Update with `actuo update`.

## Scope

In scope: actuo's code in this repository, its release binaries and install script, and the protections it documents in [docs/security.md](docs/security.md). For example:

- another local user, or a remote party, reaching actuo's socket;
- a lane run reading, writing or reaching something its sandbox settings should deny, or getting a command run outside the sandbox;
- a credential, or a value passed to an agent from actuo's environment, getting into a commit, push, comment or PR;
- agent text posted to GitHub that fetches something, notifies someone, or hides a link's target;
- a label or comment from a user who isn't trusted starting a run or reaching the agent;
- a run changing a protected path, or going over a size limit, without pausing.

Prompt injection through issue text is expected: actuo treats that text as untrusted. A report about it is in scope when it gets past one of the limits above.

Out of scope: vulnerabilities in the agents, their adapters, sandbox-runtime, Node.js, git or `gh` themselves. Report those to their projects. If actuo uses one of them in a way that makes it unsafe, that is in scope.

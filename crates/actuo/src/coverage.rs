//! Every protocol method and the `actuo` command that calls it (everything
//! actuo does works from the CLI). A test fails when a method isn't here, or
//! a command here doesn't exist.

/// Method, command. `initialize` is every command's first call; the
/// unsubscribes happen when the command's connection closes.
pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("initialize", "status"),
    ("agent/list", "agents"),
    ("project/open", "start"),
    ("project/list", "project show"),
    ("session/start", "start"),
    ("session/send", "send"),
    ("session/interrupt", "interrupt"),
    ("session/respond", "respond"),
    ("session/subscribe", "attach"),
    ("session/unsubscribe", "attach"),
    ("session/list", "sessions"),
    ("session/get", "show"),
    ("session/stop", "stop"),
    ("checkpoint/list", "checkpoints"),
    ("checkpoint/revert", "revert"),
    ("session/diff", "diff"),
    ("settings/get", "settings get"),
    ("settings/update", "settings set"),
    ("run/start", "run"),
    ("run/list", "runs"),
    ("run/get", "show"),
    ("run/subscribe", "run watch"),
    ("run/unsubscribe", "run watch"),
    ("run/resume", "run resume"),
    ("run/cancel", "run cancel"),
    ("agent/probe", "agents probe"),
    ("project/inspect", "project add"),
    ("project/add", "project add"),
    ("project/remove", "project remove"),
    ("project/check", "project check"),
    ("project/allowHost", "project allow-host"),
    ("actuo/status", "status"),
    ("actuo/drain", "service install"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use actuo_protocol::methods::ALL;
    use clap::CommandFactory;

    #[test]
    fn every_method_has_a_command() {
        let missing: Vec<_> = ALL
            .iter()
            .filter(|m| !COMMANDS.iter().any(|(c, _)| c == *m))
            .collect();
        assert!(missing.is_empty(), "no actuo command calls {missing:?}");
        for (m, _) in COMMANDS {
            assert!(ALL.contains(m), "{m} isn't a protocol method");
        }
    }

    #[test]
    fn every_command_exists() {
        for (m, path) in COMMANDS {
            let mut cmd = crate::Cli::command();
            for name in path.split(' ') {
                cmd = cmd
                    .find_subcommand(name)
                    .unwrap_or_else(|| panic!("{m}: no `actuo {path}`"))
                    .clone();
            }
        }
    }
}

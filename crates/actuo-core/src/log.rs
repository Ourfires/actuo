//! Actuo's log lines (stderr: `serve.log` under launchd, the journal under
//! systemd). Each starts with a timestamp, unless journald adds its own: it sets
//! `JOURNAL_STREAM` for the services whose output it takes.

use std::sync::OnceLock;

/// Writes one log line to stderr; see the module docs.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::line(&format!($($arg)*))
    };
}

pub fn line(text: &str) {
    eprintln!("{}", stamped(text, under_journald()));
}

/// `text` with the time in front (`2026-10-03T10:04:12.345Z text`), or as it is
/// when journald stamps lines itself.
pub fn stamped(text: &str, journald: bool) -> String {
    if journald {
        text.to_string()
    } else {
        format!("{} {text}", crate::time::now())
    }
}

fn under_journald() -> bool {
    static JOURNALD: OnceLock<bool> = OnceLock::new();
    *JOURNALD.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_unless_journald_adds_one() {
        let line = stamped("actuo listening on /s", false);
        let (at, rest) = line.split_once(' ').unwrap();
        assert_eq!(rest, "actuo listening on /s");
        assert_eq!(at.len(), "2026-10-03T10:04:12.345Z".len(), "{at}");
        assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T', "{at}");
        assert_eq!(
            stamped("actuo listening on /s", true),
            "actuo listening on /s"
        );
    }
}

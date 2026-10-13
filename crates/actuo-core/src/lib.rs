//! Sessions, agent host, store, git and settings for actuo.

pub mod agent;
pub mod config;
pub mod detect;
pub mod drain;
pub mod fs_guard;
pub mod git;
pub mod lane;
pub mod log;
pub mod login;
pub mod pass_env;
pub mod probe;
pub mod process;
pub mod release;
pub mod sandbox;
pub mod server;
pub mod session;
pub mod settings;
pub mod status;
pub mod store;
pub mod time;

/// The repo's last commit when built, `-dirty` with uncommitted
/// changes there (`build.rs`).
pub const COMMIT: Option<&str> = option_env!("ACTUO_COMMIT");

/// `0.1.0 (1a2b3c4)`, or the version alone when built outside git.
pub fn version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| match COMMIT {
        Some(c) => format!("{} ({c})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").into(),
    });
    &VERSION
}

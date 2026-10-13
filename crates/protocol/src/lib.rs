//! Types of the protocol actuo speaks on its socket, and its CLI with it.
//!
//! JSON-RPC 2.0, one JSON object per line. The first call is [`methods::Initialize`];
//! events arrive as [`EVENT_NOTIFICATION`] notifications carrying an [`events::Event`].

pub mod errors;
pub mod events;
pub mod ids;
pub mod jsonrpc;
pub mod methods;
pub mod runs;
pub mod settings;
pub mod types;

/// The protocol version this crate speaks. Additive changes (new fields, events or
/// methods) don't bump it; a reader ignores fields and event types it doesn't know.
pub const PROTOCOL_VERSION: u32 = 1;

/// Method name of the notification that carries an [`events::Event`].
pub const EVENT_NOTIFICATION: &str = "event";

/// Method name of the notification that carries a [`runs::Run`] whose state or step
/// changed (after `run/subscribe`).
pub const RUN_STATE_NOTIFICATION: &str = "run.state";

/// Method name of the notification that carries a [`runs::CheckRun`] as it starts,
/// after each step and when it ends (after `run/subscribe`).
pub const CHECK_STATE_NOTIFICATION: &str = "check.state";

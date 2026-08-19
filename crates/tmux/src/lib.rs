// CI's `-D warnings` promotes this P26 AC-10 documentation check to an error.
#![warn(missing_docs)]

//! An async Rust client for the tmux control-mode protocol.
//!
//! Spec: pandaemonium pkg/tmux/doc.go, pkg/tmux/README.md. This crate is a
//! behavioral port — the Go package is the specification, not source to
//! translate; see this workspace's root `CLAUDE.md` for that standing
//! rule. It is a sealed leaf by explicit user directive: no `ganja-*` crate
//! in this workspace may depend on it, and it depends on no `ganja-*`
//! crate, in either direction — CI-asserted, not merely documented.
//!
//! [`Client`][control_mode::Client] owns one persistent `tmux -C`
//! subprocess. Callers send normal tmux commands through it; the client
//! parses the guarded `%begin`/`%end`/`%error` response blocks and exposes
//! asynchronous `%` notifications through a bounded event stream. Command
//! execution is deliberately serialized: only one command is pending at a
//! time, and there is no pipelining. If a pending command's future is
//! dropped before tmux replies, the client is poisoned — a late response
//! can no longer be safely associated with a future command, which is the
//! Go original's context-cancellation rule translated into Rust's
//! drop-based cancellation; see
//! [`Client::exec_raw`][control_mode::Client::exec_raw]'s doc for the exact
//! rule and a reconnect example.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::control_mode::{Arg, Client, Command, Options};
//!
//! let client = Client::new(Options::new().with_session_name("work")).await?;
//! let response = client
//!     .exec(Command::from_static("display-message"), [Arg::raw("-p")])
//!     .await?;
//! println!("{}", response.lines.join("\n"));
//! client.close().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Notification delivery favors keeping the stdout reader live: the event
//! queue is bounded, and when it is full the client drops the oldest
//! buffered notification it can observe and counts the drop, rather than
//! blocking the reader on a slow consumer.
//!
//! `Client` speaks the single `-C` form, communicating with tmux over piped
//! stdio. The double `-CC` form asks tmux to change terminal attributes and
//! requires a controlling terminal on current tmux releases; use
//! [`Parser`][control_mode::Parser] directly if an external PTY-backed
//! transport needs to consume the extra `\x1bP1000p`/`\x1b\` enter/exit
//! framing `-CC` emits.
//!
//! Every command line is built from a [`Command`][control_mode::Command] and
//! zero or more [`Arg`][control_mode::Arg]s, and rendered with
//! [`CommandLine::render`][control_mode::CommandLine::render], which applies
//! the same bare/single/double-quote ladder as the Go original. Asynchronous
//! `%` notifications decode through the `notification` module; pane output
//! inside `%output`/`%extended-output` frames is tmux's own octal-escaped
//! encoding, recovered with
//! [`decode_output_value`][control_mode::decode_output_value] or the typed
//! notification helpers.
//!
//! **Divergence**: the Go package gates its real-tmux integration suite
//! behind `RUN_REAL_TMUX_TESTS=1` so it never touches a user's default
//! tmux server by accident. This port's integration suite instead
//! hard-fails when `tmux` is unavailable, matching this workspace's
//! standing test posture — a green run that skipped everything would be
//! worthless as a signal.

pub mod control_mode;
pub mod error;
pub mod ids;

// Only vocabulary shared across the crate's surfaces is re-exported at the
// root; a control-mode type is named through `tmux::control_mode::…`, so the
// root stays a listing of what every surface speaks rather than of one.
pub use error::Error;
pub use ids::{InvalidId, PaneId, SessionId, WindowId};

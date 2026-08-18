//! An async Rust client for the tmux control-mode protocol.
//!
//! Spec: pandaemonium pkg/tmux/doc.go, pkg/tmux/README.md. This crate is a
//! behavioral port — the Go package is the specification, not source to
//! translate; see this workspace's root `CLAUDE.md` for that standing
//! rule. It is a sealed leaf by explicit user directive: no `ganja-*` crate
//! in this workspace may depend on it, and it depends on no `ganja-*`
//! crate, in either direction — CI-asserted, not merely documented.
//!
//! [`Client`] owns one persistent `tmux -C` subprocess. Callers send normal
//! tmux commands through it; the client parses the guarded
//! `%begin`/`%end`/`%error` response blocks and exposes asynchronous `%`
//! notifications through a bounded event stream. Command execution is
//! deliberately serialized: only one command is pending at a time, and
//! there is no pipelining. If a pending command's future is dropped before
//! tmux replies, the client is poisoned — a late response can no longer be
//! safely associated with a future command, which is the Go original's
//! context-cancellation rule translated into Rust's drop-based
//! cancellation; see [`Client::exec_raw`]'s doc for the exact rule and a
//! reconnect example.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::{Command, Options};
//!
//! let client = tmux::Client::new(Options::new().with_session_name("work")).await?;
//! let response = client
//!     .exec(Command::from_static("display-message"), [tmux::Arg::raw("-p")])
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
//! [`Parser`] directly if an external PTY-backed transport needs to consume
//! the extra `\x1bP1000p`/`\x1b\` enter/exit framing `-CC` emits.
//!
//! Every command line is built from a [`Command`] and zero or more
//! [`Arg`]s, and rendered with [`CommandLine::render`], which applies the
//! same bare/single/double-quote ladder as the Go original. Asynchronous
//! `%` notifications decode through the `notification` module; pane output
//! inside `%output`/`%extended-output` frames is tmux's own octal-escaped
//! encoding, recovered with [`decode_output_value`] or the typed
//! notification helpers.
//!
//! **Divergence**: the Go package gates its real-tmux integration suite
//! behind `RUN_REAL_TMUX_TESTS=1` so it never touches a user's default
//! tmux server by accident. This port's integration suite instead
//! hard-fails when `tmux` is unavailable, matching this workspace's
//! standing test posture — a green run that skipped everything would be
//! worthless as a signal.

mod client;
mod commandline;
mod error;
mod flow;
mod notification;
mod options;
mod output;
mod protocol;

pub use client::*;
pub use commandline::*;
pub use error::*;
pub use flow::*;
pub use notification::*;
pub use options::*;
pub use output::*;
pub use protocol::*;

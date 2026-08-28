// CI's `-D warnings` promotes this P26 AC-10 documentation check to an error.
#![warn(missing_docs)]

//! An async Rust client for tmux, over both of the transports tmux answers
//! on: one persistent control-mode connection, and one plain client
//! invocation at a time.
//!
//! Spec: pandaemonium pkg/tmux/doc.go, pkg/tmux/README.md. This crate is a
//! behavioral port — the Go package is the specification, not source to
//! translate; see this workspace's root `CLAUDE.md` for that standing
//! rule. It is a sealed leaf by explicit user directive: no `ganja-*` crate
//! in this workspace may depend on it, and it depends on no `ganja-*`
//! crate, in either direction — CI-asserted, not merely documented.
//!
//! # Two transports
//!
//! **Control mode** is the persistent one. [`control_mode::Client`] owns a
//! `tmux -C` subprocess for as long as it lives, writes commands to its
//! stdin, reads the guarded `%begin`/`%end`/`%error` block each one is
//! answered with — and, the reason to pay for a subprocess at all, receives
//! tmux's asynchronous `%` notifications as they happen. Everything under
//! [`control_mode`] is the port of the Go specification named above, and
//! every file there names the Go file it ports on its own `Spec:` line.
//!
//! **Client invocations** are the other. A [`Server`] runs one
//! `tmux <command>` to completion per call, owns nothing between calls and
//! has nothing to close — the transport every shell script already speaks.
//! The surface is this port's own synthesis — the Go package spells no such
//! thing — so neither [`server`] nor [`commands`] carries a `Spec:` line,
//! and each says why in its own first paragraph.
//!
//! ## Which one a caller wants
//!
//! A [`Server`], normally. Asking tmux something and reading the answer —
//! split a pane, list the panes, set an option, send keys — is what a client
//! invocation is for, and keeping a subprocess alive to do it buys nothing.
//!
//! A [`control_mode::Client`] when the answer is not the point: when
//! something must *watch* a server and be told that a pane produced output,
//! that a window closed, that a session was renamed — without asking again
//! and again. That is the one thing an invocation cannot do, having exited
//! before the next thing happened.
//!
//! Both may address one server at once, and the e2e test
//! `both_transports_see_one_server` holds them to it: a world built entirely
//! through [`commands`]' builders, watched by a `Client` attached to the
//! same socket.
//!
//! ## What they share
//!
//! [`ids`] and [`error`], and deliberately nothing else. The `%0` an
//! `%output` notification carries is the same `%0` a `list-panes` prints, so
//! [`PaneId`], [`WindowId`] and [`SessionId`] are vocabulary wider than
//! either transport and live where both can speak them; one [`Error`] spans
//! both for the same reason, so a caller holding both matches on one enum
//! rather than on two that would have to be joined anyway.
//!
//! These two are also where the directory stops answering the provenance
//! question by itself. Their contents were **hoisted out of the port**
//! rather than invented, so they keep the `Spec:` line naming the Go file
//! they came from even though they sit at the root — and what the one-shot
//! surface later added to [`Error`] says `Synthesized, with no Go
//! counterpart` at the variant. The rule is therefore: provenance is stated
//! at the smallest scope where it is true — by directory under
//! [`control_mode`], by module for the one-shot surface, by item where the
//! two meet.
//!
//! # Words on a line, and words in an argv
//!
//! The two transports part company where it matters most, and no
//! abstraction papers over it.
//!
//! A control-mode command travels as one *line* down a pipe, so something
//! has to decide where each word ends: that is
//! [`control_mode::CommandLine`]'s bare/single/double-quote ladder, ported
//! from the Go original, and it is why that half speaks `&str` — a protocol
//! line is text by definition.
//!
//! An invocation has no line. Its words are handed to execve as separate
//! arguments, and quoting one would put the quotes *inside* the argument
//! tmux reads. The root layer therefore takes [`OsString`][std::ffi::OsString]
//! words and passes them through byte for byte — a path is not obliged to be
//! UTF-8 — and imports nothing from [`control_mode`], least of all the
//! renderer. The ladder stays quarantined where it is needed.
//!
//! # A client invocation
//!
//! [`Server::run`] carries any tmux command at all, named by this crate or
//! not, and answers with a [`Captured`] — the bytes tmux printed, with text
//! views over them. [`commands`] is the typed layer above it: one builder
//! per command, rendering the argv words that command wants,
//! with [`commands::REGISTRY`] and [`commands::EXCLUDED`] measured against
//! the running tmux's own `list-commands` rather than claimed.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::Server;
//! use tmux::commands::{ListPanes, NewSession};
//!
//! // A server of this caller's own, addressed by its socket; `Server::current()`
//! // instead reads the `$TMUX` of a process tmux itself started.
//! let server = Server::at("/tmp/example.sock", None);
//! server.run(NewSession::new().detached().session_name("work").args()).await?;
//!
//! let panes = server.run(ListPanes::new().all().format("#{pane_id}").args()).await?;
//! for pane in panes.text_lossy().lines() {
//!     println!("{pane}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # A control-mode connection
//!
//! [`control_mode::Client`] owns one persistent `tmux -C` subprocess.
//! Callers send normal tmux commands through it; the client parses the
//! guarded response blocks and exposes asynchronous `%` notifications
//! through a bounded event stream. Command execution is deliberately
//! serialized: only one command is pending at a time, and there is no
//! pipelining. If a pending command's future is dropped before tmux replies,
//! the client is poisoned — a late response can no longer be safely
//! associated with a future command, which is the Go original's
//! context-cancellation rule translated into Rust's drop-based cancellation;
//! see [`Client::exec_raw`][control_mode::Client::exec_raw]'s doc for the
//! exact rule and a reconnect example.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::control_mode::{Arg, Client, Command, Options};
//!
//! let client = Client::new(Options::new().with_session_name("work")).await?;
//! let response = client.exec(Command::from_static("display-message"), [Arg::raw("-p")]).await?;
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
//! Asynchronous `%` notifications decode into
//! [`Notification`][control_mode::Notification] and its typed accessors;
//! pane output inside `%output`/`%extended-output` frames is tmux's own
//! octal-escaped encoding, recovered with
//! [`decode_output_value`][control_mode::decode_output_value] or those
//! helpers.
//!
//! **Divergence**: the Go package gates its real-tmux integration suite
//! behind `RUN_REAL_TMUX_TESTS=1` so it never touches a user's default
//! tmux server by accident. This port's integration suite instead
//! hard-fails when `tmux` is unavailable, matching this workspace's
//! standing test posture — a green run that skipped everything would be
//! worthless as a signal.

pub mod commands;
pub mod control_mode;
pub mod error;
pub mod ids;
pub mod server;

// Only vocabulary shared across the crate's surfaces is re-exported at the
// root; a control-mode type is named through `tmux::control_mode::…`, so the
// root stays a listing of what every surface speaks rather than of one.
// `Server` is the exception that proves the rule: the surface it heads *is*
// the root, so `tmux::Server` and `tmux::server::Server` are one name said
// twice rather than a type lifted out of a module it belongs to. `Captured`
// comes with it under the same exception rather than a second one: it is
// what every `Server::run` hands back, so a caller who names one already
// holds the other. By the same rule `commands` re-exports nothing: a builder
// is one surface's vocabulary, so it is named `tmux::commands::SplitWindow`
// exactly as a control-mode type is named `tmux::control_mode::Client`.
pub use error::Error;
pub use ids::{InvalidId, PaneId, SessionId, WindowId};
pub use server::{Captured, Server};

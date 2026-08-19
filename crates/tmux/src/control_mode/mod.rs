//! The tmux control-mode protocol: one persistent `tmux -C` subprocess,
//! spoken over piped stdio.
//!
//! Everything under this module is the pandaemonium `pkg/tmux` port — each
//! file names the Go file it ports on its own `Spec:` line, and every
//! deliberate divergence is documented where it occurs. Above it the crate
//! root holds two different things: the one-shot surface
//! ([`crate::server`], [`crate::commands`]), which is this port's own
//! synthesis and says so instead of citing a Go file, and the vocabulary a
//! surface other than control mode also needs — which is why
//! [`crate::ids`] and [`crate::error`] sit outside this module while still
//! carrying the `Spec:` line of the Go file each was hoisted from.
//!
//! [`Client`] is the entry point; see the crate doc for how a session is
//! started, how responses are guarded, and how notifications are delivered.

pub(crate) mod client;
pub(crate) mod commandline;
pub(crate) mod flow;
pub(crate) mod notification;
pub(crate) mod options;
pub(crate) mod output;
pub(crate) mod protocol;

pub use client::*;
pub use commandline::*;
pub use flow::*;
pub use notification::*;
pub use options::*;
pub use output::*;
pub use protocol::*;

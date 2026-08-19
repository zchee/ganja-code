//! The tmux control-mode protocol: one persistent `tmux -C` subprocess,
//! spoken over piped stdio.
//!
//! Everything under this module is the pandaemonium `pkg/tmux` port — each
//! file names the Go file it ports on its own `Spec:` line, and every
//! deliberate divergence is documented where it occurs. The crate root's own
//! modules are this port's synthesis rather than anything Go spells: they
//! hold the vocabulary a surface other than control mode also needs, which
//! is why [`crate::ids`] and [`crate::error`] sit outside this module rather
//! than beneath it.
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

//! A teammate that is a real `claude` pane (P25b).
//!
//! Upstream opencode has **no counterpart**. This is the backend the shared
//! on-disk format exists for: a real `claude` process, in a pane of its own,
//! reading and writing the same team directory through `ganja-team` — the whole
//! of D-1's interop claim, and the only backend that can falsify it.
//!
//! # What lands here in P25b, and what already holds
//!
//! The body — §4.1's flags, `$CLAUDE_CONFIG_DIR/teams` as the root, and §5.5.1's
//! "address the lead by name, never `main`" preamble — is P25b's. Two things
//! the trait itself decides are settled now (**D501**).
//!
//! The first is [`Delivery`], and it is not shared with
//! [`crate::teammate::pane`]: a real `claude` pane is
//! [`Delivery::FireAndForget`], because it has no steer and no consumption
//! signal — its `markMessagesAsRead` runs when a message is *read*, not when a
//! turn takes it on. So the lead retires such an entry at write time rather
//! than waiting for an acknowledgement that never comes; without that split a
//! claude peer's message sits pending in the lead's queue strip forever.
//!
//! The second is that a spawn is refused with
//! [`crate::teammate::Unsupported`] naming P25b, in the same sentence
//! [`crate::teammate::pane`] uses.
//!
//! # §5.5.1, recorded here because it is this backend's to carry
//!
//! `"main"` names *the sender's own parent conversation*. A pane-backed
//! teammate is the main conversation of its own session, so it has no parent
//! and a send addressed to `main` fails. The correct address from any teammate
//! back to its lead is the lead's name — a preamble telling a worker to answer
//! `main` is broken for exactly the backend this module spawns.

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;

use crate::teammate::{Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported};

/// The real-`claude` pane backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudePane;

#[async_trait]
impl TeammateBackend for ClaudePane {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Claude
    }

    async fn spawn(&self, _spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        Err(Unsupported::until_p25b(MemberBackend::Claude))
    }

    async fn kill(&self, handle: &Handle) {
        tracing::warn!(
            ?handle,
            "a claude backend was asked to end something it did not start"
        );
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

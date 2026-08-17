//! A teammate with a `ganja` pane of its own (P25b).
//!
//! Upstream opencode has **no counterpart**; the sequence being ported is
//! Claude Code's §4.1, with §10.3's finding that a pane teammate is a resident
//! full `ganja` TUI rather than a headless worker — its own process, its own
//! session id, its own transcript, which is what makes "message a teammate that
//! finished and it resumes with its context" possible.
//!
//! # What lands here in P25b, and what already holds
//!
//! The body — `current_exe()`, the enumerated non-secret environment
//! allowlist, the split-window call and the failure-cleanup closure — is
//! P25b's. The two things that are settled now are the ones the trait itself
//! decides (**D501**): this backend's [`Delivery`] is
//! [`Delivery::Acknowledged`], because a `ganja` pane prunes an inbox entry
//! when it takes the message into a turn and the lead can watch that happen;
//! and until the body lands, a spawn is refused with
//! [`crate::teammate::Unsupported`] naming P25b rather than falling back to an
//! in-process teammate. The refusal is the same sentence
//! [`crate::teammate::claude`] answers with, which is what AC-14's P25a leg
//! asserts: one door must not spawn where the other refuses.

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;

use crate::teammate::{Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported};

/// The `ganja`-pane backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct GanjaPane;

#[async_trait]
impl TeammateBackend for GanjaPane {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Pane
    }

    async fn spawn(&self, _spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        Err(Unsupported::until_p25b(MemberBackend::Pane))
    }

    async fn kill(&self, handle: &Handle) {
        // Nothing this backend made can be here to end: its `spawn` has never
        // returned a handle. Named rather than ignored, because a handle
        // arriving here would mean a registry had crossed two backends.
        tracing::warn!(
            ?handle,
            "a pane backend was asked to end something it did not start"
        );
    }

    fn delivery(&self) -> Delivery {
        Delivery::Acknowledged
    }
}

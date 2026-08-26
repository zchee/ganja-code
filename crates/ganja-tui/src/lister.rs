//! The seam a teamless-or-lead interactive session's live-session listing is
//! injected through (**D529** Axis 5, **D530**'s re-derived gate): the `@`
//! menu's session rows, and nothing else, come from here.
//!
//! No upstream counterpart: opencode has no cross-session addressing, so
//! there is no `[ref]`/`ListAgents` typeahead to port. The specification is
//! v2's own "`@session` mentions offer live sessions beside files" behavior
//! (v2 §"What `@session` does"), read as behavior and reimplemented over
//! ganja's own registry rather than CC's.
//!
//! # Why a trait here rather than a call
//!
//! Health-checking a live session means dialling its socket, which is
//! `ganja-client`'s `reqwest`-over-UDS — the same dependency the CI gate
//! `! cargo tree -p ganja-tui -e normal | grep -q axum` and this crate's own
//! terminal-only mandate keep out of this crate, the D505 [`crate::binder`]
//! precedent exactly. So the lister is a value the binary provides
//! ([`crate::run`]'s `lister` parameter), which `ganja-cli` implements over
//! the registry read plus a `ganja-client` health probe, sharing its
//! vocabulary with `ganja sessions --live` so the two listings cannot drift.
//! A build that hands no lister in offers files and roster only — the same
//! graceful absence the binder has (**AC-27**).
//!
//! # Which sessions get one
//!
//! Decided in [`crate::run`]: every **interactive non-member** assembly —
//! team or none (**D530**'s re-derived gate, superseding D529's lead-only
//! one) — is handed a lister, because a teamless session needs the live
//! listing for its own `@` menu and its own `send_message` resolution just as
//! a lead does. A pane member and a headless `ganja run` get [`None`]: a
//! member's `uds:` sends already answer `NoTransport`, so offering
//! live-session rows there would advertise a door that is walled, and
//! headless has no composer to menu at all.
//!
//! # How the menu calls it
//!
//! Through the same spawned-async pattern the file walk already uses
//! (`app.rs`'s `spawn_file_walk`): invoked on menu open, never blocking a
//! keystroke, snapshot-cached while the menu stays up so a slow health probe
//! degrades to a stale-but-shown row rather than a frozen composer.

use std::path::PathBuf;

use futures::future::BoxFuture;
use ganja_tool::registry::NameSource;

/// What answering a live session's health came back as — the vocabulary
/// `ganja sessions --live` already reads its own probe through
/// (`ganja-cli/src/main.rs`'s `HELD`/`UNREADABLE` marks), named here rather
/// than shared as a type: this crate cannot link `ganja-client`, so the
/// binary's implementation translates its own probe into these three states
/// rather than this crate importing the binary's vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// The far side answered health, and named itself.
    Answered,
    /// The name's lock is held — a live session — but nothing answered:
    /// held is live, whatever the silence.
    Held,
    /// Something answered, but not with a session identity this build could
    /// trust as one.
    Unreadable,
}

/// One live session, as the lister found it: enough for the `@` menu to show
/// a row and for a completed mention to name the session precisely.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveSession {
    /// The name as that session's registration typed it — comparison folds
    /// through [`ganja_tool::registry::same_name`], this does not.
    pub name: String,
    /// Where the name came from — the honest self-chosen/unverified label
    /// the menu and the reminder both carry (v2 §"What `@session` does").
    pub name_source: NameSource,
    /// The full bare UUIDv7 the session runs under.
    pub session_id: String,
    /// Its socket stem — the disambiguator (**D528**).
    pub stem: String,
    /// The socket it bound, ready to be spelled as a `uds:` address.
    pub socket: PathBuf,
    /// The directory it was launched in, for the menu's disambiguation.
    pub cwd: PathBuf,
    /// What the lister's own health probe answered.
    pub health: Health,
}

/// What one call to [`Lister::list`] came back with: every row it could
/// account for, whole or not.
#[derive(Clone, Debug, PartialEq)]
pub enum Listing {
    /// Every live session the lister could read answered for.
    Complete(Vec<LiveSession>),
    /// Some rows are missing — a directory read failed partway, a probe
    /// timed out — and the caller is told rather than handed a quietly
    /// short list. The menu marks itself incomplete and still completes
    /// (**AC-28**): the engine's own resolution at send time is the
    /// authority, not this snapshot.
    Partial {
        /// Whatever the lister did manage to read.
        rows: Vec<LiveSession>,
        /// What went wrong with the rest, for the menu's incomplete marker.
        error: String,
    },
}

/// The live-session listing the `@` menu offers beside files and roster.
///
/// Implemented outside this crate, by whoever links the server and the
/// client — the [`crate::binder::Binder`] shape exactly, and the same reason:
/// this crate may not depend on what answering a session's health needs.
pub trait Lister: Send + Sync {
    /// Every live session this build can currently account for.
    fn list(&self) -> BoxFuture<'static, Listing>;
}

/// A lister that answers nothing real, for tests: it hands back whatever
/// [`Recording::set`] last stored, recording every call it was asked to
/// serve — the `binder.rs::fake` shape, for the shape's own reason: a test
/// exercising the menu or the registration lifecycle should not reach a real
/// registry or a real socket to do it.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use futures::FutureExt as _;

    use super::{Lister, Listing};

    #[derive(Default)]
    pub(crate) struct Recording {
        /// What the next (and every later) call answers with.
        listing: Mutex<Listing>,
        /// How many times `list` was called.
        pub(crate) calls: AtomicUsize,
    }

    impl Default for Listing {
        fn default() -> Self {
            Listing::Complete(Vec::new())
        }
    }

    impl Recording {
        /// Sets what the next call to [`Lister::list`] answers with.
        pub(crate) fn set(&self, listing: Listing) {
            *self.listing.lock().expect("not poisoned") = listing;
        }
    }

    impl Lister for Arc<Recording> {
        fn list(&self) -> futures::future::BoxFuture<'static, Listing> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let listing = self.listing.lock().expect("not poisoned").clone();

            async move { listing }.boxed()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ganja_tool::registry::NameSource;

    use super::{Health, Listing, LiveSession, fake::Recording};
    use crate::lister::Lister as _;

    fn session(name: &str, stem: &str) -> LiveSession {
        LiveSession {
            name: name.to_owned(),
            name_source: NameSource::User,
            session_id: format!("{stem}-0000-7000-8000-000000000001"),
            stem: stem.to_owned(),
            socket: format!("/tmp/ganja-0/{stem}.sock").into(),
            cwd: format!("/work/{stem}").into(),
            health: Health::Answered,
        }
    }

    /// A fake lister answers exactly what it was told, and counts its calls
    /// so a menu-open test can assert it was actually reached.
    #[tokio::test]
    async fn a_fake_lister_answers_what_it_was_set_to_and_counts_its_calls() {
        let recording = Arc::new(Recording::default());
        recording.set(Listing::Complete(vec![session("worker", "0198c1a2")]));

        let listing = recording.list().await;
        assert_eq!(
            listing,
            Listing::Complete(vec![session("worker", "0198c1a2")])
        );

        recording.set(Listing::Partial {
            rows: vec![],
            error: "the directory could not be read".to_owned(),
        });
        let listing = recording.list().await;
        assert_eq!(
            listing,
            Listing::Partial {
                rows: vec![],
                error: "the directory could not be read".to_owned(),
            }
        );

        assert_eq!(recording.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}

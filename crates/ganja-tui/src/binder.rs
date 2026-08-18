//! The seam a lead session's socket is bound through: **one socket per
//! session** (**D505**; spec D-12), bound by the frontend that owns the
//! session's lifetime, through a value the binary hands in.
//!
//! No upstream counterpart: opencode's TUI attaches to a server it never
//! binds, and that server serves TCP alone. The Claude reference (§5.6) names
//! the `uds:` address form and says nothing about which process binds; the
//! spec's D-12 answers "each session, under the user's socket directory".
//!
//! # Why a trait here rather than a call
//!
//! The socket is `ganja-serve`'s HTTP over UDS, and `ganja-serve` brings
//! `axum`; the CI gate `! cargo tree -p ganja-tui -e normal | grep -q axum`
//! keeps this crate a terminal and nothing else. The engine cannot bind
//! either — `ganja-core` is gated off `axum` the same way. So the binder is a
//! value the binary provides ([`crate::run`]'s `binder`), which `ganja-cli`
//! implements over `ganja_serve::serve`, and this module speaks only in words
//! this crate already has: the engine, a session id, a path, and the
//! read-only context the server's informational routes answer from
//! ([`Served`]). Nothing here can name a listener, and a build that hands no
//! binder in runs exactly as it always did.
//!
//! # Which sessions bind
//!
//! Decided in [`crate::run`], where the gate is: a lead binds, and a pane
//! member, a build with no config home and a headless `ganja run` hand the
//! binder back unused.
//!
//! # Which moments rebind
//!
//! The engine's session slot has four change moments (`engine.rs`'s
//! `session` field): minted at construction, adopted by the first prompt's
//! lazy create, replaced by a resume, re-minted by `NewSession`. The socket is
//! named by the id, so it follows the id and only the id: bound once at
//! startup, **after** the startup resume; rebound whenever a resume or a
//! `NewSession` moves the slot — the picker's, or `/new`'s. Never a peer's
//! doing: the socket serves three routes and no session route (D505's ruling
//! — `GET /global/health`, `GET /team`, `POST /team/{name}/message`; `POST
//! /session` and every `/session/{id}/…` route are TCP's alone), so nothing
//! that reaches a session through its socket can move its slot.
//! [`SessionSocket`] compares the engine's id against the bound one after
//! every event the app handles — one lock and one id compare per event
//! (`Engine::session_id` locks the slot and clones the id) — which makes the
//! socket follow the slot however it moves, so a door added later cannot
//! leave a stale socket bound behind a session it no longer names. **Not**
//! on first-prompt adoption, which gives the row the id the engine already
//! had and moves nothing. Torn down at the tail of the app's run, on the
//! same exit path as the MCP servers and the jobs.
//!
//! The old socket is shut down **before** the new one is bound, and the two
//! are sequential on purpose: two sessions minted inside one 65-second
//! UUIDv7 bucket share their first eight hex digits, so a `/new` right after
//! startup would otherwise walk to a nine-digit name while the old binder
//! still held the eight-digit lock. The gap is a few milliseconds of an
//! unserved session, and nothing anybody could reach in it.
//!
//! # Best-effort
//!
//! A bind that fails costs the session its socket and a status-bar sentence,
//! never the session — a locked-down `/tmp` must not brick the app. It is not
//! retried for the same id, which would be a warning per tick, and is tried
//! again the next time the slot moves.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use futures::future::BoxFuture;
use ganja_core::{Engine, SessionId, Storage, config::Config};

/// What the socket serves beside the engine: the read-only context the
/// server's informational routes answer from, assembled once by the startup
/// path and cloned per bind. Every field is one `ganja serve` fills from its
/// own assembly; here the frontend already built each of them.
#[derive(Clone)]
pub struct Served {
    /// The directory this session was launched in — the only one served.
    pub directory: PathBuf,
    /// The project root that directory resolves into.
    pub root: PathBuf,
    /// The project's data directory, when one resolved.
    pub data: Option<PathBuf>,
    /// The store the engine writes, for the session routes.
    pub storage: Option<Storage>,
    /// The configuration the engine was assembled from.
    pub config: Option<Config>,
}

/// Binds a session's socket over the engine that owns it. Implemented
/// outside this crate, by whoever links the server.
pub trait Binder: Send + Sync {
    /// Binds `id`'s socket, serving `engine` and `served`, and answers what
    /// was bound — or why nothing was, as a sentence for the status bar.
    fn bind(
        &self,
        engine: Arc<Engine>,
        id: SessionId,
        served: Served,
    ) -> BoxFuture<'static, Result<Box<dyn Bound>>>;
}

/// A bound socket: where it is, and the way to stop it.
pub trait Bound: Send {
    /// The path the socket answers at.
    fn path(&self) -> &Path;

    /// Stops serving and unlinks the socket file, returning once the server
    /// task is over — or with what it ended badly with, which the caller
    /// logs and nothing more: the socket is gone either way.
    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>>;
}

/// The socket a lead session keeps bound under its current id, and the state
/// that keeps it current: the binder, what it serves, what is bound now, and
/// the one id a bind was refused for.
pub struct SessionSocket {
    binder: Box<dyn Binder>,
    served: Served,
    bound: Option<(SessionId, Box<dyn Bound>)>,
    refused: Option<SessionId>,
}

/// What a [`SessionSocket::sync`] pass changed, for the status bar and the
/// tests: nothing, a bind, or a refusal with its sentence.
#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// The socket was already bound under the engine's current id.
    Unchanged,
    /// A socket was bound (or rebound) at this path.
    Bound(PathBuf),
    /// The bind for the engine's current id was refused, and will not be
    /// retried until the id moves.
    Refused(String),
}

impl SessionSocket {
    /// A socket that binds through `binder` and serves `served`, bound to
    /// nothing until the first [`SessionSocket::sync`].
    #[must_use]
    pub fn new(binder: Box<dyn Binder>, served: Served) -> Self {
        Self {
            binder,
            served,
            bound: None,
            refused: None,
        }
    }

    /// The path bound right now, when one is.
    #[cfg(test)]
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.bound.as_ref().map(|(_, bound)| bound.path())
    }

    /// Makes the bound socket the one for `engine`'s current session: binds
    /// when nothing is bound, rebinds when the slot moved, and leaves a bound
    /// socket alone otherwise. A refusal is remembered per id and not
    /// retried until the id moves.
    pub async fn sync(&mut self, engine: &Arc<Engine>) -> Synced {
        let wanted = engine.session_id();
        if self.bound.as_ref().is_some_and(|(id, _)| *id == wanted) {
            return Synced::Unchanged;
        }
        if self.refused.as_ref() == Some(&wanted) {
            return Synced::Unchanged;
        }

        // The old one first, and awaited: see the module doc's paragraph on
        // sequencing — the two names may share a stem.
        self.shutdown().await;

        match self
            .binder
            .bind(Arc::clone(engine), wanted.clone(), self.served.clone())
            .await
        {
            Ok(bound) => {
                let path = bound.path().to_path_buf();
                tracing::info!(session = wanted.as_str(), path = %path.display(), "session socket bound");
                self.refused = None;
                self.bound = Some((wanted, bound));
                Synced::Bound(path)
            }
            Err(error) => {
                let sentence = format!("no session socket: {error:#}");
                tracing::warn!(session = wanted.as_str(), %error, "the session socket was not bound");
                self.refused = Some(wanted);
                Synced::Refused(sentence)
            }
        }
    }

    /// Stops the bound socket, when one is: the exit path, and the first half
    /// of a rebind.
    pub async fn shutdown(&mut self) {
        let Some((id, bound)) = self.bound.take() else {
            return;
        };
        let path = bound.path().to_path_buf();
        match bound.shutdown().await {
            Ok(()) => {
                tracing::info!(session = id.as_str(), path = %path.display(), "session socket closed");
            }
            Err(error) => {
                tracing::warn!(session = id.as_str(), path = %path.display(), %error, "the session socket did not stop cleanly");
            }
        }
    }
}

/// A binder that binds nothing real, for the tests here and in `app.rs`:
/// it records every id it was asked for and every path it shut down, and
/// refuses while told to.
#[cfg(test)]
pub(crate) mod fake {
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use futures::{FutureExt as _, future::BoxFuture};
    use ganja_core::{Engine, SessionId};

    use super::{Binder, Bound, Served};

    #[derive(Default)]
    pub(crate) struct Recording {
        /// Every id bound, in order.
        pub(crate) bound: Mutex<Vec<SessionId>>,
        /// Every path shut down, in order.
        pub(crate) closed: Arc<Mutex<Vec<PathBuf>>>,
        /// Refuse the next binds while set.
        pub(crate) refuse: AtomicBool,
        /// How many binds were asked for, refusals included.
        pub(crate) binds: AtomicUsize,
    }

    impl Recording {
        /// The path a fake bind answers with for `id`.
        pub(crate) fn path_for(id: &SessionId) -> PathBuf {
            PathBuf::from(format!("/nowhere/{}.sock", id.as_str()))
        }
    }

    struct Fake {
        path: PathBuf,
        closed: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl Bound for Fake {
        fn path(&self) -> &Path {
            &self.path
        }

        fn shutdown(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
            self.closed
                .lock()
                .expect("not poisoned")
                .push(self.path.clone());
            async { Ok(()) }.boxed()
        }
    }

    impl Binder for Arc<Recording> {
        fn bind(
            &self,
            _engine: Arc<Engine>,
            id: SessionId,
            _served: Served,
        ) -> BoxFuture<'static, anyhow::Result<Box<dyn Bound>>> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            if self.refuse.load(Ordering::SeqCst) {
                return async { Err(anyhow::anyhow!("the directory is not ours")) }.boxed();
            }
            self.bound.lock().expect("not poisoned").push(id.clone());
            let fake: Box<dyn Bound> = Box::new(Fake {
                path: Recording::path_for(&id),
                closed: Arc::clone(&self.closed),
            });
            async move { Ok(fake) }.boxed()
        }
    }

    /// A served context that names nothing real.
    pub(crate) fn served() -> Served {
        Served {
            directory: PathBuf::from("/nowhere"),
            root: PathBuf::from("/nowhere"),
            data: None,
            storage: None,
            config: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use ganja_core::{Engine, provider::FakeProvider};
    use ganja_protocol::Command;

    use super::{
        SessionSocket, Synced,
        fake::{Recording, served},
    };

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            Arc::new(FakeProvider::default()),
            "fake",
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        ))
    }

    #[tokio::test]
    async fn the_socket_follows_the_session_slot_and_is_bound_once_per_id() {
        let engine = engine();
        let recording = Arc::new(Recording::default());
        let mut socket = SessionSocket::new(Box::new(Arc::clone(&recording)), served());

        let first = engine.session_id();
        assert_eq!(
            socket.sync(&engine).await,
            Synced::Bound(Recording::path_for(&first)),
            "the first pass binds under the engine's id"
        );
        assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
        assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
        assert_eq!(
            recording.binds.load(Ordering::SeqCst),
            1,
            "a pass over an unmoved slot binds nothing"
        );

        engine
            .send(Command::NewSession)
            .await
            .expect("a fresh session");
        let second = engine.session_id();
        assert_ne!(first, second, "NewSession re-mints the id");
        assert_eq!(
            socket.sync(&engine).await,
            Synced::Bound(Recording::path_for(&second)),
            "the slot moved, so the socket moved"
        );
        assert_eq!(
            recording.closed.lock().expect("not poisoned").as_slice(),
            &[Recording::path_for(&first)],
            "the old socket was shut down before the new one was bound"
        );
        assert_eq!(
            recording.bound.lock().expect("not poisoned").as_slice(),
            &[first.clone(), second.clone()]
        );

        socket.shutdown().await;
        assert_eq!(socket.path(), None);
        assert_eq!(
            recording.closed.lock().expect("not poisoned").len(),
            2,
            "the exit path shuts the bound socket down"
        );
        socket.shutdown().await;
        assert_eq!(
            recording.closed.lock().expect("not poisoned").len(),
            2,
            "a second shutdown has nothing to shut down"
        );
    }

    #[tokio::test]
    async fn a_refused_bind_is_a_sentence_not_retried_until_the_slot_moves() {
        let engine = engine();
        let recording = Arc::new(Recording::default());
        recording.refuse.store(true, Ordering::SeqCst);
        let mut socket = SessionSocket::new(Box::new(Arc::clone(&recording)), served());

        assert_eq!(
            socket.sync(&engine).await,
            Synced::Refused("no session socket: the directory is not ours".to_owned())
        );
        assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
        assert_eq!(
            recording.binds.load(Ordering::SeqCst),
            1,
            "the same id is not asked for again"
        );
        assert_eq!(socket.path(), None, "nothing is bound");

        recording.refuse.store(false, Ordering::SeqCst);
        assert_eq!(
            socket.sync(&engine).await,
            Synced::Unchanged,
            "and still not, while the slot stands"
        );
        engine
            .send(Command::NewSession)
            .await
            .expect("a fresh session");
        assert!(
            matches!(socket.sync(&engine).await, Synced::Bound(_)),
            "a moved slot is a new question"
        );
        socket.shutdown().await;
    }
}

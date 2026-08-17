//! The session-socket binder the UI is handed: `ganja_tui::binder::Binder`
//! implemented over `ganja_serve` (**D505**, one socket per lead session).
//!
//! No upstream counterpart — opencode's TUI attaches to a server and binds
//! nothing. This module exists because of two CI gates that between them
//! leave the binary as the only place the socket can be bound: the engine may
//! not reach `axum` (`! cargo tree -p ganja-core … | grep -q axum`), and
//! neither may the terminal frontend (the frontends-in-their-lanes gate). So
//! `ganja-tui` decides *when* — a lead session's startup, every move of its
//! session slot, its exit — through a trait spoken in the engine's own words,
//! and this file, which already links both crates, is *how*.
//!
//! What is served is [`ganja_serve::Listen::session`]: the socket named by
//! the session's id in this user's own `/tmp/ganja-<uid>/` directory, no
//! password (the directory and the peer-uid check say who may connect), the
//! same read-only context `ganja serve` fills from its own assembly, and the
//! default heartbeat. A `ganja serve` running beside a UI never fights it
//! over a name: `serve` binds TCP, and were a second binder ever pointed at
//! the same session's socket, the name's lock makes it walk to the next
//! candidate rather than steal the file.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use futures::{FutureExt as _, future::BoxFuture};
use ganja_core::{Engine, SessionId};
use ganja_tui::binder::{Binder, Bound, Served};

/// Binds a lead session's socket through `ganja_serve`.
pub struct SocketBinder {
    /// The directory the socket goes in: this user's own, or — through the
    /// hidden `--socket-dir` — a private one a test owns, so the suite never
    /// binds into, or lists, the developer's real `/tmp/ganja-<uid>/`.
    directory: Option<PathBuf>,
}

impl SocketBinder {
    /// A binder into `directory`, or into this user's own socket directory
    /// when [`None`].
    #[must_use]
    pub fn new(directory: Option<PathBuf>) -> Self {
        Self { directory }
    }
}

impl Binder for SocketBinder {
    fn bind(
        &self,
        engine: Arc<Engine>,
        id: SessionId,
        served: Served,
    ) -> BoxFuture<'static, Result<Box<dyn Bound>>> {
        let listen = match &self.directory {
            Some(directory) => ganja_serve::Listen::Session {
                id,
                directory: directory.clone(),
            },
            None => ganja_serve::Listen::session(id),
        };
        let config = ganja_serve::ServeConfig {
            listen,
            credentials: None,
            directory: served.directory,
            root: served.root,
            data: served.data,
            storage: served.storage,
            config: served.config,
            heartbeat: ganja_serve::HEARTBEAT,
        };

        async move {
            let handle = ganja_serve::serve(engine, config)
                .await
                .context("the session socket could not be bound")?;
            let path = handle
                .address()
                .path()
                .map(Path::to_path_buf)
                // A session listen binds a socket and nothing else; the
                // accessor's TCP arm is unreachable from here.
                .context("a session socket was bound at no path")?;

            Ok(Box::new(Socket { handle, path }) as Box<dyn Bound>)
        }
        .boxed()
    }
}

/// A bound session socket: the serve handle and the path it answered with.
struct Socket {
    handle: ganja_serve::Handle,
    path: PathBuf,
}

impl Bound for Socket {
    fn path(&self) -> &Path {
        &self.path
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        // `Handle::shutdown` unlinks the file before it signals the task, so
        // by the time this returns the name is free for the next binder. A
        // failure is a server task that ended badly — the caller's to log,
        // since this binary writes no log of its own.
        async move {
            self.handle
                .shutdown()
                .await
                .context("the session socket did not stop cleanly")
        }
        .boxed()
    }
}

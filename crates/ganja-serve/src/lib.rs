//! The engine over a socket: the HTTP routes and the SSE event stream a
//! remote client drives a session through.
//!
//! Spec: upstream packages/opencode/src/server/server.ts
//!
//! Its own crate rather than a module in `ganja-core` for the same reason the
//! engine carries no terminal dependency: a build that only wants the
//! terminal must never pull an HTTP server, and CI asserts it the same
//! inverted way (`! cargo tree -p ganja-core -e normal | grep -q axum`).
//!
//! Four postures are load-bearing here and pinned by `tests/`:
//!
//! * **A non-loopback bind with no password is refused at startup.** Upstream
//!   warns and serves anyway (`cli/cmd/serve.ts:15-17`); this build treats an
//!   open engine on a network interface as a mistake nobody meant to make
//!   (deviation: non-loopback-requires-a-password).
//! * **The launch directory is the only directory served.** Upstream loads an
//!   app instance per `x-opencode-directory` header
//!   (`server/routes/instance/httpapi/middleware/workspace-routing.ts:87`);
//!   this engine is built in one directory, so a request naming another is
//!   answered `400` rather than silently served the wrong worktree.
//! * **The serve layer never logs a request's query string** — paths and
//!   methods only — because `?auth_token=` puts a credential in the URL, and
//!   a log line is the one place it must never land.
//! * **Question events are observable but not answerable from this side.** A
//!   served session's `QuestionAsked`, `QuestionReplied` and
//!   `QuestionRejected` reach subscribers on `GET /event` like every other
//!   protocol event, but there is no `/question` or
//!   `/question/{id}/reply` route yet. Mirroring the existing `GET
//!   /permission` plus `POST /permission/{id}/reply` pair is follow-up work.
//! * **A Unix socket serves the same routes to the same user and nobody
//!   else** (**D505**, no upstream counterpart — opencode serves TCP only).
//!   [`Listen`] names the transport, [`Address`] reports the one bound, and
//!   [`socket`] holds the scheme: a private `/tmp/ganja-<uid>/` directory the
//!   bind refuses unless it is ours at `0700`, one `0600` socket per session
//!   named by its id, a stale file reused and a live one never stolen, and a
//!   peer-uid check on every accepted connection. The password posture is
//!   untouched — a socket takes no password because the filesystem already
//!   said who may connect — and the guard that reads the transport is
//!   `routes.rs`'s.

mod auth;
mod error;
mod routes;
pub mod socket;
mod sse;
mod state;

use std::{
    fmt,
    future::IntoFuture as _,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use auth::{Credentials, PASSWORD_ENV};
use futures::StreamExt as _;
use ganja_core::{Config, Engine, EngineError, Storage};
use ganja_protocol::{Event, SessionId};
pub use routes::DIRECTORY_HEADER;
pub use socket::DirectoryRefusal;
use tokio::net::TcpListener;

/// The hostname a server binds when nobody chose one: loopback, so an
/// unsecured default cannot face a network.
pub const DEFAULT_HOSTNAME: &str = "127.0.0.1";

/// The port tried first when none was asked for, upstream's legacy listener
/// port (`server/server.ts:117-122`); when it is taken, the OS assigns one.
pub const DEFAULT_PORT: u16 = 4096;

/// How often the event stream proves it is alive when nothing is happening,
/// upstream's tick (`server/routes/instance/httpapi/handlers/event.ts:63`).
pub const HEARTBEAT: Duration = Duration::from_secs(10);

/// Where a server listens: the ask, which [`Address`] answers with the truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listen {
    /// A TCP listener, upstream's only shape.
    Tcp {
        /// Hostname to bind: an IP address, or `"localhost"`.
        hostname: String,
        /// Port to bind, taken exactly or refused. [`None`] tries
        /// [`DEFAULT_PORT`] first and falls back to an OS-assigned one.
        port: Option<u16>,
    },
    /// A Unix domain socket at exactly this path (**D505**). Its directory is
    /// created at `0700` when absent and refused when it is not ours at that
    /// mode; a stale socket file there is unlinked, a live one is refused as
    /// [`ServeError::SocketInUse`]; the bound socket is left at `0600` and
    /// answers only peers of this process's uid.
    Unix {
        /// The socket path.
        path: PathBuf,
    },
    /// The socket a session owns, under `directory`, at the first of the
    /// names [`socket::candidates`] gives it that no live peer holds — the
    /// per-session door the plan describes, with the collision rule kept in
    /// one place. The bound path is what [`Handle::address`] reports; the
    /// same hygiene as [`Listen::Unix`] applies. [`Listen::session`] fills
    /// the directory with [`socket::directory`].
    Session {
        /// The session the socket is for.
        id: SessionId,
        /// The directory the socket lives in.
        directory: PathBuf,
    },
}

impl Listen {
    /// The default: loopback TCP, no fixed port.
    #[must_use]
    pub fn loopback() -> Self {
        Self::Tcp {
            hostname: DEFAULT_HOSTNAME.to_owned(),
            port: None,
        }
    }

    /// The socket `id` owns in this user's own socket directory.
    #[cfg(unix)]
    #[must_use]
    pub fn session(id: SessionId) -> Self {
        Self::Session {
            id,
            directory: socket::directory(),
        }
    }
}

/// Where a server is bound: the truth, as [`Handle::address`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// A TCP address, port included — the one the OS assigned, when it did.
    Tcp(SocketAddr),
    /// A Unix domain socket's path — the one the session's name landed on,
    /// when it had to extend past a collision.
    Unix(PathBuf),
}

impl Address {
    /// The TCP address, when this is one.
    #[must_use]
    pub fn tcp(&self) -> Option<SocketAddr> {
        match self {
            Self::Tcp(address) => Some(*address),
            Self::Unix(_) => None,
        }
    }

    /// The socket path, when this is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Tcp(_) => None,
            Self::Unix(path) => Some(path),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => address.fmt(f),
            Self::Unix(path) => path.display().fmt(f),
        }
    }
}

/// Everything a server needs beyond the engine itself: where to bind, who may
/// connect, and the read-only context the informational routes answer from.
pub struct ServeConfig {
    /// Where to listen: TCP by hostname and port, or a Unix socket.
    pub listen: Listen,
    /// The credential every route requires when present. Required — not
    /// optional — for a TCP bind that is not loopback; a socket needs none,
    /// its directory and the peer-uid check having already said who may
    /// connect.
    pub credentials: Option<Credentials>,
    /// The directory this server serves, and the only one: a request naming
    /// another via [`DIRECTORY_HEADER`] or `?directory=` is answered `400`.
    pub directory: PathBuf,
    /// The project root the directory resolves into, for `GET /path`.
    pub root: PathBuf,
    /// The project's data directory, when one resolved, for `GET /path`.
    pub data: Option<PathBuf>,
    /// The store the read-only session routes answer from — the same store
    /// the engine writes, cloned by whoever opened it. [`None`] serves an
    /// ephemeral engine: nothing stored, so `GET /session` lists nothing and
    /// `GET /session/{id}` finds nothing.
    pub storage: Option<Storage>,
    /// The configuration the engine was assembled from, for the `GET /config`
    /// projection. [`None`] serves an empty one.
    pub config: Option<Config>,
    /// How often the event stream heartbeats. [`HEARTBEAT`] everywhere but a
    /// test that cannot wait ten seconds.
    pub heartbeat: Duration,
}

impl ServeConfig {
    /// A loopback server for `directory` with every default: no fixed port,
    /// no password, nothing stored to serve.
    #[must_use]
    pub fn in_directory(directory: PathBuf) -> Self {
        Self {
            listen: Listen::loopback(),
            credentials: None,
            root: directory.clone(),
            directory,
            data: None,
            storage: None,
            config: None,
            heartbeat: HEARTBEAT,
        }
    }
}

/// Why a server did not come up.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// A bind that faces the network was asked for with nothing guarding it.
    /// Upstream warns and serves anyway; refusing is deliberate — see the
    /// crate docs (deviation: non-loopback-requires-a-password).
    #[error(
        "refusing to bind {hostname} without a password: every route would be \
         open to the network; set {PASSWORD_ENV}, or bind loopback"
    )]
    UnsecuredNonLoopback {
        /// The hostname that was asked for.
        hostname: String,
    },
    /// The hostname is neither an IP address nor `localhost`. Resolving names
    /// is a resolver's job, and guessing what a name binds to is not.
    #[error("cannot bind {hostname}: use an IP address, or \"localhost\"")]
    UnknownHostname {
        /// The hostname nothing answers to.
        hostname: String,
    },
    /// The socket could not be bound — an explicitly chosen port that is
    /// taken, most of the time; for a Unix socket, a path the OS refused, or
    /// one occupied by something that is not a socket.
    #[error("failed to bind {address}")]
    Bind {
        /// The address that was refused.
        address: Address,
        /// What the OS said.
        #[source]
        source: io::Error,
    },
    /// A live server already answers at the socket path, and a live socket
    /// is never stolen. [`Listen::Session`] walks past this to the next
    /// name; [`Listen::Unix`] surfaces it.
    #[error("a live server already answers at {}", path.display())]
    SocketInUse {
        /// The socket somebody else is holding.
        path: PathBuf,
    },
    /// The socket directory is not a private directory of ours: it is not a
    /// directory, somebody else owns it, or its mode is not `0700` (AC-22 as
    /// Resolution 5 replaced it). Refused by name rather than used, because
    /// `/tmp` is world-writable and whatever is there first was put there by
    /// somebody.
    #[error("refusing the socket directory {}: {reason}", path.display())]
    UnsafeSocketDirectory {
        /// The directory that was refused.
        path: PathBuf,
        /// What was wrong with it.
        reason: DirectoryRefusal,
    },
    /// The engine refused the subscription the permission tracker lives on.
    /// Documented as unreachable today; carried rather than unwrapped so the
    /// day it becomes reachable is a compile-time fact here.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// A running server: the address it answers at, and the way to stop it.
pub struct Handle {
    address: Address,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl fmt::Debug for Handle {
    /// The address alone: the signal and the task say nothing a reader of a
    /// failed assertion could use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// The address the server is actually bound to — the truth, not the ask.
    /// The two differ exactly when the port was OS-assigned, and, for a
    /// session's socket, when its name had to extend past one a live peer
    /// held.
    #[must_use]
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Stops accepting, ends every open event stream, drains what is left,
    /// and returns when the server task is over.
    ///
    /// The event streams are the reason this is a broadcast rather than a
    /// plain stop signal: an SSE response is otherwise endless, and a
    /// graceful shutdown that waited for one would wait forever. Upstream
    /// force-closes its open connections for the same reason
    /// (`server/server.ts:195-211`); here each stream watches the signal and
    /// ends itself, so the connection drains instead of being torn.
    ///
    /// # Errors
    ///
    /// What the accept loop failed with, when it failed rather than finished.
    pub async fn shutdown(self) -> io::Result<()> {
        // A socket's name is given back *before* the listener closes, while it
        // still answers: a peer binding into the same name meanwhile then
        // finds nothing rather than a dead file — which it would unlink and
        // replace, and which this side would then unlink again, taking the
        // peer's live socket with it. Unlinking a bound socket only stops
        // new connections; the ones open drain below like any other.
        if let Address::Unix(path) = &self.address
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %error, "could not remove the socket file");
        }

        // A channel whose receivers are all gone means the task already
        // ended, which is exactly what the join below reports.
        let _ = self.shutdown.send(true);

        match self.task.await {
            Ok(result) => result,
            Err(join) => Err(io::Error::other(join)),
        }
    }
}

/// Binds and serves `engine` under `config`, returning once the socket is
/// listening.
///
/// # Errors
///
/// [`ServeError::UnsecuredNonLoopback`] before anything is bound, then the
/// hostname and bind refusals above; for a Unix socket, the directory and
/// live-socket refusals.
pub async fn serve(engine: Arc<Engine>, config: ServeConfig) -> Result<Handle, ServeError> {
    let bound = match &config.listen {
        Listen::Tcp { hostname, port } => {
            let ip = resolve_hostname(hostname)?;
            if !ip.is_loopback() && config.credentials.is_none() {
                return Err(ServeError::UnsecuredNonLoopback {
                    hostname: hostname.clone(),
                });
            }

            let listener = bind(ip, *port).await?;
            let address = listener.local_addr().map_err(|source| ServeError::Bind {
                address: Address::Tcp(SocketAddr::new(ip, port.unwrap_or(0))),
                source,
            })?;
            Bound::Tcp(listener, address)
        }
        Listen::Unix { path } => bind_unix(path).await?,
        Listen::Session { id, directory } => bind_session(directory, id).await?,
    };

    // The tracker's subscription is claimed before the router exists, so no
    // request can race a dialog past it: every `PermissionRequested` the
    // engine ever emits from here on is in this queue. It is lossless — the
    // tracker only moves a map entry, so it always drains and the turn task
    // never waits on it.
    let subscription = engine.subscribe().await?;
    let pending = state::Pending::default();
    spawn_permission_tracker(subscription, pending.clone());

    let (shutdown, watch) = tokio::sync::watch::channel(false);
    let state = state::AppState::new(engine, config, pending, watch.clone());
    let app = routes::router(state);

    let (address, task) = match bound {
        Bound::Tcp(listener, address) => {
            (Address::Tcp(address), spawn_server(listener, app, watch))
        }
        #[cfg(unix)]
        Bound::Unix(listener, path) => (Address::Unix(path), spawn_server(listener, app, watch)),
    };

    Ok(Handle {
        address,
        shutdown,
        task,
    })
}

/// A listener the OS handed back, held beside the truth about where.
enum Bound {
    Tcp(TcpListener, SocketAddr),
    #[cfg(unix)]
    Unix(socket::PeerChecked, PathBuf),
}

#[cfg(unix)]
async fn bind_unix(path: &Path) -> Result<Bound, ServeError> {
    let listener = socket::bind_path(path).await?;
    Ok(Bound::Unix(listener, path.to_path_buf()))
}

#[cfg(unix)]
async fn bind_session(directory: &Path, id: &SessionId) -> Result<Bound, ServeError> {
    let (listener, path) = socket::bind_session(directory, id).await?;
    Ok(Bound::Unix(listener, path))
}

/// A Unix socket needs a Unix host; elsewhere the ask is refused as the bind
/// it could not be, with the OS's own word for it.
#[cfg(not(unix))]
async fn bind_unix(path: &Path) -> Result<Bound, ServeError> {
    Err(ServeError::Bind {
        address: Address::Unix(path.to_path_buf()),
        source: io::Error::new(io::ErrorKind::Unsupported, "unix sockets need a unix host"),
    })
}

#[cfg(not(unix))]
async fn bind_session(directory: &Path, id: &SessionId) -> Result<Bound, ServeError> {
    let path = socket::candidates(directory, id)
        .next()
        .unwrap_or_else(|| directory.to_path_buf());
    bind_unix(&path).await
}

/// Serves `app` on `listener` until `watch` flips, on a task of its own. One
/// function for both transports, because axum's `serve` is generic over its
/// [`axum::serve::Listener`] and the graceful-shutdown shape is the same.
fn spawn_server<L>(
    listener: L,
    app: axum::Router,
    watch: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<io::Result<()>>
where
    L: axum::serve::Listener,
    L::Addr: fmt::Debug,
{
    tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut watch = watch;
                // Either the flag flips or the handle is gone; both mean stop.
                let _ = watch.changed().await;
            })
            .into_future(),
    )
}

/// Keeps the map behind `GET /permission` current: a request enters when the
/// engine asks, and leaves when anything answers it — a person over the
/// reply route, or a cancel refusing it.
fn spawn_permission_tracker(
    mut events: futures::stream::BoxStream<'static, Event>,
    pending: state::Pending,
) {
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            match event {
                Event::PermissionRequested {
                    session_id,
                    id,
                    call_id,
                    tool,
                    title,
                    args,
                    directories,
                } => {
                    pending.insert(state::PendingPermission {
                        session_id,
                        id,
                        call_id,
                        tool,
                        title,
                        args,
                        directories,
                    });
                }
                Event::PermissionReplied { id, .. } => pending.remove(&id),
                _ => {}
            }
        }
    });
}

/// The address `hostname` names, without a resolver: an IP address is itself,
/// `localhost` is IPv4 loopback, and anything else is refused rather than
/// guessed at.
fn resolve_hostname(hostname: &str) -> Result<IpAddr, ServeError> {
    if hostname.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    hostname.parse().map_err(|_| ServeError::UnknownHostname {
        hostname: hostname.to_owned(),
    })
}

/// Upstream's port policy (`server/server.ts:117-122`): an explicit port is
/// taken exactly or refused, and no port means [`DEFAULT_PORT`] first with an
/// OS-assigned fallback.
async fn bind(ip: IpAddr, port: Option<u16>) -> Result<TcpListener, ServeError> {
    match port {
        Some(port) => try_bind(ip, port).await,
        None => match try_bind(ip, DEFAULT_PORT).await {
            Ok(listener) => Ok(listener),
            Err(_) => try_bind(ip, 0).await,
        },
    }
}

async fn try_bind(ip: IpAddr, port: u16) -> Result<TcpListener, ServeError> {
    let address = SocketAddr::new(ip, port);

    TcpListener::bind(address)
        .await
        .map_err(|source| ServeError::Bind {
            address: Address::Tcp(address),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::{ServeError, resolve_hostname};

    #[test]
    fn localhost_and_loopback_addresses_resolve_and_names_are_refused() {
        assert!(
            resolve_hostname("127.0.0.1")
                .expect("an IPv4 literal")
                .is_loopback()
        );
        assert!(
            resolve_hostname("::1")
                .expect("an IPv6 literal")
                .is_loopback()
        );
        assert!(
            resolve_hostname("localhost")
                .expect("the one name")
                .is_loopback()
        );
        assert!(
            resolve_hostname("LOCALHOST")
                .expect("case cannot matter")
                .is_loopback()
        );
        assert!(
            !resolve_hostname("0.0.0.0")
                .expect("unspecified")
                .is_loopback()
        );
        assert!(
            !resolve_hostname("192.168.1.10")
                .expect("a lan address")
                .is_loopback()
        );

        let refused = resolve_hostname("example.internal");
        assert!(
            matches!(refused, Err(ServeError::UnknownHostname { ref hostname }) if hostname == "example.internal"),
            "a name this build cannot resolve is refused, not guessed: {refused:?}"
        );
    }
}

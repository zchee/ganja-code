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
//! Three postures are load-bearing here and pinned by `tests/`:
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

mod auth;
mod error;
mod routes;
mod sse;
mod state;

use std::{
    future::IntoFuture as _,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

pub use auth::{Credentials, PASSWORD_ENV};
use futures::StreamExt as _;
use ganja_core::{Config, Engine, EngineError, Storage};
use ganja_protocol::Event;
pub use routes::DIRECTORY_HEADER;
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

/// Everything a server needs beyond the engine itself: where to bind, who may
/// connect, and the read-only context the informational routes answer from.
pub struct ServeConfig {
    /// Hostname to bind: an IP address, or `"localhost"`.
    pub hostname: String,
    /// Port to bind, taken exactly or refused. [`None`] tries
    /// [`DEFAULT_PORT`] first and falls back to an OS-assigned one.
    pub port: Option<u16>,
    /// The credential every route requires when present. Required — not
    /// optional — for a bind that is not loopback.
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
            hostname: DEFAULT_HOSTNAME.to_owned(),
            port: None,
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
    /// taken, most of the time.
    #[error("failed to bind {address}")]
    Bind {
        /// The address that was refused.
        address: SocketAddr,
        /// What the OS said.
        #[source]
        source: io::Error,
    },
    /// The engine refused the subscription the permission tracker lives on.
    /// Documented as unreachable today; carried rather than unwrapped so the
    /// day it becomes reachable is a compile-time fact here.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// A running server: the address it answers at, and the way to stop it.
pub struct Handle {
    address: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl Handle {
    /// The address the server is actually bound to — the truth, not the ask,
    /// which differ exactly when the port was OS-assigned.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
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
/// hostname and bind refusals above.
pub async fn serve(engine: Arc<Engine>, config: ServeConfig) -> Result<Handle, ServeError> {
    let ip = resolve_hostname(&config.hostname)?;
    if !ip.is_loopback() && config.credentials.is_none() {
        return Err(ServeError::UnsecuredNonLoopback {
            hostname: config.hostname,
        });
    }

    let listener = bind(ip, config.port).await?;
    let address = listener.local_addr().map_err(|source| ServeError::Bind {
        address: SocketAddr::new(ip, config.port.unwrap_or(0)),
        source,
    })?;

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

    let task = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut watch = watch;
                // Either the flag flips or the handle is gone; both mean stop.
                let _ = watch.changed().await;
            })
            .into_future(),
    );

    Ok(Handle {
        address,
        shutdown,
        task,
    })
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
        .map_err(|source| ServeError::Bind { address, source })
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

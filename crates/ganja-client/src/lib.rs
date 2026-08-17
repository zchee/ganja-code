//! A typed client for the ganja engine served over HTTP + SSE.
//!
//! Spec: `ganja-serve`'s route surface (`crates/ganja-serve/src/routes.rs`)
//! and its SSE framing (`crates/ganja-serve/src/sse.rs`) — the frame
//! vocabulary is declared in [`sse`] and pinned against a real server's bytes,
//! because a `ganja-client → ganja-serve` dependency would drag the HTTP
//! server into every consumer of the client (see the workspace manifest's
//! entry for this crate).
//!
//! What this crate is for: `ganja run --attach` drives a session on a server
//! somebody else is running, through the same four surfaces a frontend drives
//! in-process — a prompt, the event stream, the pending dialogs, and the reply
//! to one. Its internal dependency list is exactly `ganja-protocol`, and CI
//! asserts it: a client that linked the engine would quietly become a second
//! frontend instead of a consumer of the served one.
//!
//! # Two address forms, one wire (**D505**)
//!
//! A server is reached at a TCP base ([`Client::new`]) or at a session's own
//! Unix socket ([`Client::on_socket`]), and everything above the connection is
//! the same bytes — one wire, spoken by one client. What differs is *which
//! routes answer*: `ganja-serve`'s socket serves exactly three — `GET
//! /global/health`, `GET /team`, `POST /team/{name}/message` — and every
//! other route this client declares is TCP's alone, `404` over a socket.
//! The socket form rides `reqwest`'s own
//! `ClientBuilder::unix_socket`, which routes **every** request of that
//! `reqwest::Client` through the one path, so a [`Client`] is bound to
//! exactly one socket for its whole life and never switches: one client per
//! socket path, which is also why the form takes no credential — a same-uid
//! socket is authorized by the filesystem, and the server's guard asks it for
//! none. Two routes are the socket's reason to exist: [`Client::team`]
//! answers on both forms, and [`Client::send_team_message`] is registered on
//! the socket alone, so calling it on a TCP client is refused by the server
//! with `404` rather than by anything here — this crate declares routes, it
//! does not second-guess which listener answers them.
//!
//! # Version skew is unsupported, and refused readably
//!
//! [`Event`] is internally tagged with no unknown-variant tolerance, so a
//! server one version ahead sends frames this build cannot name. That is not
//! something to paper over: a client that skipped the events it did not
//! recognize would render a transcript that is missing exactly the parts the
//! two builds disagree about. **Every** shape this crate cannot read — an
//! event `type` it has no variant for, a body field nobody declared, an SSE
//! frame outside [`sse::FRAMES`] — becomes one [`ClientError::Skew`] naming
//! the mismatch, and a stream that hits one ends. Run the same version on both
//! ends; there is no compatibility window and this says so.
//!
//! # What is declared here
//!
//! A client declares the request bodies it sends and the response bodies it
//! reads — that is what a client *is*. Two of them are worth naming: the frame
//! vocabulary (pinned; see [`sse`]) and [`PendingPermission`], which is
//! serve's own projection of `Event::PermissionRequested` and is declared
//! whole with `deny_unknown_fields`, so the skew posture above catches a drift
//! rather than a hand-written pin having to. [`SessionRow`] is deliberately
//! *not* whole: the listing is `ganja-core`'s `SessionInfo`, a type this crate
//! has no business duplicating, so it reads the two fields it acts on and
//! tolerates the rest.

pub mod sse;

#[cfg(unix)]
use std::path::Path;

use futures::{Stream, StreamExt as _, stream::BoxStream};
pub use ganja_protocol::{
    Event, Mention, PermissionId, PermissionReply, SessionId, team::TeamView,
};
use serde::{Deserialize, Serialize};

/// The credential a password-protected server requires on every route.
///
/// Held as plain text rather than in a `SecretString`: this crate's dependency
/// list is load-bearing (CI asserts the internal half, and the external half
/// is five crates on purpose), and the credential lives for one process's
/// lifetime. What is *not* left to chance is rendering — [`Debug`] is written
/// by hand here and in [`Client`] so no formatter can put a password in a log
/// line.
#[derive(Clone)]
pub struct Credentials {
    username: String,
    password: String,
}

impl Credentials {
    /// The pair a server started with.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Why a call to the served engine did not produce an answer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The address is not one this client can drive.
    #[error(
        "{address} is not a server address: {reason}; expected something like http://127.0.0.1:4096"
    )]
    Address {
        /// What was given.
        address: String,
        /// Why it cannot be used.
        reason: String,
    },
    /// The server answered more than this client reads. Every route here
    /// answers a bounded document — a roster, a listing, a receipt — and a
    /// body past [`BODY_CAP`] is not one of them, whatever it says it is; it
    /// is refused rather than buffered, because a socket's far end is
    /// another process's word.
    #[error("the ganja server answered {method} {path} with more than {cap} bytes; refused unread")]
    Oversized {
        /// The method that was answered.
        method: &'static str,
        /// The route that was answered.
        path: String,
        /// The most this client reads.
        cap: usize,
    },
    /// The socket path is not one a client can be bound to.
    #[error("{path} is not a socket path this client can be bound to: {reason}")]
    SocketPath {
        /// What was given.
        path: String,
        /// Why it cannot be used.
        reason: String,
    },
    /// Nothing answered, or the connection failed part-way through.
    #[error("failed to reach the ganja server at {address}")]
    Transport {
        /// The server that was being reached.
        address: String,
        /// What the transport said.
        #[source]
        source: reqwest::Error,
    },
    /// The server has a password and this client did not present it, or
    /// presented the wrong one.
    #[error(
        "the ganja server at {address} refused the credential; set GANJA_SERVER_PASSWORD \
         (and GANJA_SERVER_USERNAME, if it is not the default) to what it was started with"
    )]
    Unauthorized {
        /// The server that refused.
        address: String,
    },
    /// The server answered, and the answer was a refusal.
    #[error("the ganja server refused {method} {path} with {status}: {body}")]
    Refused {
        /// The method that was refused.
        method: &'static str,
        /// The route that was refused.
        path: String,
        /// The status it carried.
        status: u16,
        /// What the server said, which for this server is a JSON message.
        body: String,
    },
    /// The server speaks a wire this build cannot read — the declared posture
    /// for version skew; see the crate documentation.
    #[error(
        "the ganja server speaks a wire this build does not understand: {detail}. \
         The server and this client are different versions of ganja"
    )]
    Skew {
        /// What could not be read, and why.
        detail: String,
    },
    /// This subscriber fell behind and the engine dropped it. Everything read
    /// before this is real and in order; everything after it was never queued,
    /// which is why it is an error and not a quiet end of stream.
    #[error("the ganja server dropped this event stream: {notice}")]
    Evicted {
        /// The engine's own account of the eviction.
        notice: String,
    },
}

/// What `GET /global/health` answers.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// Always true when the server answers at all; carried because the route
    /// carries it.
    pub healthy: bool,
    /// The server's own version, which is the first thing to compare when
    /// [`ClientError::Skew`] shows up.
    pub version: String,
    /// The session the server is serving right now (**D505**) — what maps a
    /// live socket, named by a prefix of its session's id, back to the one
    /// session it belongs to. Required, as this crate requires every field
    /// it declares: a server that omits it is a version this build does not
    /// speak to.
    pub session_id: SessionId,
}

/// One row of `GET /session`, narrowed to what a client acts on.
///
/// Deliberately partial: the listing is `ganja-core`'s `SessionInfo`, and
/// duplicating a ten-field type belonging to a crate this one must not link
/// would be a maintenance debt for no gain. `--continue` needs an id and
/// whether the session has a parent; those are read, and the rest is left
/// alone.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SessionRow {
    /// Names the session.
    pub id: SessionId,
    /// The session that spawned this one, when a `task` call did. A run
    /// continues roots only, exactly as `ganja sessions` lists them.
    #[serde(default)]
    pub parent: Option<SessionId>,
}

/// One request the engine is waiting on, as `GET /permission` lists it.
///
/// Declared whole and closed: these are the fields of
/// `Event::PermissionRequested`, projected by serve
/// (`ganja-serve/src/state.rs:21-38`), and a server that changed the shape is
/// a server this build does not understand.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PendingPermission {
    /// Session whose turn is waiting.
    pub session_id: SessionId,
    /// What a reply names.
    pub id: PermissionId,
    /// The tool call waiting on the decision.
    pub call_id: String,
    /// Tool asking to run.
    pub tool: String,
    /// One line saying what would run.
    pub title: String,
    /// The arguments it would run with.
    pub args: serde_json::Value,
    /// Directories outside the project the call would touch. Serve omits the
    /// field when there are none.
    #[serde(default)]
    pub directories: Vec<String>,
}

/// What a prompt carries, the body both prompt routes take
/// (`ganja-serve/src/routes.rs:336-344`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Prompt {
    /// What to ask.
    pub text: String,
    /// Files named in the message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
    /// Run this turn as this agent instead of the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Ask this model instead of the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Prompt {
    /// A plain prompt: text, and nothing switched.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Runs the turn as `agent`, when one was named.
    #[must_use]
    pub fn as_agent(mut self, agent: Option<String>) -> Self {
        self.agent = agent;
        self
    }

    /// Asks `model`, when one was named.
    #[must_use]
    pub fn asking(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }
}

/// What `POST /team/{name}/message` carries: a plain message from another
/// session, as `ganja-serve`'s socket route takes it (**D505**).
///
/// Declared here as a client declares every body it sends; the server's side
/// is `ganja-core`'s `SocketMessage`, three fields of the same names, and a
/// drift between the two is refused by the server rather than guessed at.
/// Plain text only, by shape: there is no field a protocol frame could ride
/// in, which is §5.2-6's rule made a type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TeamMessage {
    /// The sender's derived identity, `<name>@<team>` — what the receiving
    /// session stamps the message with. A bare member name is refused there,
    /// since it could name a member of *that* team.
    pub from: String,
    /// What the recipient reads.
    pub text: String,
    /// One line about it, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl TeamMessage {
    /// A message from `from` saying `text`, with nothing summarized.
    #[must_use]
    pub fn new(from: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            text: text.into(),
            summary: None,
        }
    }

    /// Adds the one-line summary.
    #[must_use]
    pub fn summarized(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

/// What `POST /team/{name}/message` answers when the message landed:
/// `ganja-core`'s `SocketDelivered`, declared whole and closed.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delivered {
    /// The bare member name the server wrote to.
    pub to: String,
    /// What became of it, in the server's words.
    pub note: String,
}

/// The scheme and host every socket request is spelled under. `reqwest`
/// resolves nothing once a client is bound to a socket, so the host is a
/// label the URL needs and the server's router never reads.
const SOCKET_URL: &str = "http://ganja";

/// How long a socket-bound client waits to connect: a Unix socket connects
/// in microseconds or fails at once, so this bounds only a peer that
/// accepted the connection and then went silent before its first byte.
#[cfg(unix)]
const SOCKET_CONNECT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a socket-bound client waits between bytes of an answer before
/// it gives the peer up. A per-read bound rather than a whole-request one,
/// deliberately: the same client may open `GET /event`, whose body is
/// endless by design and heartbeats every ten seconds, and a total deadline
/// would end that stream while a per-read one only ends a silent peer.
#[cfg(unix)]
const SOCKET_READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The most of any answer this client reads into memory, in bytes — a
/// refusal's envelope included. Every route here answers a bounded document;
/// the event stream is read frame by frame and never through this. Past it,
/// [`ClientError::Oversized`].
pub const BODY_CAP: usize = 8 << 20;

/// The scheme a socket address is *shown* under — §5.6's own spelling of a
/// Unix-socket address, so an error about one reads the way a `send_message`
/// call would have written it.
const SOCKET_SCHEME: &str = "uds:";

/// A connection to a served engine.
///
/// Nothing here holds a session: the server does, and every call names the one
/// it acts on — which is what lets one client drive a session another client
/// started.
///
/// Two fields carry the address, on purpose: `address` is what a person reads
/// in every error and every `Debug`, and `base` is what the requests are
/// spelled under. They are one string for a TCP client and differ for a
/// socket-bound one, whose requests need an `http://` base the socket does
/// not have and whose errors should name the socket, not the label.
pub struct Client {
    http: reqwest::Client,
    address: String,
    base: String,
    credentials: Option<Credentials>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("address", &self.address)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl Client {
    /// A client for the server at `address`, presenting `credentials` when the
    /// server has a password.
    ///
    /// # Errors
    ///
    /// [`ClientError::Address`] when `address` is not an absolute `http` or
    /// `https` URL — a bare host and port is refused rather than guessed at,
    /// because guessing the scheme is guessing whether the credential travels
    /// in the clear.
    pub fn new(address: &str, credentials: Option<Credentials>) -> Result<Self, ClientError> {
        let parsed = reqwest::Url::parse(address).map_err(|error| ClientError::Address {
            address: address.to_owned(),
            reason: error.to_string(),
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ClientError::Address {
                address: address.to_owned(),
                reason: format!("{} is not a scheme this client speaks", parsed.scheme()),
            });
        }

        // Trailing slashes are stripped so every route below can be written
        // the way the router spells it.
        let address = address.trim_end_matches('/').to_owned();

        Ok(Self {
            http: reqwest::Client::new(),
            base: address.clone(),
            address,
            credentials,
        })
    }

    /// A client bound to the session socket at `path` (**D505**).
    ///
    /// One `reqwest::Client` per socket path, and this is where that rule is
    /// kept: `unix_socket` routes every request of the client it is set on
    /// through that path, so a client that switched addresses would be
    /// sending to the wrong session. No credential, because the server's
    /// transport-aware guard asks a same-uid socket for none — the
    /// filesystem already answered who may connect.
    ///
    /// Bounded in time as well as in bytes: a connect that does not complete
    /// inside two seconds and a read that stalls past thirty are transport
    /// failures rather than hangs, and every body is read under [`BODY_CAP`]. The far end of a socket is
    /// another process's word, and a client that would wait on it forever or
    /// buffer whatever it sent is a client a hostile listener could stall or
    /// bloat — `ganja sessions --live` walks every socket in the directory
    /// through this.
    ///
    /// Available on Unix only, where the socket is; the type is not.
    ///
    /// # Errors
    ///
    /// [`ClientError::SocketPath`] when `path` is empty or carries a NUL,
    /// neither of which any socket can be bound at — refused here in words
    /// rather than at the first request as an OS error about a name.
    #[cfg(unix)]
    pub fn on_socket(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref();
        let shown = path.display().to_string();
        if shown.is_empty() {
            return Err(ClientError::SocketPath {
                path: shown,
                reason: "it is empty".to_owned(),
            });
        }
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(ClientError::SocketPath {
                path: shown,
                reason: "it carries a NUL byte".to_owned(),
            });
        }
        let http = reqwest::Client::builder()
            .unix_socket(path)
            .connect_timeout(SOCKET_CONNECT_DEADLINE)
            .read_timeout(SOCKET_READ_DEADLINE)
            .build()
            .map_err(|error| ClientError::SocketPath {
                path: shown.clone(),
                reason: error.to_string(),
            })?;

        Ok(Self {
            http,
            address: format!("{SOCKET_SCHEME}{shown}"),
            base: SOCKET_URL.to_owned(),
            credentials: None,
        })
    }

    /// The address this client drives, as it will appear in every error.
    #[cfg(test)]
    #[must_use]
    fn address(&self) -> &str {
        &self.address
    }

    /// `GET /global/health`: whether anything is there, and what version it is.
    ///
    /// Driven first by everything that attaches, so a typo'd address or a
    /// missing password is one readable sentence rather than a failure three
    /// calls later.
    ///
    /// # Errors
    ///
    /// The transport, credential and skew refusals above.
    pub async fn health(&self) -> Result<Health, ClientError> {
        self.get("/global/health").await
    }

    /// `GET /team` (D-13): the team the served session leads, on either
    /// address form.
    ///
    /// # Errors
    ///
    /// As [`Client::health`], plus [`ClientError::Refused`] carrying `404`
    /// when the session leads no team.
    pub async fn team(&self) -> Result<TeamView, ClientError> {
        self.get("/team").await
    }

    /// `POST /team/{name}/message` (D-13): a plain message to `name`, a
    /// member of the served session's team — its lead's own name included,
    /// which is how a message reaches that session's next turn.
    ///
    /// The route is served on the socket alone. Called on a TCP client it is
    /// refused by the server with `404`, exactly as any route that is not
    /// there — declared here all the same, because which listener answers a
    /// route is the server's fact and not this crate's to pre-empt.
    ///
    /// # Errors
    ///
    /// As [`Client::health`], plus [`ClientError::Refused`] carrying `404`
    /// (no team, no such member, or a TCP server), `400` (a blank text, a
    /// protocol frame, or a `from` that is not `<name>@<team>`) or `500` (an
    /// inbox that would not take it), each with the server's own sentence.
    pub async fn send_team_message(
        &self,
        name: &str,
        message: &TeamMessage,
    ) -> Result<Delivered, ClientError> {
        let body = serde_json::to_value(message).map_err(|error| ClientError::Skew {
            detail: format!("a team message does not serialize: {error}"),
        })?;

        self.send("POST", &format!("/team/{name}/message"), Some(body))
            .await
    }

    /// `POST /session`: points the server's engine at a fresh session and
    /// answers its id.
    ///
    /// # Errors
    ///
    /// As [`Client::health`], plus [`ClientError::Refused`] when the engine
    /// would not take the command — a turn already streaming, most of all.
    pub async fn create_session(&self) -> Result<SessionId, ClientError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Created {
            id: SessionId,
        }

        let created: Created = self.send("POST", "/session", None).await?;

        Ok(created.id)
    }

    /// `GET /session`: every stored session, newest first.
    ///
    /// # Errors
    ///
    /// As [`Client::health`].
    pub async fn sessions(&self) -> Result<Vec<SessionRow>, ClientError> {
        self.get("/session").await
    }

    /// `POST /session/{id}/prompt_async`: accepted, and the turn reports
    /// itself on the event stream.
    ///
    /// # Errors
    ///
    /// As [`Client::health`], plus [`ClientError::Refused`] carrying `404`
    /// when nothing stored answers to `session` and `409` when a turn is
    /// already streaming.
    pub async fn prompt(&self, session: &SessionId, prompt: &Prompt) -> Result<(), ClientError> {
        let body = serde_json::to_value(prompt).map_err(|error| ClientError::Skew {
            detail: format!("a prompt does not serialize: {error}"),
        })?;
        self.send_empty(
            "POST",
            &format!("/session/{}/prompt_async", session.as_str()),
            Some(body),
        )
        .await
    }

    /// `GET /permission`: every request the engine is waiting on, oldest
    /// first — what a client that just connected reads before it can answer
    /// anything.
    ///
    /// # Errors
    ///
    /// As [`Client::health`].
    pub async fn permissions(&self) -> Result<Vec<PendingPermission>, ClientError> {
        self.get("/permission").await
    }

    /// `POST /permission/{id}/reply`: answers one dialog.
    ///
    /// A reply nothing is waiting for is defined to be ignored — which is what
    /// a reply racing a cancelled turn becomes — so this does not fail on a
    /// race.
    ///
    /// # Errors
    ///
    /// As [`Client::health`].
    pub async fn reply_permission(
        &self,
        id: &PermissionId,
        reply: PermissionReply,
    ) -> Result<(), ClientError> {
        self.send_empty(
            "POST",
            &format!("/permission/{}/reply", id.as_str()),
            Some(serde_json::json!({"response": reply})),
        )
        .await
    }

    /// `GET /event`: the engine's events, in order, as they happen.
    ///
    /// Returns once the server's `connected` frame has been read, which is the
    /// registration guarantee: serve claims its subscription before the
    /// response exists, so everything the engine emits after this call returns
    /// is either in the stream or after it — never lost between. Prompt
    /// *after* this returns, exactly as a frontend subscribes before it sends.
    ///
    /// Heartbeats are swallowed; they say the connection is alive and nothing
    /// about the conversation.
    ///
    /// # Errors
    ///
    /// As [`Client::health`], plus [`ClientError::Skew`] when the stream opens
    /// with anything but `connected`.
    pub async fn events(&self) -> Result<Events, ClientError> {
        let path = "/event";
        let response = self
            .request(reqwest::Method::GET, path)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        let response = self.checked("GET", path, response).await?;

        let mut bytes = response.bytes_stream().boxed();
        let mut frames = sse::Frames::new();
        loop {
            if let Some(frame) = frames.pop() {
                let frame = frame?;
                if frame == sse::Frame::Connected {
                    break;
                }
                return Err(ClientError::Skew {
                    detail: format!(
                        "the event stream opened with {frame:?} rather than the \
                         {} frame that carries the registration guarantee",
                        sse::CONNECTED
                    ),
                });
            }

            match bytes.next().await {
                Some(Ok(chunk)) => frames.push(chunk.as_ref()),
                Some(Err(source)) => return Err(self.transport(source)),
                None => {
                    return Err(ClientError::Skew {
                        detail: "the event stream ended before it said hello".to_owned(),
                    });
                }
            }
        }

        Ok(Events::new(self.address.clone(), bytes, frames))
    }

    /// A request with the credential attached, when there is one.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, format!("{}{path}", self.base));

        match &self.credentials {
            Some(credentials) => {
                builder.basic_auth(&credentials.username, Some(&credentials.password))
            }
            None => builder,
        }
    }

    fn transport(&self, source: reqwest::Error) -> ClientError {
        ClientError::Transport {
            address: self.address.clone(),
            source,
        }
    }

    /// The status half every call shares: a credential refusal is named as
    /// one, and any other failure carries what the server said.
    async fn checked(
        &self,
        method: &'static str,
        path: &str,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, ClientError> {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized {
                address: self.address.clone(),
            });
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = self.bounded(method, path, response).await?;

            return Err(ClientError::Refused {
                method,
                path: path.to_owned(),
                status,
                body,
            });
        }

        Ok(response)
    }

    /// The whole body, read under [`BODY_CAP`]: a declared length past the
    /// cap is refused before a byte, an undeclared or lying one the moment
    /// the cap is passed.
    async fn bounded(
        &self,
        method: &'static str,
        path: &str,
        mut response: reqwest::Response,
    ) -> Result<String, ClientError> {
        let oversized = || ClientError::Oversized {
            method,
            path: path.to_owned(),
            cap: BODY_CAP,
        };
        if response
            .content_length()
            .is_some_and(|length| length > BODY_CAP as u64)
        {
            return Err(oversized());
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| self.transport(source))?
        {
            if body.len() + chunk.len() > BODY_CAP {
                return Err(oversized());
            }
            body.extend_from_slice(&chunk);
        }

        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send("GET", path, None).await
    }

    /// One request whose answer is a JSON document this build has a type for.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let text = self.text(method, path, body).await?;

        serde_json::from_str(&text).map_err(|error| ClientError::Skew {
            detail: format!("{method} {path} answered a body this build cannot read: {error}"),
        })
    }

    /// One request whose answer is a `204` nobody reads.
    async fn send_empty(
        &self,
        method: &'static str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(), ClientError> {
        self.text(method, path, body).await.map(|_| ())
    }

    async fn text(
        &self,
        method: &'static str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String, ClientError> {
        let verb = reqwest::Method::from_bytes(method.as_bytes())
            .expect("every method this crate sends is a valid one");
        let mut builder = self.request(verb, path);
        if let Some(body) = body {
            builder = builder.json(&body);
        }

        let response = builder
            .send()
            .await
            .map_err(|source| self.transport(source))?;
        let response = self.checked(method, path, response).await?;

        self.bounded(method, path, response).await
    }
}

/// The engine's events as they arrive, one item per event.
///
/// Ends when the server's stream ends, and ends with
/// [`ClientError::Evicted`] when this subscriber was the one dropped — the
/// distinction the whole `evicted` frame exists to preserve.
pub struct Events {
    inner: BoxStream<'static, Result<Event, ClientError>>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nothing inside is renderable, and the one thing that would be — the
        // buffered bytes — is a transcript, not a diagnostic.
        formatter.debug_struct("Events").finish_non_exhaustive()
    }
}

impl Events {
    fn new<S, B>(address: String, bytes: S, frames: sse::Frames) -> Self
    where
        S: Stream<Item = reqwest::Result<B>> + Send + 'static,
        B: AsRef<[u8]>,
    {
        let state = Reading {
            address,
            bytes: Box::pin(bytes),
            frames,
            done: false,
        };

        Self {
            inner: futures::stream::unfold(state, |mut state| async move {
                if state.done {
                    return None;
                }

                loop {
                    if let Some(frame) = state.frames.pop() {
                        match frame {
                            Ok(sse::Frame::Message(event)) => return Some((Ok(*event), state)),
                            // Both say the connection is alive and nothing
                            // about the conversation.
                            Ok(sse::Frame::Connected | sse::Frame::Heartbeat) => continue,
                            Ok(sse::Frame::Evicted(notice)) => {
                                state.done = true;
                                let evicted = ClientError::Evicted {
                                    notice: notice.message,
                                };

                                return Some((Err(evicted), state));
                            }
                            // A frame this build cannot read leaves a
                            // transcript nobody should trust; it is the last
                            // thing this stream says.
                            Err(error) => {
                                state.done = true;

                                return Some((Err(error), state));
                            }
                        }
                    }

                    match state.bytes.next().await {
                        Some(Ok(chunk)) => state.frames.push(chunk.as_ref()),
                        Some(Err(source)) => {
                            state.done = true;
                            let error = ClientError::Transport {
                                address: state.address.clone(),
                                source,
                            };

                            return Some((Err(error), state));
                        }
                        None => return None,
                    }
                }
            })
            .boxed(),
        }
    }
}

/// What [`Events`] carries between polls: the bytes still arriving, and the
/// frame that may be half-read.
struct Reading<S> {
    address: String,
    bytes: std::pin::Pin<Box<S>>,
    frames: sse::Frames,
    done: bool,
}

impl Stream for Events {
    type Item = Result<Event, ClientError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(context)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt as _;

    use super::{Client, ClientError, Credentials};

    /// Nothing may render a password — the canary every credential-carrying
    /// type in this workspace is held to.
    #[test]
    fn no_rendering_of_a_client_or_its_credential_shows_the_password() {
        let credentials = Credentials::new("ganja", "hunter2");
        let client = Client::new("http://127.0.0.1:4096", Some(credentials.clone()))
            .expect("a loopback address is usable");

        for rendered in [format!("{credentials:?}"), format!("{client:?}")] {
            assert!(
                !rendered.contains("hunter2"),
                "a password reached a formatter: {rendered}"
            );
            assert!(
                rendered.contains("redacted"),
                "and the redaction is visible: {rendered}"
            );
        }
    }

    #[test]
    fn an_address_without_a_scheme_is_refused_rather_than_guessed_at() {
        let error = Client::new("127.0.0.1:4096", None).expect_err("a bare host is not an address");
        let said = error.to_string();
        assert!(said.contains("127.0.0.1:4096"), "{said}");
        assert!(said.contains("http://127.0.0.1:4096"), "{said}");

        // A URL that parses but is not HTTP is refused for a different reason,
        // and says which.
        let error = Client::new("ftp://example.invalid", None).expect_err("not a scheme we speak");
        assert!(error.to_string().contains("ftp"), "{error}");
    }

    #[test]
    fn a_trailing_slash_does_not_double_up_in_a_route() {
        let client = Client::new("http://127.0.0.1:4096/", None).expect("an address with a slash");
        assert_eq!(client.address(), "http://127.0.0.1:4096");
    }

    /// A socket-bound client is shown under §5.6's own `uds:` spelling, so
    /// an error about it reads as the address a `send_message` call would
    /// have written, while its requests are spelled under the one `http://`
    /// base the socket needs and never resolves.
    #[cfg(unix)]
    #[test]
    fn a_socket_client_is_shown_as_uds_and_spells_its_requests_under_the_socket_base() {
        let client =
            Client::on_socket("/tmp/ganja/abcd1234.sock").expect("a socket path is usable");
        assert_eq!(client.address(), "uds:/tmp/ganja/abcd1234.sock");
        assert_eq!(client.base, super::SOCKET_URL);
        assert!(
            client.credentials.is_none(),
            "a same-uid socket presents no credential"
        );
        assert!(
            format!("{client:?}").contains("uds:/tmp/ganja/abcd1234.sock"),
            "and Debug shows the socket, not the label"
        );
    }

    /// The two paths no socket can be bound at are refused here, in words,
    /// rather than at the first request as an OS error about a name.
    #[cfg(unix)]
    #[test]
    fn an_empty_or_nul_bearing_socket_path_is_refused_in_words() {
        let empty = Client::on_socket("").expect_err("nothing listens at nowhere");
        assert!(
            matches!(empty, ClientError::SocketPath { ref reason, .. } if reason.contains("empty")),
            "{empty}"
        );

        let path = std::ffi::OsStr::from_bytes(b"/tmp/ganja/bad\0name.sock");
        let nul = Client::on_socket(path).expect_err("a NUL cannot travel in a socket path");
        assert!(
            matches!(nul, ClientError::SocketPath { ref reason, .. } if reason.contains("NUL")),
            "{nul}"
        );
    }
}

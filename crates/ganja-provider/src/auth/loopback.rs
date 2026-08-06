//! The loopback listener an OAuth redirect comes back to.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts:46-92` and
//! `packages/opencode/src/plugin/openai/codex.ts:154-245`.
//!
//! A browser login sends the person to the provider and gets an authorization
//! code back by having the provider redirect the browser at a server on this
//! machine. This is that server: it binds loopback, answers exactly one
//! callback, and stops. Nothing in it knows which provider it is serving —
//! ChatGPT is the first user, xAI's flow is the same shape at a different port
//! and path — so the provider's identity arrives as arguments.
//!
//! **This module never writes to the credential store.** It hands a code back
//! to whoever asked for it, and that caller decides what to do with it. So "a
//! login that was cancelled, refused, forged or timed out stores nothing" is a
//! property of the module graph rather than a promise about a code path: there
//! is no call in this file that could store anything, in any branch, ever.
//!
//! Three bounds, and each of them is load-bearing:
//!
//! - **Loopback only.** `127.0.0.1`, never `0.0.0.0` — an authorization code is
//!   a credential in flight, and the whole point of a loopback redirect is that
//!   it never reaches a network. Upstream's `codex.ts:220` binds every
//!   interface, which puts a one-shot code-accepting endpoint on the local
//!   network for the length of a login; its own newer implementation binds
//!   `localhost` (`openai.ts:80`).
//! - **A deadline.** Someone who closes the browser tab is not coming back, and
//!   a listener waiting for them forever is a port held open forever.
//! - **A cancellation token.** The deadline is minutes; a `Ctrl-C` is now.
//!
//! **Deliberate divergence, and it is a security one.** Upstream reads the
//! redirect's `error` parameter before it validates `state`
//! (`openai.ts:58-66`); this validates `state` first, so nothing in a request
//! that cannot prove it belongs to this login is ever read out of it. The cost
//! is that a provider which omitted `state` from an error redirect would be
//! reported as a forged callback rather than by its own error — RFC 6749
//! §4.1.2.1 requires the echo on error responses too, so that is a conformance
//! bet rather than a guess.

use std::{io, time::Duration};

use secrecy::{ExposeSecret as _, SecretString};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

/// Path that ends the wait without a credential (`codex.ts:207-213`).
const CANCEL_PATH: &str = "/cancel";

/// Longest request head this will read before giving up on a connection.
///
/// A callback is one GET line and a browser's usual headers. An order of
/// magnitude past that is not a browser completing a login, and reading it
/// would be reading whatever a local process felt like sending.
const MAX_HEAD: usize = 8 * 1024;

/// How long one connection has to finish sending its request head.
///
/// Connections are served one at a time, so without this a local process could
/// hold a login open by connecting and saying nothing. The overall deadline
/// would still end it, but minutes later.
const HEAD_WINDOW: Duration = Duration::from_secs(10);

/// Longest OAuth `error` code this will repeat.
///
/// Every code RFC 6749 registers is well under this; the cap is about what
/// arrives, not about what should.
const MAX_ERROR_CODE: usize = 64;

/// Stands in for a refusal whose reason was not something worth repeating.
const UNNAMED: &str = "no usable reason given";

/// A wait for a callback that ended without one.
#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    /// The port could not be listened on.
    ///
    /// Almost always another copy of the same login already waiting: a
    /// provider's callback port is fixed by its client registration, so two
    /// browser logins for one provider cannot run at once.
    #[error("the login could not listen on 127.0.0.1:{port} for the browser's callback: {source}")]
    Bind {
        /// The port that was asked for.
        port: u16,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The listener stopped being able to accept connections.
    #[error("the login stopped being able to accept the browser's callback: {source}")]
    Accept {
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The provider redirected back with a refusal instead of a code.
    #[error("the provider refused the authorization ({error})")]
    Denied {
        /// The provider's own error code, or [`UNNAMED`] when what arrived was
        /// not code-shaped. Never anything longer, and never free text.
        error: String,
    },
    /// A callback arrived that could not prove it belonged to this login.
    ///
    /// Missing `state`, the wrong `state`, or `state` given twice. Which of
    /// those it was is deliberately not reported: there is nothing a person
    /// does differently about each, and the only party that would act on the
    /// difference is whoever sent it.
    #[error(
        "a callback arrived that did not belong to this login and was refused; \
         start the login again"
    )]
    Forged,
    /// The callback belonged to this login and carried no code.
    #[error("the browser's callback carried no authorization code")]
    NoCode,
    /// The login was cancelled — by the `/cancel` path, or by the caller.
    #[error("the login was cancelled")]
    Cancelled,
    /// Nobody completed the authorization in time.
    #[error("nobody completed the authorization within {}s", .after.as_secs())]
    TimedOut {
        /// How long was allowed.
        after: Duration,
    },
}

/// A bound loopback socket waiting for one OAuth redirect.
///
/// Binding and waiting are separate steps because the authorize URL has to name
/// the redirect this is listening on, and when the port was left to the
/// operating system that is not known until the socket exists. It is the right
/// order regardless: a browser opened before the socket is a browser that can
/// complete the login before anything is listening for the answer.
#[derive(Debug)]
pub struct Listener {
    /// The socket.
    socket: tokio::net::TcpListener,
    /// The port it actually got.
    port: u16,
}

impl Listener {
    /// Binds `127.0.0.1` at `port`.
    ///
    /// Port `0` takes whatever the operating system has free, which is how a
    /// test avoids contending for a provider's fixed callback port.
    ///
    /// # Errors
    ///
    /// Returns [`LoopbackError::Bind`] when the port cannot be listened on.
    pub async fn bind(port: u16) -> Result<Self, LoopbackError> {
        let socket = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| LoopbackError::Bind { port, source })?;
        let bound = socket
            .local_addr()
            .map_err(|source| LoopbackError::Bind { port, source })?
            .port();

        Ok(Self {
            socket,
            port: bound,
        })
    }

    /// The port the redirect URI has to name.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serves one callback at `path` and stops.
    ///
    /// `state` is the value the authorize URL published; a callback that does
    /// not echo exactly it is answered `400` and ends the wait. Requests for
    /// any other path are answered `404` and change nothing — a browser
    /// fetching a favicon must not cost somebody their login — except
    /// `/cancel`, which ends the wait deliberately.
    ///
    /// Taking `self` is how "one callback and stop" is stated: the socket is
    /// dropped when this returns, whatever it returns.
    ///
    /// # Errors
    ///
    /// Returns [`LoopbackError`]: [`Forged`] for a callback that could not
    /// prove it belonged to this login, [`Denied`] when the provider refused,
    /// [`NoCode`] when it belonged and carried nothing, [`Cancelled`] for
    /// either cancellation route, [`TimedOut`] past `within`, and [`Accept`]
    /// when the socket itself failed.
    ///
    /// [`Forged`]: LoopbackError::Forged
    /// [`Denied`]: LoopbackError::Denied
    /// [`NoCode`]: LoopbackError::NoCode
    /// [`Cancelled`]: LoopbackError::Cancelled
    /// [`TimedOut`]: LoopbackError::TimedOut
    /// [`Accept`]: LoopbackError::Accept
    pub async fn wait(
        self,
        path: &str,
        state: &SecretString,
        within: Duration,
        cancel: &CancellationToken,
    ) -> Result<SecretString, LoopbackError> {
        tokio::select! {
            () = cancel.cancelled() => Err(LoopbackError::Cancelled),
            served = tokio::time::timeout(within, self.serve(path, state)) => {
                served.unwrap_or(Err(LoopbackError::TimedOut { after: within }))
            }
        }
    }

    /// Accepts until one request settles the login.
    async fn serve(&self, path: &str, state: &SecretString) -> Result<SecretString, LoopbackError> {
        loop {
            let (mut socket, _) = self
                .socket
                .accept()
                .await
                .map_err(|source| LoopbackError::Accept { source })?;

            let Some(head) = head(&mut socket).await else {
                // Said nothing usable before it was cut off. Not a callback,
                // and not worth an answer.
                continue;
            };
            let Some(target) = target(&head) else {
                answer(&mut socket, BAD_REQUEST, MALFORMED_PAGE).await;
                continue;
            };
            let (requested, query) = split(target);

            if requested == CANCEL_PATH {
                answer(&mut socket, OK, CANCELLED_PAGE).await;
                return Err(LoopbackError::Cancelled);
            }
            if requested != path {
                answer(&mut socket, NOT_FOUND, NOT_FOUND_PAGE).await;
                continue;
            }

            return settle(&mut socket, query, state).await;
        }
    }
}

/// Decides what a request at the callback path means, and answers it.
///
/// The `state` check is first and unconditional: everything after it reads
/// values out of a request, and a request that cannot prove it belongs to this
/// login has not earned that. A mismatch ends the wait rather than waiting for
/// a better one, which also means whoever sent it gets exactly one guess and no
/// oracle to iterate against — that is why an ordinary string comparison is
/// enough here and a constant-time one would be decoration.
async fn settle(
    socket: &mut TcpStream,
    query: &str,
    state: &SecretString,
) -> Result<SecretString, LoopbackError> {
    match only(query, "state") {
        Some(echoed) if echoed == *state.expose_secret() => {}
        _ => {
            answer(socket, BAD_REQUEST, FORGED_PAGE).await;
            return Err(LoopbackError::Forged);
        }
    }

    if let Some(refusal) = only(query, "error") {
        answer(socket, BAD_REQUEST, DENIED_PAGE).await;
        return Err(LoopbackError::Denied {
            error: error_code(&refusal).unwrap_or_else(|| UNNAMED.to_owned()),
        });
    }

    let Some(code) = only(query, "code") else {
        answer(socket, BAD_REQUEST, NO_CODE_PAGE).await;
        return Err(LoopbackError::NoCode);
    };

    answer(socket, OK, SUCCESS_PAGE).await;

    Ok(SecretString::from(code))
}

/// The OAuth `error` code in `value`, when `value` is one.
///
/// RFC 6749 defines this field — §4.1.2.1 on a redirect, §5.2 in a token
/// endpoint's body — as a short code from a registered set, spelled without
/// spaces or quoting. Anything else is not a code, and this build has no
/// business repeating it: on a redirect the field is whatever arrived in a
/// query, and it ends up in a message a person reads and a log keeps.
///
/// `error_description` is deliberately never read at all. It is free text by
/// definition, and a server having a bad day is entirely capable of putting the
/// token it is complaining about into it.
pub(crate) fn error_code(value: &str) -> Option<String> {
    let code_shaped = !value.is_empty()
        && value.len() <= MAX_ERROR_CODE
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');

    code_shaped.then(|| value.to_owned())
}

/// The one value `name` has in `query`, or `None`.
///
/// `None` also covers `name` given more than once, which is not pedantry:
/// `?state=mine&state=theirs` is a request whose meaning depends on which end
/// of the parser you ask, and the safe reading of a value two parties disagree
/// about is that it was not given. Upstream's `URLSearchParams.get` takes the
/// first; a duplicate here reads as absent, and for `state` that is a refusal.
fn only(query: &str, name: &str) -> Option<String> {
    let mut found = None;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key == name {
            if found.is_some() {
                return None;
            }
            found = Some(value.into_owned());
        }
    }

    found
}

/// The path and the query of a request target.
///
/// A fragment never reaches a server, but a client is free to be wrong, so it
/// is cut first. An absolute-form target — what a proxy would send — keeps its
/// scheme and authority in the path here and simply will not match the callback
/// path, which is a `404` and the right answer for a request that was not meant
/// for this socket.
fn split(target: &str) -> (&str, &str) {
    let target = target.split_once('#').map_or(target, |(head, _)| head);

    target.split_once('?').unwrap_or((target, ""))
}

/// The request target out of a request head.
fn target(head: &str) -> Option<&str> {
    head.lines().next()?.split(' ').nth(1)
}

/// The request head, up to but not including the blank line that ends it.
///
/// `None` for a connection that closed first, took too long, said more than a
/// browser ever would, or sent bytes that are not text. Every one of those is a
/// connection that is not a callback, and the caller's answer to all of them is
/// the same: wait for the next one.
async fn head(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        let read = tokio::time::timeout(HEAD_WINDOW, socket.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);

        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            head.truncate(end);
            return String::from_utf8(head).ok();
        }
        if head.len() > MAX_HEAD {
            return None;
        }
    }
}

/// Answers one request and closes the connection.
///
/// The outcome is deliberately dropped. The login is already decided by the
/// time this is called, and a browser that hung up before reading the page has
/// not changed what happened; propagating a write error here would be the one
/// place in this module where a socket could overwrite a real answer.
async fn answer(socket: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {length}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        length = body.len()
    );

    if socket.write_all(response.as_bytes()).await.is_ok() {
        let _ = socket.flush().await;
    }
    let _ = socket.shutdown().await;
}

/// `200`.
const OK: &str = "200 OK";

/// `400`, which is what a callback that settled nothing gets.
const BAD_REQUEST: &str = "400 Bad Request";

/// `404`, for a request that was not the callback at all.
const NOT_FOUND: &str = "404 Not Found";

/// One of the pages this serves, assembled at compile time.
///
/// A macro rather than a function because every page is built from words this
/// module owns, and that is the point: no value out of the query — not the
/// code, not the `state`, not the provider's own error text — is ever put in
/// front of a browser, so there is no escaping anywhere in this file and no way
/// to forget it. Upstream reflects the provider's error message into its page
/// (`openai.ts:64`); a page served from `localhost` that echoes whatever
/// arrived in a query will eventually be handed something other than an error
/// message.
macro_rules! page {
    ($heading:expr, $detail:expr) => {
        concat!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n",
            "<meta charset=\"utf-8\">\n",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n",
            "<title>ganja</title>\n",
            "<style>body{font:16px/1.5 system-ui,sans-serif;margin:0;display:grid;",
            "place-content:center;min-height:100vh;padding:2rem;text-align:center}",
            "h1{font-size:1.25rem;font-weight:600;margin:0 0 .5rem}",
            "p{margin:0;opacity:.7}</style>\n",
            "</head>\n<body>\n<h1>",
            $heading,
            "</h1>\n<p>",
            $detail,
            "</p>\n</body>\n</html>\n",
        )
    };
}

/// The page a completed login leaves in the browser.
const SUCCESS_PAGE: &str = page!(
    "Signed in",
    "You can close this tab and go back to the terminal."
);

/// The page a forged or misdirected callback leaves in the browser.
const FORGED_PAGE: &str = page!(
    "That did not belong to this login",
    "Nothing was signed in. Start the login again from the terminal."
);

/// The page a provider's refusal leaves in the browser.
const DENIED_PAGE: &str = page!(
    "The provider refused",
    "Nothing was signed in. The terminal has the reason."
);

/// The page a callback with no code leaves in the browser.
const NO_CODE_PAGE: &str = page!(
    "That callback carried nothing",
    "Nothing was signed in. Start the login again from the terminal."
);

/// The page `/cancel` leaves in the browser.
const CANCELLED_PAGE: &str = page!(
    "Login cancelled",
    "Nothing was signed in. You can close this tab."
);

/// The page a request this could not parse leaves in the browser.
const MALFORMED_PAGE: &str = page!("Not something this understood", "Nothing was signed in.");

/// The page anything else leaves in the browser.
const NOT_FOUND_PAGE: &str = page!("Nothing here", "This is a login callback and nothing else.");

#[cfg(test)]
mod tests {
    use std::{net::IpAddr, time::Duration};

    use secrecy::{ExposeSecret as _, SecretString};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpStream,
        task::JoinHandle,
    };
    use tokio_util::sync::CancellationToken;

    use super::{Listener, LoopbackError, error_code};

    /// The `state` a login published in its authorize URL.
    const STATE: &str = "state-QYRC7g0nJ8mWfZ2v";

    /// The path the provider was told to redirect to.
    const PATH: &str = "/auth/callback";

    /// The code a provider hands back.
    const CODE: &str = "ac-8sJcqL41xTn0";

    /// Longer than any test spends. The deadline has its own test, and every
    /// other test's outcome is decided by a request rather than by a clock.
    const AMPLE: Duration = Duration::from_secs(60);

    /// Short enough to be quick, long enough that a loaded machine still gets
    /// there. Nothing connects in the test that uses it, so no scheduling can
    /// change the outcome — only how soon it arrives.
    const BRIEF: Duration = Duration::from_millis(250);

    /// A bound listener already waiting, and the port to reach it on.
    fn waiting(
        listener: Listener,
        cancel: &CancellationToken,
    ) -> (u16, JoinHandle<Result<SecretString, LoopbackError>>) {
        let port = listener.port();
        let cancel = cancel.clone();
        let served = tokio::spawn(async move {
            listener
                .wait(PATH, &SecretString::from(STATE), AMPLE, &cancel)
                .await
        });

        (port, served)
    }

    /// Sends one raw request and returns the whole response, status line first.
    ///
    /// Raw rather than through an HTTP client because the status code is half
    /// of what is being asserted: "answered 400" is a claim about the wire, and
    /// a client that hid the status behind an error type would not check it.
    async fn request(port: u16, target: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the listener is bound");
        socket
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("the request is written");

        let mut response = String::new();
        socket
            .read_to_string(&mut response)
            .await
            .expect("the response is read");

        response
    }

    /// The status line of a response.
    fn status(response: &str) -> &str {
        response.lines().next().unwrap_or_default()
    }

    #[tokio::test]
    async fn the_listener_binds_loopback_and_nothing_else() {
        let listener = Listener::bind(0).await.expect("loopback is bindable");
        let bound = listener
            .socket
            .local_addr()
            .expect("a bound socket has an address");

        assert_eq!(
            bound.ip(),
            IpAddr::from([127, 0, 0, 1]),
            "an authorization code must never be accepted from a network"
        );
        assert_eq!(bound.port(), listener.port());
    }

    #[tokio::test]
    async fn a_callback_that_echoes_the_state_hands_back_its_code() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(port, &format!("{PATH}?code={CODE}&state={STATE}")).await;
        let code = served
            .await
            .expect("the wait finished")
            .expect("the callback was accepted");

        assert_eq!(code.expose_secret(), CODE);
        assert_eq!(status(&response), "HTTP/1.1 200 OK");
    }

    #[tokio::test]
    async fn a_callback_answering_with_the_wrong_state_is_refused() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(port, &format!("{PATH}?code={CODE}&state=not-the-one")).await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("a callback from another login must not be accepted");

        assert!(matches!(refused, LoopbackError::Forged), "{refused:?}");
        assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    }

    #[tokio::test]
    async fn a_callback_carrying_no_state_at_all_is_refused() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(port, &format!("{PATH}?code={CODE}")).await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("a callback that proves nothing must not be accepted");

        assert!(matches!(refused, LoopbackError::Forged), "{refused:?}");
        assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    }

    #[tokio::test]
    async fn a_callback_that_gives_the_state_twice_is_refused() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        // One of the two is the real login's, so a parser that took either end
        // of the query would accept this.
        let response = request(
            port,
            &format!("{PATH}?code={CODE}&state={STATE}&state=not-the-one"),
        )
        .await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("a value two parties disagree about was not given");

        assert!(matches!(refused, LoopbackError::Forged), "{refused:?}");
        assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    }

    #[tokio::test]
    async fn a_callback_that_belongs_here_and_carries_no_code_is_refused() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(port, &format!("{PATH}?state={STATE}")).await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("there is nothing to exchange");

        assert!(matches!(refused, LoopbackError::NoCode), "{refused:?}");
        assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    }

    #[tokio::test]
    async fn a_redirect_carrying_the_providers_refusal_ends_the_login_with_it() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(
            port,
            &format!("{PATH}?error=access_denied&error_description=user+said+no&state={STATE}"),
        )
        .await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("the provider refused");

        assert!(
            matches!(&refused, LoopbackError::Denied { error } if error == "access_denied"),
            "{refused:?}"
        );
        // The free-text description is never read, so it cannot reach anything.
        assert!(!refused.to_string().contains("user said no"));
        assert_eq!(status(&response), "HTTP/1.1 400 Bad Request");
    }

    #[tokio::test]
    async fn a_refusal_that_is_not_a_code_is_reported_without_repeating_it() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        request(
            port,
            &format!("{PATH}?error=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state={STATE}"),
        )
        .await;
        let refused = served
            .await
            .expect("the wait finished")
            .expect_err("the provider refused");

        let message = refused.to_string();
        assert!(!message.contains("script"), "{message}");
        assert!(message.contains("no usable reason given"), "{message}");
    }

    #[tokio::test]
    async fn a_request_for_another_path_is_404_and_the_login_keeps_waiting() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        // What a browser does on its own, without being asked.
        let ignored = request(port, "/favicon.ico").await;
        assert_eq!(status(&ignored), "HTTP/1.1 404 Not Found");

        let response = request(port, &format!("{PATH}?code={CODE}&state={STATE}")).await;
        let code = served
            .await
            .expect("the wait finished")
            .expect("the callback still arrived");

        assert_eq!(code.expose_secret(), CODE);
        assert_eq!(status(&response), "HTTP/1.1 200 OK");
    }

    #[tokio::test]
    async fn the_cancel_path_ends_the_wait_without_a_code() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let response = request(port, "/cancel").await;
        let ended = served
            .await
            .expect("the wait finished")
            .expect_err("cancelling yields no code");

        assert!(matches!(ended, LoopbackError::Cancelled), "{ended:?}");
        assert_eq!(status(&response), "HTTP/1.1 200 OK");
    }

    #[tokio::test]
    async fn a_wait_nobody_answers_ends_at_its_deadline() {
        let listener = Listener::bind(0).await.expect("bindable");

        let ended = listener
            .wait(
                PATH,
                &SecretString::from(STATE),
                BRIEF,
                &CancellationToken::new(),
            )
            .await
            .expect_err("nobody completed the authorization");

        assert!(
            matches!(ended, LoopbackError::TimedOut { after } if after == BRIEF),
            "{ended:?}"
        );
    }

    #[tokio::test]
    async fn a_wait_ends_promptly_when_it_is_cancelled() {
        let cancel = CancellationToken::new();
        let (_port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        cancel.cancel();

        // `AMPLE` is what the wait was given, so anything that arrives inside
        // `BRIEF` arrived because the token ended it and not because it expired.
        let ended = tokio::time::timeout(BRIEF, served)
            .await
            .expect("cancelling is not something to wait out")
            .expect("the wait finished")
            .expect_err("cancelling yields no code");

        assert!(matches!(ended, LoopbackError::Cancelled), "{ended:?}");
    }

    #[tokio::test]
    async fn no_page_ever_contains_the_code_or_the_state() {
        // Two listeners because either outcome ends its own wait, and the page
        // for each has to be checked: the browser is the one place a value out
        // of the query could be reflected back at whoever sent it.
        let cancel = CancellationToken::new();
        let (refused_port, refused_wait) =
            waiting(Listener::bind(0).await.expect("bindable"), &cancel);
        let (accepted_port, accepted_wait) =
            waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        let refused = request(
            refused_port,
            &format!("{PATH}?code={CODE}&state=not-the-one"),
        )
        .await;
        let accepted = request(accepted_port, &format!("{PATH}?code={CODE}&state={STATE}")).await;
        refused_wait.await.expect("the wait finished").ok();
        accepted_wait.await.expect("the wait finished").ok();

        for page in [&refused, &accepted] {
            assert!(!page.contains(CODE), "a code reached the browser: {page}");
            assert!(!page.contains(STATE), "a state reached the browser: {page}");
        }
    }

    #[tokio::test]
    async fn a_connection_that_says_nothing_costs_the_login_nothing() {
        let cancel = CancellationToken::new();
        let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

        drop(
            TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("the listener is bound"),
        );

        let response = request(port, &format!("{PATH}?code={CODE}&state={STATE}")).await;
        let code = served
            .await
            .expect("the wait finished")
            .expect("the callback still arrived");

        assert_eq!(code.expose_secret(), CODE);
        assert_eq!(status(&response), "HTTP/1.1 200 OK");
    }

    #[test]
    fn only_a_code_shaped_refusal_is_worth_repeating() {
        for code in ["access_denied", "invalid-request", "server_error", "x"] {
            assert_eq!(error_code(code).as_deref(), Some(code));
        }
        for not_a_code in [
            "",
            "user said no",
            "<script>alert(1)</script>",
            "sk-ant-\"quoted\"",
            &"a".repeat(65),
        ] {
            assert_eq!(error_code(not_a_code), None, "{not_a_code:?}");
        }
    }
}

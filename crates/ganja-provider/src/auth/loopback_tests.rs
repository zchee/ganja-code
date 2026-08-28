use std::net::IpAddr;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
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
        listener.wait(PATH, &SecretString::from(STATE), AMPLE, &cancel).await
    });

    (port, served)
}

/// Sends one raw request and returns the whole response, status line first.
///
/// Raw rather than through an HTTP client because the status code is half
/// of what is being asserted: "answered 400" is a claim about the wire, and
/// a client that hid the status behind an error type would not check it.
async fn request(port: u16, target: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.expect("the listener is bound");
    socket
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("the request is written");

    let mut response = String::new();
    socket.read_to_string(&mut response).await.expect("the response is read");

    response
}

/// The status line of a response.
fn status(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

#[tokio::test]
async fn the_listener_binds_loopback_and_nothing_else() {
    let listener = Listener::bind(0).await.expect("loopback is bindable");
    let bound = listener.socket.local_addr().expect("a bound socket has an address");

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
    let code = served.await.expect("the wait finished").expect("the callback was accepted");

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
    let response =
        request(port, &format!("{PATH}?code={CODE}&state={STATE}&state=not-the-one")).await;
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
    let refused =
        served.await.expect("the wait finished").expect_err("there is nothing to exchange");

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
    let refused = served.await.expect("the wait finished").expect_err("the provider refused");

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

    request(port, &format!("{PATH}?error=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state={STATE}")).await;
    let refused = served.await.expect("the wait finished").expect_err("the provider refused");

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
    let code = served.await.expect("the wait finished").expect("the callback still arrived");

    assert_eq!(code.expose_secret(), CODE);
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn the_cancel_path_ends_the_wait_without_a_code() {
    let cancel = CancellationToken::new();
    let (port, served) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);

    let response = request(port, "/cancel").await;
    let ended = served.await.expect("the wait finished").expect_err("cancelling yields no code");

    assert!(matches!(ended, LoopbackError::Cancelled), "{ended:?}");
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
}

#[tokio::test]
async fn a_wait_nobody_answers_ends_at_its_deadline() {
    let listener = Listener::bind(0).await.expect("bindable");

    let ended = listener
        .wait(PATH, &SecretString::from(STATE), BRIEF, &CancellationToken::new())
        .await
        .expect_err("nobody completed the authorization");

    assert!(matches!(ended, LoopbackError::TimedOut { after } if after == BRIEF), "{ended:?}");
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
    let (refused_port, refused_wait) = waiting(Listener::bind(0).await.expect("bindable"), &cancel);
    let (accepted_port, accepted_wait) =
        waiting(Listener::bind(0).await.expect("bindable"), &cancel);

    let refused = request(refused_port, &format!("{PATH}?code={CODE}&state=not-the-one")).await;
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

    drop(TcpStream::connect(("127.0.0.1", port)).await.expect("the listener is bound"));

    let response = request(port, &format!("{PATH}?code={CODE}&state={STATE}")).await;
    let code = served.await.expect("the wait finished").expect("the callback still arrived");

    assert_eq!(code.expose_secret(), CODE);
    assert_eq!(status(&response), "HTTP/1.1 200 OK");
}

#[test]
fn only_a_code_shaped_refusal_is_worth_repeating() {
    for code in ["access_denied", "invalid-request", "server_error", "x"] {
        assert_eq!(error_code(code).as_deref(), Some(code));
    }
    for not_a_code in
        ["", "user said no", "<script>alert(1)</script>", "sk-ant-\"quoted\"", &"a".repeat(65)]
    {
        assert_eq!(error_code(not_a_code), None, "{not_a_code:?}");
    }
}

//! A credential planted in the environment must not come back out.
//!
//! The risk this pins down is not a `println!` somebody forgot: it is that a
//! credential travels through a `Debug` rendering, a `tracing` field, or an
//! error body the provider echoed, and nobody notices because none of those
//! look like printing a secret. So a canary key is planted the way a real one
//! is — in the environment — a turn is run against a socket that rejects it by
//! quoting it back, and every byte of what came out is searched.
//!
//! An OAuth credential is two more secrets on the same paths, and one of them
//! travels further than an API key ever does: a refresh token is *sent* to a
//! token endpoint, and a token endpoint that refuses it routinely quotes it
//! back in the body it refuses with. So the same drill runs a second time
//! against an access token and a refresh token — planted in the store rather
//! than the environment, because that is where OAuth credentials live — with
//! the renewal refused by a socket that echoes the token at it.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.
//!
//! The capture is installed as the *global* subscriber rather than a
//! thread-local default. A thread-local one only sees what the calling thread
//! traces, so it would quietly stop covering the library the day someone gave
//! this test a multi-threaded runtime — the assertions would still pass, on an
//! empty search space. Being global means the flavour cannot matter, and the
//! assertion that a library-internal trace arrived is what proves it.

use std::{
    env, io,
    sync::{Arc, Mutex},
};

use futures::StreamExt as _;
use ganja_core::{
    auth::{self, AuthErrorKind, OauthCredential, RefreshOauth as _, grok},
    provider::{
        self, AnthropicProvider, ChatRequest, OpenAiProvider, Provider as _, ProviderEvent,
    },
};
use secrecy::SecretString;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;

/// The key planted in the environment. Nothing may render it.
const CANARY: &str = "sk-test-canary-XYZ";

/// The access token planted in the credential store. Nothing may render it.
const ACCESS_CANARY: &str = "at-test-canary-UVW";

/// The refresh token planted beside it. This one is also *sent*, which is one
/// more way out than the other two have.
const REFRESH_CANARY: &str = "rt-test-canary-RST";

/// A `tracing` writer a test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log is never poisoned")
            .extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Serves `responses`, one per connection, then closes.
async fn serve(responses: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );

    let server = tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (url, server)
}

/// A close-delimited response.
fn response(status: &str, content_type: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-type: {content_type}\r\n\r\n{body}")
}

#[tokio::test]
async fn a_key_planted_in_the_environment_never_renders_and_never_logs() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var("ANTHROPIC_API_KEY", CANARY);
        env::set_var("OPENAI_API_KEY", CANARY);
        env::set_var("GANJA_PROVIDER", "anthropic");
        env::remove_var("GANJA_MODEL");
        env::remove_var("ANTHROPIC_BASE_URL");
        env::remove_var("OPENAI_BASE_URL");
    }

    let selection = provider::from_env().expect("the planted key selects anthropic");
    assert_eq!(selection.provider.id(), "anthropic");
    assert!(
        selection.notice.is_none(),
        "a provider that was asked for by name needs no notice"
    );

    let (url, _server) = serve(vec![
        response(
            "401 Unauthorized",
            "application/json",
            // The shape that makes this worth testing: the provider quotes the
            // credential it refused.
            &format!(
                r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key: {CANARY}"}}}}"#
            ),
        ),
        response(
            "200 OK",
            "text/event-stream",
            include_str!("../../ganja-provider/tests/fixtures/anthropic_happy_path.sse"),
        ),
        // The third turn is a config-declared endpoint's, and it is refused
        // the same way: a provider a person configured is a request path like
        // any other, and the redaction it inherits from the wire it rides has
        // to be inherited in fact and not only in prose.
        response(
            "401 Unauthorized",
            "application/json",
            &format!(
                r#"{{"error":{{"message":"Incorrect API key provided: {CANARY}","type":"invalid_request_error"}}}}"#
            ),
        ),
    ])
    .await;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    // Global rather than thread-local, so that what this catches does not
    // depend on which thread the library happens to trace from.
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary holds one test, so nothing else has installed one");

    let rendered = {
        let anthropic = AnthropicProvider::from_env()
            .expect("the planted key builds a provider")
            .with_base_url(&url);
        let openai = OpenAiProvider::from_env()
            .expect("the planted key builds a provider")
            .with_base_url(&url);

        let request = ChatRequest {
            model: "test-model".to_owned(),
            system: None,
            messages: vec![ganja_core::protocol::Message::user("hello")],
            tools: Vec::new(),
        };

        // First turn: refused, with the key quoted back at us. Matched rather
        // than `expect_err`, because the success type is a stream and streams
        // have no `Debug`.
        let Err(refusal) = anthropic
            .stream(request.clone(), CancellationToken::new())
            .await
        else {
            panic!("a 401 is not answerable");
        };

        // Second turn: answered, which is the log-heavy path — unknown event
        // types, unknown delta types, comments and stop reasons all trace.
        let events: Vec<ProviderEvent> = anthropic
            .stream(request, CancellationToken::new())
            .await
            .expect("the second response is a stream")
            .collect()
            .await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta(_))),
            "the answered turn should have streamed text, got {events:?}"
        );

        // And the same drill through a provider a *config* named, which is the
        // newest way a credential reaches a socket: the key comes from the
        // variable the entry names, the wire is one of the two above, and the
        // rendering is the compat provider's own.
        let mut config = ganja_core::config::Config::default();
        config.provider.insert(
            "local-llama".to_owned(),
            ganja_core::config::ProviderConfig {
                dialect: provider::Dialect::OpenaiChatCompletions,
                base_url: url.clone(),
                key_env: Some("ANTHROPIC_API_KEY".to_owned()),
                headers: std::collections::BTreeMap::new(),
            },
        );
        // The flag tier, which outranks the `GANJA_PROVIDER=anthropic` this
        // test planted, so the same process reaches both kinds of provider.
        config.overrides.model = Some("local-llama/tiny-instruct".to_owned());
        let configured = provider::select(&config).expect("the entry's own variable holds the key");
        assert_eq!(configured.provider.id(), "local-llama");

        let Err(compat_refusal) = configured
            .provider
            .stream(
                ChatRequest {
                    model: configured.model.clone(),
                    system: None,
                    messages: vec![ganja_core::protocol::Message::user("hello")],
                    tools: Vec::new(),
                },
                CancellationToken::new(),
            )
            .await
        else {
            panic!("a 401 is not answerable");
        };
        // Asserted here rather than only in the sweep below, because the sweep
        // would pass on the *anthropic* refusal's mask alone: what has to be
        // shown is that this path masked the key this endpoint quoted back.
        assert!(
            compat_refusal.to_string().contains("[redacted]"),
            "a configured endpoint's refusal should mask the key it echoed: {compat_refusal}"
        );

        // The `Selection` rather than the provider, because `dyn Provider` has
        // no `Debug` — and that is the rendering a caller actually holds. The
        // compat provider's own is pinned beside it, in `provider/compat.rs`.
        format!(
            "{anthropic:?} {openai:?} {refusal} {refusal:?} \
             {configured:?} {compat_refusal} {compat_refusal:?}"
        )
    };

    // The same drill for an OAuth credential. The store is the environment for
    // this kind, so the canaries are planted there, read back through the
    // public path, and then spent against an endpoint that refuses the renewal
    // by quoting the token it refused.
    let oauth_rendered = {
        let credential = OauthCredential::new(
            SecretString::from(REFRESH_CANARY),
            SecretString::from(ACCESS_CANARY),
            auth::now_ms(),
        );
        auth::set_oauth(grok::PROVIDER_ID, &credential).expect("the credential stores");
        let read_back = auth::oauth_for(grok::PROVIDER_ID)
            .expect("the store reads")
            .expect("the credential is there");

        let (token_url, _token_server) = serve(vec![response(
            "401 Unauthorized",
            "application/json",
            // The shape that makes this worth testing: the endpoint refuses
            // the token by repeating it.
            &format!(
                r#"{{"error":"invalid_grant","error_description":"refresh token {REFRESH_CANARY} was already used; access token {ACCESS_CANARY} is revoked"}}"#
            ),
        )])
        .await;
        let refusal = grok::Refresh::at(format!("{token_url}/token"))
            .expect("a client builds")
            .refresh(grok::PROVIDER_ID, &read_back)
            .await
            .expect_err("a refused renewal is not a credential");

        assert_eq!(
            refusal.kind(),
            AuthErrorKind::ReauthRequired,
            "an endpoint that refused the token is asking for a new login"
        );
        let listed = auth::list_providers().expect("the store lists");

        format!(
            "{credential:?} {credential} {read_back:?} {read_back} {refusal} {refusal:?} \
             {listed:?}"
        )
    };

    let logged = capture.logged();

    // Not just "something was captured": something the *library* traced, from
    // wherever it traced it. Anything less and a change to how the turn is
    // driven could empty the search space without failing a single assertion.
    // These two are the mapper's own lines, one per fixture frame it skips or
    // reads, and neither is written by this test.
    for line in [
        "skipping an unfamiliar Anthropic frame",
        "the model stopped",
        // The refused renewal's own line, for the same reason: without it the
        // OAuth half of this test would be searching an empty space.
        "the token endpoint would not renew",
    ] {
        assert!(
            logged.contains(line),
            "the capture never saw the library trace {line:?}, so finding no key \
             in it would prove nothing:\n{logged}"
        );
    }
    for secret in [CANARY, ACCESS_CANARY, REFRESH_CANARY] {
        assert!(
            !logged.contains(secret),
            "a credential reached the log:\n{logged}"
        );
    }
    assert!(
        !rendered.contains(CANARY),
        "a credential reached a rendering: {rendered}"
    );
    assert!(
        rendered.contains("[redacted]"),
        "the echoed key should be masked rather than dropped: {rendered}"
    );
    for secret in [ACCESS_CANARY, REFRESH_CANARY] {
        assert!(
            !oauth_rendered.contains(secret),
            "an OAuth token reached a rendering: {oauth_rendered}"
        );
    }
    assert!(
        oauth_rendered.contains("****") && oauth_rendered.contains("invalid_grant"),
        "the tokens should be masked and the error code kept - the code is what a \
         person acts on: {oauth_rendered}"
    );
    assert!(
        !oauth_rendered.contains("already used") && !oauth_rendered.contains("revoked"),
        "the refused body must not travel into a message that will be logged: \
         {oauth_rendered}"
    );

    // The store itself is the one place these do belong, and a test that
    // proved nothing rendered them because nothing ever held them would prove
    // nothing at all.
    let file = std::fs::read_to_string(auth::store_path().expect("the store has a path"))
        .expect("the store was written");
    assert!(
        file.contains(ACCESS_CANARY) && file.contains(REFRESH_CANARY),
        "the credential store is where a credential is supposed to be"
    );
}

//! A config-declared chat-completions endpoint becoming a whole turn.
//!
//! The point is the **request**, asserted whole against a loopback socket: an
//! endpoint nobody shipped, reached at the URL its entry named, authenticated
//! by the variable its entry named, carrying the headers its entry declared
//! and none of the ones a builtin provider would have added. Everything about
//! that comes off config, so a mock of the provider would prove nothing — what
//! is under test is the path from a `ganja.jsonc` to the bytes on a socket.
//!
//! The turn is driven through the ordinary [`Engine`], not through
//! `Provider::stream` directly, because a configured provider has to be
//! priceable, titleable and compactable like any other — and the session layer
//! reaches all three through [`Provider::id`], which for this provider is a
//! name a person chose.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::{env, sync::Arc};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    config::{Config, ProviderConfig},
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, PartId},
    provider::{self, Dialect},
    tool::Registry,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// The key the endpoint is reached with. It is named by the config entry and
/// exported here, and nothing may render it.
const CANARY: &str = "sk-test-canary-XYZ";

/// The variable the config entry names. Deliberately not one of `auth`'s
/// builtin key variables: what this proves is that an entry's own `key_env` is
/// read, and a builtin name would pass whether or not it was.
const KEY_VAR: &str = "GANJA_TEST_LOCAL_LLAMA_KEY";

/// The id the config entry is written under, which is what the provider must
/// answer to.
const PROVIDER_ID: &str = "local-llama";

/// A model no catalog carries, which is every model a private endpoint serves.
const MODEL: &str = "tiny-instruct";

/// A header the entry declares. A configured endpoint may want one — a routing
/// hint a proxy dispatches on — and nothing else in this build would add it.
const ROUTE: &str = "x-route";

#[tokio::test(flavor = "multi_thread")]
async fn a_config_named_openai_compatible_endpoint_takes_a_whole_turn_on_the_key_its_entry_names() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        // Redirected so the machine running the suite cannot contribute a
        // credential, and so nothing this writes reaches a real store.
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var(KEY_VAR, CANARY);
        // The tiers above the config, cleared so the config is what decides.
        env::remove_var("GANJA_PROVIDER");
        env::remove_var("GANJA_MODEL");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let base_url = format!(
        "http://{}/v1",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let seen = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the turn connects");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        // Read until the body is in hand rather than until the headers are:
        // what this test asserts is the whole request, and the model name is
        // the last thing in it that the assertions need.
        loop {
            let read = socket.read(&mut buffer).await.expect("the request arrives");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request);
            if text.contains("\r\n\r\n") && text.contains(MODEL) {
                break;
            }
        }

        let body = include_str!("../../ganja-provider/tests/fixtures/openai_happy_path.sse");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("the reply is writable");
        socket.flush().await.expect("the reply flushes");
        drop(socket);

        String::from_utf8_lossy(&request).into_owned()
    });

    let mut config = Config::default();
    config.provider.insert(
        PROVIDER_ID.to_owned(),
        ProviderConfig {
            dialect: Dialect::OpenaiChatCompletions,
            base_url: base_url.clone(),
            key_env: Some(KEY_VAR.to_owned()),
            headers: [(ROUTE.to_owned(), "gpu-0".to_owned())]
                .into_iter()
                .collect(),
        },
    );
    // The config's own `model` tier names both halves, which is the route a
    // project takes: nothing in the environment says anything.
    config.model = Some(format!("{PROVIDER_ID}/{MODEL}"));

    let selection = provider::select(&config).expect("the config declares this endpoint");
    assert_eq!(
        selection.provider.id(),
        PROVIDER_ID,
        "the turn is priced, gated and disclosed under the name the entry was written under"
    );
    assert_eq!(selection.model, MODEL);
    assert!(
        selection.notice.is_none(),
        "an endpoint that was asked for is not one that was defaulted to"
    );

    let engine = Engine::new(
        Arc::clone(&selection.provider),
        &selection.model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Told apart by the part each fragment grows: this endpoint streams a
    // thought beside the reply, and the two reach the frontend as two parts
    // rather than as one run-together string.
    let mut streamed = String::new();
    let mut thought = String::new();
    let mut thoughts: Vec<PartId> = Vec::new();
    let mut finish = None;
    while let Some(event) = events.next().await {
        match event {
            Event::PartStarted { part, .. } => {
                if matches!(part.body, PartBody::ReasoningText { .. }) {
                    thoughts.push(part.id);
                }
            }
            Event::PartDelta { part_id, delta, .. } => {
                if thoughts.contains(&part_id) {
                    thought.push_str(&delta);
                } else {
                    streamed.push_str(&delta);
                }
            }
            Event::MessageFinished { reason, error, .. } => {
                finish = Some((reason, error));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        finish,
        Some((FinishReason::Completed, None)),
        "a configured endpoint takes a whole turn like any other"
    );
    assert_eq!(streamed, "Hello, world!");
    assert_eq!(
        thought, "A greeting is enough.",
        "the thought arrives as its own part rather than run into the reply"
    );

    let request = seen.await.expect("the endpoint answered");
    let head = request
        .split("\r\n\r\n")
        .next()
        .expect("a request has a head")
        .to_owned();

    assert!(
        request.starts_with("POST /v1/chat/completions "),
        "the entry's own path is what was asked, not a builtin's: {head}"
    );
    assert!(
        header(&head, "authorization").as_deref() == Some(&format!("Bearer {CANARY}")),
        "the key the entry's `key_env` named is what authenticated it: {head}"
    );
    assert_eq!(
        header(&head, ROUTE).as_deref(),
        Some("gpu-0"),
        "the entry's declared header travels: {head}"
    );
    // Nothing this build adds for a provider of its own may reach an endpoint
    // somebody else configured: those headers describe a vendor's product, and
    // sending them names a session as something it is not.
    for builtin in [
        "x-api-key",
        "anthropic-version",
        "x-github-api-version",
        "openai-intent",
        "x-initiator",
        "chatgpt-account-id",
        "originator",
        "openai-beta",
    ] {
        assert!(
            header(&head, builtin).is_none(),
            "{builtin} belongs to a provider this build ships, not to a configured one: {head}"
        );
    }
    assert!(
        request.contains(&format!("\"model\":\"{MODEL}\"")),
        "the model the config named is what was asked for: {request}"
    );
    assert!(
        request.contains("\"stream\":true"),
        "a turn streams: {request}"
    );
}

/// The value `head` carries for the header `name`.
///
/// Field names are case-insensitive, and which case a client sends is its own
/// business, so the comparison has to be too.
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;

        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

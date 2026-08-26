//! A config-declared Messages endpoint becoming a whole turn.
//!
//! The sibling of `compat_openai_wire.rs`, and the reason there are two: the
//! dialect a config names decides the **wire**, and the two wires authenticate
//! differently, spell their path differently and pin an API version between
//! them. A test that only proved one of them would leave the half of the
//! feature that says "which API is this" unasserted.
//!
//! What is asserted here that the sibling cannot be: `x-api-key` rather than a
//! bearer, `/v1/messages` rather than `/chat/completions`, and the dated
//! `anthropic-version` the Messages API requires travelling to an endpoint
//! that is not Anthropic's — because the version is a fact about the wire, not
//! about the vendor, and a compatible endpoint that did not receive it would
//! refuse the request.
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
    protocol::{Command, Event, FinishReason},
    provider::{self, Dialect, anthropic},
    tool::Registry,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// The key the endpoint is reached with. Nothing may render it.
const CANARY: &str = "sk-test-canary-XYZ";

/// The variable the config entry names — deliberately not one of `auth`'s
/// builtin key variables, so that reading it proves the entry was read.
const KEY_VAR: &str = "GANJA_TEST_GATEWAY_KEY";

/// The id the config entry is written under.
const PROVIDER_ID: &str = "gateway";

/// A model no catalog carries, which is every model a private endpoint serves.
const MODEL: &str = "gateway-large";

#[tokio::test(flavor = "multi_thread")]
async fn a_config_named_anthropic_compatible_endpoint_speaks_messages_on_the_key_its_entry_names() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var(KEY_VAR, CANARY);
        // A key exported for the *builtin* of this dialect, so that a wire
        // reading the vendor's variable instead of the entry's would fail
        // loudly here rather than pass by coincidence.
        env::set_var("ANTHROPIC_API_KEY", "sk-test-wrong-key-ZZZ");
        env::remove_var("GANJA_PROVIDER");
        env::remove_var("GANJA_MODEL");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let base_url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let seen = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the turn connects");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
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

        let body = include_str!("../../ganja-provider/tests/fixtures/anthropic_happy_path.sse");
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
            dialect: Dialect::AnthropicMessages,
            base_url: base_url.clone(),
            key_env: Some(KEY_VAR.to_owned()),
            headers: std::collections::BTreeMap::new(),
        },
    );
    // The flag tier this time, so that both spellings a person can reach a
    // configured endpoint by are proved somewhere: this one, and the config's
    // own `model` key in the sibling suite.
    config.overrides.model = Some(format!("{PROVIDER_ID}/{MODEL}"));

    let selection = provider::select(&config).expect("the config declares this endpoint");
    assert_eq!(selection.provider.id(), PROVIDER_ID);
    assert_eq!(selection.model, MODEL);

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
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut finish = None;
    while let Some(event) = events.next().await {
        if let Event::MessageFinished { reason, error, .. } = event {
            finish = Some((reason, error));
            break;
        }
    }
    let Some((reason, error)) = finish else {
        panic!("a turn always ends with a finish");
    };
    assert_eq!(
        reason,
        FinishReason::Completed,
        "a configured Messages endpoint takes a whole turn: {error:?}"
    );

    let request = seen.await.expect("the endpoint answered");
    let head = request
        .split("\r\n\r\n")
        .next()
        .expect("a request has a head")
        .to_owned();

    assert!(
        request.starts_with("POST /v1/messages "),
        "the Messages path is what the dialect decides: {head}"
    );
    assert_eq!(
        header(&head, "x-api-key").as_deref(),
        Some(CANARY),
        "this dialect authenticates with a key header, and with the key the \
         entry's own `key_env` named: {head}"
    );
    assert_eq!(
        header(&head, "anthropic-version").as_deref(),
        Some(anthropic::API_VERSION),
        "the dated version is a fact about the wire, not about the vendor, and \
         an endpoint that did not receive it would refuse the request: {head}"
    );
    assert!(
        header(&head, "authorization").is_none(),
        "a bearer belongs to the other dialect: {head}"
    );
    assert!(
        !request.contains("sk-test-wrong-key-ZZZ"),
        "the builtin's exported key reached an endpoint it was not for: {request}"
    );
    assert!(
        request.contains(&format!("\"model\":\"{MODEL}\"")),
        "the model the flag named is what was asked for: {request}"
    );
}

/// The value `head` carries for the header `name`, compared case-insensitively
/// because which case a client sends is its own business.
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;

        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

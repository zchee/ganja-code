//! A config-declared Responses-dialect endpoint becoming a whole turn.
//!
//! [`compat_openai_wire`]'s drill pointed at the third dialect, asserting the
//! half its siblings cannot prove: the `/responses` path under the entry's own
//! base, and the refuse-to-guess posture crossing the whole config→socket
//! path — `store: false` with **no** `include` and **no** `reasoning` object,
//! on a model the vendor's own backends would have asked sealed reasoning
//! for. A unit test pins that posture on the body encoder; what is under test
//! here is that nothing between a `ganja.toml` and the socket reintroduces
//! it.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.
//!
//! [`compat_openai_wire`]: ./compat_openai_wire.rs

use std::{env, sync::Arc};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    config::{Config, ProviderConfig},
    permission::Permissions,
    protocol::{Command, Event, FinishReason},
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
const KEY_VAR: &str = "GANJA_TEST_PROXY_KEY";

/// The id the config entry is written under, which is what the provider must
/// answer to.
const PROVIDER_ID: &str = "proxy";

/// A model id `seals_reasoning` recognizes, on purpose: the vendor's own
/// backends would put `include: ["reasoning.encrypted_content"]` on a request
/// for it, so the field's absence below is the config tier refusing, not the
/// model failing to qualify.
const MODEL: &str = "gpt-5.4";

/// A header the entry declares. Its value is a second canary: a header is
/// somewhere a token fits, so the rendering sweep below covers it too.
const ROUTE: &str = "x-route";

/// The value [`ROUTE`] carries — asserted on the wire, refused in renderings.
const ROUTE_CANARY: &str = "hdr-canary-7513";

#[tokio::test(flavor = "multi_thread")]
async fn a_config_named_responses_endpoint_takes_a_whole_turn_and_is_asked_for_nothing_sealed() {
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

        // The Responses wire's own event spellings, the way the vendor
        // streams them; two frames are a whole turn.
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",",
            "\"item_id\":\"msg_1\",\"delta\":\"Hello, world!\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
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
            dialect: Dialect::OpenaiResponses,
            base_url: base_url.clone(),
            key_env: Some(KEY_VAR.to_owned()),
            headers: [(ROUTE.to_owned(), ROUTE_CANARY.to_owned())]
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

    // The rendering a caller actually holds, swept for both canaries: the
    // credential, and the header value a config is allowed to put a token in.
    let rendered = format!("{selection:?}");
    assert!(
        !rendered.contains(CANARY) && !rendered.contains(ROUTE_CANARY),
        "a selection's rendering carried a credential: {rendered}"
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
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut streamed = String::new();
    let mut finish = None;
    while let Some(event) = events.next().await {
        match event {
            Event::PartDelta { delta, .. } => streamed.push_str(&delta),
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

    let request = seen.await.expect("the endpoint answered");
    let head = request
        .split("\r\n\r\n")
        .next()
        .expect("a request has a head")
        .to_owned();

    assert!(
        request.starts_with("POST /v1/responses "),
        "the wire's own path under the entry's own base, not a builtin's: {head}"
    );
    assert_eq!(
        header(&head, "authorization").as_deref(),
        Some(format!("Bearer {CANARY}")).as_deref(),
        "the key the entry's `key_env` named is what authenticated it: {head}"
    );
    assert_eq!(
        header(&head, ROUTE).as_deref(),
        Some(ROUTE_CANARY),
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
        request.contains("\"stream\":true") && request.contains("\"store\":false"),
        "a turn streams, statelessly: {request}"
    );
    // The refuse-to-guess posture, end to end: on the vendor's own backends
    // this model's request would carry the `include` ask and a defaulted
    // `reasoning.summary`; an endpoint a config named gets neither.
    assert!(
        !request.contains("\"include\":"),
        "nothing sealed is asked of an endpoint this build has never met: {request}"
    );
    assert!(
        !request.contains("\"reasoning\""),
        "no default is written into somebody else's `reasoning` object: {request}"
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

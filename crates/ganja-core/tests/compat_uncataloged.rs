//! What a session on a provider the catalog has never heard of gives up.
//!
//! The degradation path is not new — it was built at three sites and each is
//! documented — but until a config could name a provider, nothing *rode* it
//! that a person would deliberately choose. This pins it by name for exactly
//! that case, because the two things it gives up are the two a person would
//! otherwise assume were still working.
//!
//! **Auto-compaction is off.** Compaction triggers on how full the context
//! window is, and only the catalog knows how big one is; a session whose model
//! has no row therefore never compacts on its own. The pin is a count of
//! requests: a session seeded past any threshold takes **one** request for its
//! turn, where the same seed on a cataloged model takes two — the summarize
//! request and then the turn, which `persistence.rs` pins from the other side.
//!
//! **Nothing prices it.** `catalog::cost` needs a row, so a turn on a
//! configured endpoint reports tokens and no money. That is the honest answer
//! for an endpoint whose bill is somebody's electricity, and it is asserted
//! here as the absence it is.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::{env, sync::Arc};

use futures::StreamExt as _;
use ganja_core::{
    Engine, Storage, catalog,
    config::{Config, ProviderConfig},
    permission::Permissions,
    protocol::{Command, Event, Role},
    provider::{self, Dialect},
    tool::Registry,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::mpsc,
};

const CANARY: &str = "sk-test-canary-XYZ";
const KEY_VAR: &str = "GANJA_TEST_LOCAL_LLAMA_KEY";
const PROVIDER_ID: &str = "local-llama";
const MODEL: &str = "tiny-instruct";

/// A window fill no catalogued model's ceiling survives, so a session that
/// *could* compact certainly would.
const SEEDED_TOKENS: u64 = 100_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn an_uncataloged_providers_session_never_auto_compacts_and_reports_no_cost() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var(KEY_VAR, CANARY);
        env::remove_var("GANJA_PROVIDER");
        env::remove_var("GANJA_MODEL");
    }

    // The tier this whole suite is about, stated before anything is driven:
    // the endpoint is selectable and the catalog knows nothing about it.
    assert!(
        !catalog::carries(PROVIDER_ID),
        "no published catalog knows a private endpoint"
    );
    assert!(
        catalog::model(MODEL).is_none(),
        "and it has no row to size or price the model it serves"
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let base_url = format!(
        "http://{}/v1",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let (counted, mut requests) = mpsc::unbounded_channel();
    let endpoint = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let counted = counted.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if text.contains("\r\n\r\n") && text.contains(MODEL) {
                        break;
                    }
                }
                let _ = counted.send(String::from_utf8_lossy(&request).into_owned());

                let body =
                    include_str!("../../ganja-provider/tests/fixtures/openai_happy_path.sse");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    let mut config = Config::default();
    config.provider.insert(
        PROVIDER_ID.to_owned(),
        ProviderConfig {
            dialect: Dialect::OpenaiChatCompletions,
            base_url,
            key_env: Some(KEY_VAR.to_owned()),
            headers: std::collections::BTreeMap::new(),
        },
    );
    config.model = Some(format!("{PROVIDER_ID}/{MODEL}"));
    let selection = provider::select(&config).expect("the config declares this endpoint");

    // A session already past every threshold, and pre-titled so the title
    // machinery — which would spend a request of its own — stays out of a
    // count that is about compaction.
    let storage = Storage::open(home.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, SEEDED_TOKENS);
    ganja_testkit::seed_message(
        &storage,
        &session,
        &ganja_core::protocol::Message::user("the window this session already holds"),
    );

    let engine = Engine::persistent(
        Arc::clone(&selection.provider),
        &selection.model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage.clone(),
    );
    engine.resume(&session).await.expect("the session resumes");

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "next step please".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut usage = None;
    let mut opened = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Event::MessageStarted { message, .. } => opened.push(message.role),
            Event::MessageFinished {
                usage: reported, ..
            } => {
                usage = reported;
                break;
            }
            _ => {}
        }
    }
    endpoint.abort();

    let mut asked = Vec::new();
    while let Ok(request) = requests.try_recv() {
        asked.push(request);
    }
    assert_eq!(
        asked.len(),
        1,
        "a session whose window nothing can measure takes its turn uncompacted; \
         a second request here would be the summarize one: {asked:#?}"
    );
    assert!(
        !asked[0].contains("Create a new anchored summary"),
        "and the one request it did make is the turn, not a summarization: {}",
        asked[0]
    );
    // A compaction announces its summary as an assistant message *before* the
    // prompt that triggered it, which is what makes its absence observable
    // from the event stream alone: what opens this turn is the user's own
    // message.
    assert_eq!(
        opened,
        vec![Role::User, Role::Assistant],
        "a summary would have opened the stream ahead of the prompt"
    );

    // The other half of the tier: tokens are counted, and there is nothing to
    // multiply them by. `catalog::cost` needs a row, and asking for one is how
    // every caller — the status bar included — discovers there is no price.
    let usage = usage.expect("a completed turn reports what it spent");
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "the counters a quota is spent against still work: {usage:?}"
    );
    assert!(
        catalog::model(&selection.model).is_none(),
        "nothing can price this turn, which is the honest answer for an \
         endpoint whose bill is somebody's electricity"
    );
}

//! The REST surface against a live loopback socket: the informational
//! routes, the session round-trips against a seeded store, and the
//! error-mapping table — 404, 409, 400 — each observed through a real route.

mod support;

use std::sync::Arc;

use ganja_core::{Engine, Storage, permission::Permissions, tool::Registry};
use ganja_protocol::Message;
use ganja_testkit::{BlockingTool, ScriptedProvider, says, seed_message, tool_call};
use support::{DEADLINE, base_url, loopback_config};

/// A persistent engine over a scripted provider, its store seeded with one
/// session holding one user message, served on a loopback socket.
struct Fixture {
    engine: Arc<Engine>,
    handle: ganja_serve::Handle,
    seeded: ganja_core::SessionId,
    _data: tempfile::TempDir,
}

async fn fixture(
    scripts: Vec<Vec<ganja_core::provider::ProviderEvent>>,
    tools: Registry,
) -> Fixture {
    let data = ganja_testkit::temp_dir();
    let storage = Storage::open(data.path().join("storage"));
    let seeded = ganja_testkit::seed_session(&storage, 0);
    seed_message(&storage, &seeded, &Message::user("hello from the seed"));

    let (provider, _requests) = ScriptedProvider::new(scripts);
    let engine = Arc::new(Engine::persistent(
        provider,
        "scripted-model",
        Arc::new(tools),
        Permissions::default(),
        storage.clone(),
    ));

    let mut config = loopback_config();
    config.storage = Some(storage);
    let handle = ganja_serve::serve(Arc::clone(&engine), config)
        .await
        .expect("a loopback server with no password comes up");

    Fixture {
        engine,
        handle,
        seeded,
        _data: data,
    }
}

fn url(fixture: &Fixture, path: &str) -> String {
    format!("{}{path}", base_url(&fixture.handle))
}

async fn get_json(fixture: &Fixture, path: &str) -> (reqwest::StatusCode, serde_json::Value) {
    let response = reqwest::get(url(fixture, path))
        .await
        .expect("the route answers");
    let status = response.status();
    let value = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);

    (status, value)
}

async fn post_json(
    fixture: &Fixture,
    path: &str,
    body: &str,
) -> (reqwest::StatusCode, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(url(fixture, path))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("the route answers");
    let status = response.status();
    let value = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);

    (status, value)
}

#[tokio::test]
async fn the_informational_routes_answer_about_this_server() {
    let fixture = fixture(vec![says("hi")], Registry::new(Vec::new())).await;

    let (status, health) = get_json(&fixture, "/global/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["healthy"], true);
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));

    let (status, path) = get_json(&fixture, "/path").await;
    assert_eq!(status, 200);
    let cwd = std::env::current_dir().expect("the working directory resolves");
    assert_eq!(path["directory"], cwd.display().to_string());
    assert_eq!(path["root"], cwd.display().to_string());

    // No config was handed over, so the projection is honest about that.
    let (status, config) = get_json(&fixture, "/config").await;
    assert_eq!(status, 200);
    assert_eq!(config, serde_json::json!({}));

    // An engine built without an agent registry has no roster to list.
    let (status, agents) = get_json(&fixture, "/agent").await;
    assert_eq!(status, 200);
    assert_eq!(agents, serde_json::json!([]));

    // The builtin commands are always there, sorted by name.
    let (status, commands) = get_json(&fixture, "/command").await;
    assert_eq!(status, 200);
    let names: Vec<&str> = commands
        .as_array()
        .expect("a listing")
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect();
    assert!(
        names.contains(&"init"),
        "the builtins are listed: {names:?}"
    );

    fixture.handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_seeded_store_round_trips_through_the_session_routes() {
    let fixture = fixture(vec![says("hi")], Registry::new(Vec::new())).await;
    let seeded = fixture.seeded.as_str().to_owned();

    let (status, listed) = get_json(&fixture, "/session").await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = listed
        .as_array()
        .expect("a listing")
        .iter()
        .filter_map(|session| session["id"].as_str())
        .collect();
    assert!(
        ids.contains(&seeded.as_str()),
        "the seeded session is listed: {ids:?}"
    );

    let (status, info) = get_json(&fixture, &format!("/session/{seeded}")).await;
    assert_eq!(status, 200);
    assert_eq!(info["id"], seeded);

    let (status, messages) = get_json(&fixture, &format!("/session/{seeded}/message")).await;
    assert_eq!(status, 200);
    let texts: Vec<&str> = messages
        .as_array()
        .expect("a transcript")
        .iter()
        .flat_map(|message| message["parts"].as_array().into_iter().flatten())
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert_eq!(texts, ["hello from the seed"]);

    // Creating a session points the engine somewhere fresh and says where.
    let before = fixture.engine.session_id();
    let (status, created) = post_json(&fixture, "/session", "").await;
    assert_eq!(status, 200);
    let minted = created["id"].as_str().expect("the new id travels");
    assert_ne!(minted, before.as_str());
    assert_eq!(minted, fixture.engine.session_id().as_str());

    fixture.handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn the_refusal_table_is_observable_through_the_routes() {
    let fixture = fixture(vec![says("hi")], Registry::new(Vec::new())).await;

    // 404: a session nothing stored answers to.
    let (status, body) = get_json(&fixture, "/session/ses_nothing_here").await;
    assert_eq!(status, 404);
    assert_eq!(body["type"], "not_found");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("ses_nothing_here"))
    );

    let (status, body) = post_json(
        &fixture,
        "/session/ses_nothing_here/prompt_async",
        r#"{"text":"hi"}"#,
    )
    .await;
    assert_eq!(
        status, 404,
        "a write route names the missing session too: {body}"
    );
    assert_eq!(body["type"], "not_found");

    // 400: a payload that is not the route's JSON.
    let current = fixture.engine.session_id().as_str().to_owned();
    let (status, body) = post_json(
        &fixture,
        &format!("/session/{current}/prompt_async"),
        "this is not json",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["type"], "invalid_request");

    // A field the route does not take is a refusal, not a silent drop.
    let (status, body) = post_json(
        &fixture,
        &format!("/session/{current}/prompt_async"),
        r#"{"text":"hi","surprise":true}"#,
    )
    .await;
    assert_eq!(status, 400, "unknown fields are refused: {body}");

    fixture.handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_streaming_turn_makes_the_engine_busy_and_abort_ends_it() {
    // A turn that blocks inside its tool until cancelled, so the busy window
    // is exactly as wide as the test needs it.
    let (entered_tx, mut entered) = tokio::sync::mpsc::channel(1);
    let tool = BlockingTool::with_entry_signal("blocker", "blocks until cancelled", entered_tx);
    let mut turn = vec![ganja_core::provider::ProviderEvent::TextDelta(
        "starting".to_owned(),
    )];
    turn.extend(tool_call("blocker", serde_json::json!({})));
    let fixture = fixture(vec![turn, says("done")], Registry::new(vec![tool])).await;

    let mut direct = fixture
        .engine
        .subscribe()
        .await
        .expect("a subscriber registers");
    let current = fixture.engine.session_id().as_str().to_owned();

    let (status, _) = post_json(
        &fixture,
        &format!("/session/{current}/prompt_async"),
        r#"{"text":"go"}"#,
    )
    .await;
    assert_eq!(status, 204);

    tokio::time::timeout(DEADLINE, entered.recv())
        .await
        .expect("the tool starts within the deadline")
        .expect("the tool reports entry");

    // 409: one turn at a time is engine law, and the wire says so.
    let (status, body) = post_json(
        &fixture,
        &format!("/session/{current}/prompt_async"),
        r#"{"text":"another"}"#,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["type"], "conflict");

    // A different stored session cannot be switched to mid-turn either.
    let seeded = fixture.seeded.as_str().to_owned();
    let (status, body) = post_json(
        &fixture,
        &format!("/session/{seeded}/prompt_async"),
        r#"{"text":"switch"}"#,
    )
    .await;
    assert_eq!(status, 409, "a resume mid-turn is the same refusal: {body}");

    // Abort ends the turn observably.
    let (status, aborted) = post_json(&fixture, &format!("/session/{current}/abort"), "").await;
    assert_eq!(status, 200);
    assert_eq!(aborted, serde_json::json!(true));

    let events = ganja_testkit::drain(&mut direct).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ganja_protocol::Event::MessageFinished {
                reason: ganja_protocol::FinishReason::Cancelled,
                ..
            }
        )),
        "the turn ends cancelled: {events:?}"
    );

    fixture.handle.shutdown().await.expect("a clean stop");
}

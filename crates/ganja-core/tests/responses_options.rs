//! The Responses options ladder from the engine's side (**D563**): what a
//! session's `/fast` choice, its configured tables and a headless run's JSON
//! schema put on each request it makes, and what the engine reports back.
//!
//! Driven through [`ScriptedProvider::named`] under the two Responses ids, which
//! records every [`ChatRequest`] the engine built — the value the wire turns
//! into bytes, whose own byte pins live in `ganja-provider`. The one suite
//! here that reads bytes off a real socket is `responses_wire.rs`.
//!
//! Every test runs on `#[tokio::test]`'s current-thread runtime, and two of
//! them depend on it: [`LogCapture`] installs a thread-local subscriber, so a
//! log line is captured only when the turn task that writes it runs on the
//! test's own thread.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::config::ResponsesOptions;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, FastChoice, FinishReason, PartBody};
use ganja_core::provider::responses::CHATGPT_ID;
use ganja_core::provider::{ChatRequest, ProviderError, ProviderEvent};
use ganja_core::responses_ladder::{Source, TierView};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput};
use ganja_core::{Config, Engine, EngineError, Storage};
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{
    LogCapture, RecordedSpawns, ScriptedProvider, caller, drain, drain_allowing, prompt, says,
    served, spawn_with_prompt, tool_call,
};
use serde_json::json;

const SOL: &str = "gpt-5.6-sol";
const FIVE: &str = "gpt-5.5";
const OPENAI: &str = "openai";

/// How long a turn's tail is given to settle before a test moves on.
const SETTLE: Duration = Duration::from_secs(10);

/// A table keyed the way a config's `provider` entries are.
fn tables(id: &str, text: &str) -> BTreeMap<String, ResponsesOptions> {
    let table: ResponsesOptions =
        toml::from_str(text).unwrap_or_else(|error| panic!("{text}: {error}"));

    BTreeMap::from([(id.to_owned(), table)])
}

/// An in-memory engine over `provider` asking `model`, offering `tools`.
fn in_memory(provider: Arc<ScriptedProvider>, model: &str, tools: Vec<Arc<dyn Tool>>) -> Engine {
    Engine::new(provider, model, Arc::new(Registry::new(tools)), Permissions::default())
}

/// Takes one whole turn and waits for its tail, so the next command is never
/// refused as busy.
async fn turn(engine: &Engine, events: &mut BoxStream<'static, Event>, text: &str) -> Vec<Event> {
    engine.send(prompt(text)).await.expect("an idle engine accepts a prompt");
    let seen = drain_allowing(engine, events).await;
    assert!(engine.settle(SETTLE).await, "the turn's tail settles");

    seen
}

/// The last request the provider was asked, which is the step the latest turn
/// took.
fn last(requests: &Mutex<Vec<ChatRequest>>) -> ChatRequest {
    requests.lock().expect("the request log is never poisoned").last().cloned().expect("a request")
}

/// Polls `check` until it yields, for work a turn leaves to a detached task.
async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..500 {
        if let Some(found) = check() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

/// Takes a turn and answers with the tier its (last) request carried.
async fn asked(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    requests: &Mutex<Vec<ChatRequest>>,
) -> Option<String> {
    turn(engine, events, "go").await;
    last(requests).responses.service_tier
}

/// Whether `request` is the one a session's title is asked with.
fn is_title(request: &ChatRequest) -> bool {
    request.system.as_deref().is_some_and(|system| system.contains("title generator"))
}

/// Whether `request` is a compaction's summarize request.
fn is_summary(request: &ChatRequest) -> bool {
    !is_title(request) && request.tools.is_empty() && request.messages.len() == 1
}

/// A tool that holds its call open until the test releases it, saying the
/// moment it started — so a test can change something *during* a turn.
struct Gate {
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for Gate {
    fn id(&self) -> &str {
        "gate"
    }

    fn description(&self) -> &str {
        "waits until the test lets it finish"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _ = self.entered.send(()).await;
        self.release.notified().await;

        Ok(ToolOutput { title: "gate".to_owned(), output: "open".to_owned(), metadata: json!({}) })
    }
}

/// **AC-22.** A choice is announced, reaches the next request, is written onto
/// the row and comes back on a resume; it is refused while a turn streams and
/// refused outright on a provider with no tier to move.
#[tokio::test]
async fn a_fast_choice_is_announced_stored_and_restored() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (provider, requests) = ScriptedProvider::named(CHATGPT_ID, vec![says("ok")]);
    let engine = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(Command::SetFast { fast: Some(FastChoice::Off) }).await.expect("idle takes it");
    let announced = events.next().await.expect("the choice is announced");
    assert!(
        matches!(announced, Event::FastChanged { fast: Some(FastChoice::Off), .. }),
        "got {announced:?}"
    );
    assert_eq!(engine.fast(), Some(FastChoice::Off));

    turn(&engine, &mut events, "hello").await;
    let step = requests.lock().expect("the log").first().cloned().expect("the step was asked");
    assert_eq!(step.responses.service_tier.as_deref(), Some("default"));

    let session = engine.current_session().expect("the prompt minted a session");
    let stored = Storage::open(directory.path().join("storage"))
        .load_info(&session.id)
        .expect("the row reads")
        .expect("the row exists");
    assert_eq!(stored.fast, Some(FastChoice::Off), "the choice is on the row");

    let (reopened_provider, _) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let reopened = Engine::persistent(
        reopened_provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut reopened_events = reopened.subscribe().await.expect("the first subscriber wins");
    reopened.resume(&session.id).await.expect("the session loads");
    assert_eq!(reopened.fast(), Some(FastChoice::Off), "a resume restores the choice");
    let restored = tokio::time::timeout(SETTLE, async {
        loop {
            match reopened_events.next().await.expect("the stream stays open") {
                Event::FastChanged { fast, .. } => return fast,
                _ => continue,
            }
        }
    })
    .await
    .expect("the restored choice is announced");
    assert_eq!(restored, Some(FastChoice::Off));

    // Busy: a turn held open on a call refuses the switch, as `/effort` does.
    let (entered, mut entering) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let (busy_provider, _) =
        ScriptedProvider::named(CHATGPT_ID, vec![tool_call("gate", json!({})), says("done")]);
    let busy = engine_with_gate(busy_provider, FIVE, entered, Arc::clone(&release));
    let mut busy_events = busy.subscribe().await.expect("the first subscriber wins");
    busy.send(prompt("hold")).await.expect("an idle engine accepts a prompt");
    let refused = async {
        entering.recv().await.expect("the call started");
        let refused = busy.send(Command::SetFast { fast: Some(FastChoice::On) }).await;
        release.notify_one();
        refused
    };
    let (refused, _) = tokio::join!(refused, drain_allowing(&busy, &mut busy_events));
    assert!(matches!(refused, Err(EngineError::Busy)), "got {refused:?}");
    assert_eq!(busy.fast(), None, "a refused switch adopts nothing");

    // E4 = E1, byte for byte, on a provider that does not speak Responses.
    let (anthropic, _) = ScriptedProvider::named("anthropic", Vec::new());
    let elsewhere = in_memory(anthropic, "claude-sonnet-4-5", Vec::new());
    for fast in [Some(FastChoice::On), Some(FastChoice::Off), None] {
        let refusal = elsewhere
            .send(Command::SetFast { fast })
            .await
            .expect_err("a provider with no tier refuses the choice");
        assert!(matches!(refusal, EngineError::Fast { .. }), "got {refusal:?}");
        assert_eq!(
            refusal.to_string(),
            "/fast moves service_tier on the chatgpt and openai providers; this session is on anthropic"
        );
    }
    assert_eq!(elsewhere.service_tier(), None);
}

fn engine_with_gate(
    provider: Arc<ScriptedProvider>,
    model: &str,
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Notify>,
) -> Engine {
    in_memory(provider, model, vec![Arc::new(Gate { entered, release })])
}

/// **AC-23.** The charter's example end to end, on the bytes' source: the tier
/// each request carries follows the model, the choice and the configuration,
/// in that order of precedence, on both Responses ids.
#[tokio::test]
async fn the_charters_example_resolves_per_model_and_per_switch() {
    let configured =
        "service_tier = \"priority\"\n[model.\"gpt-5.6-sol\"]\nservice_tier = \"ultrafast\"\n";

    let (provider, requests) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let chatgpt =
        in_memory(provider, SOL, Vec::new()).with_provider_options(tables(CHATGPT_ID, configured));
    let mut events = chatgpt.subscribe().await.expect("the first subscriber wins");

    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("ultrafast"));
    chatgpt
        .send(Command::SwitchModel { model: FIVE.to_owned() })
        .await
        .expect("the seat serves it");
    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("priority"));
    chatgpt.send(Command::SwitchModel { model: SOL.to_owned() }).await.expect("the seat serves it");
    chatgpt.send(Command::SetFast { fast: Some(FastChoice::On) }).await.expect("idle");
    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("ultrafast"));
    chatgpt
        .send(Command::SwitchModel { model: FIVE.to_owned() })
        .await
        .expect("the seat serves it");
    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("priority"));
    chatgpt.send(Command::SetFast { fast: Some(FastChoice::Off) }).await.expect("idle");
    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("default"));
    chatgpt.send(Command::SwitchModel { model: SOL.to_owned() }).await.expect("the seat serves it");
    assert_eq!(asked(&chatgpt, &mut events, &requests).await.as_deref(), Some("default"));
    chatgpt.send(Command::SetFast { fast: None }).await.expect("idle");
    assert_eq!(
        asked(&chatgpt, &mut events, &requests).await.as_deref(),
        Some("ultrafast"),
        "back to config"
    );

    // No config: the seat's own default is the model's fast tier.
    let (bare, bare_requests) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let bare = in_memory(bare, FIVE, Vec::new());
    let mut bare_events = bare.subscribe().await.expect("the first subscriber wins");
    turn(&bare, &mut bare_events, "go").await;
    assert_eq!(last(&bare_requests).responses.service_tier.as_deref(), Some("priority"));
    bare.send(Command::SwitchModel { model: SOL.to_owned() }).await.expect("the seat serves it");
    turn(&bare, &mut bare_events, "go").await;
    assert_eq!(last(&bare_requests).responses.service_tier.as_deref(), Some("ultrafast"));

    // The platform: nothing unless asked, and `/fast on` is priority everywhere.
    let (platform, platform_requests) = ScriptedProvider::named(OPENAI, Vec::new());
    let platform = in_memory(platform, SOL, Vec::new()).with_provider_options(tables(
        OPENAI,
        "[model.\"gpt-5.6-sol\"]\nservice_tier = \"flex\"\n",
    ));
    let mut platform_events = platform.subscribe().await.expect("the first subscriber wins");
    platform.send(Command::SwitchModel { model: FIVE.to_owned() }).await.expect("served");
    turn(&platform, &mut platform_events, "go").await;
    assert_eq!(last(&platform_requests).responses.service_tier, None, "no config, no tier");
    assert_eq!(platform.service_tier(), None);
    platform.send(Command::SwitchModel { model: SOL.to_owned() }).await.expect("served");
    turn(&platform, &mut platform_events, "go").await;
    assert_eq!(last(&platform_requests).responses.service_tier.as_deref(), Some("flex"));
    platform.send(Command::SetFast { fast: Some(FastChoice::On) }).await.expect("idle");
    turn(&platform, &mut platform_events, "go").await;
    assert_eq!(
        last(&platform_requests).responses.service_tier.as_deref(),
        Some("priority"),
        "never ultrafast on the platform, and above the per-model flex"
    );
    platform.send(Command::SwitchModel { model: FIVE.to_owned() }).await.expect("served");
    turn(&platform, &mut platform_events, "go").await;
    assert_eq!(last(&platform_requests).responses.service_tier.as_deref(), Some("priority"));

    let (flex, flex_requests) = ScriptedProvider::named(OPENAI, Vec::new());
    let flex = in_memory(flex, FIVE, Vec::new())
        .with_provider_options(tables(OPENAI, "service_tier = \"flex\""));
    let mut flex_events = flex.subscribe().await.expect("the first subscriber wins");
    turn(&flex, &mut flex_events, "go").await;
    assert_eq!(last(&flex_requests).responses.service_tier.as_deref(), Some("flex"));
}

/// **AC-24.** What the engine reports: the requested tier and its rung before
/// anything was asked, what the backend echoed after a turn — never what it
/// echoed to a title — and nothing at all off a Responses id. L1 names both
/// halves of the request.
#[tokio::test]
async fn the_service_tier_view_reports_the_request_its_rung_and_what_was_served() {
    let (capture, _guard) = LogCapture::install(tracing::Level::DEBUG);
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (provider, requests) = ScriptedProvider::named(
        CHATGPT_ID,
        vec![
            vec![
                ProviderEvent::TextDelta("ok".to_owned()),
                served("default"),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            // What the title request is answered with: an echo that is not the
            // session's to report.
            vec![
                ProviderEvent::TextDelta("A title".to_owned()),
                served("x"),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
        ],
    );
    let engine = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(
        engine.service_tier(),
        Some(TierView {
            requested: "priority".to_owned(),
            source: Source::ChatgptDefault,
            served: None
        })
    );
    assert_eq!(engine.fast_tier(SOL), Some("ultrafast"));

    turn(&engine, &mut events, "hello").await;
    eventually("the title request", || {
        requests.lock().expect("the log").iter().find(|it| is_title(it)).cloned()
    })
    .await;
    eventually("the stored title", || engine.current_session().and_then(|info| info.title)).await;

    assert_eq!(
        engine.service_tier(),
        Some(TierView {
            requested: "priority".to_owned(),
            source: Source::ChatgptDefault,
            served: Some("default".to_owned()),
        }),
        "the step's echo is reported, and the title's is not"
    );
    let logged = capture.logged();
    assert!(
        logged.lines().any(|line| line.contains("service_tier on the request")
            && line.contains(r#"requested="priority""#)
            && line.contains(r#"source="chatgpt default""#)),
        "{logged}"
    );

    let (anthropic, _) = ScriptedProvider::named("anthropic", Vec::new());
    assert_eq!(in_memory(anthropic, "claude-sonnet-4-5", Vec::new()).service_tier(), None);
}

/// **AC-25**, the one-shot half: a JSON schema rides every step of the turns
/// after it was set, and never a title or a compaction, and clearing it clears
/// it.
#[tokio::test]
async fn a_text_format_rides_the_steps_and_never_the_one_shot_requests() {
    let format = json!({"type": "json_schema", "name": "ganja_run", "schema": {"type": "object"}, "strict": true});
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (provider, requests) = ScriptedProvider::named(
        CHATGPT_ID,
        vec![tool_call("gate", json!({})), says("done"), says("A title"), says("## Summary")],
    );
    let (entered, _entering) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    release.notify_one();
    let engine = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(vec![Arc::new(Gate { entered, release })])),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.set_text_format(Some(format.clone()));
    turn(&engine, &mut events, "answer in JSON").await;
    eventually("the title request", || {
        requests.lock().expect("the log").iter().find(|it| is_title(it)).cloned()
    })
    .await;
    engine.send(Command::Compact).await.expect("an idle engine compacts");
    drain(&mut events).await;
    assert!(engine.settle(SETTLE).await);

    let seen = requests.lock().expect("the log").clone();
    let steps: Vec<&ChatRequest> =
        seen.iter().filter(|it| !is_title(it) && !is_summary(it)).collect();
    assert_eq!(steps.len(), 2, "a tool call makes the turn two steps: {seen:?}");
    for step in &steps {
        assert_eq!(step.responses.text_format.as_ref(), Some(&format), "every step carries it");
    }
    let title = seen.iter().find(|it| is_title(it)).expect("titled");
    assert_eq!(
        title.responses,
        Default::default(),
        "a title carries none of the session's options"
    );
    let summary = seen.iter().find(|it| is_summary(it)).expect("compacted");
    assert_eq!(
        summary.responses.text_format, None,
        "a summary is not the answer the schema describes"
    );
    assert_eq!(
        summary.responses.service_tier.as_deref(),
        Some("priority"),
        "but it is billed like a step"
    );

    engine.set_text_format(None);
    turn(&engine, &mut events, "and now in prose").await;
    assert_eq!(last(&requests).responses.text_format, None, "clearing clears it");
}

/// The configuration a child test runs under: a subagent pinned to `gpt-5.5`.
fn helper_agents() -> Config {
    toml::from_str(
        "[agent.helper]\nmode = \"subagent\"\nmodel = \"chatgpt/gpt-5.5\"\n\
         description = \"helps\"\nprompt = \"help\"\n",
    )
    .expect("the agent config decodes")
}

fn delegates() -> Vec<ProviderEvent> {
    tool_call(
        "task",
        json!({"description": "help", "prompt": "help out", "subagent_type": "helper"}),
    )
}

/// **AC-25**, the child half: a subagent resolves its tier for its own model
/// from the session's table and choice, carries no JSON schema, and a table
/// replaced mid-turn reaches the next turn's children and not this one's —
/// nor this turn's own next step (**AC-27b**).
#[tokio::test]
async fn a_child_resolves_its_own_models_tier_from_the_turns_snapshot() {
    let per_model = "[model.\"gpt-5.6-sol\"]\nservice_tier = \"ultrafast\"\n\
                     [model.\"gpt-5.5\"]\nservice_tier = \"default\"\n";
    let replaced = "[model.\"gpt-5.6-sol\"]\nservice_tier = \"priority\"\n\
                    [model.\"gpt-5.5\"]\nservice_tier = \"priority\"\n";
    let (provider, requests) = ScriptedProvider::named(
        CHATGPT_ID,
        vec![
            // Turn one: a child, then a held call during which the table is
            // replaced, then another child, then the sign-off.
            delegates(),
            says("the child's answer"),
            tool_call("gate", json!({})),
            delegates(),
            says("the second child's answer"),
            says("done"),
            // Turn two, under the replaced table and `/fast on`.
            delegates(),
            says("the third child's answer"),
            says("done"),
        ],
    );
    let (entered, mut entering) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let engine = engine_with_gate(provider, SOL, entered, Arc::clone(&release))
        .with_agents(ganja_testkit::agent_registry(&helper_agents()))
        .with_provider_options(tables(CHATGPT_ID, per_model));
    engine.set_text_format(Some(json!({"type": "json_schema"})));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("delegate")).await.expect("an idle engine accepts a prompt");
    let replace = async {
        entering.recv().await.expect("the held call started");
        engine.replace_provider_options(tables(CHATGPT_ID, replaced));
        release.notify_one();
    };
    tokio::join!(replace, drain_allowing(&engine, &mut events));
    assert!(engine.settle(SETTLE).await);

    let first_turn = requests.lock().expect("the log").clone();
    let tier = |request: &ChatRequest| request.responses.service_tier.clone();
    let children: Vec<&ChatRequest> = first_turn.iter().filter(|it| it.model == FIVE).collect();
    let parents: Vec<&ChatRequest> = first_turn.iter().filter(|it| it.model == SOL).collect();
    assert_eq!((children.len(), parents.len()), (2, 4), "{first_turn:?}");
    for child in &children {
        assert_eq!(
            tier(child).as_deref(),
            Some("default"),
            "the child's own model's entry, turn-start table"
        );
        assert_eq!(child.responses.text_format, None, "a child carries no schema");
        assert!(child.responses.custom_tools.is_empty() && child.responses.include.is_empty());
    }
    for parent in &parents {
        assert_eq!(tier(parent).as_deref(), Some("ultrafast"), "the running turn keeps its table");
        assert!(parent.responses.text_format.is_some(), "the parent's steps carry the schema");
    }

    engine.send(Command::SetFast { fast: Some(FastChoice::On) }).await.expect("idle");
    turn(&engine, &mut events, "delegate again").await;
    let second_turn = requests.lock().expect("the log")[first_turn.len()..].to_vec();
    let child = second_turn.iter().find(|it| it.model == FIVE).expect("the third child asked");
    let parent = second_turn.iter().find(|it| it.model == SOL).expect("the parent asked");
    assert_eq!(tier(child).as_deref(), Some("priority"), "`/fast on` for the child's model");
    assert_eq!(tier(parent).as_deref(), Some("ultrafast"), "and for the parent's");

    engine.send(Command::SetFast { fast: None }).await.expect("idle");
    turn(&engine, &mut events, "delegate once more").await;
    assert_eq!(
        last(&requests).responses.service_tier.as_deref(),
        Some("priority"),
        "the next turn reads the replaced table"
    );
}

/// **AC-26.** The marker lands on the stored row the moment it arrives: a
/// stream cut before the call closes still leaves a call marked custom.
#[tokio::test]
async fn a_custom_call_is_stored_as_one_even_when_the_stream_is_cut() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (provider, _) = ScriptedProvider::named(
        CHATGPT_ID,
        vec![vec![
            ProviderEvent::ToolCallStart { id: "c1".to_owned(), name: "bash".to_owned() },
            ProviderEvent::ToolCallCustom { id: "c1".to_owned() },
            ProviderEvent::ToolCallDelta {
                id: "c1".to_owned(),
                json: r#"{"command":"ls"}"#.to_owned(),
            },
            ProviderEvent::Failed(ProviderError::Transport("cut".to_owned())),
        ]],
    );
    let engine = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = turn(&engine, &mut events, "list").await;
    assert!(
        seen.iter().any(|event| matches!(event,
            Event::PartUpdated { part, .. } if matches!(part.body, PartBody::Tool { custom: true, .. }))),
        "the marker is published, so a frontend holds what the transcript holds: {seen:?}"
    );

    let session = engine.current_session().expect("the prompt minted a session");
    let transcript = Storage::open(directory.path().join("storage"))
        .load_transcript(&session.id)
        .expect("the transcript reads");
    let stored: Vec<bool> = transcript
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match &part.body {
            PartBody::Tool { tool, custom, .. } if tool == "bash" => Some(*custom),
            _ => None,
        })
        .collect();
    assert_eq!(stored, [true], "{transcript:?}");
}

/// **AC-27**, the log clause: server-side compaction beside ganja's own is
/// said once per session, not once per turn.
#[tokio::test]
async fn configured_server_compaction_is_mentioned_once_per_session() {
    let (capture, _guard) = LogCapture::install(tracing::Level::INFO);
    let (provider, _) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let engine = in_memory(provider, FIVE, Vec::new()).with_provider_options(tables(
        CHATGPT_ID,
        "context_management = [{ type = \"compaction\", compact_threshold = 1000 }]",
    ));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    turn(&engine, &mut events, "one").await;
    turn(&engine, &mut events, "two").await;

    let said = capture
        .logged()
        .lines()
        .filter(|line| {
            line.contains(
                "server-side compaction is configured; ganja's own compaction still runs off the reported input tokens",
            )
        })
        .count();
    assert_eq!(said, 1, "{}", capture.logged());
}

/// An in-process teammate asks the lead's provider, so it is billed at the tier
/// the lead's config names (**D563**, review note on W3b): a `chatgpt` table
/// saying `service_tier = "default"` reaches the teammate's own request, where
/// a teammate engine given no table would have resolved the fast default
/// nobody configured.
#[tokio::test]
async fn an_in_process_teammate_resolves_its_tier_from_the_leads_table() {
    const TEAMMATE_PROMPT: &str = "the teammate's instructions, quux";

    let home = tempfile::tempdir().expect("a temporary directory");
    let (provider, requests) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let registry = Arc::new(TeammateRegistry::new(
        TeamsRoot::new(home.path().join("teams")),
        TeamName::parse(ganja_testkit::TEAM).expect("a team name"),
        ganja_testkit::LEAD_SESSION_ID,
        home.path(),
    ));
    let lead = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(home.path().join("storage")),
    )
    .with_teammates(Arc::clone(&registry), ganja_testkit::externals())
    // Installed after the team, as `ganja-tui` and `assemble.rs` are free to:
    // the backend reads the table at the spawn, not when it was built.
    .with_provider_options(tables(CHATGPT_ID, "service_tier = \"default\""));
    let mut events = lead.subscribe().await.expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    let asker = RecordedSpawns::default();
    lead.teammates()
        .expect("this session leads a team")
        .start(
            spawn_with_prompt("worker", Some("in-process"), TEAMMATE_PROMPT),
            &caller(home.path()),
            &asker,
        )
        .await
        .expect("an in-process teammate starts on a session that has a store");

    let step = ganja_testkit::eventually(
        Duration::from_secs(20),
        "the teammate to have asked about its task",
        async || {
            requests.lock().expect("the request log is never poisoned").iter().find_map(|request| {
                let about =
                    request.messages.iter().flat_map(|message| &message.parts).any(|part| {
                        part.as_text().is_some_and(|text| text.contains(TEAMMATE_PROMPT))
                    });

                (about && !is_title(request)).then(|| request.clone())
            })
        },
    )
    .await;
    assert_eq!(
        step.responses.service_tier.as_deref(),
        Some("default"),
        "the lead's configured tier, not the seat's fast default for {:?}",
        step.model
    );

    lead.shutdown_teammates().await;
}

/// A new session is a new conversation, so the `/fast` choice made in the one
/// being left does not follow it: the choice clears and the clear is announced,
/// so a frontend's bar stops drawing it (**D563**).
#[tokio::test]
async fn a_new_session_clears_the_fast_choice_and_announces_it() {
    let (provider, _) = ScriptedProvider::named(CHATGPT_ID, Vec::new());
    let engine = in_memory(provider, FIVE, Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(Command::SetFast { fast: Some(FastChoice::On) }).await.expect("idle takes it");
    assert_eq!(engine.fast(), Some(FastChoice::On));
    engine.send(Command::NewSession).await.expect("an idle engine starts a new session");

    let announced = tokio::time::timeout(SETTLE, async {
        let mut seen = Vec::new();
        loop {
            let event = events.next().await.expect("the stream stays open");
            if let Event::FastChanged { fast, .. } = &event
                && seen.iter().any(|it| matches!(it, Event::FastChanged { fast: Some(_), .. }))
            {
                return *fast;
            }
            seen.push(event);
        }
    })
    .await
    .expect("the clear is announced after the choice was");
    assert_eq!(announced, None, "the announcement carries the cleared choice");
    assert_eq!(engine.fast(), None, "the new session has no choice of its own");
    assert_eq!(
        engine.service_tier().map(|view| view.source),
        Some(Source::ChatgptDefault),
        "and resolves as a session nobody chose for"
    );
}

/// What the backend served and whether server-side compaction was mentioned
/// are facts about one conversation (Dv-27): a `NewSession` and a resume each
/// forget both, so `/usage` never pairs the next request with the last
/// conversation's echo, and the next conversation is told about compaction
/// once of its own.
#[tokio::test]
async fn a_new_session_and_a_resume_forget_the_served_echo_and_the_compaction_notice() {
    const NOTICE: &str = "server-side compaction is configured; ganja's own compaction still runs off the reported input tokens";

    let (capture, _guard) = LogCapture::install(tracing::Level::INFO);
    let directory = tempfile::tempdir().expect("a temporary directory");
    // Every request is answered with an echo, so whichever order the detached
    // title requests take, each step is answered with one; a title's echo is
    // never the session's to report.
    let echoing = vec![
        ProviderEvent::TextDelta("ok".to_owned()),
        served("default"),
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let (provider, _) = ScriptedProvider::named(CHATGPT_ID, vec![echoing; 8]);
    let engine = Engine::persistent(
        provider,
        FIVE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_provider_options(tables(
        CHATGPT_ID,
        "context_management = [{ type = \"compaction\", compact_threshold = 1000 }]",
    ));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    let echoed = |engine: &Engine| engine.service_tier().and_then(|view| view.served);
    let noticed = || capture.logged().lines().filter(|line| line.contains(NOTICE)).count();

    turn(&engine, &mut events, "one").await;
    assert_eq!(echoed(&engine).as_deref(), Some("default"), "the first turn's echo is reported");
    assert_eq!(noticed(), 1, "{}", capture.logged());
    let first = engine.current_session().expect("the prompt minted a session").id;

    engine.send(Command::NewSession).await.expect("an idle engine starts a new session");
    assert_eq!(echoed(&engine), None, "a new session forgets the last conversation's echo");
    turn(&engine, &mut events, "two").await;
    assert_eq!(echoed(&engine).as_deref(), Some("default"));
    assert_eq!(noticed(), 2, "the new session is told once of its own: {}", capture.logged());

    engine.resume(&first).await.expect("the first session loads");
    assert_eq!(echoed(&engine), None, "a resume forgets the conversation being left's echo");
    turn(&engine, &mut events, "three").await;
    assert_eq!(noticed(), 3, "the resumed session is told once of its own: {}", capture.logged());
}

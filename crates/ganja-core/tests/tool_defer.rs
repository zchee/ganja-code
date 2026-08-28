//! MCP tool defer-load end to end (**D492**): whole servers' schemas leave
//! the advertised roster past the config's `tool_defer_threshold`, a resident
//! `tool_search` activates them on demand, an activated tool is callable from
//! the next step of the same turn, and a directly-called deferred tool still
//! executes — and activates by executing.
//!
//! Nothing here dials a server. Candidates are computed by grouping the
//! composed registry's own `mcp__<server>__<tool>` names, so the whole suite
//! runs on fake base tools registered under such names — no bun, no upstream
//! checkout — and the real-server path is covered once in `tests/mcp.rs`.
//!
//! Two engines never mint the same message ids, so "equal request bodies" is
//! asserted over what deferral could possibly touch: the tools array exactly
//! (definitions compare whole), and every message's role and text content.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ganja_core::permission::{Action, Permissions, Rule};
use ganja_core::protocol::{
    Command, Event, Message, Part, PartBody, PermissionReply, Role, ToolState,
};
use ganja_core::provider::{ChatRequest, Provider};
use ganja_core::tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput};
use ganja_core::{Config, Engine, SessionId, Storage};
use ganja_testkit::{BlockingTool, RecorderTool, drain, drain_answering, says, tool_call};
use serde_json::json;

/// Fails every invocation, the way a live MCP `isError` result does.
struct FailingMcp;

#[async_trait]
impl Tool for FailingMcp {
    fn id(&self) -> &str {
        "mcp__big__fail"
    }

    fn description(&self) -> &str {
        "fails on purpose, the way a live server's isError answer does"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed("the fake server refused".to_owned()))
    }
}

/// Two fake servers' worth of `mcp__`-named base tools: `big` lends three,
/// `small` lends one. Registration order is this order.
fn server_tools() -> Vec<Arc<dyn Tool>> {
    ["mcp__big__alpha", "mcp__big__beta", "mcp__big__gamma", "mcp__small__solo"]
        .into_iter()
        .map(|name| RecorderTool::new(name, name, "answered").0 as Arc<dyn Tool>)
        .collect()
}

/// Rules that let an `mcp__*` call run unasked, so a suite about deferral is
/// not a suite about dialogs. Criterion 9's own test builds a deny instead.
fn allow_mcp() -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: "mcp__*".to_owned(),
        pattern: "*".to_owned(),
        action: Action::Allow,
    }]);

    permissions
}

fn prompt() -> Command {
    Command::SendPrompt {
        text: "go".to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// The step requests alone: a persistent engine's title request advertises no
/// tools, and nothing in this suite is about titles.
fn steps(requests: &Arc<Mutex<Vec<ChatRequest>>>) -> Vec<ChatRequest> {
    requests
        .lock()
        .expect("the request log is never poisoned")
        .iter()
        .filter(|request| !request.tools.is_empty())
        .cloned()
        .collect()
}

fn tool_names(request: &ChatRequest) -> Vec<String> {
    request.tools.iter().map(|definition| definition.name.clone()).collect()
}

/// The `<deferred_tools>` block on the request's last user message, if one
/// rode along.
fn listing_text(request: &ChatRequest) -> Option<String> {
    request.messages.iter().rev().find(|message| message.role == Role::User).and_then(|message| {
        message.parts.iter().find_map(|part| match &part.body {
            PartBody::Text { text } if text.starts_with("<deferred_tools>") => Some(text.clone()),
            _ => None,
        })
    })
}

/// What deferral could possibly touch, per request: the tools array whole,
/// and every message's role and text content.
fn comparable(
    request: &ChatRequest,
) -> (Vec<ganja_core::tool::ToolDefinition>, Vec<(Role, Vec<String>)>) {
    let messages = request
        .messages
        .iter()
        .map(|message| {
            let texts = message
                .parts
                .iter()
                .filter_map(|part| match &part.body {
                    PartBody::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();

            (message.role, texts)
        })
        .collect();

    (request.tools.clone(), messages)
}

/// The final state of the call named `tool`, read off the event stream.
fn final_state(seen: &[Event], tool: &str) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { tool: name, state, .. } if name == tool => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("the turn never resolved a call to {tool}"))
}

/// One scripted turn on an ephemeral engine, answering with the request log.
async fn run_turn_at(
    threshold: usize,
    scripts: Vec<Vec<ganja_core::provider::ProviderEvent>>,
) -> (Vec<ChatRequest>, Vec<Event>) {
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(scripts);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(server_tools())),
        allow_mcp(),
    )
    .with_defer_threshold(threshold);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    (steps(&requests), seen)
}

/// Criteria 1 and 2: at or below the threshold nothing changes — the default
/// engine and one at a huge threshold build the same requests, no
/// `tool_search` joins the roster, every schema is advertised, and no listing
/// rides the messages.
#[tokio::test]
async fn below_the_threshold_the_requests_are_what_they_always_were() {
    let (default_steps, _) = run_turn_at(32, vec![says("hello")]).await;
    let (huge_steps, _) = run_turn_at(usize::MAX, vec![says("hello")]).await;

    assert_eq!(default_steps.len(), 1);
    assert_eq!(huge_steps.len(), 1);
    assert_eq!(
        comparable(&default_steps[0]),
        comparable(&huge_steps[0]),
        "four mcp tools fit both budgets, so the two engines ask identically"
    );

    let names = tool_names(&default_steps[0]);
    assert!(!names.contains(&"tool_search".to_owned()));
    assert_eq!(
        names,
        ["mcp__big__alpha", "mcp__big__beta", "mcp__big__gamma", "mcp__small__solo"],
        "every schema is advertised, in registration order"
    );
    assert!(listing_text(&default_steps[0]).is_none());
}

/// Criterion 3: above the threshold the first request omits exactly the
/// deferred servers' entries (order of the rest preserved), carries
/// `tool_search`, and the last user message names every deferred tool —
/// while the smaller server stays fully advertised.
#[tokio::test]
async fn above_the_threshold_whole_servers_defer_largest_first() {
    let (steps, _) = run_turn_at(1, vec![says("hello")]).await;

    let names = tool_names(&steps[0]);
    assert_eq!(
        names,
        ["mcp__small__solo", "tool_search"],
        "big defers whole, small stays, order of the rest preserved, the door is resident"
    );

    let listing = listing_text(&steps[0]).expect("something is deferred, so the listing rides");
    for name in ["mcp__big__alpha", "mcp__big__beta", "mcp__big__gamma"] {
        assert!(listing.contains(name), "{name} is named: {listing}");
    }
    assert!(
        !listing.contains("mcp__small__solo"),
        "an advertised tool is not listed as deferred: {listing}"
    );
}

/// Criterion 4: a `tool_search` hit is advertised on the very next step, and
/// a batch `select:` of two names activates both in the one call — with no
/// permission dialog anywhere, because the door runs unasked.
#[tokio::test]
async fn a_search_hit_is_advertised_on_the_very_next_step() {
    let (steps, seen) = run_turn_at(
        0,
        vec![
            tool_call("tool_search", json!({ "query": "select:mcp__big__alpha, mcp__big__beta" })),
            says("done"),
        ],
    )
    .await;

    assert_eq!(tool_names(&steps[0]), ["tool_search"], "a threshold of zero defers every server");

    let next = tool_names(&steps[1]);
    for name in ["mcp__big__alpha", "mcp__big__beta"] {
        assert!(next.contains(&name.to_owned()), "{name} rides step two: {next:?}");
    }
    assert!(!next.contains(&"mcp__big__gamma".to_owned()));
    assert!(!next.contains(&"mcp__small__solo".to_owned()));

    let listing = listing_text(&steps[1]).expect("two servers still have deferred tools");
    assert!(!listing.contains("mcp__big__alpha"), "activated entries drop out");
    assert!(listing.contains("mcp__big__gamma"));
    assert!(listing.contains("mcp__small__solo"));

    assert!(
        !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "tool_search runs unasked"
    );
    if let ToolState::Completed { output, .. } = final_state(&seen, "tool_search") {
        assert!(output.contains("## mcp__big__alpha"), "{output}");
        assert!(output.contains("## mcp__big__beta"), "{output}");
    } else {
        panic!("the search call completed");
    }
}

/// Criterion 7, the succeeding half: a deferred, never-activated tool called
/// directly executes through the complete registry, and the next step's
/// request advertises its schema.
#[tokio::test]
async fn a_direct_call_to_a_deferred_tool_executes_and_activates() {
    let (steps, seen) =
        run_turn_at(0, vec![tool_call("mcp__big__alpha", json!({})), says("done")]).await;

    assert_eq!(tool_names(&steps[0]), ["tool_search"]);
    assert!(
        matches!(final_state(&seen, "mcp__big__alpha"), ToolState::Completed { .. }),
        "the call resolved in the complete registry and ran"
    );
    assert!(
        tool_names(&steps[1]).contains(&"mcp__big__alpha".to_owned()),
        "executing activated it: {:?}",
        tool_names(&steps[1])
    );
}

/// Criterion 7, the failing half: a failed call activates exactly the same
/// way, because a failure is the moment the model most needs the schema.
#[tokio::test]
async fn a_failed_direct_call_activates_all_the_same() {
    let mut tools = server_tools();
    tools.push(Arc::new(FailingMcp));
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("mcp__big__fail", json!({})),
        says("done"),
    ]);
    let engine =
        Engine::new(provider, "scripted-model", Arc::new(Registry::new(tools)), allow_mcp())
            .with_defer_threshold(0);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    assert!(
        matches!(
            final_state(&seen, "mcp__big__fail"),
            ToolState::Error { ref error, .. } if error.contains("the fake server refused")
        ),
        "the failure is what the model reads next"
    );
    let steps = steps(&requests);
    assert!(
        tool_names(&steps[1]).contains(&"mcp__big__fail".to_owned()),
        "a failed call advertises the schema too: {:?}",
        tool_names(&steps[1])
    );
}

/// Criterion 5's integration half: an exact name that matches nothing is
/// answered with near-misses, and the turn carries on.
#[tokio::test]
async fn a_failed_select_answers_with_near_misses_and_the_turn_continues() {
    let (_, seen) = run_turn_at(
        0,
        vec![tool_call("tool_search", json!({ "query": "select:mcp__big__alphaa" })), says("done")],
    )
    .await;

    if let ToolState::Completed { output, .. } = final_state(&seen, "tool_search") {
        assert!(output.contains("No deferred tool is named `mcp__big__alphaa`"), "{output}");
        assert!(output.contains("mcp__big__alpha"), "the near-misses name the neighbours");
    } else {
        panic!("a miss is information, never a failed call");
    }
}

/// Criterion 9: a stored rule for one `mcp__` name gates the call
/// identically whether the tool is deferred or advertised from the start —
/// same refusal text, no dialog either way, and a refused call never
/// activates.
#[tokio::test]
async fn a_permission_rule_gates_a_deferred_call_exactly_as_an_advertised_one() {
    let denied = |threshold: usize| async move {
        let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
            tool_call("mcp__big__alpha", json!({})),
            says("done"),
        ]);
        let mut permissions = Permissions::default();
        permissions.set_baseline(vec![Rule {
            permission: "mcp__big__alpha".to_owned(),
            pattern: "*".to_owned(),
            action: Action::Deny,
        }]);
        let engine = Engine::new(
            provider,
            "scripted-model",
            Arc::new(Registry::new(server_tools())),
            permissions,
        )
        .with_defer_threshold(threshold);
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine.send(prompt()).await.expect("an idle engine accepts");
        let seen = drain(&mut events).await;

        assert!(
            !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
            "a deny rule refuses without a dialog"
        );
        let ToolState::Error { error, .. } = final_state(&seen, "mcp__big__alpha") else {
            panic!("the denied call ends as an error the model reads");
        };
        let advertised_after =
            tool_names(&steps(&requests)[1]).contains(&"mcp__big__alpha".to_owned());

        (error, advertised_after)
    };

    let (deferred_text, deferred_activated) = denied(0).await;
    let (advertised_text, _) = denied(usize::MAX).await;

    assert_eq!(deferred_text, advertised_text, "the rule speaks the same key either way");
    assert!(!deferred_activated, "a refused call never executed, so it never activated");
}

/// Criterion 10's constructible half: deferral filters what is *advertised*
/// and touches neither the registry nor the `/mcp`-facing counts — the
/// definitions snapshot `tool_search` answers from still holds every
/// registered name, and `mcp_tool_counts` is byte-identical between a
/// deferring engine and one that defers nothing. The live `Servers` half
/// rides `tests/mcp.rs`.
#[tokio::test]
async fn the_registry_and_the_mcp_counts_stay_whole_under_deferral() {
    let (provider, _) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("tool_search", json!({ "query": "select:read" })),
        says("done"),
    ]);
    let mut tools = server_tools();
    tools.push(Arc::new(ganja_core::tool::read::ReadTool));
    let engine =
        Engine::new(provider, "scripted-model", Arc::new(Registry::new(tools)), allow_mcp())
            .with_defer_threshold(0);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    if let ToolState::Completed { output, .. } = final_state(&seen, "tool_search") {
        assert!(
            output.contains("`read` is already advertised"),
            "the snapshot holds the whole roster, advertised names included: {output}"
        );
    } else {
        panic!("the search call completed");
    }

    let (bare_provider, _) = ganja_testkit::ScriptedProvider::new(Vec::new());
    let undeferring = Engine::new(
        bare_provider,
        "scripted-model",
        Arc::new(Registry::new(server_tools())),
        allow_mcp(),
    );
    assert_eq!(
        engine.mcp_tool_counts(),
        undeferring.mcp_tool_counts(),
        "deferral never reaches the Servers-facing counts"
    );
}

/// Criterion 11: the listing drops a tool the step after its activation, and
/// the unavailable-tool refusal names only the advertised subset.
#[tokio::test]
async fn the_listing_shrinks_and_the_refusal_names_only_the_advertised() {
    let (steps, seen) = run_turn_at(
        0,
        vec![tool_call("mcp__big__alpha", json!({})), tool_call("nope", json!({})), says("done")],
    )
    .await;

    let listing = listing_text(&steps[1]).expect("plenty is still deferred");
    assert!(!listing.contains("mcp__big__alpha"), "activated by call, dropped: {listing}");
    assert!(listing.contains("mcp__big__beta"));

    let ToolState::Error { error, .. } = final_state(&seen, "nope") else {
        panic!("an unknown tool ends as an error the model reads");
    };
    assert!(error.contains("Available tools:"), "{error}");
    assert!(
        error.contains("mcp__big__alpha") && error.contains("tool_search"),
        "the activated tool and the door are advertised: {error}"
    );
    assert!(
        !error.contains("mcp__big__beta") && !error.contains("mcp__small__solo"),
        "a deferred name the request never offered is not quoted: {error}"
    );
}

/// A persistent engine over `storage`, deferring everything.
fn persistent(provider: Arc<dyn Provider>, storage: Storage) -> Engine {
    Engine::persistent(
        provider,
        "scripted-model",
        Arc::new(Registry::new(server_tools())),
        allow_mcp(),
        storage,
    )
    .with_defer_threshold(0)
}

fn store() -> (tempfile::TempDir, Storage) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));

    (directory, storage)
}

/// Criterion 8, drills (a) and (c): a direct-call activation lands on the
/// session row, survives a cross-process resume — and when the field is
/// cleared with the transcript intact (the crash-equivalent state), the
/// resume union alone recovers it.
#[tokio::test]
async fn an_activation_survives_a_resume_and_the_union_recovers_a_cleared_row() {
    let (_directory, storage) = store();

    let (provider, _) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("mcp__big__alpha", json!({})),
        says("done"),
    ]);
    let engine = persistent(provider, storage.clone());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    let id = engine.current_session().expect("the first prompt creates a session").id;
    let row = storage.load_info(&id).expect("the row loads").expect("it exists");
    assert!(
        row.activated_tools.contains("mcp__big__alpha"),
        "the executed-call activation was flushed: {:?}",
        row.activated_tools
    );

    // Drill (a): a clean cross-process resume re-advertises the name.
    let resumed_steps = resume_and_prompt(&storage, &id).await;
    assert!(
        tool_names(&resumed_steps[0]).contains(&"mcp__big__alpha".to_owned()),
        "the persisted set seeds the resumed roster: {:?}",
        tool_names(&resumed_steps[0])
    );

    // Drill (c): clear the field through the public door, transcript intact —
    // the crash-equivalent state — and the transcript union alone recovers it.
    let mut cleared = storage.load_info(&id).expect("the row loads").expect("it exists");
    cleared.activated_tools = BTreeSet::new();
    storage.save_info(&cleared).expect("the cleared row writes");

    let recovered_steps = resume_and_prompt(&storage, &id).await;
    assert!(
        tool_names(&recovered_steps[0]).contains(&"mcp__big__alpha".to_owned()),
        "the union over the stored transcript's mcp__* calls recovers the activation: {:?}",
        tool_names(&recovered_steps[0])
    );
}

/// A fresh engine over the same store: resume `id`, take one scripted turn,
/// answer with its step requests.
async fn resume_and_prompt(storage: &Storage, id: &SessionId) -> Vec<ChatRequest> {
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![says("again")]);
    let engine = persistent(provider, storage.clone());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.resume(id).await.expect("the session resumes");
    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    steps(&requests)
}

/// Criterion 8, drill (b): a pre-feature row — the field absent by
/// construction, since an empty set writes no key — whose transcript carries
/// an `mcp__*` call is seeded from the transcript alone.
#[tokio::test]
async fn a_pre_feature_row_is_seeded_from_its_transcript() {
    let (_directory, storage) = store();

    let id = SessionId::ascending();
    storage
        .save_info(&ganja_testkit::seeded_session_info(id.clone(), 0))
        .expect("the seeded record writes");
    ganja_testkit::seed_message(&storage, &id, &Message::user("earlier work"));
    let mut reply = Message::assistant("scripted-model");
    reply.parts.push(Part::tool("call_0", "mcp__big__beta"));
    ganja_testkit::seed_message(&storage, &id, &reply);

    let steps = resume_and_prompt(&storage, &id).await;
    let names = tool_names(&steps[0]);
    assert!(
        names.contains(&"mcp__big__beta".to_owned()),
        "the transcript's call seeds the set: {names:?}"
    );
    assert!(!names.contains(&"mcp__big__alpha".to_owned()), "nothing else was touched: {names:?}");
}

/// Criterion 8, drill (d): once the ScriptedProvider log holds the step
/// *after* the search — strictly after the search call's `finish` returned —
/// the row already carries the name. The turn is still mid-flight (held open
/// by a blocking call, then cancelled), so no tail write can have run: the
/// mid-turn flush alone put it there. `save_info` waits on the writer
/// thread's answer, which is what keeps the read unracy.
#[tokio::test]
async fn a_search_activation_is_on_the_row_before_the_turn_ends() {
    let (_directory, storage) = store();
    let (entered_tx, mut entered) = tokio::sync::mpsc::channel(1);

    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("tool_search", json!({ "query": "select:mcp__big__alpha" })),
        tool_call("hold", json!({})),
    ]);
    let mut tools = server_tools();
    tools.push(BlockingTool::with_entry_signal("hold", "blocks until cancelled", entered_tx));
    let engine = Arc::new(
        Engine::persistent(
            provider,
            "scripted-model",
            Arc::new(Registry::new(tools)),
            allow_mcp(),
            storage.clone(),
        )
        .with_defer_threshold(0),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt()).await.expect("an idle engine accepts");

    let watcher = tokio::spawn({
        let engine = Arc::clone(&engine);
        let storage = storage.clone();
        async move {
            entered.recv().await.expect("the hold tool runs");
            assert!(
                steps(&requests).len() >= 2,
                "the next step's request was built before its call ran"
            );
            let id = engine.current_session().expect("the first prompt creates a session").id;
            let carried = storage
                .load_info(&id)
                .expect("the row loads")
                .expect("it exists")
                .activated_tools
                .contains("mcp__big__alpha");
            engine.send(Command::CancelTurn).await.expect("a cancel lands");

            carried
        }
    });

    drain(&mut events).await;
    assert!(
        watcher.await.expect("the watcher finishes"),
        "the flush at the search call's own finish made the activation durable mid-turn"
    );
}

/// The tail of criterion 8: `NewSession` resets the set, and the next
/// conversation defers the name again.
#[tokio::test]
async fn a_new_session_starts_with_nothing_activated() {
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("mcp__big__alpha", json!({})),
        says("done"),
        says("fresh"),
    ]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(server_tools())),
        allow_mcp(),
    )
    .with_defer_threshold(0);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;
    assert!(
        tool_names(&steps(&requests)[1]).contains(&"mcp__big__alpha".to_owned()),
        "the first conversation activated it"
    );

    engine.send(Command::NewSession).await.expect("a clear lands");
    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    let fresh = steps(&requests);
    let names = tool_names(fresh.last().expect("the second conversation asked"));
    assert!(
        !names.contains(&"mcp__big__alpha".to_owned()),
        "the next conversation defers it again: {names:?}"
    );
    assert!(
        listing_text(fresh.last().expect("the second conversation asked"))
            .expect("the listing is back")
            .contains("mcp__big__alpha")
    );
}

/// Criterion 13: a subagent reads the same advertised subset (the resident
/// door included), its direct-call activation is visible to the parent's
/// next step in memory, and it reaches the root row at the parent's
/// `task`-call finish — asserted by `load_info` while the parent is still
/// mid-turn, before any tail write could run.
#[tokio::test]
async fn a_childs_activation_reaches_the_parent_and_the_root_row_at_fan_in() {
    let (_directory, storage) = store();
    let (entered_tx, mut entered) = tokio::sync::mpsc::channel(1);

    let config: Config =
        serde_json::from_str(r#"{"agent": {"general": {"permission": {"mcp__*": "allow"}}}}"#)
            .expect("the fixture config parses");
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call(
            "task",
            json!({
                "description": "touch the deferred tool",
                "prompt": "call mcp__big__alpha",
                "subagent_type": "general",
            }),
        ),
        tool_call("mcp__big__alpha", json!({})),
        says("child done"),
        tool_call("hold", json!({})),
    ]);
    let mut tools = server_tools();
    tools.push(BlockingTool::with_entry_signal("hold", "blocks until cancelled", entered_tx));
    let engine = Arc::new(
        Engine::persistent(
            provider,
            "scripted-model",
            Arc::new(Registry::new(tools)),
            allow_mcp(),
            storage.clone(),
        )
        .with_agents(ganja_testkit::agent_registry(&config))
        .with_defer_threshold(0),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt()).await.expect("an idle engine accepts");

    let requests_for_watch = Arc::clone(&requests);
    let watcher = tokio::spawn({
        let engine = Arc::clone(&engine);
        let storage = storage.clone();
        async move {
            entered.recv().await.expect("the hold tool runs after fan-in");
            let id = engine.current_session().expect("the first prompt creates a session").id;
            let carried = storage
                .load_info(&id)
                .expect("the row loads")
                .expect("it exists")
                .activated_tools
                .contains("mcp__big__alpha");
            engine.send(Command::CancelTurn).await.expect("a cancel lands");

            (carried, steps(&requests_for_watch))
        }
    });

    drain_answering(&engine, &mut events, PermissionReply::Once).await;
    let (carried, mid_turn_steps) = watcher.await.expect("the watcher finishes");

    assert!(
        carried,
        "the parent's task-call finish is the fan-in flush: the root row carries \
         the child's activation before the turn ends"
    );

    let is_parent = |request: &ChatRequest| tool_names(request).contains(&"task".to_owned());
    let child_first =
        mid_turn_steps.iter().find(|request| !is_parent(request)).expect("the child asked");
    let child_names = tool_names(child_first);
    assert!(
        child_names.contains(&"tool_search".to_owned()),
        "the child holds the same resident door: {child_names:?}"
    );
    assert!(
        !child_names.iter().any(|name| name.starts_with("mcp__")),
        "the child reads the same advertised subset: {child_names:?}"
    );
    assert!(
        !child_names.contains(&"task".to_owned()),
        "the depth guard is untouched: {child_names:?}"
    );

    let parent_after = mid_turn_steps
        .iter()
        .filter(|request| is_parent(request))
        .nth(1)
        .expect("the parent asked again after the task call");
    assert!(
        tool_names(parent_after).contains(&"mcp__big__alpha".to_owned()),
        "the child's activation is visible to the parent's next step in memory: {:?}",
        tool_names(parent_after)
    );
}

/// Criteria 14 and 15: a `replace_base_tools` recomposition — the `/plugin`
/// Reload seam, and the recompute shape a reconnect shares — keeps
/// `tool_search` and the candidates alive, never defers an activated name,
/// and excludes activated names from the threshold arithmetic; a recomposed
/// set missing a server drops that server's tools from the listing entirely.
#[tokio::test]
async fn a_recompute_survives_reload_and_never_defers_an_activated_name() {
    // Threshold 2 over big's three tools: everything defers. Activating one
    // leaves two never-touched names, which fit the budget — so the
    // recompute defers nothing at all.
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        tool_call("mcp__big__alpha", json!({})),
        says("done"),
        says("after reload"),
        says("after shrink"),
    ]);
    let big_only = || -> Vec<Arc<dyn Tool>> {
        ["mcp__big__alpha", "mcp__big__beta", "mcp__big__gamma"]
            .into_iter()
            .map(|name| RecorderTool::new(name, name, "answered").0 as Arc<dyn Tool>)
            .collect()
    };
    let engine =
        Engine::new(provider, "scripted-model", Arc::new(Registry::new(big_only())), allow_mcp())
            .with_defer_threshold(2);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;
    assert_eq!(
        tool_names(&steps(&requests)[0]),
        ["tool_search"],
        "three names over a budget of two defer whole"
    );

    engine.replace_base_tools(Arc::new(Registry::new(big_only())));
    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    let after_reload = steps(&requests);
    let names = tool_names(after_reload.last().expect("the reloaded engine asked"));
    assert!(
        names.contains(&"mcp__big__alpha".to_owned()),
        "an activated name is never deferred by a recompute: {names:?}"
    );
    assert!(
        names.contains(&"mcp__big__beta".to_owned())
            && names.contains(&"mcp__big__gamma".to_owned()),
        "the activated name is exempt from the arithmetic, so the two never-touched \
         names fit the budget and nothing defers: {names:?}"
    );
    assert!(
        !names.contains(&"tool_search".to_owned()),
        "nothing defers, so the door leaves the roster: {names:?}"
    );

    // The reaped-server analog: a recomposed set missing `big` entirely
    // drops its tools from roster and listing both — only what exists is
    // ever named. A fresh single-tool server under a budget of zero shows
    // the listing again, naming exactly what survived.
    let solo: Vec<Arc<dyn Tool>> = vec![
        RecorderTool::new("mcp__small__solo", "mcp__small__solo", "answered").0 as Arc<dyn Tool>,
    ];
    let engine = engine.with_defer_threshold(0);
    engine.replace_base_tools(Arc::new(Registry::new(solo)));
    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    let after_shrink = steps(&requests);
    let last = after_shrink.last().expect("the shrunken engine asked");
    let listing = listing_text(last).expect("the solo server defers under a budget of zero");
    assert!(listing.contains("mcp__small__solo"), "{listing}");
    assert!(
        !listing.contains("mcp__big__beta"),
        "a reaped server's tools leave the listing entirely: {listing}"
    );
}

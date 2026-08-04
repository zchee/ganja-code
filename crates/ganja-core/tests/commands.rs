//! Slash commands, compaction on demand, and starting over.
//!
//! A command is a template that becomes a prompt: nothing about `/init` is
//! special-cased in code, so what proves it works is a scripted loop that
//! actually writes `AGENTS.md` with the ordinary `write` tool.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Command, Config, Engine, EngineError, Event, FinishReason, PermissionReply, Permissions,
    Registry, Role, SessionId, SessionInfo, Storage, Usage, command,
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
    storage,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Answers each request with the next script, and records what it was asked.
struct Recorder {
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Recorder {
    fn new(scripts: Vec<Vec<ProviderEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        (
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }
}

#[async_trait]
impl Provider for Recorder {
    fn id(&self) -> &str {
        "recorder"
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.seen
            .lock()
            .expect("the request log is never poisoned")
            .push(request);

        let script = self
            .scripts
            .lock()
            .expect("the scripts are never poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ProviderEvent::Finish(FinishReason::Completed)]);

        Ok(stream::iter(script).boxed())
    }
}

/// A step that calls `tool` with `args` and stops.
fn calls(tool: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: "call".to_owned(),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: "call".to_owned(),
            json: args.to_string(),
        },
        ProviderEvent::ToolCallEnd {
            id: "call".to_owned(),
        },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// A step that says `text` and stops.
fn says(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// Drains until the turn finishes, answering every permission with `Once`.
async fn drain_allowing(engine: &Engine, events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let Some(event) = events.next().await else {
            return seen;
        };
        if let Event::PermissionRequested { id, .. } = &event {
            engine
                .send(Command::ReplyPermission {
                    id: id.clone(),
                    reply: PermissionReply::Once,
                })
                .await
                .expect("a reply is never refused");
        }
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// What the user message of `request` said.
fn prompt_of(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::Part::as_text)
        .collect()
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// `/init` is a template and nothing else: it reaches the model as an ordinary
/// prompt, and `AGENTS.md` appears because the model reached for `write`.
///
/// The path is absolute so the fixture writes into a temporary directory rather
/// than into whatever checkout the suite is running in.
#[tokio::test]
async fn the_init_command_produces_an_agents_file_through_the_ordinary_loop() {
    let workspace = temporary();
    let target = workspace.path().join("AGENTS.md");
    let (provider, requests) = Recorder::new(vec![
        calls(
            "write",
            json!({
                "filePath": target.to_string_lossy(),
                "content": "# ganja-code\n\nA terminal-first agent.\n",
            }),
        ),
        says("written"),
    ]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "init".to_owned(),
            args: String::new(),
        })
        .await
        .expect("init is builtin");
    let seen = drain_allowing(&engine, &mut events).await;

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Completed);

    assert_eq!(
        std::fs::read_to_string(&target).expect("the model wrote AGENTS.md"),
        "# ganja-code\n\nA terminal-first agent.\n"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = prompt_of(&requests[0]);
    assert!(
        sent.starts_with("Create or update `AGENTS.md` for this repository."),
        "the user message carries upstream's template verbatim: {sent}"
    );
    assert!(
        !sent.contains("${path}") && !sent.contains("$ARGUMENTS"),
        "with its placeholders already filled: {sent}"
    );
}

/// What the user typed after the name reaches the template where the template
/// asked for it.
#[tokio::test]
async fn a_commands_arguments_reach_the_prompt_it_sends() {
    let (provider, requests) = Recorder::new(vec![says("noted")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "init".to_owned(),
            args: "focus on the test suite".to_owned(),
        })
        .await
        .expect("init is builtin");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert!(
        prompt_of(&requests[0]).contains("focus on the test suite"),
        "the template's $ARGUMENTS slot is where they land"
    );
}

#[tokio::test]
async fn a_command_nothing_answers_to_says_what_would_have_worked() {
    let (provider, _) = Recorder::new(Vec::new());
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );

    let refused = engine
        .send(Command::RunCommand {
            name: "nope".to_owned(),
            args: String::new(),
        })
        .await
        .expect_err("no command answers to that");

    assert!(matches!(refused, EngineError::UnknownCommand { .. }));
    let said = refused.to_string();
    assert!(said.contains("/nope"), "it names what was asked: {said}");
    assert!(said.contains("init"), "and what there is: {said}");
}

#[tokio::test]
async fn a_configured_command_runs_like_a_builtin() {
    let config: Config = serde_json::from_value(json!({
        "command": {
            "review": {
                "template": "review the diff, focusing on $1",
                "description": "review the diff",
            }
        }
    }))
    .expect("the fixture is a config");
    let (provider, requests) = Recorder::new(vec![says("looks fine")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_commands(Arc::new(command::Registry::build(
        &config,
        std::path::Path::new("/repo"),
    )));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(
        engine.commands().names(),
        vec!["init".to_owned(), "review".to_owned()],
        "a config command joins the roster the builtins are in"
    );

    engine
        .send(Command::RunCommand {
            name: "review".to_owned(),
            args: "the shell tool".to_owned(),
        })
        .await
        .expect("the configured command runs");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(
        prompt_of(&requests[0]),
        "review the diff, focusing on the shell tool"
    );
}

/// A command that names an agent runs as it — its prompt and its rules — for
/// that one turn, without the session becoming it.
#[tokio::test]
async fn a_command_that_names_an_agent_runs_as_it_for_one_turn() {
    let config: Config = serde_json::from_value(json!({
        "agent": { "reviewer": { "prompt": "you review and nothing else", "mode": "primary" } },
        "command": { "review": { "template": "review it", "agent": "reviewer" } }
    }))
    .expect("the fixture is a config");
    let (provider, requests) = Recorder::new(vec![says("looks fine"), says("hello")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(Arc::new(
        ganja_core::AgentRegistry::build(&config).expect("the fixture resolves an agent"),
    ))
    .with_commands(Arc::new(command::Registry::build(
        &config,
        std::path::Path::new("/repo"),
    )));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "review".to_owned(),
            args: String::new(),
        })
        .await
        .expect("the configured command runs");
    drain_allowing(&engine, &mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(
        requests[0].system.as_deref(),
        Some("you review and nothing else"),
        "the command's turn ran as the agent it named"
    );
    assert_eq!(
        engine.agent().as_deref(),
        Some("build"),
        "and the session is still what it was"
    );
    assert!(
        requests[1].system.is_none(),
        "so the next turn is not the reviewer: {:?}",
        requests[1].system
    );
}

/// A stored session with one message in it, already titled so the title
/// machinery stays out of a test that is not about it.
fn seeded(storage: &Storage) -> SessionId {
    let id = SessionId::ascending();
    let info = SessionInfo {
        id: id.clone(),
        version: storage::VERSION,
        title: Some("seeded".to_owned()),
        created: 1,
        updated: 2,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        parent: None,
    };
    storage.save_info(&info).expect("the seeded record writes");

    let earlier = ganja_core::Message::user("the objective");
    storage
        .save_message(&id, &earlier)
        .expect("the seeded envelope writes");
    for part in &earlier.parts {
        storage
            .save_part(&id, &earlier.id, part)
            .expect("the seeded part writes");
    }

    id
}

/// The manual compaction takes the same path the automatic one does, with the
/// fill-level question skipped — and the window afterwards is the summary.
#[tokio::test]
async fn compacting_on_demand_summarizes_a_session_that_is_nowhere_near_full() {
    let directory = temporary();
    let storage = Storage::open(directory.path().join("storage"));
    let session = seeded(&storage);
    let model = ganja_core::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");

    let (provider, requests) = Recorder::new(vec![says("## Objective\n- find the thing")]);
    let engine = Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine
        .send(Command::Compact)
        .await
        .expect("an idle engine accepts a compaction");
    let seen = drain_allowing(&engine, &mut events).await;

    {
        let requests = requests.lock().expect("the request log is never poisoned");
        assert_eq!(requests.len(), 1, "a compaction asks once: to summarize");
        assert!(
            requests[0].tools.is_empty(),
            "and the summarize request is the toolless one"
        );
        assert!(
            prompt_of(&requests[0]).contains("[User]: the objective"),
            "with the conversation serialized into it"
        );
    }

    let summary = seen
        .iter()
        .find_map(|event| match event {
            Event::MessageStarted { message } if message.role == Role::Assistant => {
                Some(message.clone())
            }
            _ => None,
        })
        .expect("the summary enters the transcript");
    assert_eq!(
        summary.parts.first().and_then(ganja_core::Part::as_text),
        Some("## Objective\n- find the thing")
    );

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a compacting turn still ends with a terminal event");
    };
    assert_eq!(*reason, FinishReason::Completed);
    assert_eq!(
        engine
            .current_session()
            .and_then(|info| info.summary)
            .as_ref(),
        Some(&summary.id),
        "the window now opens at the summary"
    );

    // And the next turn is held inside it.
    engine
        .send(Command::SendPrompt {
            text: "carry on".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the model was asked again");
    assert_eq!(
        next.messages.len(),
        2,
        "the summary and the new prompt, and nothing before them: {:?}",
        next.messages
    );
}

#[tokio::test]
async fn starting_a_new_session_leaves_the_old_one_on_disk_and_the_next_prompt_fresh() {
    let directory = temporary();
    let storage = Storage::open(directory.path().join("storage"));
    let (provider, _) = Recorder::new(vec![says("one"), says("two")]);
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;
    let first = engine
        .current_session()
        .expect("the first prompt minted a session")
        .id;

    engine
        .send(Command::NewSession)
        .await
        .expect("an idle engine accepts a reset");
    assert!(
        engine.current_session().is_none(),
        "the engine is between sessions"
    );

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;
    let second = engine
        .current_session()
        .expect("the next prompt minted another")
        .id;

    assert_ne!(first, second, "a fresh session, not the old one reopened");
    let stored: Vec<SessionId> = engine
        .sessions()
        .await
        .expect("the store lists")
        .into_iter()
        .map(|info| info.id)
        .collect();
    assert!(
        stored.contains(&first) && stored.contains(&second),
        "and the old one is still there to resume: {stored:?}"
    );
}

//! Slash commands, compaction on demand, and starting over.
//!
//! A command is a template that becomes a prompt: nothing about `/init` is
//! special-cased in code, so what proves it works is a scripted loop that
//! actually writes `AGENTS.md` with the ordinary `write` tool.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use ganja_core::config::CONFIG_HOME_ENV;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, FinishReason, Message, PartBody, Role, ToolState};
use ganja_core::provider::ChatRequest;
use ganja_core::tool::Registry;
use ganja_core::{Config, Engine, EngineError, SessionId, Storage, command};
use ganja_testkit::{ScriptedProvider, drain_allowing, says, tool_call};
use serde_json::json;

/// [`command::Registry::build`] against a config home this binary owns.
///
/// The global command tier (**D481**) is `<config home>/commands`, resolved
/// through [`CONFIG_HOME_ENV`] on every build — so a bare `Registry::build`
/// here reads whatever `*.md` files the developer running the suite keeps in
/// their own home, and the roster assertions below would be describing that
/// machine rather than the fixture. The suite was green only while nobody had
/// such a directory (`ganja-code-qh1`).
///
/// The home named is a path this binary never creates: [`config_home`] returns
/// the variable as written, and `commands/` under a directory that is not there
/// is exactly the empty tier these tests want — with nothing left behind to
/// clean up. The `/repo` worktree empties the project tier the same way.
///
/// The redirect is forced from a `LazyLock` rather than written into each test
/// because this binary's tests share one process and run on parallel threads
/// under a plain `cargo test`: routing every build through here means the one
/// `set_var` happens before the first read of that variable, with any other
/// builder parked on the lock while it does.
///
/// [`config_home`]: ganja_core::config::config_home
fn command_registry(config: &Config) -> command::Registry {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let home =
            std::env::temp_dir().join(format!("ganja-no-global-commands-{}", std::process::id()));
        // SAFETY: this binary's only write to the environment, run exactly
        // once, under the lock every reader of that variable here goes
        // through.
        unsafe { std::env::set_var(CONFIG_HOME_ENV, &home) };
        home
    });
    LazyLock::force(&HOME);

    command::Registry::build(config, Path::new("/repo"))
}

/// What the user message of `request` said.
fn prompt_of(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect()
}

/// `/init` is a template and nothing else: it reaches the model as an ordinary
/// prompt, and `AGENTS.md` appears because the model reached for `write`.
///
/// The path is absolute so the fixture writes into a temporary directory rather
/// than into whatever checkout the suite is running in.
#[tokio::test]
async fn the_init_command_produces_an_agents_file_through_the_ordinary_loop() {
    let workspace = ganja_testkit::temp_dir();
    let target = workspace.path().join("AGENTS.md");
    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call(
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
        .send(Command::RunCommand { name: "init".to_owned(), args: String::new() })
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
    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
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
    let (provider, _) = ScriptedProvider::new(Vec::new());
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );

    let refused = engine
        .send(Command::RunCommand { name: "nope".to_owned(), args: String::new() })
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
    let (provider, requests) = ScriptedProvider::new(vec![says("looks fine")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_commands(Arc::new(command_registry(&config)));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(
        engine.commands().names(),
        vec!["init".to_owned(), "review".to_owned()],
        "a config command joins the roster the builtins are in"
    );

    engine
        .send(Command::RunCommand { name: "review".to_owned(), args: "the shell tool".to_owned() })
        .await
        .expect("the configured command runs");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(prompt_of(&requests[0]), "review the diff, focusing on the shell tool");
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
    let (provider, requests) = ScriptedProvider::new(vec![says("looks fine"), says("hello")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(Arc::new(
        ganja_core::AgentRegistry::from_config(&config).expect("the fixture resolves an agent"),
    ))
    .with_commands(Arc::new(command_registry(&config)));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand { name: "review".to_owned(), args: String::new() })
        .await
        .expect("the configured command runs");
    drain_allowing(&engine, &mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
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
    assert_eq!(engine.agent().as_deref(), Some("build"), "and the session is still what it was");
    assert!(
        requests[1].system.is_none(),
        "so the next turn is not the reviewer: {:?}",
        requests[1].system
    );
}

/// A stored session with one message in it, already titled so the title
/// machinery stays out of a test that is not about it.
fn seeded(storage: &Storage) -> SessionId {
    let id = ganja_testkit::seed_session(storage, 0);
    ganja_testkit::seed_message(storage, &id, &Message::user("the objective"));

    id
}

/// The manual compaction takes the same path the automatic one does, with the
/// fill-level question skipped — and the window afterwards is the summary.
#[tokio::test]
async fn compacting_on_demand_summarizes_a_session_that_is_nowhere_near_full() {
    let directory = ganja_testkit::temp_dir();
    let storage = Storage::open(directory.path().join("storage"));
    let session = seeded(&storage);
    let model = ganja_core::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");

    let (provider, requests) = ScriptedProvider::new(vec![says("## Objective\n- find the thing")]);
    let engine = Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine.send(Command::Compact).await.expect("an idle engine accepts a compaction");
    let seen = drain_allowing(&engine, &mut events).await;

    {
        let requests = requests.lock().expect("the request log is never poisoned");
        assert_eq!(requests.len(), 1, "a compaction asks once: to summarize");
        assert!(requests[0].tools.is_empty(), "and the summarize request is the toolless one");
        assert!(
            prompt_of(&requests[0]).contains("[User]: the objective"),
            "with the conversation serialized into it"
        );
    }

    let summary = seen
        .iter()
        .find_map(|event| match event {
            Event::MessageStarted { session_id: _, message } if message.role == Role::Assistant => {
                Some(message.clone())
            }
            _ => None,
        })
        .expect("the summary enters the transcript");
    assert_eq!(
        summary.parts.first().and_then(ganja_core::protocol::Part::as_text),
        Some("## Objective\n- find the thing")
    );

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a compacting turn still ends with a terminal event");
    };
    assert_eq!(*reason, FinishReason::Completed);
    assert_eq!(
        engine.current_session().and_then(|info| info.summary).as_ref(),
        Some(&summary.id),
        "the window now opens at the summary"
    );
    assert_eq!(
        engine.current_session().map(|info| info.context_tokens),
        Some(7),
        "and the stored measure says what that window carries — the summary \
         alone, estimated at four characters a token when the provider \
         reported no usage (2026-08-25)"
    );

    // And the next turn is held inside it.
    engine
        .send(Command::SendPrompt {
            text: "carry on".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
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
    let directory = ganja_testkit::temp_dir();
    let storage = Storage::open(directory.path().join("storage"));
    let (provider, _) = ScriptedProvider::new(vec![says("one"), says("two")]);
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
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;
    let first = engine.current_session().expect("the first prompt minted a session").id;

    engine.send(Command::NewSession).await.expect("an idle engine accepts a reset");
    assert!(engine.current_session().is_none(), "the engine is between sessions");

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;
    let second = engine.current_session().expect("the next prompt minted another").id;

    assert_ne!(first, second, "a fresh session, not the old one reopened");
    let stored: Vec<SessionId> =
        engine.sessions().await.expect("the store lists").into_iter().map(|info| info.id).collect();
    assert!(
        stored.contains(&first) && stored.contains(&second),
        "and the old one is still there to resume: {stored:?}"
    );
}

/// Read-before-write is a rule about one conversation. Starting a new one has
/// to leave the old one's reads behind, or the first thing a fresh session
/// could do is overwrite a file it never opened.
#[tokio::test]
async fn a_new_session_does_not_inherit_what_the_last_one_had_read() {
    let directory = ganja_testkit::temp_dir();
    let target = directory.path().join("notes.md");
    std::fs::write(&target, "what was there before\n").expect("the fixture file is writable");
    let path = target.display().to_string();

    let (provider, _) = ScriptedProvider::new(vec![
        tool_call("read", json!({ "filePath": path })),
        says("read it"),
        tool_call("write", json!({ "filePath": path, "content": "something else\n" })),
        says("tried to write it"),
    ]);
    // No store, so no title request: this is about the read log, and an engine
    // that titles its sessions would spend a scripted answer on doing so.
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![
            Arc::new(ganja_core::tool::read::ReadTool),
            Arc::new(ganja_core::tool::write::WriteTool),
        ])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "read it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;

    engine.send(Command::NewSession).await.expect("an idle engine accepts a reset");

    engine
        .send(Command::SendPrompt {
            text: "now write it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_allowing(&engine, &mut events).await;

    let refused: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { state: ToolState::Error { error, .. }, .. } => Some(error.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        refused.iter().any(|error| error.contains("has not been read this session")),
        "the write had to be refused as unread, got {refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("the file is still readable"),
        "what was there before\n",
        "and the file it would have overwritten is untouched"
    );
}

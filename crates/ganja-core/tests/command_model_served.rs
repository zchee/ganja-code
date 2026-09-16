//! Whose echo `/usage` reports when a `/command` runs on a model of its own
//! (**D563**).
//!
//! The served slot is one half of a pair: `/usage` draws it beside the tier the
//! *session's* next request will carry, so an echo written by a turn that asked
//! a different model would pair a question about one model with an answer about
//! another. A `/command` whose frontmatter names a model is exactly such a turn
//! — it runs on that model for one turn and the session's own selection never
//! moves — so its echo is dropped, which is the rule a child turn running
//! another model already keeps.
//!
//! A binary of its own because reaching that path means reading the real global
//! command tier (`<config home>/commands`), and [`pin_config_home`] moves it
//! with a process-wide `set_var`.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::stream::BoxStream;
use ganja_core::config::CONFIG_HOME_ENV;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, FinishReason};
use ganja_core::provider::responses::CHATGPT_ID;
use ganja_core::provider::{ChatRequest, ProviderEvent};
use ganja_core::tool::Registry as ToolRegistry;
use ganja_core::{Config, Engine};
use ganja_testkit::{ScriptedProvider, drain, prompt, served};

/// The model the session is on.
const SOL: &str = "gpt-5.6-sol";
/// The model the command file names, and the session never switches to.
const FIVE: &str = "gpt-5.5";
/// How long a turn's tail is given to settle before the test moves on.
const SETTLE: Duration = Duration::from_secs(10);

/// Points the global command tier (**D481**) at a directory this binary owns
/// and fills it with the one command file under test.
///
/// That tier is `<config home>/commands`, resolved through [`CONFIG_HOME_ENV`]
/// on every build, so without this the command registry would be built over
/// whatever `*.md` files the developer running the suite keeps in their own
/// home — one of which could take this fixture's name. The `command_rules.rs`
/// shape, with one difference: that binary wants the tier *empty* and names a
/// directory it never creates, where this one needs a file in it, so the
/// directory is real and lives under a temporary root held for the process.
///
/// Forced from a `LazyLock` because this binary's tests share one process:
/// routing every build through here means the one `set_var` happens before the
/// first read of that variable, with any other builder parked on the lock.
fn pin_config_home() {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let home = std::env::temp_dir().join(format!("ganja-command-model-{}", std::process::id()));
        let commands = home.join("commands");
        std::fs::create_dir_all(&commands).expect("a commands directory");
        std::fs::write(
            commands.join("aside.md"),
            format!(
                "---\ndescription: ask the other model\nmodel: {FIVE}\n---\naside $ARGUMENTS\n"
            ),
        )
        .expect("the command file");
        // SAFETY: this binary's only write to the environment, run exactly
        // once, under the lock every reader of that variable here goes
        // through.
        unsafe { std::env::set_var(CONFIG_HOME_ENV, &home) };
        home
    });
    LazyLock::force(&HOME);
}

/// The last request the provider was asked.
fn last(requests: &Mutex<Vec<ChatRequest>>) -> ChatRequest {
    requests.lock().expect("the request log is never poisoned").last().cloned().expect("a request")
}

/// Takes one whole turn and waits for its tail, so the next command is never
/// refused as busy.
async fn settled(engine: &Engine, events: &mut BoxStream<'static, Event>, command: Command) {
    engine.send(command).await.expect("an idle engine accepts it");
    drain(events).await;
    assert!(engine.settle(SETTLE).await, "the turn's tail settles");
}

/// A turn that says one word, reports a served tier and stops.
fn reports(tier: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta("ok".to_owned()),
        served(tier),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// **Dv-42.** A `/command` carrying a model override asks that model and its
/// echo stays out of the session's slot; the session's own turn writes it.
#[tokio::test]
async fn a_command_that_names_its_own_model_leaves_the_sessions_served_echo_alone() {
    pin_config_home();
    let worktree = tempfile::tempdir().expect("a temporary directory");
    let (provider, requests) =
        ScriptedProvider::named(CHATGPT_ID, vec![reports("default"), reports("priority")]);
    let engine =
        Engine::new(provider, SOL, Arc::new(ToolRegistry::new(Vec::new())), Permissions::default())
            .with_commands(Arc::new(ganja_core::command::Registry::build(
                &Config::default(),
                worktree.path(),
            )));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    settled(
        &engine,
        &mut events,
        Command::RunCommand { name: "aside".to_owned(), args: "this".to_owned() },
    )
    .await;

    // The override took, or the rest of this test would be green about nothing.
    assert_eq!(last(&requests).model, FIVE, "the command's own model ran the turn");
    let view = engine.service_tier().expect("a chatgpt session reports a tier");
    assert_eq!(view.requested, "ultrafast", "the session is still on sol, unmoved by the command");
    assert_eq!(view.served, None, "a turn that asked another model reports nothing about this one");

    settled(&engine, &mut events, prompt("and now ask my own model")).await;

    assert_eq!(last(&requests).model, SOL, "the session's own model is back");
    assert_eq!(
        engine.service_tier().expect("a tier").served.as_deref(),
        Some("priority"),
        "the session's own turn is what fills the slot"
    );
}

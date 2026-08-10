//! Command-template expansion as it crosses the engine into a model request.
//!
//! This has its own test binary because the suite spawns real shells. That
//! keeps the shell-facing contract isolated under the integration-suite rule
//! in `tests/AGENTS.md` while the assertions observe the public engine seam.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use ganja_core::{
    CommandConfig, Config, Engine, command,
    permission::Permissions,
    project::Project,
    protocol::{Command, Event, FinishReason, Role},
    provider::{ChatRequest, Provider},
    tool::Registry as ToolRegistry,
};
use ganja_testkit::{ScriptedProvider, drain_allowing, says, temp_dir};

const COMMAND: &str = "fixture";

/// Includes resolved file blocks so attachment assertions inspect exactly what
/// the model received rather than the transcript's unresolved reference.
fn prompt_of(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect()
}

fn configured_engine(
    provider: Arc<dyn Provider>,
    template: String,
    worktree: &std::path::Path,
) -> Engine {
    let mut command = BTreeMap::new();
    command.insert(
        COMMAND.to_owned(),
        CommandConfig {
            template,
            description: None,
            agent: None,
            model: None,
        },
    );
    let registry = command::Registry::build(
        &Config {
            command,
            ..Config::default()
        },
        worktree,
    );

    Engine::new(
        provider,
        "recorder-model",
        Arc::new(ToolRegistry::new(Vec::new())),
        Permissions::default(),
    )
    .with_commands(Arc::new(registry))
}

fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().expect("the test process has a directory");
    Project::resolve(&cwd).root().to_owned()
}

async fn run_configured_command(engine: &Engine) -> Vec<Event> {
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::RunCommand {
            name: COMMAND.to_owned(),
            args: String::new(),
        })
        .await
        .expect("the configured command runs");
    drain_allowing(engine, &mut events).await
}

/// Shared with `crates/ganja-core/tests/passthrough.rs`; integration test
/// binaries are separate crates, so neither can borrow the other's helper.
#[cfg(windows)]
fn native(text: &str) -> PathBuf {
    let rest = text.strip_prefix('/').unwrap_or(text);
    let rest = rest
        .strip_prefix("cygdrive/")
        .or_else(|| rest.strip_prefix("mnt/"))
        .unwrap_or(rest);
    let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));

    match head.strip_suffix(':').unwrap_or(head).as_bytes() {
        [drive] if drive.is_ascii_alphabetic() => PathBuf::from(format!(
            "{}:\\{}",
            drive.to_ascii_uppercase() as char,
            tail.replace('/', "\\")
        )),
        _ => PathBuf::from(text),
    }
}

/// Nothing to translate where a shell and the filesystem already agree.
#[cfg(not(windows))]
fn native(text: &str) -> PathBuf {
    PathBuf::from(text)
}

#[tokio::test]
async fn a_templates_backtick_command_reaches_the_model_as_its_output() {
    let root = project_root();
    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = configured_engine(provider, "say: !`echo hi`".to_owned(), &root);

    run_configured_command(&engine).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = prompt_of(&requests[0]);
    assert_eq!(sent, "say: hi");
    assert!(
        !sent.contains('`') && !sent.contains("echo"),
        "none of the source expression survives expansion: {sent}"
    );
}

#[tokio::test]
async fn a_failing_template_command_still_sends_what_it_printed() {
    let root = project_root();
    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = configured_engine(provider, "result: !`echo out; exit 7`".to_owned(), &root);

    let seen = run_configured_command(&engine).await;

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always finishes, got {seen:?}");
    };
    assert_eq!(*reason, FinishReason::Completed);

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(
        prompt_of(&requests[0]),
        "result: out",
        "a non-zero exit still substitutes only the command's output"
    );
}

#[tokio::test]
async fn an_existing_template_mention_attaches_while_a_missing_one_stays_literal() {
    let workspace = temp_dir();
    let present = workspace.path().join("present.md");
    let missing = workspace.path().join("missing.md");
    std::fs::write(&present, "the attached fixture").expect("the fixture file is writable");

    let root = project_root();

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = configured_engine(provider, format!("read @{}", present.display()), &root);
    run_configured_command(&engine).await;

    {
        let requests = requests.lock().expect("the request log is never poisoned");
        let sent = prompt_of(&requests[0]);
        assert!(
            sent.contains(&format!("<attached-file path=\"{}\">", present.display())),
            "the file that exists becomes an attachment block: {sent}"
        );
        assert!(
            sent.contains("the attached fixture"),
            "the attachment block carries the file's contents: {sent}"
        );
    }

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = configured_engine(provider, format!("read @{}", missing.display()), &root);
    run_configured_command(&engine).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = prompt_of(&requests[0]);
    assert!(
        sent.contains(&format!("@{}", missing.display())),
        "a path that resolves to nothing stays in the prompt: {sent}"
    );
    assert!(
        !sent.contains("<attached-file"),
        "a path that resolves to nothing contributes no attachment block: {sent}"
    );
}

#[tokio::test]
async fn template_expansion_runs_at_the_project_root_not_the_process_directory() {
    let cwd = std::env::current_dir().expect("the test process has a directory");
    let root = Project::resolve(&cwd).root().to_owned();
    assert_ne!(
        root, cwd,
        "this test only proves anything while the crate directory is below the project root"
    );

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = configured_engine(provider, "!`pwd -P`".to_owned(), &root);
    run_configured_command(&engine).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = prompt_of(&requests[0]);
    assert_eq!(
        std::fs::canonicalize(native(sent.trim()))
            .expect("the directory the template command reported exists"),
        root,
        "the template command ran somewhere else"
    );
}

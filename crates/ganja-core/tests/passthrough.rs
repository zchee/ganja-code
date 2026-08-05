//! The `!` passthrough: the user's own command, and both it and its output in
//! the transcript where the next model request reads them.
//!
//! Two things here are contracts rather than conveniences. The synthetic user
//! text is **exact** — it is what tells the model why a `bash` call it never
//! made is sitting in its history — and the run is **ungated**, because this is
//! a person running their own command rather than a model asking to (**D13**).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Engine,
    permission::{Action, Permissions, Rule},
    project::Project,
    protocol::{Command, Event, FinishReason, PartBody, Role, ToolState},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
    tool::Registry,
};
use tokio_util::sync::CancellationToken;

/// `text`, which a shell printed, as this platform spells a path.
///
/// A POSIX shell on Windows answers `pwd` with `/d/a/project` where the native
/// spelling is `D:\a\project`; Cygwin writes `/cygdrive/d/...` and WSL
/// `/mnt/d/...` for the same place. All of them name one directory and only one
/// of them is a path anything else here can open.
///
/// Gated to Windows rather than merely documented as Windows-only: on unix
/// `/d/a/project` *is* the path, and rewriting it would invent a drive that
/// does not exist. A single letter is the whole test, so `/usr/bin` keeps its
/// meaning.
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

/// The whole of the synthetic user message, pinned to upstream
/// `packages/opencode/src/session/prompt.ts` (`shellImpl`).
const NOTICE: &str = "The following tool was executed by the user";

/// Records what it was asked and says one thing.
struct Recorder {
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Recorder {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        (
            Arc::new(Self {
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

        Ok(stream::iter([
            ProviderEvent::TextDelta("noted".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ])
        .boxed())
    }
}

/// Never answers, so a test can hold the engine busy.
struct Silent;

#[async_trait]
impl Provider for Silent {
    fn id(&self) -> &str {
        "silent"
    }

    async fn stream(
        &self,
        _request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        Ok(stream::once(async move {
            cancel.cancelled().await;
            ProviderEvent::Finish(FinishReason::Cancelled)
        })
        .boxed())
    }
}

fn engine(provider: Arc<dyn Provider>) -> Engine {
    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
}

/// Drains until the turn finishes.
async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let Some(event) = events.next().await else {
            return seen;
        };
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// The shell part as it finally stood.
fn shell_part(seen: &[Event]) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { tool, state, .. } if tool == "bash" => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the passthrough opened a bash part")
}

#[tokio::test]
async fn a_passthrough_writes_the_command_and_its_output_into_the_transcript() {
    let (provider, requests) = Recorder::new();
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunShell {
            command: "printf 'hello from the shell'".to_owned(),
        })
        .await
        .expect("an idle engine accepts a command");
    let seen = drain(&mut events).await;

    let Some(Event::MessageStarted {
        session_id: _,
        message: user,
    }) = seen.first()
    else {
        panic!("a passthrough opens with the synthetic user message, got {seen:?}");
    };
    assert_eq!(user.role, Role::User);
    assert_eq!(
        user.parts.len(),
        1,
        "one text part and nothing else: {:?}",
        user.parts
    );
    assert_eq!(
        user.parts[0].as_text(),
        Some(NOTICE),
        "the notice is exact, because it is what explains the call below it"
    );

    let ToolState::Completed {
        input,
        output,
        metadata,
        ..
    } = shell_part(&seen)
    else {
        panic!("the command completed");
    };
    assert_eq!(input["command"], "printf 'hello from the shell'");
    assert_eq!(output, "hello from the shell");
    assert_eq!(metadata["output"], "hello from the shell");
    assert!(
        metadata.get("exit").is_none(),
        "the exit code is awaited and discarded: {metadata}"
    );

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Completed);
    assert!(
        requests.lock().expect("the request log").is_empty(),
        "a passthrough asks the model nothing"
    );

    // The next model turn is where it pays off.
    engine
        .send(Command::SendPrompt {
            text: "what did that print".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let asked = requests.first().expect("the model was asked");
    assert!(
        asked.messages.iter().any(|message| message
            .parts
            .iter()
            .any(|part| part.as_text() == Some(NOTICE))),
        "the model reads why the call is there: {:?}",
        asked.messages
    );
    assert!(
        asked
            .messages
            .iter()
            .any(|message| message.parts.iter().any(|part| matches!(
                &part.body,
                PartBody::Tool { tool, state: ToolState::Completed { output, .. }, .. }
                    if tool == "bash" && output == "hello from the shell"
            ))),
        "and it reads the command and what it printed: {:?}",
        asked.messages
    );
}

/// `!pwd` answers with the project, not with wherever the process happens to
/// have been started (**R10**) — which is what a person typing `!git status`
/// means by "here".
///
/// The test binary runs with its working directory at the crate, and the
/// project root is the repository above it, so the two are provably different
/// and the assertion cannot pass by coincidence.
#[tokio::test]
async fn a_passthrough_runs_at_the_project_root_and_not_at_the_process_directory() {
    let cwd = std::env::current_dir().expect("the test process has a directory");
    let root = Project::resolve(&cwd).root().to_owned();
    assert_ne!(
        root, cwd,
        "this test only proves anything while the crate directory is below the project root"
    );

    let (provider, _) = Recorder::new();
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunShell {
            // `-P` so the answer is the directory the process was really given
            // rather than an inherited `PWD` spelling of it.
            command: "pwd -P".to_owned(),
        })
        .await
        .expect("an idle engine accepts a command");
    let seen = drain(&mut events).await;

    let ToolState::Completed { output, .. } = shell_part(&seen) else {
        panic!("the command completed");
    };
    // Canonicalised, not compared as text: `root` is canonical already, and on
    // Windows that is the verbatim spelling — so the shell's answer has to be
    // put through the same resolution before the two can be one string.
    assert_eq!(
        std::fs::canonicalize(native(output.trim()))
            .expect("the directory the passthrough reported exists"),
        root,
        "the passthrough ran somewhere else"
    );
}

/// A rule that would refuse the model the very same command does not refuse the
/// user (**D13**). The command is typed by a person, not asked for by a model.
#[tokio::test]
async fn a_passthrough_runs_even_where_a_rule_refuses_the_model() {
    let (provider, _) = Recorder::new();
    let engine = engine(provider);
    engine
        .permissions()
        .lock()
        .expect("the rules are never poisoned")
        .set_baseline(vec![Rule {
            permission: "bash".to_owned(),
            pattern: "*".to_owned(),
            action: Action::Deny,
        }]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunShell {
            command: "printf refused-nothing".to_owned(),
        })
        .await
        .expect("an idle engine accepts a command");
    let seen = drain(&mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "and nobody is asked either: {seen:?}"
    );
    let ToolState::Completed { output, .. } = shell_part(&seen) else {
        panic!("the command ran: {seen:?}");
    };
    assert_eq!(output, "refused-nothing");
}

#[tokio::test]
async fn cancelling_a_passthrough_stops_the_command() {
    let (provider, _) = Recorder::new();
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunShell {
            command: "sleep 30".to_owned(),
        })
        .await
        .expect("an idle engine accepts a command");

    // Wait until the part is open, so the cancel lands on a running command.
    loop {
        let event = events.next().await.expect("the stream is live");
        if matches!(
            &event,
            Event::PartStarted { part, .. } if matches!(&part.body, PartBody::Tool { tool, .. } if tool == "bash")
        ) {
            break;
        }
    }

    let started = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");
    let seen = drain(&mut events).await;

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a cancelled command should not run its 30 seconds out: {:?}",
        started.elapsed()
    );
    assert!(
        matches!(shell_part(&seen), ToolState::Error { .. }),
        "and the part closes rather than spinning forever"
    );
}

#[tokio::test]
async fn a_passthrough_is_refused_while_a_turn_is_streaming() {
    let engine = engine(Arc::new(Silent));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "think about it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));

    assert!(
        engine
            .send(Command::RunShell {
                command: "printf nope".to_owned(),
            })
            .await
            .is_err(),
        "the engine runs one turn at a time, whatever kind it is"
    );

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");
    drain(&mut events).await;
}

//! A permission request says *where* the call would work, not only what it
//! would run.
//!
//! The gate has always collected those directories — an "always" answer stores
//! a rule for each of them — but until now they never left the engine, so a
//! dialog could show `cat notes.txt` while the answer it collected covered
//! somebody else's home directory. This proves they reach the event.
//!
//! It lives in a binary of its own because it sets `XDG_DATA_HOME`, which is
//! process-wide: [`Permissions::load`] resolves the project's store beneath it,
//! and a test that reached the real one could read or write a person's own
//! answers. Everything in this file is therefore one test.

use std::{
    collections::VecDeque,
    fs,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Command, Engine, Event, FinishReason, PermissionReply, Permissions, Registry, Tool, ToolCtx,
    ToolError, ToolOutput,
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Answers each request with the next script, in order.
struct Scripted {
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
}

#[async_trait]
impl Provider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let script = self
            .scripts
            .lock()
            .expect("the scripts are never poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ProviderEvent::Finish(FinishReason::Completed)]);

        Ok(stream::iter(script).boxed())
    }
}

/// Arguments the stand-in shell tool takes.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct Args {
    command: Option<String>,
}

/// A tool registered under the shell's id, so the gate treats its argument as
/// a command. It never runs here — every call in this test is refused.
struct Shell;

#[async_trait]
impl Tool for Shell {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "stands in for the shell"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            title: "bash".to_owned(),
            output: String::new(),
            metadata: json!({}),
        })
    }
}

/// One step calling `bash` with `command`.
fn runs(command: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: "call_1".to_owned(),
            json: json!({ "command": command }).to_string(),
        },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

#[tokio::test]
async fn a_request_discloses_the_directories_an_always_answer_would_cover() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    // SAFETY: nothing else runs yet — this is the only test in this binary, and
    // it has not started a thread.
    unsafe { std::env::set_var("XDG_DATA_HOME", home.path()) };

    let project = TempDir::new().expect("a temporary directory is creatable");
    fs::create_dir(project.path().join(".git")).expect("the fixture repository is creatable");
    let outside = TempDir::new().expect("a temporary directory is creatable");
    fs::write(outside.path().join("notes.txt"), "elsewhere").expect("the fixture file writes");
    let named = outside.path().join("notes.txt");
    let elsewhere = fs::canonicalize(outside.path())
        .expect("the fixture directory resolves")
        .to_string_lossy()
        .into_owned();

    // Two turns of two requests each: the call, then the step that reads its
    // refusal and says so. Without the second script the loop would take the
    // *next* turn's call inside this one.
    let provider = Arc::new(Scripted {
        scripts: Mutex::new(VecDeque::from(vec![
            runs("cargo test"),
            vec![
                ProviderEvent::TextDelta("refused".to_owned()),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            runs(&format!("cat {}", named.display())),
            vec![
                ProviderEvent::TextDelta("refused".to_owned()),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
        ])),
    });
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![Arc::new(Shell)])),
        Permissions::load(project.path()),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    /// Runs one turn, refusing whatever it asks about, and hands back what
    /// every request in it disclosed.
    async fn ask(engine: &Engine, events: &mut BoxStream<'static, Event>) -> Vec<Vec<String>> {
        engine
            .send(Command::SendPrompt {
                text: "go".to_owned(),
                mentions: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        let mut disclosed = Vec::new();
        loop {
            let Some(event) = events.next().await else {
                panic!("the stream ended before the turn did");
            };
            match event {
                Event::PermissionRequested {
                    id, directories, ..
                } => {
                    disclosed.push(directories);
                    engine
                        .send(Command::ReplyPermission {
                            id,
                            reply: PermissionReply::Reject,
                        })
                        .await
                        .expect("a reply is always accepted");
                }
                Event::MessageFinished { .. } => return disclosed,
                _ => {}
            }
        }
    }

    assert_eq!(
        ask(&engine, &mut events).await,
        vec![Vec::<String>::new()],
        "a command that stays inside the checkout names nowhere else"
    );
    assert_eq!(
        ask(&engine, &mut events).await,
        vec![vec![elsewhere]],
        "a command naming a file elsewhere discloses the directory holding it"
    );
}

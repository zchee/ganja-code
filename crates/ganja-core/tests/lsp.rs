//! The accept for LSP diagnostics: an edit that introduces a type error comes
//! back with rust-analyzer's complaint attached to the tool result, inside
//! three seconds — or inside `GANJA_LSP_EDIT_BUDGET_MS` on a machine whose
//! scheduling is not the drill's to control (CI holds the drill to the
//! client's own five-second ceiling instead).
//!
//! **This suite hard-fails when `rust-analyzer` is not on `PATH`.** It does not
//! skip, for `golden.rs`'s reason: the whole point is that a real language
//! server really analysed a real crate, and a green run that started nothing
//! would be worth less than no run at all. The binary ships with the rustup
//! component of the same name.
//!
//! # Why the drill is gated on a readiness signal rather than a sleep
//!
//! rust-analyzer's first useful answer comes after it has loaded a sysroot and
//! built a crate graph, which takes as long as the machine takes. Timing an
//! edit against a server that is still starting would measure the startup, and
//! padding the budget to cover it would measure nothing at all.
//!
//! So the fixture ships a file that is *already* wrong, the test touches it and
//! waits until that error is published, and only then does it run the one edit
//! it times. At that point the server is demonstrably initialized and
//! analysing this crate, and what the clock sees is what a session sees: sync
//! the file, wait for the fresh publish, format the block.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Engine, LspConfig,
    lsp::{Lsp, lsp_types},
    permission::{Action, Permissions, Rule},
    protocol::{Command, Event, FinishReason, PartBody, ToolState},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
    tool::Registry,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// What the accept pins by default: from the tool starting to the tool
/// result, with the diagnostics block in it (plan:205).
///
/// The client's own ceiling is five seconds. This is the number a development
/// machine is held to, and it is deliberately the tighter of the two.
const EDIT_BUDGET: Duration = Duration::from_millis(3_000);

/// The budget this run is held to: `GANJA_LSP_EDIT_BUDGET_MS`, or the default
/// above.
///
/// The override exists for machines whose scheduling is not the drill's to
/// control: a shared CI runner under load adds whole seconds of queueing to
/// an edit the same code answers in milliseconds on an idle machine. CI sets
/// a budget just past the client's own five-second ceiling — at the ceiling
/// the client stops waiting and the missing diagnostics block fails the
/// assertions ahead of the clock, so no value of this variable can turn a
/// wedged pull into a green run; the margin past it covers only this test's
/// own bookkeeping around the call, which this clock includes and the
/// client's does not. A value that does not parse falls back to the default,
/// which is the strict direction to fail in.
fn edit_budget() -> Duration {
    std::env::var("GANJA_LSP_EDIT_BUDGET_MS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(EDIT_BUDGET)
}

/// How long the server is given to come up and analyse the crate before the
/// timed edit runs. Not part of the accept — the drill has not started yet.
const READINESS_BUDGET: Duration = Duration::from_secs(180);

/// A fixture file that is already wrong, so that a publish about it is proof
/// the server is analysing this crate and not merely running.
const SEEDED: &str = "src/seeded.rs";

/// The file the timed edit breaks. Correct until then.
const FRESH: &str = "src/fresh.rs";

/// What `fresh.rs` says before the edit, and the half of it the edit replaces.
const CORRECT_BODY: &str = "0";

/// What the edit puts there instead: a `&str` where an `i32` is promised.
const BROKEN_BODY: &str = "\"not an integer\"";

/// Answers each request with the next script.
struct ScriptedProvider {
    scripts: Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "lsp-drill"
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
            .expect("the script has a step for every request");

        Ok(stream::iter(script).boxed())
    }
}

/// One complete tool call, as a provider streams it.
fn call(id: &str, tool: &str, arguments: &serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: id.to_owned(),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: id.to_owned(),
            json: arguments.to_string(),
        },
        ProviderEvent::ToolCallEnd { id: id.to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// Writes `contents` at `root/relative`, creating the directories above it.
fn plant(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("the file has a parent"))
        .expect("the fixture directories are created");
    std::fs::write(path, contents).expect("the fixture file is written");
}

/// A one-crate cargo project: a file that is already wrong, and one that is not
/// yet.
///
/// `[workspace]` is present so that rust-analyzer roots here whatever directory
/// the temporary files landed in — a temp dir inside somebody's workspace would
/// otherwise drag the whole of it into the crate graph, and the drill would be
/// timing their project instead of this one.
fn fixture() -> TempDir {
    let temp = TempDir::new().expect("a temp dir");
    let root = temp.path();

    plant(
        root,
        "Cargo.toml",
        "[workspace]\n\
         \n\
         [package]\n\
         name = \"lsp-fixture\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n",
    );
    plant(root, "src/lib.rs", "pub mod fresh;\npub mod seeded;\n");
    plant(
        root,
        SEEDED,
        "//! Wrong on purpose: the readiness signal the drill waits for.\n\
         pub fn seeded() -> i32 {\n    \"not an integer\"\n}\n",
    );
    plant(
        root,
        FRESH,
        &format!("pub fn fresh() -> i32 {{\n    {CORRECT_BODY}\n}}\n"),
    );

    temp
}

/// Whether a binary named `binary` is on `PATH`.
///
/// Asked through the product's own resolver rather than by joining the bare
/// name. A precondition check is a claim that *the engine* will find the
/// server, so anything that answers it differently can refuse to run a suite
/// the engine would have been perfectly able to drive — which is exactly what a
/// bare join does on Windows, where the binary rustup installs is
/// `rust-analyzer.exe`.
fn on_path(binary: &str) -> bool {
    ganja_core::lsp::server::which(binary).is_some()
}

/// Every error message the servers currently hold about `path`.
fn errors_for(lsp: &Lsp, path: &Path) -> Vec<String> {
    lsp.diagnostics()
        .get(path)
        .map(|issues| {
            issues
                .iter()
                .filter(|issue| {
                    issue
                        .severity
                        .is_none_or(|severity| severity == lsp_types::DiagnosticSeverity::ERROR)
                })
                .map(|issue| issue.message.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Touches the seeded file until its error is published, so that the timed edit
/// below starts against a server that is demonstrably working.
///
/// Panics rather than returning a flag: a drill that ran without the server
/// ready would measure startup and call it latency.
async fn wait_until_analysing(lsp: &Arc<Lsp>, seeded: &Path) {
    let started = Instant::now();
    let mut touches = 0_u32;
    while started.elapsed() < READINESS_BUDGET {
        touches += 1;
        // Each touch syncs the file and waits up to the client's own budget for
        // a fresh publish, so this loop is a re-ask and not a spin.
        lsp.touch(seeded, true).await;
        let errors = errors_for(lsp, seeded);
        if !errors.is_empty() {
            println!(
                "lsp drill: rust-analyzer is analysing the fixture after {:?} \
                 and {touches} touch(es); it says {errors:?}",
                started.elapsed()
            );

            return;
        }
    }

    panic!(
        "rust-analyzer published no error for the seeded file within {READINESS_BUDGET:?}, \
         so nothing after this would be measuring diagnostics latency"
    );
}

/// The completed `edit` part from a drained event stream, with the timestamps
/// the engine stamped around the call.
fn completed_edit(events: &[Event]) -> (String, u64, u64) {
    for event in events {
        let Event::PartUpdated { part, .. } = event else {
            continue;
        };
        let PartBody::Tool { tool, state, .. } = &part.body else {
            continue;
        };
        if tool != "edit" {
            continue;
        }
        if let ToolState::Completed {
            output,
            started,
            completed,
            ..
        } = state
        {
            return (output.clone(), *started, *completed);
        }
    }

    panic!("the turn never completed an edit call");
}

#[tokio::test]
async fn an_edit_that_breaks_a_type_comes_back_with_rust_analyzers_complaint_attached() {
    assert!(
        on_path("rust-analyzer"),
        "this suite needs rust-analyzer on PATH — install it with \
         `rustup component add rust-analyzer`. It hard-fails rather than skipping, \
         because a run that started no language server would prove nothing."
    );

    let temp = fixture();
    // Canonicalized because a publish names a real path: on macOS the temp dir
    // is reached through `/var`, which is a link to `/private/var`, and a map
    // keyed on the un-canonicalized spelling would never match what comes back.
    let root = temp
        .path()
        .canonicalize()
        .expect("the fixture directory resolves");
    let seeded = root.join(SEEDED);
    let fresh = root.join(FRESH);

    let lsp = Lsp::new(Some(&LspConfig::Enabled(true)), &root).expect("`true` is the builtins");
    wait_until_analysing(&lsp, &seeded).await;
    assert!(
        errors_for(&lsp, &fresh).is_empty(),
        "the file the drill is about to break must start out sound"
    );

    // Two calls in one turn: `read` earns the right to edit — the
    // read-before-write rule is the engine's, not this test's — and `edit` is
    // the one whose result is timed.
    let provider = Arc::new(ScriptedProvider {
        scripts: Mutex::new(
            vec![
                call(
                    "read-1",
                    "read",
                    &serde_json::json!({ "filePath": fresh.to_string_lossy() }),
                ),
                call(
                    "edit-1",
                    "edit",
                    &serde_json::json!({
                        "filePath": fresh.to_string_lossy(),
                        "oldString": CORRECT_BODY,
                        "newString": BROKEN_BODY,
                    }),
                ),
                vec![ProviderEvent::Finish(FinishReason::Completed)],
            ]
            .into(),
        ),
    });
    let mut permissions = Permissions::default();
    // `edit` asks by default. The drill is about diagnostics, not about the
    // gate, and the gate has its own suite.
    permissions.set_baseline(vec![Rule {
        permission: "edit".to_owned(),
        pattern: "*".to_owned(),
        action: Action::Allow,
    }]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::with_builtins()),
        permissions,
    )
    .with_lsp(Arc::clone(&lsp));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "break it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts");

    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);
        if finished {
            break;
        }
    }

    let (output, started, completed) = completed_edit(&seen);
    let elapsed = completed.saturating_sub(started);
    let budget = edit_budget();
    println!(
        "lsp drill: edit tool start to tool result was {elapsed}ms \
         (budget {}ms)",
        budget.as_millis()
    );

    assert!(
        output.contains("<diagnostics"),
        "the edit's result carries a diagnostics block: {output}"
    );
    assert!(
        output.contains("LSP errors detected in this file, please fix:"),
        "under the heading the model is meant to act on: {output}"
    );
    assert!(
        output.contains("ERROR ["),
        "with at least one error line in it: {output}"
    );
    assert!(
        output.contains(&fresh.to_string_lossy().to_string()),
        "naming the file that was edited: {output}"
    );
    assert!(
        u128::from(elapsed) <= budget.as_millis(),
        "the diagnostic came back in {elapsed}ms, over the {}ms the accept allows",
        budget.as_millis()
    );

    // The other half of the append rule: `edit` speaks about its own file only,
    // even though the server has just as much to say about the seeded one.
    assert!(
        !output.contains("LSP errors detected in other files"),
        "an edit carries no cross-file section: {output}"
    );
    assert!(
        !output.contains(SEEDED.rsplit('/').next().expect("a file name")),
        "and does not mention the file it did not touch: {output}"
    );

    drop(engine);
    lsp.shutdown();
}

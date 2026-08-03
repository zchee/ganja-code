//! One live turn, end to end, through the real agent loop.
//!
//! `tests/live.rs` proves the request this build sends is one the vendor still
//! accepts. This proves the rest of the sentence: that a real model, offered
//! this build's real tools, calls them, that the arguments it streams parse
//! into what those tools expect, and that running them changes the disk.
//! Everything between is exercised for real — the permission gate, the
//! multi-step loop, `write`, and a shell command in its own process group.
//!
//! Mock first, live second, and the same gating as `tests/live.rs`:
//! `#[ignore]` keeps `cargo test` away, and even `cargo test -- --ignored`
//! finds it inert unless `GANJA_LIVE_TEST=1` and `ANTHROPIC_API_KEY` are both
//! set.
//!
//! # Why this is its own binary
//!
//! It mutates two pieces of process-wide state that `tests/live.rs` does not.
//! The engine captures the working directory at construction, so proving that
//! the model's `hello.py` landed somewhere disposable means moving the process
//! into that directory first; and `XDG_DATA_HOME` is redirected so that
//! nothing here reads or writes the real user's stored permissions or spilled
//! tool output. `cargo test` runs the tests inside a binary on parallel
//! threads, so a binary that does either has to hold exactly one test.

use std::{env, path::Path, sync::Arc};

use futures::StreamExt as _;
use ganja_core::{
    Command, Engine, Event, FinishReason, PartBody, PermissionReply, Permissions, Registry,
    ToolState, catalog, provider::AnthropicProvider,
};

/// Variable that has to be `1` before this talks to a vendor.
const LIVE_ENV: &str = "GANJA_LIVE_TEST";

/// The credential the provider is built from. Never read here — only its
/// presence is checked, so that no path through this file holds key material.
const KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// What the model is asked to do, in `directory`.
///
/// Named tools and a named interpreter, because the assertion is about a file
/// existing and a command having run: leaving the model to choose between
/// `write` and `edit`, or between `python3` and a heredoc, would make a green
/// run a statement about that day's phrasing.
///
/// The directory is spelled out for a sharper reason. `write` and `read` tell
/// the model their paths must be absolute, and the engine sends no system
/// prompt, so a bare "create hello.py" leaves the model to invent an absolute
/// path — it picks `/tmp/hello.py`, which is both outside anything this test
/// owns and not a thing to leave behind on a contributor's machine. A frontend
/// supplies that context in its system prompt; here the request carries it.
fn prompt(directory: &Path) -> String {
    let directory = directory.display();

    format!(
        "Your working directory is {directory}. Using the write tool, create \
         the file {directory}/hello.py containing exactly: print(\"hello\"). \
         Then, using the bash tool, run it with: python3 {directory}/hello.py"
    )
}

/// Whether a live run was asked for, and can be paid for.
fn enabled() -> bool {
    if env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipping: {LIVE_ENV} is not 1");
        return false;
    }
    if env::var(KEY_ENV).is_ok_and(|key| !key.trim().is_empty()) {
        return true;
    }

    eprintln!("skipping: {KEY_ENV} is unset");
    false
}

/// One tool call the turn finished: its tool, and what it returned.
type Finished = (String, serde_json::Value, String);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "talks to Anthropic; needs GANJA_LIVE_TEST=1 and ANTHROPIC_API_KEY"]
async fn a_live_model_writes_a_file_and_runs_it() {
    if !enabled() {
        return;
    }

    let workspace = tempfile::tempdir().expect("a temp directory");
    let data = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", data.path());
    }
    // Before the engine is built, which is when it captures the directory
    // every relative path in a tool call resolves against.
    env::set_current_dir(workspace.path()).expect("the workspace is enterable");
    // What the engine captured, which on a platform that hands out temporary
    // directories behind a symlink is not what `tempfile` reported. The prompt
    // has to name the directory the tools will actually resolve against.
    let root = env::current_dir().expect("the process has a directory");

    let model = env::var("GANJA_MODEL").ok().unwrap_or_else(|| {
        catalog::default_model("anthropic")
            .expect("the catalog has a default")
            .to_owned()
    });
    let engine = Engine::new(
        Arc::new(AnthropicProvider::from_env().expect("a provider builds from the environment")),
        &model,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: prompt(&root),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // A turn spans as many model requests as its calls demand, and the model is
    // free to spend an extra one saying it is done; `MessageFinished` is the
    // end of all of them, not of one step.
    let mut finished: Vec<Finished> = Vec::new();
    let (reason, error) = loop {
        let event = events
            .next()
            .await
            .expect("the turn should finish before the stream ends");

        match event {
            Event::PermissionRequested { id, .. } => {
                engine
                    .send(Command::ReplyPermission {
                        id,
                        reply: PermissionReply::Once,
                    })
                    .await
                    .expect("a reply is always accepted");
            }
            Event::PartUpdated { part, .. } => {
                if let PartBody::Tool {
                    tool,
                    state: ToolState::Completed { input, output, .. },
                    ..
                } = part.body
                {
                    finished.push((tool, input, output));
                }
            }
            Event::MessageFinished { reason, error, .. } => break (reason, error),
            _ => {}
        }
    };

    assert_eq!(
        reason,
        FinishReason::Completed,
        "a live turn should complete: {error:?}, calls so far: {:?}",
        names(&finished)
    );

    let script = root.join("hello.py");
    assert!(
        script.is_file(),
        "the model was asked to create {}; it ran {:?} and left {:?}",
        script.display(),
        names(&finished),
        listing(&root)
    );
    let source = std::fs::read_to_string(&script).expect("the written file is readable");
    assert!(
        source.contains("hello"),
        "the file the model wrote should print something: {source:?}"
    );

    let ran = finished
        .iter()
        .find(|(tool, _, output)| tool == "bash" && output.contains("hello"));
    assert!(
        ran.is_some(),
        "no shell call came back having printed hello; the turn ran {:?}",
        names(&finished)
    );

    eprintln!(
        "{model} ran {:?} and left {}",
        names(&finished),
        relative(&root, &script)
    );
}

/// Just the tool names, for a failure message that does not paste a whole
/// file back.
fn names(finished: &[Finished]) -> Vec<String> {
    finished
        .iter()
        .map(|(tool, input, _)| {
            let mut rendered = format!("{tool} {input}");
            rendered.truncate(160);
            rendered
        })
        .collect()
}

/// What `directory` holds, for a failure that has to say where the file went
/// instead.
fn listing(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// `path` as it reads from `root`.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

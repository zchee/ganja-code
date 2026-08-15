//! Nested `AGENTS.md` walked in from below the project root (**D480**).
//!
//! Upstream opencode reads instruction files by walking **up** from the
//! working directory to the project root, so a monorepo's
//! `packages/web/AGENTS.md` is invisible to a session launched at the root
//! however much work happens inside `packages/web`. D480 closes that the lazy
//! way: when a turn's tool call *opens or writes* a file, the directories
//! between it and the root are walked, and any instruction file found there
//! joins the **next** request's system prompt.
//!
//! Everything below is asserted at the **request-assembly seam** — what the
//! provider was actually asked — because that is the only place the claim is
//! about. A `ScriptedProvider` records every [`ChatRequest`], so each test
//! reads the system prompt of request *n* and request *n+1* and says which of
//! them carries the file.
//!
//! # Why this is one binary of its own
//!
//! [`Engine`] captures its working directory and project root at construction
//! from the **process's** own, so every fixture here has to `chdir` into a
//! temporary checkout before building an engine. That is process-wide state,
//! and this suite's own convention (`tests/AGENTS.md`) is that such a test
//! gets its own binary. Under `nextest` each test is already its own process;
//! under a plain `cargo test` they share one, so [`FIXTURE`] serializes them
//! rather than leaving the pass rate to the scheduler.
//!
//! # What is deliberately *not* pinned here
//!
//! There is no assertion that the injection happens once and then stops. There
//! is no per-session loaded set to stop: the walk is recomputed from the
//! transcript's own tool calls on every request, which is what makes the
//! resume and revert tests below true by construction rather than by
//! bookkeeping. See `instruction::nested_suffix` for the carrier's reasoning.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

use futures::stream::BoxStream;
use ganja_core::{
    Engine, Storage,
    permission::{Action, Permissions, Rule},
    protocol::{Command, Event, MessageId, RevertScope, Role},
    provider::ChatRequest,
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};
use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

/// Serializes the process working directory across this binary's tests.
///
/// Held for a whole test rather than only across the `chdir`: an engine reads
/// the directory at construction, but a test builds more than one engine (the
/// resume drill builds two), and a neighbour that moved the directory in
/// between would point the second one at another fixture.
static FIXTURE: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

/// What the up-walk tier and the nested walk both introduce a file with.
const HEADER: &str = "Instructions from: ";

/// A temporary checkout to work in, entered, with the guard that keeps any
/// other test in this binary out of it while it is.
///
/// The directory handle is returned because dropping it deletes the tree.
async fn checkout() -> (tempfile::TempDir, PathBuf, MutexGuard<'static, ()>) {
    let guard = FIXTURE.lock().await;
    let directory = tempfile::tempdir().expect("a temporary directory");
    // `Project::resolve` stops at a `.git`, which is what makes the fixture a
    // root rather than one directory inside whatever holds the temp tree.
    std::fs::create_dir_all(directory.path().join(".git")).expect("the fixture repository");
    std::env::set_current_dir(directory.path()).expect("the fixture is enterable");
    // The engine canonicalizes what it reads back; on a platform whose
    // temporary directory is itself a symlink the two spellings differ, and
    // every path this suite plants has to be the one the engine will see.
    let root = std::env::current_dir().expect("the fixture is readable");

    (directory, root, guard)
}

/// Writes `text` at `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    std::fs::write(path, text).expect("the fixture file is writable");
}

/// Rules that let the file tools run without a dialog: this suite is about
/// which instructions reach the prompt, and the gate has suites of its own.
fn permissive() -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(
        ["read", "edit", "write", "glob"]
            .into_iter()
            .map(|tool| Rule {
                permission: tool.to_owned(),
                pattern: "*".to_owned(),
                action: Action::Allow,
            })
            .collect(),
    );

    permissions
}

/// The system prompt of every request the provider was asked, in order.
fn systems(requests: &Mutex<Vec<ChatRequest>>) -> Vec<String> {
    requests
        .lock()
        .expect("the request log is never poisoned")
        .iter()
        .map(|request| request.system.clone().unwrap_or_default())
        .collect()
}

/// How many times `system` names `path` as an instruction file.
fn named(system: &str, path: &str) -> usize {
    system.matches(&format!("{HEADER}{path}")).count()
}

/// The tools whose calls had **completed** by the time the last request was
/// assembled, read out of that request's own messages.
///
/// Every negative assertion in this suite — "a listing walks nothing in", "a
/// root-level read moves nothing" — is a claim about a call that ran. A call
/// that was refused or that failed would satisfy the same assertion while
/// proving nothing at all, so each of them says out loud which call it is
/// asserting about.
fn completed_calls(requests: &Mutex<Vec<ChatRequest>>) -> Vec<String> {
    use ganja_core::protocol::{PartBody, ToolState};

    requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .map(|request| {
            request
                .messages
                .iter()
                .flat_map(|message| &message.parts)
                .filter_map(|part| match &part.body {
                    PartBody::Tool {
                        tool,
                        state: ToolState::Completed { .. },
                        ..
                    } => Some(tool.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Sends `text` and waits for the turn it starts to finish, keeping the id of
/// the user message that asked — the anchor a revert names.
async fn ask(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    text: &str,
) -> Option<MessageId> {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    drain(events)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::MessageStarted { message, .. } if message.role == Role::User => Some(message.id),
            _ => None,
        })
}

/// An engine over the entered fixture, playing `script`, with the file tools
/// allowed and a system prompt of its own so the injection has something to be
/// appended to.
fn engine(provider: Arc<ScriptedProvider>) -> Engine {
    Engine::new(
        provider,
        "fake-1",
        Arc::new(Registry::with_builtins()),
        permissive(),
    )
    .with_system_parts(Some("the composed prompt".to_owned()), None)
}

/// A script that reads `path` and then says it is done: two requests, the
/// second of which is the one under test.
fn reads(path: &Path) -> Vec<Vec<ganja_core::provider::ProviderEvent>> {
    vec![
        tool_call("read", json!({ "filePath": path.to_string_lossy() })),
        says("done"),
    ]
}

/// The defect D480 closes: a session working inside `sub/` was never told what
/// `sub/AGENTS.md` says.
#[tokio::test]
async fn a_read_below_the_root_puts_that_directorys_instructions_in_the_next_request() {
    let (_directory, root, _guard) = checkout().await;
    plant(&root.join("AGENTS.md"), "root rules");
    plant(&root.join("sub").join("AGENTS.md"), "sub rules");
    plant(&root.join("sub").join("file.rs"), "fn main() {}");

    let (provider, requests) = ScriptedProvider::new(reads(&root.join("sub").join("file.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look at sub/file.rs").await;

    let systems = systems(&requests);
    assert_eq!(systems.len(), 2, "one request, then one after the read");
    assert_eq!(
        named(&systems[0], "sub/AGENTS.md"),
        0,
        "nothing had been touched yet: {}",
        systems[0]
    );
    assert_eq!(
        named(&systems[1], "sub/AGENTS.md"),
        1,
        "the read walked it in, once: {}",
        systems[1]
    );
    assert!(
        systems[1].contains("sub rules"),
        "and its contents came with it: {}",
        systems[1]
    );
    assert!(
        systems[1].starts_with("the composed prompt"),
        "appended to the prompt rather than replacing it: {}",
        systems[1]
    );
}

/// The dedup is per directory, not per touch: a directory's instructions are
/// one file however much work happens inside it.
#[tokio::test]
async fn a_second_touch_in_the_same_directory_adds_nothing() {
    let (_directory, root, _guard) = checkout().await;
    let sub = root.join("sub");
    plant(&sub.join("AGENTS.md"), "sub rules");
    plant(&sub.join("one.rs"), "fn one() {}");
    plant(&sub.join("two.rs"), "fn two() {}");

    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call(
            "read",
            json!({ "filePath": sub.join("one.rs").to_string_lossy() }),
        ),
        tool_call(
            "read",
            json!({ "filePath": sub.join("two.rs").to_string_lossy() }),
        ),
        says("done"),
    ]);
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look at both").await;

    let systems = systems(&requests);
    assert_eq!(systems.len(), 3, "two reads, three requests");
    for (index, system) in systems.iter().enumerate().skip(1) {
        assert_eq!(
            named(system, "sub/AGENTS.md"),
            1,
            "request {index} names it exactly once: {system}"
        );
    }
}

/// The root's own file is the up-walk tier's, and a walk that named it again
/// would send it twice.
#[tokio::test]
async fn a_touch_at_the_root_adds_nothing() {
    let (_directory, root, _guard) = checkout().await;
    plant(&root.join("AGENTS.md"), "root rules");
    plant(&root.join("main.rs"), "fn main() {}");

    let (provider, requests) = ScriptedProvider::new(reads(&root.join("main.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look at main.rs").await;

    assert_eq!(
        completed_calls(&requests),
        vec!["read".to_owned()],
        "the read really ran, so the assertion below is about a touch"
    );
    let systems = systems(&requests);
    assert_eq!(
        systems[1], systems[0],
        "a root-level read moves nothing: {}",
        systems[1]
    );
}

/// Closest-last, the same order the up-walk tier stacks in: the most specific
/// instructions are the ones the model read most recently.
#[tokio::test]
async fn a_deeper_instruction_file_is_read_after_the_shallower_one() {
    let (_directory, root, _guard) = checkout().await;
    let deep = root.join("sub").join("nested");
    plant(&root.join("sub").join("AGENTS.md"), "sub rules");
    plant(&deep.join("AGENTS.md"), "nested rules");
    plant(&deep.join("file.rs"), "fn main() {}");

    let (provider, requests) = ScriptedProvider::new(reads(&deep.join("file.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look deep").await;

    let system = systems(&requests).remove(1);
    let shallow = system
        .find(&format!("{HEADER}sub/AGENTS.md"))
        .expect("the shallower file is there");
    let deeper = system
        .find(&format!("{HEADER}sub/nested/AGENTS.md"))
        .expect("and so is the deeper one");
    assert!(shallow < deeper, "closest last: {system}");
}

/// The project vocabulary, per directory: a subtree that spells its file
/// `CLAUDE.md` is not muted by a sibling that spells it `AGENTS.md`.
#[tokio::test]
async fn a_subdirectory_carrying_only_a_claude_file_is_honoured() {
    let (_directory, root, _guard) = checkout().await;
    plant(&root.join("agents").join("AGENTS.md"), "agents rules");
    plant(&root.join("claude").join("CLAUDE.md"), "claude rules");
    plant(&root.join("claude").join("file.rs"), "fn main() {}");

    let (provider, requests) = ScriptedProvider::new(reads(&root.join("claude").join("file.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look at the claude subtree").await;

    let system = systems(&requests).remove(1);
    assert_eq!(named(&system, "claude/CLAUDE.md"), 1, "{system}");
    assert!(
        system.contains("claude rules"),
        "with its contents: {system}"
    );
}

/// Pre-mortem 2, pinned: a listing is not a touch. One unanchored glob over a
/// vendored tree must not walk that whole tree's instructions into the prompt.
#[tokio::test]
async fn a_glob_over_the_tree_walks_nothing_in() {
    let (_directory, root, _guard) = checkout().await;
    for vendor in ["one", "two", "three"] {
        plant(
            &root.join("third_party").join(vendor).join("AGENTS.md"),
            "vendored rules",
        );
        plant(
            &root.join("third_party").join(vendor).join("lib.rs"),
            "fn vendored() {}",
        );
    }

    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call("glob", json!({ "pattern": "**/*.rs" })),
        says("done"),
    ]);
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "what is in here").await;

    assert_eq!(
        completed_calls(&requests),
        vec!["glob".to_owned()],
        "the glob really ran and really listed the tree"
    );
    let systems = systems(&requests);
    assert_eq!(
        systems[1], systems[0],
        "a listing names files nobody asked to work in: {}",
        systems[1]
    );
}

/// Principle 3, at the file level: one enormous instruction file cannot spend
/// the whole context window, and the model is told what it is missing.
#[tokio::test]
async fn an_oversized_nested_file_is_clamped_and_says_so() {
    let (_directory, root, _guard) = checkout().await;
    let sub = root.join("sub");
    // Twice the shared one-shot budget, so the marker names a round number
    // this test does not have to compute a second way.
    let budget = ganja_core::tool::truncate::MAX_CHARS;
    plant(&sub.join("AGENTS.md"), &"x".repeat(budget * 2));
    plant(&sub.join("file.rs"), "fn main() {}");

    let (provider, requests) = ScriptedProvider::new(reads(&sub.join("file.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "look at sub").await;

    let system = systems(&requests).remove(1);
    assert!(
        system.contains(&format!("...{budget} bytes truncated...")),
        "the clamp says how much it cut"
    );
    assert!(
        system.contains("Read sub/AGENTS.md for the rest."),
        "and where the rest is"
    );
}

/// AC5: whatever weight the walk adds is weight `/context` reports, in the
/// instruction-file category — not invisible prompt that only shows up as a
/// surprise at the context ceiling. The P14 grid invariant holds throughout.
#[tokio::test]
async fn the_walked_in_weight_shows_up_in_the_context_breakdown() {
    let (_directory, root, _guard) = checkout().await;
    let sub = root.join("sub");
    plant(&sub.join("AGENTS.md"), &"nested rules. ".repeat(200));
    plant(&sub.join("file.rs"), "fn main() {}");

    let (provider, _) = ScriptedProvider::new(reads(&sub.join("file.rs")));
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let before = engine.context_breakdown().await;
    ask(&engine, &mut events, "look at sub").await;
    let after = engine.context_breakdown().await;

    assert!(
        after.instructions > before.instructions,
        "the walked-in file is priced as an instruction file: {before:?} -> {after:?}"
    );
    for breakdown in [&before, &after] {
        assert_eq!(
            breakdown.total(),
            breakdown.system_prompt
                + breakdown.instructions
                + breakdown.tools_builtin
                + breakdown.tools_mcp
                + breakdown.skills
                + breakdown.conversation_user
                + breakdown.conversation_assistant,
            "the grid still sums to the total it claims: {breakdown:?}"
        );
    }
}

/// The loaded set is the transcript. A process that died and came back reads
/// the same tool calls and walks the same files in — no side map to restore,
/// and nothing to lose by not restoring it.
#[tokio::test]
async fn a_resumed_session_walks_the_same_files_in_from_the_transcript_alone() {
    let (_directory, root, _guard) = checkout().await;
    let sub = root.join("sub");
    plant(&sub.join("AGENTS.md"), "sub rules");
    plant(&sub.join("file.rs"), "fn main() {}");

    let store = root.join("storage");
    let session = {
        let (provider, _) = ScriptedProvider::new(reads(&sub.join("file.rs")));
        let engine = Engine::persistent(
            provider,
            "fake-1",
            Arc::new(Registry::with_builtins()),
            permissive(),
            Storage::open(store.clone()),
        )
        .with_system_parts(Some("the composed prompt".to_owned()), None);
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        ask(&engine, &mut events, "look at sub/file.rs").await;

        engine.session_id()
    };

    // A second process over the same store, which has touched nothing itself.
    let (provider, requests) = ScriptedProvider::new(vec![says("still here")]);
    let engine = Engine::persistent(
        provider,
        "fake-1",
        Arc::new(Registry::with_builtins()),
        permissive(),
        Storage::open(store.clone()),
    )
    .with_system_parts(Some("the composed prompt".to_owned()), None);
    engine.resume(&session).await.expect("the session resumes");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events, "carry on").await;

    let system = systems(&requests).remove(0);
    assert_eq!(
        named(&system, "sub/AGENTS.md"),
        1,
        "the resumed session read the same transcript and walked the same file in: {system}"
    );
}

/// The other half of the same claim: a revert that hides the touch hides the
/// instructions it walked in, and touching again brings them back.
#[tokio::test]
async fn a_revert_that_hides_the_touch_forgets_what_it_walked_in() {
    let (_directory, root, _guard) = checkout().await;
    let sub = root.join("sub");
    plant(&sub.join("AGENTS.md"), "sub rules");
    plant(&sub.join("file.rs"), "fn main() {}");

    let read = tool_call(
        "read",
        json!({ "filePath": sub.join("file.rs").to_string_lossy() }),
    );
    let (provider, requests) = ScriptedProvider::new(vec![
        read.clone(),
        says("done"),
        says("nothing to see"),
        read,
        says("done again"),
    ]);
    let engine = engine(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let anchor = ask(&engine, &mut events, "look at sub/file.rs")
        .await
        .expect("a turn starts with the message that asked for it");

    engine
        .send(Command::RevertTo {
            message_id: anchor,
            scope: RevertScope::Conversation,
        })
        .await
        .expect("the prompt is a checkpoint");
    // The revert announces itself before the next prompt is accepted.
    loop {
        if matches!(
            futures::StreamExt::next(&mut events).await,
            Some(Event::RevertChanged { .. })
        ) {
            break;
        }
    }

    ask(&engine, &mut events, "never mind").await;
    ask(&engine, &mut events, "look again").await;

    let systems = systems(&requests);
    assert_eq!(
        named(&systems[2], "sub/AGENTS.md"),
        0,
        "the touch is hidden, so what it walked in is gone: {}",
        systems[2]
    );
    assert_eq!(
        named(
            systems.last().expect("five requests were scripted"),
            "sub/AGENTS.md"
        ),
        1,
        "and touching again brings it back: {}",
        systems.last().expect("five requests were scripted")
    );
}

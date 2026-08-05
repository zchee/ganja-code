//! The undo drill: `/undo` puts the working tree back byte for byte, `/redo`
//! puts it forward again, and the prompt that follows an undo carries none of
//! what the undo took back.
//!
//! The main drill is the only test here that redirects `XDG_DATA_HOME`, so it
//! is the only one that has to mind the process it shares with the rest of
//! this binary — the snapshot repository and the session store both hang off
//! that variable, and neither may land in the real user's data directory.
//! nextest already gives every test its own process; this file's other tests
//! stay env-mutation-free so a plain `cargo test` run of this binary is safe
//! too.
//!
//! The main drill's turn is driven by the **fake provider playing a
//! script**, wrapped in a recorder so the requests it was handed can be read
//! back afterwards: the worktree comparison alone is blind to history, and
//! half of what an undo has to do is keep a prompt the user took back out of
//! the next request.
//!
//! `git` on `PATH` is a prerequisite rather than a skip, for the golden
//! suite's reason: a run that snapshotted nothing would prove nothing.

use std::{
    path::{Path, PathBuf},
    process::Command as Process,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, EngineError, Snapshots, Storage,
    permission::{Action, Permissions, Rule},
    project::Project,
    protocol::{Command, Event, Role},
    provider::{ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent},
    tool::Registry,
};
use tokio_util::sync::CancellationToken;

/// What the tracked file says before the turn, and what an undo has to put
/// back exactly.
const BEFORE: &str = "the original line\n";

/// What the scripted turn edits it to.
const AFTER: &str = "the edited line\n";

/// What the scripted turn writes into a file that did not exist, and which an
/// undo therefore has to delete rather than restore.
const CREATED: &str = "a file the turn invented\n";

/// The prompt the undo takes back. Distinctive enough that finding it in a
/// later request is unambiguous.
const UNDONE: &str = "make the change I asked for";

/// The prompt that follows the undo.
const KEPT: &str = "never mind, do this instead";

/// How long any single event may take to arrive before the drill gives up.
const PATIENCE: Duration = Duration::from_secs(30);

/// Plays a script through the real fake provider and keeps every request it
/// was handed.
struct Recorder {
    inner: FakeProvider,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl Provider for Recorder {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.seen
            .lock()
            .expect("the request log is never poisoned")
            .push(request.clone());

        self.inner.stream(request, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undone_turn_leaves_neither_its_files_nor_its_prompt_behind() {
    let data = tempfile::tempdir().expect("a temporary data home");
    let project = tempfile::tempdir().expect("a temporary project");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
    }

    let root = project.path();
    let tracked = root.join("tracked.txt");
    let fresh = root.join("invented.txt");
    std::fs::write(&tracked, BEFORE).expect("the fixture file is writable");
    seed_repository(root);

    let script = root.join("script.json");
    std::fs::write(&script, turns(&tracked, &fresh)).expect("the script is writable");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Recorder {
        inner: FakeProvider::default().with_script(&script),
        seen: Arc::clone(&seen),
    });

    // The three tools the script calls ask by default. The drill is about
    // undo, not about the gate, and the gate has suites of its own.
    let mut permissions = Permissions::default();
    permissions.set_baseline(
        ["read", "edit", "write"]
            .into_iter()
            .map(|tool| Rule {
                permission: tool.to_owned(),
                pattern: "*".to_owned(),
                action: Action::Allow,
            })
            .collect(),
    );

    let resolved = Project::resolve(root);
    let snapshots = Snapshots::new(&resolved, true);
    assert!(
        snapshots.enabled(),
        "the drill needs git on PATH and a checkout to snapshot: {:?}",
        snapshots.notice()
    );
    let storage = Storage::open(
        resolved
            .data_dir()
            .expect("the redirected data home resolves")
            .join("storage"),
    );
    let engine = Engine::persistent(
        provider,
        "canned",
        Arc::new(Registry::with_builtins()),
        permissions,
        storage,
    )
    .with_snapshots(Arc::new(snapshots));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // ---- the turn -------------------------------------------------------

    settled(
        &engine,
        Command::SendPrompt {
            text: UNDONE.to_owned(),
            mentions: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    finish(&mut events).await;

    assert_eq!(read(&tracked), AFTER, "the scripted turn edits the file");
    assert_eq!(read(&fresh), CREATED, "the scripted turn writes a new file");

    // ---- undo -----------------------------------------------------------

    settled(&engine, Command::Undo)
        .await
        .expect("there is a turn to undo");
    let reverted = next(&mut events).await;
    let Event::RevertChanged {
        session_id: _,
        revert: Some(revert),
        prompt,
    } = &reverted
    else {
        panic!("an undo announces where the revert stands, got {reverted:?}");
    };
    assert_eq!(
        prompt.as_deref(),
        Some(UNDONE),
        "the editor is offered the prompt the undo took back"
    );
    assert_eq!(
        sorted(&revert.files),
        vec!["invented.txt".to_owned(), "tracked.txt".to_owned()],
        "both files the turn touched are named"
    );

    assert_eq!(
        std::fs::read(&tracked).expect("the tracked file survives an undo"),
        BEFORE.as_bytes(),
        "an undone edit restores the file byte for byte"
    );
    assert!(
        !fresh.exists(),
        "a file the undone turn created is not in the snapshot it is restored from, \
         so undoing the turn removes it"
    );

    // ---- redo -----------------------------------------------------------

    settled(&engine, Command::Redo)
        .await
        .expect("there is an undo to redo");
    let restored = next(&mut events).await;
    assert!(
        matches!(restored, Event::RevertChanged { revert: None, .. }),
        "stepping past the newest reverted prompt clears the revert, got {restored:?}"
    );
    assert_eq!(
        std::fs::read(&tracked).expect("the tracked file survives a redo"),
        AFTER.as_bytes(),
        "a redo restores the whole tree the undo was taken from"
    );
    assert_eq!(read(&fresh), CREATED, "including the file it had deleted");

    // ---- undo again, then keep it ---------------------------------------

    settled(&engine, Command::Undo)
        .await
        .expect("a redone turn can be undone again");
    let _ = next(&mut events).await;

    let before_the_second_prompt = seen
        .lock()
        .expect("the request log is never poisoned")
        .len();
    settled(
        &engine,
        Command::SendPrompt {
            text: KEPT.to_owned(),
            mentions: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    finish(&mut events).await;

    // The worktree comparison above cannot see this: an undo that restored
    // every file and left the prompt in the transcript would send the model
    // the very request the user just took back.
    let asked: Vec<String> = {
        let requests = seen.lock().expect("the request log is never poisoned");
        requests
            .get(before_the_second_prompt)
            .expect("the second prompt asks the model")
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .filter_map(|message| message.parts.iter().find_map(|part| part.as_text()))
            .map(ToOwned::to_owned)
            .collect()
    };

    assert!(
        !asked.iter().any(|text| text.contains(UNDONE)),
        "the undone prompt must not ride into the request that replaces it: {asked:?}"
    );
    assert!(
        asked.iter().any(|text| text.contains(KEPT)),
        "the prompt that was actually sent has to be in it: {asked:?}"
    );

    // And the revert is over rather than merely quiet: what was hidden has
    // been deleted, so there is nothing left to step forward into.
    assert!(
        matches!(
            settled(&engine, Command::Redo).await,
            Err(EngineError::NothingToRedo)
        ),
        "a prompt after an undo makes the undo permanent"
    );
}

/// What the checkout-refusal drill's scripted turn writes into a file that
/// did not exist before the turn.
const CHECKOUT_REFUSAL_WRITE: &str = "a file this drill's turn invented\n";

/// A one-turn script: write one new file, done. There is nothing here for an
/// undo to plausibly need beyond the file it wrote — this drill is about
/// whether `Command::Undo` is reachable at all, not about what it restores.
fn single_write_turn(fresh: &Path) -> String {
    serde_json::json!({
        "cadence_ms": 0,
        "turns": [
            {
                "text": "Writing it.",
                "tool_calls": [
                    {"name": "write", "args": {
                        "filePath": fresh.to_string_lossy(),
                        "content": CHECKOUT_REFUSAL_WRITE,
                    }},
                ],
            },
        ]
    })
    .to_string()
}

/// **Contrapositive of the drill above.** That one proves an engine that
/// *is* handed a `Snapshots` can undo inside a checkout; this proves an
/// engine that never was cannot — regardless of how real the checkout
/// underneath it is. `seed_repository` gives this one an actual commit, and
/// the engine is built with `Engine::new`, the same constructor the golden
/// harness's own engine uses, with no `.with_snapshots(...)` call anywhere
/// in reach. If `Command::Undo` ever became reachable by some path other
/// than an explicitly wired `Snapshots`, this is what would catch it.
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_never_handed_snapshots_refuses_an_undo_even_in_a_checkout() {
    let project = tempfile::tempdir().expect("a temporary project");
    let root = project.path();
    let fresh = root.join("invented.txt");
    // `seed_repository` commits whatever is on disk; an empty tree has
    // nothing to commit, so this checkout needs a file before it has a
    // commit to be real.
    std::fs::write(root.join("README"), "the state before anything\n")
        .expect("the fixture file is writable");
    seed_repository(root);

    let script = root.join("script.json");
    std::fs::write(&script, single_write_turn(&fresh)).expect("the script is writable");
    let provider = Arc::new(FakeProvider::default().with_script(&script));

    let mut permissions = Permissions::default();
    permissions.set_baseline(
        ["write"]
            .into_iter()
            .map(|tool| Rule {
                permission: tool.to_owned(),
                pattern: "*".to_owned(),
                action: Action::Allow,
            })
            .collect(),
    );

    let engine = Engine::new(
        provider,
        "canned",
        Arc::new(Registry::with_builtins()),
        permissions,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    settled(
        &engine,
        Command::SendPrompt {
            text: "write the file".to_owned(),
            mentions: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    finish(&mut events).await;

    assert_eq!(
        read(&fresh),
        CHECKOUT_REFUSAL_WRITE,
        "the scripted turn ran and left something an undo could plausibly restore"
    );

    assert!(
        matches!(
            settled(&engine, Command::Undo).await,
            Err(EngineError::NoSnapshots)
        ),
        "an engine nobody handed a Snapshots instance must refuse an undo even though \
         the directory underneath it is a real, committed git checkout"
    );
}

/// The fake provider's script: read, then edit and write, then two plain
/// replies — one closing the first turn, one for the prompt that follows the
/// undo.
fn turns(tracked: &Path, fresh: &Path) -> String {
    serde_json::json!({
        "cadence_ms": 0,
        "turns": [
            {
                "text": "Reading it first.",
                "tool_calls": [
                    {"name": "read", "args": {"filePath": tracked.to_string_lossy()}}
                ],
            },
            {
                "text": "Changing both.",
                "tool_calls": [
                    {"name": "edit", "args": {
                        "filePath": tracked.to_string_lossy(),
                        "oldString": BEFORE.trim_end(),
                        "newString": AFTER.trim_end(),
                    }},
                    {"name": "write", "args": {
                        "filePath": fresh.to_string_lossy(),
                        "content": CREATED,
                    }},
                ],
            },
            {"text": "Done."},
            {"text": "Done again."},
        ]
    })
    .to_string()
}

/// A checkout with one commit in it, which is what makes the directory a
/// project worth snapshotting.
///
/// Every setting the fixture depends on is passed with `-c` rather than left
/// to whatever the machine's global git config says, so the drill measures
/// ganja and not somebody's `commit.gpgsign`.
fn seed_repository(root: &Path) {
    let common = [
        "-c",
        "user.name=ganja drill",
        "-c",
        "user.email=drill@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=",
        "-c",
        "init.defaultBranch=main",
    ];

    for arguments in [
        vec!["init"],
        vec!["add", "-A"],
        vec!["commit", "-m", "the state before anything"],
    ] {
        let status = Process::new("git")
            .args(common)
            .args(&arguments)
            .current_dir(root)
            .output()
            .expect("git is a prerequisite of this drill");
        assert!(
            status.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn sorted(files: &[String]) -> Vec<String> {
    let mut sorted = files.to_vec();
    sorted.sort();

    sorted
}

/// The engine's answer to `command`, once it is done being busy.
///
/// [`Event::MessageFinished`] is queued **before** the turn slot is released
/// (`session.rs::run_turn`), and that order is deliberate: releasing the slot
/// first opens a window where the next turn's opening events overtake the
/// finish, which is the P3 finding
/// `persistence.rs::a_finish_is_never_overtaken_by_the_next_turns_events`
/// exists to pin. The cost the engine documents at that seam is that `Busy`
/// stays observable for the moment the send takes, so a client acting the
/// instant it sees a finish waits it out — which is what `persistence.rs`
/// does at the same boundary, for the same reason.
///
/// Bounded by the drill's patience, so an engine that never goes idle fails
/// this test loudly instead of spinning in it. Retrying is safe because every
/// command here answers `Busy` before it has done anything.
async fn settled(engine: &Engine, command: Command) -> Result<(), EngineError> {
    let deadline = Instant::now() + PATIENCE;

    loop {
        match engine.send(command.clone()).await {
            Err(EngineError::Busy) if Instant::now() < deadline => {
                tokio::task::yield_now().await;
            }
            answer => return answer,
        }
    }
}

/// The next event, or a failure that says the drill stalled rather than
/// hanging until the harness kills it.
async fn next(events: &mut BoxStream<'static, Event>) -> Event {
    tokio::time::timeout(PATIENCE, events.next())
        .await
        .expect("the engine answers within the drill's patience")
        .expect("the event stream outlives the engine")
}

/// Drains a turn to its finish event.
async fn finish(events: &mut BoxStream<'static, Event>) {
    loop {
        if let Event::MessageFinished { reason, error, .. } = next(events).await {
            assert!(
                error.is_none(),
                "the scripted turn finished {reason:?}: {error:?}"
            );

            return;
        }
    }
}

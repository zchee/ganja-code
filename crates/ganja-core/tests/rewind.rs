//! The rewind drill (**F7**): `Command::RevertTo` takes a session back to a
//! checkpoint the user picked, restoring exactly what the scope names.
//!
//! Three scopes, three questions, and each is asked against a real git
//! checkout with real snapshots:
//!
//! - `Both` is `/undo` with an anchor of its own — files on disk, messages
//!   hidden, a redo that puts them back, and a prompt that makes it permanent.
//! - `Conversation` moves the transcript and must not touch one byte of the
//!   working tree.
//! - `Files` moves the working tree and must not hide one message — the one
//!   genuinely new state, where the engine records no revert at all and a redo
//!   after it finds nothing to step through.
//!
//! Plus the honesty case the plan's own pre-mortem asks for: a patch naming a
//! path the checkout cannot restore is left out of what the event reports, so
//! the file list a frontend renders is what came back rather than what was
//! meant to.
//!
//! **Only the scope drill redirects `XDG_DATA_HOME`**, because the snapshot
//! repository hangs off it and must never land in the real user's data
//! directory. The refusal tests below it take no snapshots and touch no
//! stored state, so a plain `cargo test` run of this binary — which runs a
//! binary's tests on threads of one process — is safe too. `git` on `PATH` is
//! a prerequisite rather than a skip, for the golden suite's reason: a run
//! that snapshotted nothing would prove nothing.

use std::{
    path::{Path, PathBuf},
    process::Command as Process,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, EngineError, Snapshots,
    permission::{Action, Permissions, Rule},
    project::Project,
    protocol::{Command, Event, MessageId, RevertScope, Role},
    provider::FakeProvider,
    tool::Registry,
};

/// What the tracked file holds before the turn edits it.
///
/// Only the unix-gated achieved-files drill reads these two (and
/// [`editing_turns`], which scripts them) — on Windows the drill is compiled
/// out, so its helpers are gated with it or clippy's dead-code lint reds the
/// lint lane there.
#[cfg(unix)]
const BEFORE: &str = "the original line\n";

/// What the turn edits it to.
#[cfg(unix)]
const AFTER: &str = "the edited line\n";

/// What a scripted turn writes into a file that did not exist, and which a
/// rewind therefore deletes rather than restores.
const CREATED: &str = "a file the turn invented\n";

/// How long any single event may take to arrive before the drill gives up.
const PATIENCE: Duration = Duration::from_secs(30);

/// The first prompt of every seeded session.
const FIRST: &str = "make the first change";

/// The second, which is the checkpoint every scope drill rewinds to.
const SECOND: &str = "make the second change";

#[tokio::test(flavor = "multi_thread")]
async fn every_scope_restores_exactly_what_it_names_and_nothing_else() {
    let data = tempfile::tempdir().expect("a temporary data home");

    // SAFETY: this binary's other tests neither read nor write the
    // environment, and this one sets it before it builds anything that reads
    // it.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
    }

    both().await;
    conversation_only().await;
    files_only().await;
    #[cfg(unix)]
    achieved_files().await;
}

/// **Acceptance 7, `Both`.** The scope that is `/undo` with an anchor of the
/// user's own choosing: the second turn's file goes, the first turn's stays,
/// the messages are hidden rather than deleted, a redo puts everything back,
/// and the prompt after the next rewind is what makes it permanent.
async fn both() {
    let project = tempfile::tempdir().expect("a temporary project");
    let root = project.path();
    let (engine, mut events, _, second) = seeded(root).await;
    let first_file = root.join("first.txt");
    let second_file = root.join("second.txt");

    settled(
        &engine,
        Command::RevertTo {
            message_id: second.clone(),
            scope: RevertScope::Both,
        },
    )
    .await
    .expect("the second prompt is a checkpoint");

    let (revert, prompt) = revert_changed(&mut events).await;
    let revert = revert.expect("a rewind that hides messages announces where it stands");
    assert_eq!(revert.message_id, second, "the anchor is the one picked");
    assert_eq!(
        revert.files,
        vec!["second.txt".to_owned()],
        "only the checkpoint's own turn is restored"
    );
    assert_eq!(
        prompt.as_deref(),
        Some(SECOND),
        "rewinding and retyping a prompt is editing it, exactly as an undo is"
    );
    assert!(
        !second_file.exists(),
        "a file the rewound turn created is not in the tree it is restored from"
    );
    assert_eq!(
        read(&first_file),
        CREATED,
        "the turn before the checkpoint is untouched"
    );

    // Hidden, not deleted: a redo steps back forward and the file returns.
    settled(&engine, Command::Redo)
        .await
        .expect("there is a rewind to redo");
    let (revert, _) = revert_changed(&mut events).await;
    assert!(
        revert.is_none(),
        "stepping past the newest reverted prompt clears the revert"
    );
    assert_eq!(read(&second_file), CREATED, "and puts its file back");

    // ...and a prompt after a rewind is the user keeping what it did.
    settled(
        &engine,
        Command::RevertTo {
            message_id: second,
            scope: RevertScope::Both,
        },
    )
    .await
    .expect("the checkpoint is still there");
    revert_changed(&mut events).await;
    settled(
        &engine,
        Command::SendPrompt {
            text: "never mind, do this instead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    finish(&mut events).await;

    assert!(
        matches!(
            settled(&engine, Command::Redo).await,
            Err(EngineError::NothingToRedo)
        ),
        "a prompt after a rewind makes the rewind permanent"
    );
}

/// **Acceptance 7, `Conversation`.** The transcript moves and the working tree
/// does not — the event says so with an empty file list, which is the shape a
/// frontend already reads as "the conversation and not the checkout".
async fn conversation_only() {
    let project = tempfile::tempdir().expect("a temporary project");
    let root = project.path();
    let (engine, mut events, _, second) = seeded(root).await;
    let second_file = root.join("second.txt");

    settled(
        &engine,
        Command::RevertTo {
            message_id: second.clone(),
            scope: RevertScope::Conversation,
        },
    )
    .await
    .expect("the second prompt is a checkpoint");

    let (revert, prompt) = revert_changed(&mut events).await;
    let revert = revert.expect("a conversation rewind still announces where it stands");
    assert_eq!(revert.message_id, second);
    assert!(
        revert.files.is_empty(),
        "a conversation rewind put no file back, got {:?}",
        revert.files
    );
    assert_eq!(prompt.as_deref(), Some(SECOND));
    assert_eq!(
        read(&second_file),
        CREATED,
        "the file that turn wrote is exactly where it was"
    );

    // The messages really are hidden: a redo has something to step forward to.
    settled(&engine, Command::Redo)
        .await
        .expect("there is a rewind to redo");
    let (revert, _) = revert_changed(&mut events).await;
    assert!(revert.is_none(), "and stepping past it clears the revert");
}

/// **Acceptance 7, `Files`.** The one genuinely new state: the checkout moves,
/// nothing is hidden, and the engine records no revert — so a redo after it
/// finds nothing, and the transcript is still whole enough for an ordinary
/// `/undo` to anchor on the very message the rewind restored the files of.
async fn files_only() {
    let project = tempfile::tempdir().expect("a temporary project");
    let root = project.path();
    let (engine, mut events, _, second) = seeded(root).await;
    let second_file = root.join("second.txt");

    settled(
        &engine,
        Command::RevertTo {
            message_id: second.clone(),
            scope: RevertScope::Files,
        },
    )
    .await
    .expect("the second prompt is a checkpoint");

    let (revert, prompt) = revert_changed(&mut events).await;
    let revert = revert.expect("a code-only rewind names the files it put back");
    assert_eq!(revert.message_id, second);
    assert_eq!(revert.files, vec!["second.txt".to_owned()]);
    assert_eq!(
        prompt, None,
        "nothing was taken back, so there is nothing to offer the editor"
    );
    assert!(!second_file.exists(), "the file it created is gone");

    assert!(
        matches!(
            settled(&engine, Command::Redo).await,
            Err(EngineError::NothingToRedo)
        ),
        "nothing was hidden, so there is nothing to step forward through"
    );

    // Nothing was hidden, so the newest prompt is still the newest prompt —
    // which is what an undo anchors on. Had the code-only rewind recorded a
    // revert, this would have walked back to the *first* prompt instead.
    settled(&engine, Command::Undo)
        .await
        .expect("the transcript is whole, so there is something to undo");
    let (revert, _) = revert_changed(&mut events).await;
    assert_eq!(
        revert
            .expect("an undo announces where the revert stands")
            .message_id,
        second,
        "a code-only rewind moved no message"
    );
}

/// **Pre-mortem #2.** A patch names a path the checkout cannot restore, and
/// the event lists only what really came back. The picker renders that list,
/// so a rewind that half-applied must not read as one that worked.
///
/// Making a checkout fail is harder than it looks: `git checkout <tree> --
/// <path>` will happily delete a directory that has taken the path's place, so
/// an occupied path is not a failure. What it cannot do is write into a
/// directory it has no permission to write into — which is unix-only, and
/// which a process running as root is not subject to either, so the drill
/// checks its own premise before it asserts anything.
#[cfg(unix)]
async fn achieved_files() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = tempfile::tempdir().expect("a temporary project");
    let root = project.path();
    let sealed = root.join("sealed");
    let kept = sealed.join("kept.txt");
    std::fs::create_dir(&sealed).expect("the fixture directory is creatable");
    std::fs::write(&kept, BEFORE).expect("the fixture file is writable");
    seed_repository(root);

    let script = root.join("script.json");
    std::fs::write(&script, editing_turns(&kept, &root.join("invented.txt")))
        .expect("the script is writable");
    let (engine, mut events) = built(root, &script).await;

    settled(
        &engine,
        Command::SendPrompt {
            text: FIRST.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    let anchor = drain_turn(&mut events).await;
    assert_eq!(read(&kept), AFTER, "the scripted turn edited the file");
    assert_eq!(
        read(&root.join("invented.txt")),
        CREATED,
        "and wrote another"
    );

    // Nothing may be written in there any more, so the file the patch names
    // cannot be put back — while the file beside it, which the turn created
    // and the revert therefore deletes, still can be.
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500))
        .expect("the fixture directory's mode is settable");
    let unsealed = std::fs::write(sealed.join("probe"), "x").is_ok();
    if unsealed {
        // Root, or a filesystem that does not enforce the bit. The premise
        // this drill rests on does not hold here, and asserting anyway would
        // be asserting about the machine rather than about ganja.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700))
            .expect("the fixture directory's mode is settable back");
        return;
    }

    settled(
        &engine,
        Command::RevertTo {
            message_id: anchor,
            scope: RevertScope::Files,
        },
    )
    .await
    .expect("the prompt is a checkpoint");

    let (revert, _) = revert_changed(&mut events).await;
    let revert = revert.expect("the rewind announces what it restored");
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700))
        .expect("the fixture directory's mode is settable back");

    assert_eq!(
        revert.files,
        vec!["invented.txt".to_owned()],
        "the event names what came back, not what the patch intended"
    );
    assert_eq!(
        read(&kept),
        AFTER,
        "refusing to restore is never a reason to destroy what is there"
    );
}

/// **Acceptance 7, the refusal.** An id that is not a user message in the live
/// window is named back rather than resolved to the nearest one that is.
///
/// Conversation scope on purpose: it needs no snapshots, so what this proves
/// is the anchor check and not a session that could not have rewound anyway.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_to_something_that_is_not_a_checkpoint_is_refused_by_name() {
    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    settled(
        &engine,
        Command::SendPrompt {
            text: "the only prompt there is".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    let (anchor, reply) = turn_ids(&mut events).await;

    let unknown = MessageId::from("msg_nobody".to_owned());
    match settled(
        &engine,
        Command::RevertTo {
            message_id: unknown.clone(),
            scope: RevertScope::Conversation,
        },
    )
    .await
    {
        Err(EngineError::NoSuchCheckpoint { id }) => assert_eq!(id, unknown),
        other => panic!("an id nothing answers to is refused by name, got {other:?}"),
    }

    // An assistant message is in the window and is still not a checkpoint: a
    // rewind stops at a prompt, and the reply to one is not that.
    assert!(
        matches!(
            settled(
                &engine,
                Command::RevertTo {
                    message_id: reply,
                    scope: RevertScope::Conversation,
                },
            )
            .await,
            Err(EngineError::NoSuchCheckpoint { .. })
        ),
        "a reply is not a checkpoint"
    );

    // And the prompt itself is, on a session that takes no snapshots at all —
    // which is exactly what a conversation-only rewind does not need.
    settled(
        &engine,
        Command::RevertTo {
            message_id: anchor,
            scope: RevertScope::Conversation,
        },
    )
    .await
    .expect("a conversation rewind needs no snapshots");
}

/// **The other half of that refusal.** A scope that moves files on a session
/// with no snapshots is refused rather than half-served: moving the transcript
/// while leaving the checkout would be a rewind that only half happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_scope_that_moves_files_is_refused_without_snapshots() {
    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    settled(
        &engine,
        Command::SendPrompt {
            text: "the only prompt there is".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await
    .expect("an idle engine accepts a prompt");
    let anchor = drain_turn(&mut events).await;

    for scope in [RevertScope::Both, RevertScope::Files] {
        assert!(
            matches!(
                settled(
                    &engine,
                    Command::RevertTo {
                        message_id: anchor.clone(),
                        scope,
                    },
                )
                .await,
                Err(EngineError::NoSnapshots)
            ),
            "{scope:?} has nowhere to restore from"
        );
    }
}

/// A project with a checkout, an engine over it with real snapshots, and two
/// turns already run: the ids handed back are the two prompts, oldest first.
async fn seeded(root: &Path) -> (Engine, BoxStream<'static, Event>, MessageId, MessageId) {
    std::fs::write(root.join("README"), "the state before anything\n")
        .expect("the fixture file is writable");
    seed_repository(root);

    let script = root.join("script.json");
    std::fs::write(
        &script,
        writing_turns(&root.join("first.txt"), &root.join("second.txt")),
    )
    .expect("the script is writable");
    let (engine, mut events) = built(root, &script).await;

    let mut prompts = Vec::new();
    for text in [FIRST, SECOND] {
        settled(
            &engine,
            Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
            },
        )
        .await
        .expect("an idle engine accepts a prompt");
        prompts.push(drain_turn(&mut events).await);
    }

    let second = prompts.pop().expect("two prompts were sent");
    let first = prompts.pop().expect("two prompts were sent");

    (engine, events, first, second)
}

/// An engine over `root` playing `script`, with the three tools these scripts
/// call allowed: the drill is about rewinding, not about the gate, and the
/// gate has suites of its own.
async fn built(root: &Path, script: &Path) -> (Engine, BoxStream<'static, Event>) {
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

    let engine = Engine::new(
        Arc::new(FakeProvider::default().with_script(script)),
        "canned",
        Arc::new(Registry::with_builtins()),
        permissions,
    )
    .with_snapshots(Arc::new(snapshots));
    let events = engine.subscribe().await.expect("the first subscriber wins");

    (engine, events)
}

/// Two prompts' worth of turns, each writing one new file and then closing.
fn writing_turns(first: &Path, second: &Path) -> String {
    serde_json::json!({
        "cadence_ms": 0,
        "turns": [
            {
                "text": "Writing the first.",
                "tool_calls": [
                    {"name": "write", "args": {
                        "filePath": first.to_string_lossy(),
                        "content": CREATED,
                    }},
                ],
            },
            {"text": "Done."},
            {
                "text": "Writing the second.",
                "tool_calls": [
                    {"name": "write", "args": {
                        "filePath": second.to_string_lossy(),
                        "content": CREATED,
                    }},
                ],
            },
            {"text": "Done."},
            {"text": "Done again."},
            {"text": "And again."},
        ]
    })
    .to_string()
}

/// One prompt's worth: read the tracked file, edit it, and write a new one —
/// so the turn's patch names both a path its tree holds and one it does not.
#[cfg(unix)]
fn editing_turns(tracked: &Path, fresh: &Path) -> String {
    serde_json::json!({
        "cadence_ms": 0,
        "turns": [
            {
                "text": "Changing both.",
                "tool_calls": [
                    {"name": "read", "args": {"filePath": tracked.to_string_lossy()}},
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

/// The engine's answer to `command`, once it is done being busy.
///
/// [`Event::MessageFinished`] is queued **before** the turn slot is released,
/// so `Busy` stays observable for the moment that send takes; every command
/// here answers `Busy` before it has done anything, which is what makes
/// retrying safe. Bounded by the drill's patience, so an engine that never
/// goes idle fails loudly instead of spinning.
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

/// Drains a turn to its finish event, answering the id of the user message it
/// started with — the checkpoint that turn belongs to.
async fn drain_turn(events: &mut BoxStream<'static, Event>) -> MessageId {
    turn_ids(events).await.0
}

/// The same, keeping the assistant's id too: the two messages a turn is made
/// of, which is what a test asking "is a reply a checkpoint" needs.
async fn turn_ids(events: &mut BoxStream<'static, Event>) -> (MessageId, MessageId) {
    let mut prompt = None;
    let mut reply = None;

    loop {
        match next(events).await {
            Event::MessageStarted { message, .. } => match message.role {
                Role::User => {
                    prompt.get_or_insert(message.id);
                }
                Role::Assistant => {
                    reply.get_or_insert(message.id);
                }
            },
            Event::MessageFinished { reason, error, .. } => {
                assert!(
                    error.is_none(),
                    "the scripted turn finished {reason:?}: {error:?}"
                );

                return (
                    prompt.expect("a turn starts with the message that asked for it"),
                    reply.expect("and answers with one of its own"),
                );
            }
            _ => {}
        }
    }
}

/// Drains a turn to its finish event without caring what started it.
async fn finish(events: &mut BoxStream<'static, Event>) {
    drain_turn(events).await;
}

/// The next `RevertChanged`, as its two payloads.
async fn revert_changed(
    events: &mut BoxStream<'static, Event>,
) -> (Option<ganja_core::protocol::RevertInfo>, Option<String>) {
    loop {
        if let Event::RevertChanged { revert, prompt, .. } = next(events).await {
            return (revert, prompt);
        }
    }
}

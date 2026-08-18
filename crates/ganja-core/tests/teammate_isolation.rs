//! What a second [`Engine`] over the lead's store shares, and what it must
//! never share (**D500**).
//!
//! The teammate shape decided by the W5a/L0 spike is cheap precisely because so
//! much of a conversation is already per-engine. That cheapness is also the
//! hazard: three of the four things below are true *by construction* today, and
//! a later reader tidying the construction path would break them without a
//! single test going red. So they are pinned here as behaviour — a teammate's
//! read log, a teammate's refusal to undo, and the fact that a teammate's turn
//! lands in the shared store under its own root session while the lead's event
//! stream never mentions it.
//!
//! Every test owns a [`Storage`] under a temporary directory handed straight to
//! the two engines, so nothing here reads the environment for a path and
//! nothing mutates it — which is why this binary may hold more than one test.
//! The provider claims `"fake"` so that a first completed turn takes the
//! fallback title instead of spending a request the scripts do not carry.

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    Engine, EngineError, SessionId, Storage,
    permission::Permissions,
    protocol::{Command, Event, PartBody, Role, ToolState},
    provider::{FakeProvider, Provider},
    teammate::Teammate,
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, drain, drain_allowing, says, tool_call};

/// How long the lead's stream is watched for an event it must never receive.
/// Generous: the teammate's turn has already finished by the time this runs, so
/// anything crossing engines would already be queued.
const NOTHING_ARRIVES: Duration = Duration::from_millis(200);

/// A store rooted in a directory that vanishes with the test. The directory
/// handle travels back because dropping it deletes the tree.
fn store() -> (tempfile::TempDir, Storage) {
    let dir = ganja_testkit::temp_dir();
    let storage = Storage::open(dir.path().join("storage"));

    (dir, storage)
}

/// The lead: a persistent engine over `storage`, holding `tools`.
fn lead(provider: Arc<dyn Provider>, tools: &Arc<Registry>, storage: Storage) -> Engine {
    Engine::persistent(
        provider,
        "recorder-model",
        Arc::clone(tools),
        Permissions::default(),
        storage,
    )
}

/// A teammate over the same store and the *same* registry `Arc` — the sharing
/// invariant, exercised rather than described.
fn teammate(provider: Arc<dyn Provider>, tools: &Arc<Registry>, storage: Storage) -> Teammate {
    Teammate::new(
        "worker",
        provider,
        "recorder-model",
        Arc::clone(tools),
        Permissions::default(),
        storage,
    )
}

async fn prompt(engine: &Engine, text: &str) {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
}

/// The last tool error in a drained turn, or [`None`] if every call succeeded.
fn tool_error(seen: &[Event]) -> Option<String> {
    seen.iter().rev().find_map(|event| match event {
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool {
                state: ToolState::Error { error, .. },
                ..
            } => Some(error.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// **Invariant 1.** `Engine`'s read log is built fresh at construction rather
/// than passed in, so two engines over one store hold two logs. The rule the
/// log enforces — a file must have been read *this session* before it is
/// overwritten — would be worth nothing if a teammate's read counted for the
/// lead: one conversation would be unlocking another's writes, and the model
/// that overwrote the file would never have seen what was in it.
///
/// Told in three moves, because "the write was refused" alone is satisfied by a
/// gate that refuses everything: the teammate reads, the lead is refused by
/// name, and then the teammate writes the same file successfully.
#[tokio::test]
async fn a_teammates_read_does_not_unlock_the_leads_write() {
    let workspace = ganja_testkit::temp_dir();
    let shared = workspace.path().join("notes.md");
    std::fs::write(&shared, "as the teammate found it").expect("the fixture writes");
    let (_dir, storage) = store();
    let tools = Arc::new(Registry::with_builtins());

    let (lead_provider, _) = ScriptedProvider::named(
        "fake",
        vec![
            tool_call(
                "write",
                serde_json::json!({
                    "filePath": shared.to_string_lossy(),
                    "content": "as the lead would have left it",
                }),
            ),
            says("I could not"),
        ],
    );
    let (teammate_provider, _) = ScriptedProvider::named(
        "fake",
        vec![
            tool_call(
                "read",
                serde_json::json!({"filePath": shared.to_string_lossy()}),
            ),
            says("read it"),
            tool_call(
                "write",
                serde_json::json!({
                    "filePath": shared.to_string_lossy(),
                    "content": "as the teammate left it",
                }),
            ),
            says("wrote it"),
        ],
    );

    let lead = lead(lead_provider, &tools, storage.clone());
    let worker = teammate(teammate_provider, &tools, storage.clone());
    let mut lead_events = lead.subscribe().await.expect("the first subscriber wins");
    let mut worker_events = worker
        .engine()
        .subscribe()
        .await
        .expect("the first subscriber wins");

    prompt(worker.engine(), "read the notes").await;
    let read = drain_allowing(worker.engine(), &mut worker_events).await;
    assert_eq!(
        tool_error(&read),
        None,
        "the teammate's own read should have succeeded"
    );

    prompt(&lead, "overwrite the notes").await;
    let refused = drain_allowing(&lead, &mut lead_events).await;
    let refusal = tool_error(&refused).expect("the lead's write should have been refused");
    assert!(
        refusal.contains("has not been read this session; read it first"),
        "the lead's write was refused for some other reason: {refusal}"
    );
    assert_eq!(
        std::fs::read_to_string(&shared).expect("the file survives a refused write"),
        "as the teammate found it",
        "a refused write must leave the file exactly as it was"
    );

    prompt(worker.engine(), "now rewrite them").await;
    let wrote = drain_allowing(worker.engine(), &mut worker_events).await;
    assert_eq!(
        tool_error(&wrote),
        None,
        "the teammate read the file, so the teammate may write it"
    );
    assert_eq!(
        std::fs::read_to_string(&shared).expect("the file is there"),
        "as the teammate left it",
        "the read that unlocks a write is the one the same conversation made"
    );
}

/// **Invariant 2.** A teammate engine is built without snapshots and cannot be
/// given any afterwards — [`Teammate`] hands out only a shared reference, and
/// every `with_*` builder consumes the engine. So `/undo` refuses, and the
/// refusal is the intended answer rather than a gap: two engines walking one
/// worktree's snapshot store is a hazard with no upside, since a teammate
/// putting the lead's files back is not a feature anybody asked for.
#[tokio::test]
async fn a_teammate_engine_refuses_undo() {
    let (_dir, storage) = store();
    let worker = teammate(
        Arc::new(FakeProvider::new("done", Duration::ZERO)),
        &Arc::new(Registry::new(Vec::new())),
        storage,
    );

    assert!(
        matches!(
            worker.engine().send(Command::Undo).await,
            Err(EngineError::NoSnapshots)
        ),
        "a teammate that takes no snapshots must say so rather than move its transcript"
    );
    assert!(
        matches!(
            worker.engine().send(Command::Redo).await,
            Err(EngineError::NoSnapshots)
        ),
        "and the same for the other direction"
    );
}

/// The spike's own exit gate: a teammate session is constructed, takes one turn
/// against the fake provider, and is torn down — with the two claims that make
/// the shape correct checked on the way through.
///
/// The store is one store: both conversations are rows in it, both are **root**
/// rows (`parent: None`, which is what `ganja sessions` lists and what
/// `--session <id>` reopens), and the teammate's transcript is under the
/// teammate's own id. The event streams are two streams: the lead's names the
/// lead's session and nothing else, because every event an engine emits reads
/// that engine's own session stamp. That is the whole reason this shape was
/// chosen over a child turn on the lead's engine, whose events would have
/// carried the lead's id.
#[tokio::test]
async fn a_teammate_session_runs_one_turn_against_the_fake_provider_and_settles() {
    let (_dir, storage) = store();
    let tools = Arc::new(Registry::new(Vec::new()));

    let lead = lead(
        Arc::new(FakeProvider::new("the lead answers", Duration::ZERO)),
        &tools,
        storage.clone(),
    );
    // Subscribed before either engine is prompted: the birth queue goes to the
    // first lossless subscriber, so a runner that prompted first would have to
    // claim the buffer afterwards. Subscribing first is what this suite pins.
    let mut lead_events = lead.subscribe().await.expect("the first subscriber wins");

    prompt(&lead, "the lead's own turn").await;
    let lead_seen = drain(&mut lead_events).await;

    let worker = teammate(
        Arc::new(FakeProvider::new("the teammate answers", Duration::ZERO)),
        &tools,
        storage.clone(),
    );
    let mut worker_events = worker
        .engine()
        .subscribe()
        .await
        .expect("the first subscriber wins");
    let worker_session = worker.engine().session_id();
    assert_ne!(
        worker_session,
        lead.session_id(),
        "two engines must not name one session"
    );

    prompt(worker.engine(), "the teammate's own turn").await;
    let worker_seen = drain(&mut worker_events).await;
    assert!(
        worker.shutdown(Duration::from_secs(5)).await,
        "the teammate's turn should have settled well inside the limit"
    );

    for event in &lead_seen {
        assert_eq!(
            event.session_id(),
            &lead.session_id(),
            "an event on the lead's stream named another session"
        );
    }
    for event in &worker_seen {
        assert_eq!(
            event.session_id(),
            &worker_session,
            "an event on the teammate's stream named another session"
        );
    }
    // The teammate's whole turn is behind us, so anything that crossed engines
    // is already queued: what this waits for is the absence itself.
    assert!(
        tokio::time::timeout(NOTHING_ARRIVES, lead_events.next())
            .await
            .is_err(),
        "the lead's stream received something after the teammate's turn"
    );

    let sessions = storage.list_sessions().expect("the shared store lists");
    let ids: Vec<&SessionId> = sessions.iter().map(|info| &info.id).collect();
    assert_eq!(
        sessions.len(),
        2,
        "one store, two conversations, two rows: {ids:?}"
    );
    assert!(
        sessions.iter().all(|info| info.parent.is_none()),
        "a teammate is a conversation somebody may resume, so both rows are roots"
    );
    assert!(
        ids.contains(&&worker_session) && ids.contains(&&lead.session_id()),
        "the rows are the two engines' own sessions: {ids:?}"
    );

    let transcript = storage
        .load_transcript(&worker_session)
        .expect("the teammate's transcript reads back");
    assert!(
        transcript.iter().any(|message| message.role == Role::User
            && message
                .parts
                .iter()
                .filter_map(ganja_core::protocol::Part::as_text)
                .any(|text| text.contains("the teammate's own turn"))),
        "the teammate's prompt should be in the shared store under the teammate's id"
    );
}

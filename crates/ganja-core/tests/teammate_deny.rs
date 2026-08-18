//! A teammate is bound by the lead's own rules, and a stored deny is where
//! that stops being a convention (**D-5**).
//!
//! D-8 gives a teammate a session of its own, and therefore a turn of its own,
//! which takes it out of the reach of `ganja-permission`'s standing rule for
//! delegated work. The route that opens is laundering: a call the lead was
//! refused, made by a teammate instead. This is that route, driven end to end
//! through the door a lead really spawns through — the registry, the mailbox,
//! the runner and the teammate's own engine — with the deny where a person's
//! own answers live, in the project's stored ruleset on disk.
//!
//! Three things are asserted, because the refusal alone would be satisfied by
//! a teammate that could not do anything at all: the call was refused **as tool
//! output** and the turn carried on past it, **no dialog** was raised on either
//! side — a deny is not a question — and the file the call named was never
//! written.
//!
//! It mutates `XDG_DATA_HOME`, which is process-wide (`Permissions::load`
//! resolves the project's stored answers beneath it), so it holds exactly one
//! test.

use std::{path::Path, sync::Arc, time::Duration};

use ganja_core::{
    Storage,
    permission::{self, Permissions},
    project::Project,
    protocol::{Part, PartBody, Role, ToolState},
    teammate::posture,
    tool::Registry,
};
use ganja_testkit::{RecordedSpawns, caller_with, eventually, spawn_with_prompt, team_with};

/// How long a claim about the runner is waited for before it is a failure. The
/// runner polls every 500 ms, so a spawn, a pass and a turn fit comfortably and
/// a real regression still fails in seconds rather than hanging.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// How long the lead's dialog surface is watched for a question it must never
/// be asked. The turn it would have come from has already finished by then.
const NOTHING_ARRIVES: Duration = Duration::from_millis(200);

/// Writes the project's stored ruleset — the tier a person's own answers land
/// in, and the one a spawn must not be able to step around.
fn store_deny(project: &Path, rule: serde_json::Value) {
    let directory = Project::resolve(project)
        .data_dir()
        .expect("the redirected data home is writable");
    std::fs::create_dir_all(&directory).expect("the store directory is creatable");
    std::fs::write(
        directory.join(permission::FILE),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": permission::VERSION,
            "rules": [rule],
        }))
        .expect("the ruleset encodes"),
    )
    .expect("the ruleset writes");
}

/// Every tool call the teammate's transcript ended up recording, as
/// `(tool, state)` pairs a test can read.
fn calls(storage: &Storage, session: &ganja_core::SessionId) -> Vec<(String, ToolState)> {
    storage
        .load_transcript(session)
        .expect("the transcript reads")
        .iter()
        .flat_map(|message| message.parts.clone())
        .filter_map(|part| match part.body {
            PartBody::Tool { tool, state, .. } => Some((tool, state)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_teammate_cannot_do_what_the_leads_rules_deny() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment while this writes it.
    let _data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    let project = ganja_testkit::temp_dir();
    std::fs::create_dir(project.path().join(".git")).expect("the fixture repository is creatable");
    let forbidden = project.path().join("notes.md");

    // The lead's own answer, on disk, in the tier that outranks every ruleset
    // an agent could bring: `write` is refused, whatever it names.
    store_deny(
        project.path(),
        serde_json::json!({ "permission": "write", "pattern": "*", "action": "deny" }),
    );
    let lead = Arc::new(std::sync::Mutex::new(Permissions::load(project.path())));

    let storage = Storage::open(project.path().join("storage"));
    // Two steps: the call the rules refuse, and the step that reads the refusal
    // and says so. The second is what proves the turn carried on — a refusal is
    // information, never a turn abort.
    let (provider, _) = ganja_testkit::ScriptedProvider::named(
        "fake",
        vec![
            ganja_testkit::tool_call(
                "write",
                serde_json::json!({
                    "filePath": forbidden.to_string_lossy(),
                    "content": "as the teammate would have left it",
                }),
            ),
            ganja_testkit::says("the rules refuse that"),
        ],
    );
    // Through the gated door, which is the only one there is: the registry's
    // own spawn is crate-internal so that nothing can start a teammate the
    // permission gate never saw. The posture closure is the seam this whole
    // lane lands in: the teammate's engine takes the lead's ruleset, derived
    // rather than invented.
    let (_root, _team, registry, door) = team_with(
        project.path(),
        provider,
        Arc::new(Registry::with_builtins()),
        storage.clone(),
        {
            let lead = Arc::clone(&lead);
            move |_| {
                posture::permissions_for(
                    &lead
                        .lock()
                        .expect("the permission rules are never poisoned"),
                    Vec::new(),
                )
            }
        },
    );
    // The lead's dialog surface really exists, so "nobody was asked" is read
    // off a channel that could have carried the question rather than off the
    // absence of a channel.
    let (dialogs, mut asked) = tokio::sync::mpsc::channel(4);
    registry.forward_dialogs_to(dialogs);
    // A third place a question could have gone, so "nobody was asked" is read
    // off every surface that could have carried one rather than off one of
    // them. The teammate works inside the project and asks for no bypass, so
    // the spawn gate has nothing to raise either.
    let spawn_asks = RecordedSpawns::default();

    door.start(
        spawn_with_prompt("worker", None, "leave a note in notes.md"),
        // The lead's own live handle — the rules the deny was stored in,
        // which is what the gate has to be judging by.
        &caller_with(project.path(), Arc::clone(&lead)),
        &spawn_asks,
    )
    .await
    .expect("an in-process teammate spawns");
    spawn_asks.asked_nobody();

    let session = eventually(
        EVENTUALLY,
        "the teammate's own session to exist",
        async || {
            storage
                .list_sessions()
                .expect("the store lists")
                .first()
                .map(|info| info.id.clone())
        },
    )
    .await;
    eventually(
        EVENTUALLY,
        "the teammate to read the refusal and answer it",
        async || {
            storage
                .load_transcript(&session)
                .expect("the transcript reads")
                .iter()
                .any(|message| {
                    message.role == Role::Assistant
                        && message
                            .parts
                            .iter()
                            .filter_map(Part::as_text)
                            .any(|text| text.contains("the rules refuse that"))
                })
                .then_some(())
        },
    )
    .await;

    let recorded = calls(&storage, &session);
    let [(tool, ToolState::Error { error, .. })] = recorded.as_slice() else {
        panic!("the call should have been refused as tool output, and was: {recorded:?}");
    };
    assert_eq!(tool, "write");
    assert!(
        error.contains("prevents you from using this specific tool call"),
        "the model is told a rule refused it: {error}"
    );

    assert!(
        !forbidden.exists(),
        "a refused write leaves the file it named untouched"
    );
    assert!(
        tokio::time::timeout(NOTHING_ARRIVES, asked.recv())
            .await
            .is_err(),
        "a deny is not a question, so nobody is asked one"
    );

    registry.shutdown().await;
}

//! A teammate from the spawn that registers it to the shutdown that retires it
//! (**AC-10**).
//!
//! One test, because it is one claim: §4.1's spawn, §6.1's runner and §6.2's
//! teardown are a single loop, and asserting them apart would be asserting that
//! three functions exist rather than that a teammate works. Every step is
//! observed from outside the engine — the mailbox on disk, the shared store,
//! and the registry's own view — because that is all a lead ever sees of an
//! in-process teammate.
//!
//! Mutates `XDG_DATA_HOME`, so it holds exactly one test.

use std::sync::Arc;
use std::time::Duration;

use ganja_core::Storage;
use ganja_core::permission::Permissions;
use ganja_core::protocol::Role;
use ganja_core::protocol::team::{Frame, ShutdownRequest};
use ganja_core::provider::FakeProvider;
use ganja_core::tool::Registry;
use ganja_team::{LEAD, MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::{AllowSpawn, TASK, caller, eventually, spawn, team_file, team_with};

/// How long a claim about the runner is waited for before it is a failure. The
/// runner polls every 500 ms, so two passes plus a turn fit inside this
/// comfortably and a real regression still fails in seconds rather than hanging.
const EVENTUALLY: Duration = Duration::from_secs(20);

#[tokio::test]
async fn a_teammate_runs_from_a_spawn_through_idle_to_shutdown() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment while this writes it.
    let _data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    // Through the gated door, which is the only one there is: the registry's
    // own spawn is crate-internal so that nothing can start a teammate the
    // permission gate never saw. The store is this test's own handle, so every
    // claim below reads the very sessions the teammate writes.
    let (root, team, registry, door) = team_with(
        home.path(),
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        storage.clone(),
        |_| Permissions::default(),
    );
    let caller = caller(home.path());

    // §4.1: the spawn registers the member, seeds the inbox with the task and
    // returns at once — the call does not wait for any of the work.
    let spawned = door
        .start(spawn("worker", Some("in-process")), &caller, &AllowSpawn)
        .await
        .expect("an in-process teammate spawns");
    assert_eq!(spawned.name, "worker");
    assert_eq!(spawned.agent_id, "worker@session-abcd1234");

    let member = team_file(&root, &team)
        .expect("the team file is there")
        .member("worker")
        .cloned()
        .expect("the spawn wrote a member record");
    assert_eq!(member.prompt.as_deref(), Some(TASK));
    assert_eq!(member.tmux_pane_id, "in-process");
    assert_eq!(
        registry.view().members.iter().filter(|view| view.is_lead).count(),
        1,
        "a roster has exactly one lead, and it is not the teammate"
    );

    // §6.1's first pass: the seeded task leaves the inbox and becomes a turn.
    let worker = MemberName::parse("worker").expect("a member name");
    let inbox = root.inbox_path(&team, &worker);
    eventually(EVENTUALLY, "the seeded task to be drained from the inbox", async || {
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty().then_some(())
    })
    .await;
    let session = eventually(EVENTUALLY, "the teammate's own session to exist", async || {
        storage.list_sessions().expect("the store lists").first().map(|info| info.id.clone())
    })
    .await;
    eventually(EVENTUALLY, "the seeded task to reach the teammate's transcript", async || {
        storage
            .load_transcript(&session)
            .expect("the transcript reads")
            .iter()
            .any(|message| {
                message.role == Role::User
                    && message
                        .parts
                        .iter()
                        .filter_map(ganja_core::protocol::Part::as_text)
                        .any(|text| text.contains(TASK))
            })
            .then_some(())
    })
    .await;

    // A message written after the first turn reaches the next one.
    mailbox::write(&inbox, MailboxMessage::new(LEAD, "and then the lexer", record::now_iso8601()))
        .expect("the lead writes to the teammate's inbox");
    eventually(EVENTUALLY, "the second message to reach the teammate's transcript", async || {
        storage
            .load_transcript(&session)
            .expect("the transcript reads")
            .iter()
            .any(|message| {
                message
                    .parts
                    .iter()
                    .filter_map(ganja_core::protocol::Part::as_text)
                    .any(|text| text.contains("and then the lexer"))
            })
            .then_some(())
    })
    .await;

    // §6.2: a shutdown request is answered to the lead's own inbox, and the
    // teammate stops being listed.
    let shutdown = Frame::ShutdownRequest(ShutdownRequest {
        request_id: "req-1".to_owned(),
        from: LEAD.to_owned(),
        reason: Some("that is enough".to_owned()),
        timestamp: record::now_iso8601(),
    });
    mailbox::write(
        &inbox,
        MailboxMessage::from_frame(LEAD, &shutdown, record::now_iso8601())
            .expect("a frame encodes"),
    )
    .expect("the lead writes the shutdown request");

    let lead_inbox = registry.lead_inbox();
    eventually(EVENTUALLY, "the teammate to answer the shutdown", async || {
        (!mailbox::read(&lead_inbox).expect("the lead's inbox reads").valid.is_empty())
            .then_some(())
    })
    .await;

    let answered = mailbox::read(&lead_inbox).expect("the lead's inbox reads");
    assert_eq!(answered.valid.len(), 1, "one answer, not a retry storm");
    let answer = &answered.valid[0];
    assert_eq!(answer.from, "worker", "the answer is stamped with the teammate's own name");
    let Some(Frame::ShutdownApproved(approved)) = answer.frame() else {
        panic!("the lead was told something other than a shutdown answer");
    };
    assert_eq!(approved.request_id, "req-1");
    assert_eq!(approved.from, "worker");
    assert_eq!(approved.pane_id.as_deref(), Some("in-process"));
    assert_eq!(approved.backend_type.as_deref(), Some("in-process"));

    eventually(EVENTUALLY, "the teammate to stop being listed", async || {
        (registry.running() == 0).then_some(())
    })
    .await;
    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "the request it answered is pruned, not answered again forever"
    );

    // The teardown is idempotent, and the whole registry comes down cleanly.
    registry.shutdown().await;
    assert_eq!(registry.view().members.len(), 1, "only the lead is left");
}

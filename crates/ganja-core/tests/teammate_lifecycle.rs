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

use std::{path::Path, sync::Arc, time::Duration};

use ganja_core::{
    Storage,
    permission::Permissions,
    protocol::{
        Role,
        team::{Frame, ShutdownRequest},
    },
    provider::FakeProvider,
    teammate::{InProcess, SpawnRequest, TeammateRegistry},
    tool::Registry,
};
use ganja_team::{
    LEAD, MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record, record::TeamFile,
};

/// How long a claim about the runner is waited for before it is a failure. The
/// runner polls every 500 ms, so two passes plus a turn fit inside this
/// comfortably and a real regression still fails in seconds rather than hanging.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// How often the assertion below re-reads what it is waiting for.
const CHECK: Duration = Duration::from_millis(25);

/// Waits for `claim` to hold, or fails saying what it was waiting for.
async fn eventually(what: &str, mut claim: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    while tokio::time::Instant::now() < deadline {
        if claim() {
            return;
        }
        tokio::time::sleep(CHECK).await;
    }

    panic!("waited {EVENTUALLY:?} for {what}, and it never happened");
}

/// The team file as it is on disk right now.
fn team_file(path: &Path) -> TeamFile {
    let text = std::fs::read_to_string(path).expect("the team file is there");

    serde_json::from_str(&text).expect("the team file reads back")
}

#[tokio::test]
async fn a_teammate_runs_from_a_spawn_through_idle_to_shutdown() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment while this writes it.
    let _data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));
    let backend = Arc::new(InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        storage.clone(),
        |_| Permissions::default(),
    ));

    // §4.1: the spawn registers the member, seeds the inbox with the task and
    // returns at once — the call does not wait for any of the work.
    let spawned = registry
        .spawn(
            backend,
            SpawnRequest {
                name: "worker".to_owned(),
                backend: ganja_core::teammate::DEFAULT_BACKEND,
                agent_type: "general".to_owned(),
                model: "recorder-model".to_owned(),
                color: None,
                prompt: "have a look at the parser".to_owned(),
                cwd: home.path().to_path_buf(),
                plan_mode_required: false,
                bypass: false,
            },
        )
        .await
        .expect("an in-process teammate spawns");
    assert_eq!(spawned.name.as_str(), "worker");
    assert_eq!(spawned.agent_id, "worker@session-abcd1234");

    let member = team_file(&root.config_path(&team))
        .member("worker")
        .cloned()
        .expect("the spawn wrote a member record");
    assert_eq!(member.prompt.as_deref(), Some("have a look at the parser"));
    assert_eq!(member.tmux_pane_id, "in-process");
    assert_eq!(
        registry
            .view()
            .members
            .iter()
            .filter(|view| view.is_lead)
            .count(),
        1,
        "a roster has exactly one lead, and it is not the teammate"
    );

    // §6.1's first pass: the seeded task leaves the inbox and becomes a turn.
    let worker = MemberName::parse("worker").expect("a member name");
    let inbox = root.inbox_path(&team, &worker);
    eventually("the seeded task to be drained from the inbox", || {
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty()
    })
    .await;
    eventually("the teammate's own session to exist", || {
        !storage.list_sessions().expect("the store lists").is_empty()
    })
    .await;

    let session = storage.list_sessions().expect("the store lists")[0]
        .id
        .clone();
    eventually("the seeded task to reach the teammate's transcript", || {
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
                        .any(|text| text.contains("have a look at the parser"))
            })
    })
    .await;

    // A message written after the first turn reaches the next one.
    mailbox::write(
        &inbox,
        MailboxMessage::new(LEAD, "and then the lexer", record::now_iso8601()),
    )
    .expect("the lead writes to the teammate's inbox");
    eventually(
        "the second message to reach the teammate's transcript",
        || {
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
        },
    )
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
    eventually("the teammate to answer the shutdown", || {
        !mailbox::read(&lead_inbox)
            .expect("the lead's inbox reads")
            .valid
            .is_empty()
    })
    .await;

    let answered = mailbox::read(&lead_inbox).expect("the lead's inbox reads");
    assert_eq!(answered.valid.len(), 1, "one answer, not a retry storm");
    let answer = &answered.valid[0];
    assert_eq!(
        answer.from, "worker",
        "the answer is stamped with the teammate's own name"
    );
    let Some(Frame::ShutdownApproved(approved)) = answer.frame() else {
        panic!("the lead was told something other than a shutdown answer");
    };
    assert_eq!(approved.request_id, "req-1");
    assert_eq!(approved.from, "worker");
    assert_eq!(approved.pane_id.as_deref(), Some("in-process"));
    assert_eq!(approved.backend_type.as_deref(), Some("in-process"));

    eventually("the teammate to stop being listed", || {
        registry.running() == 0
    })
    .await;
    assert!(
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "the request it answered is pruned, not answered again forever"
    );

    // The teardown is idempotent, and the whole registry comes down cleanly.
    registry.shutdown().await;
    assert_eq!(registry.view().members.len(), 1, "only the lead is left");
}

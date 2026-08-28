//! A teammate's conversation is a conversation, not a delegated turn (**D-8**,
//! **D500**, **AC-25**).
//!
//! What that means in the surface a person actually uses: `ganja sessions`
//! lists it, and `ganja run --session <id>` opens it and carries on. Both
//! follow from one fact about the row a teammate's engine writes — its `parent`
//! is [`None`] — and `sessions_command` filters on exactly that, so a teammate
//! whose row carried a parent would be invisible to the listing and to anybody
//! looking for the id to resume.
//!
//! # Why the teammate is built here rather than spawned through a door
//!
//! It is built with the constructor both doors — the `task` tool's `name`
//! argument and the `/team spawn` dialog — call, so the row is what is under
//! test, not the door. What is exercised end to end is everything from the
//! row outward: the listing, the resume, and the transcript the resumed run
//! appended to.
//!
//! # The environment stays in the children
//!
//! Nothing here calls `std::env::set_var`. The children are given their own
//! data home and the store is *found* under it rather than computed from a
//! layout this file would otherwise have to be taught again on every change —
//! `id_collision.rs`'s rule, for its reason. So this binary may hold more than
//! one test.
//!
//! # The pane half
//!
//! [`a_pane_teammates_own_process_writes_a_row_that_is_listed_and_resumable`]
//! is AC-25's other leg, and it drives the row from where a pane really writes
//! it: **a second `ganja` process launched with §4.1's flags**, in a pty of
//! its own, reading the inbox its lead seeded. What it is *not* is a tmux
//! pane. The window is tmux's to split and kill; what a pane's process
//! does inside it — resolve the launch line, join the team, take the seeded
//! task as its first turn, tell the lead it went idle, answer the shutdown
//! request and leave — is this crate's and the TUI's, and is what the row a
//! resume opens depends on. So the test launches the binary itself as the
//! member, hands it a `TMUX_PANE` the way tmux would, and reads back the
//! frames and the store. `/team spawn w1 --backend ganja` runs this very
//! launch line, and AC-11's own binary (`teammate_pane.rs`) drives it
//! through that door on a private server.
//!
//! The team is reached through `ganja_core::team` — the crate re-exports
//! `ganja-team` under that name — so no second dependency is needed to seed
//! an inbox or read the lead's back.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use expectrl::{Eof, Expect as _, Session, process::unix::WaitStatus, session::OsSession};
use futures::StreamExt as _;
use ganja_core::Storage;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command as EngineCommand, Role};
use ganja_core::provider::FakeProvider;
use ganja_core::teammate::{SETTLE, Teammate};
use ganja_core::tool::Registry;
#[cfg(unix)]
use ganja_core::{
    protocol::PartBody,
    provider::fake,
    team::{MailboxMessage, MemberName, Spawn, Surface, mailbox, record},
    teammate::TeammateRegistry,
};
#[cfg(unix)]
use ganja_protocol::team::{Frame, IdleReason, ShutdownRequest};
use ganja_testkit::Homes;
use serde_json::json;

/// The script the child runs play. One turn, one word.
const SCRIPT: &str = "script.json";

/// What the fake provider says, appearing nowhere else.
const REPLY: &str = "child-turn-zarquon";

/// What the teammate is asked, and what the resumed run adds after it. Both
/// are read back out of one transcript, which is what makes "it opened *that*
/// session" a fact rather than an exit code.
const TEAMMATE_PROMPT: &str = "the teammate's own first turn";
const RESUMED_PROMPT: &str = "the same conversation, opened again";

/// The shared project/data pair, with this suite's one-turn script written
/// at birth.
struct Fixture {
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let homes = Homes::new();
        homes.script(SCRIPT, json!([{"text": REPLY}]));

        Self { homes }
    }

    fn path(&self) -> &Path {
        self.homes.project()
    }

    /// The binary, pinned to this fixture's own directories
    /// ([`Homes::pin`]): a developer's global config can choose a provider,
    /// and their cached catalog can decide what a model is sized at.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        self.homes.pin(&mut command, &self.path().join(SCRIPT));

        command
    }

    /// Runs the binary and hands back its standard output, failing here rather
    /// than downstream when it did not exit 0.
    fn run(&self, arguments: &[&str]) -> String {
        let output = self.ganja().args(arguments).output().expect("the binary is runnable");

        assert!(
            output.status.success(),
            "`ganja {}` exited {}\n--- stderr ---\n{}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        String::from_utf8(output.stdout).expect("the binary writes UTF-8")
    }

    /// The store the runs wrote into — found rather than computed
    /// ([`Homes::store`]).
    fn store(&self) -> Storage {
        self.homes.store()
    }
}

/// D-8's second half, in the surface it is claimed about.
///
/// The lead's own run is here to give the store a second root row: a listing
/// that showed the teammate because it shows everything would prove nothing
/// about `parent`, and two rows is what makes the filter's answer visible.
#[tokio::test]
async fn a_teammate_session_is_listed_and_resumable_on_both_backends() {
    let fixture = Fixture::new();

    // A first ordinary run, which is also what creates the store.
    fixture.run(&["run", "the lead's own turn"]);
    let storage = fixture.store();

    // The teammate: a second engine over a clone of that same handle, which is
    // the D500 shape. Its row is written by the engine's own lazy create, and
    // that is the whole of why it is a root.
    let teammate = Teammate::new(
        "worker",
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage.clone(),
    );
    // The birth queue is a lossless lane, and one nobody drains fills and then
    // makes the teammate's own turn wait.
    let mut events = teammate.engine().subscribe().await.expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    let session = teammate.engine().session_id();
    teammate
        .engine()
        .send(EngineCommand::SendPrompt {
            text: TEAMMATE_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(
        teammate.shutdown(SETTLE).await,
        "the teammate's turn should have settled well inside the limit"
    );

    // Listed: `ganja sessions` shows roots only, so the id being here *is* the
    // claim that the row carries no parent.
    let listed = fixture.run(&["sessions"]);
    assert!(listed.contains(session.as_str()), "the teammate's session should be listed: {listed}");

    // Resumable: the binary opens that id and adds to it. Nothing about the
    // exit code says *which* session was opened, so the transcript does.
    fixture.run(&["run", "--session", session.as_str(), RESUMED_PROMPT]);

    let transcript =
        storage.load_transcript(&session).expect("the teammate's transcript reads back");
    let said: Vec<&str> = transcript
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect();

    assert!(
        said.iter().any(|text| text.contains(TEAMMATE_PROMPT)),
        "the teammate's own turn is in this transcript: {said:?}"
    );
    assert!(
        said.iter().any(|text| text.contains(RESUMED_PROMPT)),
        "and the resumed run continued the same conversation: {said:?}"
    );
}

/// The escape that opens the alternate screen — the same synchronization the
/// pty smoke tests wait on: past it, the app owns the terminal.
#[cfg(unix)]
const ALT_SCREEN: &str = "\x1b[?1049h";

/// How long the member's process is given to take the terminal, to finish its
/// seeded turn, and to leave once told to.
#[cfg(unix)]
const DEADLINE: Duration = Duration::from_secs(20);

/// §2.1's own example session, so the team on disk is `session-224cbeab` and a
/// reader can find it by hand.
#[cfg(unix)]
const LEAD_SESSION: &str = "224cbeab-4e62-497c-aa8f-d05cc33ce7ba";

/// The pane id handed to the member the way tmux hands one to every pane it
/// runs — through the environment, never the launch line — so that the
/// `shutdown_approved` it writes names the pane the lead would kill.
#[cfg(unix)]
const PANE: &str = "%99";

/// Everything the lead's inbox holds, decoded, oldest first.
#[cfg(unix)]
fn lead_heard(lead_inbox: &Path) -> Vec<Frame> {
    mailbox::read(lead_inbox)
        .expect("the lead's inbox reads")
        .valid
        .iter()
        .filter_map(MailboxMessage::frame)
        .collect()
}

/// Reads whatever the member has drawn since the last read, and drops it.
///
/// A terminal nobody reads is a pipe nobody reads: the app draws at frame
/// rate, the pty's buffer fills, and its next `write` blocks the loop that
/// would have polled the inbox. Every wait here that is not an `expect` drains
/// through this.
#[cfg(unix)]
fn drain(session: &mut OsSession) {
    let mut chunk = [0u8; 65536];
    while let Ok(read) = session.try_read(&mut chunk) {
        if read == 0 {
            break;
        }
    }
}

/// **AC-25's pane leg.** A `ganja` process launched with §4.1's flags is a
/// member of the team those flags name: it takes the seeded task as its first
/// turn, tells the lead it went idle, answers the lead's shutdown request
/// naming its pane, and leaves — and the row it wrote is a root `ganja
/// sessions` lists and `ganja run --session` resumes.
///
/// A pty rather than a pipe because the member is the full terminal UI: the
/// launch line is exactly what a lead composes, and running it any other way
/// would test a program nobody launches.
#[cfg(unix)]
#[test]
fn a_pane_teammates_own_process_writes_a_row_that_is_listed_and_resumable() {
    let fixture = Fixture::new();
    // The lead's own run, so the listing has a second root row and the store
    // exists before the member joins it.
    fixture.run(&["run", "the lead's own turn"]);
    let storage = fixture.store();
    let lead_row = storage
        .list_sessions()
        .expect("the store lists")
        .into_iter()
        .map(|session| session.id)
        .collect::<Vec<_>>();
    assert_eq!(lead_row.len(), 1, "one row so far, the lead's");

    // The team as a lead of `LEAD_SESSION` would keep it under this config
    // home, and the member's inbox seeded exactly as a spawn seeds it (§4.1
    // step 5): the prompt travels through the mailbox, never the launch line.
    let config_home = fixture.homes.config_home();
    let team = TeammateRegistry::for_session(&config_home, LEAD_SESSION, fixture.path());
    let member = MemberName::parse("w1").expect("a member name");
    let inbox = team.root().inbox_path(team.team(), &member);
    let lead_inbox = team.lead_inbox();
    mailbox::seed(&inbox).expect("the inbox is created");
    mailbox::write(
        &inbox,
        MailboxMessage::new(team.lead().as_str(), TEAMMATE_PROMPT, record::now_iso8601()),
    )
    .expect("the inbox takes the seed");
    mailbox::seed(&lead_inbox).expect("the lead's inbox is created");
    // And the member record, as the registry writes it once the split has
    // answered: the pane waits for this before it builds its engine, because
    // the model it runs is the record's.
    ganja_testkit::seed_team_file(
        team.root(),
        team.team(),
        LEAD_SESSION,
        fixture.path(),
        &[(
            member.clone(),
            Spawn {
                agent_type: "general".to_owned(),
                model: fake::MODEL.to_owned(),
                color: "blue".to_owned(),
                prompt: TEAMMATE_PROMPT.to_owned(),
                plan_mode_required: false,
                surface: Surface::Pane { id: PANE.to_owned() },
                cwd: fixture.path().display().to_string(),
            },
        )],
    );

    // The launch line, as `pane.rs` composes it — the flags `MemberArgs`
    // documents, and the pane id through the environment.
    let mut command = fixture.ganja();
    command
        .args([
            "--agent-id",
            &member.agent_id(team.team()),
            "--agent-name",
            member.as_str(),
            "--team-name",
            team.team().as_str(),
            "--agent-color",
            "blue",
            "--parent-session-id",
            LEAD_SESSION,
        ])
        .env("GANJA_CONFIG_HOME", &config_home)
        .env("TMUX_PANE", PANE)
        // The kitty keyboard probe (D517) would stall 2s unanswered here.
        .env("GANJA_DISABLE_TERM_PROBE", "1");
    let mut session = Session::spawn(command).expect("the member spawns in a pty");
    session.set_expect_timeout(Some(DEADLINE));
    session.get_process_mut().set_window_size(100, 30).expect("the pty is sized");
    session.expect(ALT_SCREEN).expect("the member never took its terminal over");

    // §10.3-2 and -3: the seed becomes the first turn with no mechanism of
    // its own, and the turn's end reaches the lead as a frame. The reply
    // drawn on screen is waited for first — which also keeps the pty read,
    // because a full-screen app whose output nobody drains blocks on its next
    // frame — and the frame is then read off the file, which is the contract.
    session.expect(REPLY).expect("the seeded turn never reached the member's transcript");
    let started = Instant::now();
    let idle = loop {
        drain(&mut session);
        if let Some(idle) = lead_heard(&lead_inbox).into_iter().find_map(|frame| match frame {
            Frame::IdleNotification(idle) => Some(idle),
            _ => None,
        }) {
            break idle;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "the member never told the lead its turn ended; its inbox holds {:?}",
            mailbox::read(&inbox).map(|held| held.valid.len())
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(idle.from, member.as_str());
    assert_eq!(idle.idle_reason, Some(IdleReason::Available));
    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "the seed was delivered and left the inbox"
    );

    // §10.3-4: the lead asks, the member answers naming its pane, and leaves
    // through the exit path it always had.
    mailbox::write(
        &inbox,
        MailboxMessage::from_frame(
            team.lead().as_str(),
            &Frame::ShutdownRequest(ShutdownRequest {
                request_id: "req-1".to_owned(),
                from: team.lead().as_str().to_owned(),
                reason: None,
                timestamp: record::now_iso8601(),
            }),
            record::now_iso8601(),
        )
        .expect("the frame encodes"),
    )
    .expect("the member's inbox takes the request");
    session.expect(Eof).expect("the member did not leave within the deadline");
    let status = session.get_process().wait().expect("the member is reaped");
    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        "a shutdown is a clean exit, got {status:?}"
    );
    let heard = lead_heard(&lead_inbox);
    match heard.last() {
        Some(Frame::ShutdownApproved(approved)) => {
            assert_eq!(approved.request_id, "req-1");
            assert_eq!(approved.from, member.as_str());
            assert_eq!(approved.pane_id.as_deref(), Some(PANE));
            assert_eq!(approved.backend_type.as_deref(), Some("tmux"));
        }
        other => panic!("the last thing the lead hears is the approval, got {other:?}"),
    }

    // D-8's second half, for the row a pane's process writes: a root, so the
    // listing shows it and a resume opens it.
    let session_id = storage
        .list_sessions()
        .expect("the store lists")
        .into_iter()
        .map(|session| session.id)
        .find(|id| !lead_row.contains(id))
        .expect("the member wrote a row of its own");
    let listed = fixture.run(&["sessions"]);
    assert!(
        listed.contains(session_id.as_str()),
        "the pane teammate's session should be listed: {listed}"
    );
    fixture.run(&["run", "--session", session_id.as_str(), RESUMED_PROMPT]);

    let transcript =
        storage.load_transcript(&session_id).expect("the member's transcript reads back");
    let user_parts: Vec<&PartBody> = transcript
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .map(|part| &part.body)
        .collect();
    assert!(
        user_parts.iter().any(|body| matches!(
            body,
            PartBody::Peer { from, body, .. } if from == team.lead().as_str() && body == TEAMMATE_PROMPT
        )),
        "the seeded task is in this transcript as the lead's own attributed words: {user_parts:?}"
    );
    assert!(
        user_parts
            .iter()
            .any(|body| matches!(body, PartBody::Text { text } if text.contains(RESUMED_PROMPT))),
        "and the resumed run continued the same conversation: {user_parts:?}"
    );
}

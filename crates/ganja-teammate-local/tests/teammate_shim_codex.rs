//! The **real** codex driver against a fake `codex` (**W3**, **D508**).
//!
//! The distinction from `teammate_shim.rs` is the whole point of this file
//! existing beside it: that suite drives the shim *core* with a fixture driver
//! and asserts the mechanism, while this one drives
//! [`ganja_teammate_local::codex::Codex`] itself — the argv a real turn
//! composes, the JSONL a real turn parses — against a script that answers in
//! the shapes a probed `codex-cli 0.149.0-alpha.1` actually printed.
//!
//! So the posture assertions here are about *this vendor's flags*, which is
//! what W3's own acceptance items are about, and the fake cannot be told where
//! to log through a flag of its own: the argv is codex's, so
//! [`shim_support::FakeCodex`] bakes its log path into the script instead.

mod shim_support;

use std::sync::Arc;
use std::time::Duration;

use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_teammate_local::codex::Codex;
use ganja_teammate_local::shim;
use ganja_testkit::AllowSpawn;
use shim_support::{FakeCodex, Mode, until};

/// How long a fake CLI gets to be started, answer, and have its answer land in
/// the lead's inbox. [`teammate_shim`]'s value, for its reason: the shim polls
/// twice a second and this is a shell script exec'd cold.
const ANSWERS: Duration = Duration::from_secs(20);

/// What every spawn here asks its teammate to do.
const TASK: &str = "hold the fort";

/// The recording AC-27 compares the shipped posture sentence against.
const PROBE: &str = include_str!("fixtures/codex-posture-probe.txt");

/// Reads the lead's inbox, whatever is in it.
fn lead_mail(root: &ganja_team::TeamsRoot, team: &ganja_team::TeamName) -> Vec<String> {
    let path = root.inbox_path(team, &MemberName::lead());
    mailbox::read(&path)
        .map(|contents| contents.valid.into_iter().map(|message| message.text).collect())
        .unwrap_or_default()
}

/// Puts one message into a member's inbox, as `from`.
fn send(root: &ganja_team::TeamsRoot, team: &ganja_team::TeamName, to: &str, text: &str) {
    let member = MemberName::parse(to).expect("a member name");
    let path = root.inbox_path(team, &member);
    mailbox::write(&path, MailboxMessage::new("team-lead", text.to_owned(), record::now_iso8601()))
        .expect("the message is written");
}

/// The lead, the fake and the team, wired to the real driver.
fn lead(
    home: &std::path::Path,
    cli: &FakeCodex,
) -> (
    Arc<ganja_core::teammate::TeammateRegistry>,
    Arc<ganja_core::Teammates>,
    ganja_team::TeamsRoot,
    ganja_team::TeamName,
) {
    lead_with_timeout(home, cli, None)
}

/// [`lead`], with the key that moves the per-turn deadline down to test scale.
fn lead_with_timeout(
    home: &std::path::Path,
    cli: &FakeCodex,
    timeout: Option<Duration>,
) -> (
    Arc<ganja_core::teammate::TeammateRegistry>,
    Arc<ganja_core::Teammates>,
    ganja_team::TeamsRoot,
    ganja_team::TeamName,
) {
    let (registry, door) =
        shim_support::lead_with_timeout(home, home, Arc::new(Codex::new()), cli.path(), timeout);
    let (root, team) = shim_support::team_of(&registry);

    (registry, door, root, team)
}

/// **AC-5.** `send_async_message` is what lets one turn say something before it
/// ends, so a turn can produce several `AgentMessage` items — and each becomes
/// one mail, in the order it arrived. Folding them into one would lose the
/// order, and dropping the mid-turn one would lose the half a lead is waiting
/// on.
#[tokio::test]
async fn every_agent_message_in_one_turn_becomes_one_lead_mail_in_arrival_order() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team).len() >= 2).await,
        "both messages arrive: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert_eq!(
        mail.iter().filter(|text| text.contains("starting on it") || text.contains("done")).count(),
        2,
        "{mail:?}"
    );
    let started =
        mail.iter().position(|text| text.contains("starting on it")).expect("the mid-turn message");
    let done = mail.iter().position(|text| text.contains("done")).expect("the final message");
    assert!(started < done, "arrival order is preserved: {mail:?}");
    // The item that was not a teammate talking is not mail: a `reasoning` item
    // is the model thinking, and a lead reading it as a peer's words would be
    // reading something nobody said to it.
    assert!(!mail.iter().any(|text| text.contains("thinking is not mail")), "{mail:?}");

    registry.shutdown().await;
}

/// **AC-15**, codex's half, end to end rather than over a composed value: the
/// first turn states the posture twice and the resume states it once, because
/// `codex exec resume` has no `-s` at all.
#[tokio::test]
async fn every_turn_carries_the_pinned_posture_and_the_second_resumes_the_first() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(
        until(ANSWERS, || cli.turns().len() == 1).await,
        "the spawn prompt is itself one message, so one turn is owed: {:?}",
        cli.received()
    );
    send(&root, &team, "w1", "and now the other thing");
    assert!(
        until(ANSWERS, || cli.turns().len() == 2).await,
        "a second message is a second child: {:?}",
        cli.received()
    );

    let turns = cli.turns();
    for argv in &turns {
        assert!(
            argv.contains(r#"-c sandbox_mode="read-only""#),
            "the sandbox override, quotes included: {argv}"
        );
        assert!(
            argv.contains(r#"-c approval_policy="never""#),
            "the approval override travels beside it: {argv}"
        );
        for refused in ganja_teammate_local::codex::NEVER_COMPOSED {
            assert!(
                !argv.split(' ').any(|token| token == refused),
                "{refused} must never be composed: {argv}"
            );
        }
    }
    assert!(
        turns[0].contains("-s read-only"),
        "the documented flag rides the first turn: {}",
        turns[0]
    );
    assert!(
        !turns[1].contains("-s read-only"),
        "and never the resume, which has no such flag: {}",
        turns[1]
    );
    assert!(
        turns[1].starts_with("exec resume thread-"),
        "the second turn resumes the thread the first revealed: {}",
        turns[1]
    );

    registry.shutdown().await;
}

/// **AC-21**, on this vendor's own argv: the prompt reaches the child on stdin,
/// because `-` is what codex calls that door, and never on a command line.
#[tokio::test]
async fn the_prompt_reaches_codex_on_stdin_and_never_in_its_argv() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    let secret = "sk-ant-not-a-real-key-0123456789";
    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), secret),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || !cli.records("stdin").is_empty()).await,
        "the prompt reached the child: {:?}",
        cli.received()
    );
    assert!(
        cli.records("stdin")[0].contains(secret),
        "and it reached it on stdin: {:?}",
        cli.records("stdin")
    );
    for argv in cli.records("argv") {
        assert!(!argv.contains(secret), "argv is for flags: {argv}");
    }

    registry.shutdown().await;
}

/// **AC-19.** Two teammates on one CLI hold conversations of their own, and the
/// second's resume names its own thread rather than the first's — the failure
/// this rules out is one member silently continuing another's conversation.
#[tokio::test]
async fn two_codex_teammates_hold_conversation_ids_of_their_own() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    for name in ["w1", "w2"] {
        door.start(
            ganja_testkit::spawn_with_prompt(name, Some("codex"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect("the fake codex spawns");
    }
    assert!(
        until(ANSWERS, || cli.turns().len() == 2).await,
        "both spawn prompts are taken: {:?}",
        cli.received()
    );
    for name in ["w1", "w2"] {
        send(&root, &team, name, "and now the other thing");
    }
    assert!(
        until(ANSWERS, || cli.turns().len() == 4).await,
        "and both second messages: {:?}",
        cli.received()
    );

    let resumes: Vec<String> = cli
        .turns()
        .into_iter()
        .filter(|argv| argv.starts_with("exec resume "))
        .map(|argv| {
            argv.split(' ').nth(2).expect("the id is the resume's own positional").to_owned()
        })
        .collect();
    assert_eq!(resumes.len(), 2, "{:?}", cli.turns());
    assert_ne!(resumes[0], resumes[1], "two members, two threads: {resumes:?}");
    // Neither ever asks for "the most recent" — the failure `--last` and
    // `--all` would introduce is exactly a member resuming somebody else's.
    for argv in cli.turns() {
        for refused in ["--last", "--all"] {
            assert!(!argv.split(' ').any(|token| token == refused), "{refused}: {argv}");
        }
    }

    registry.shutdown().await;
}

/// **AC-10**, codex's own arm: it is the one of the three CLIs that answers the
/// question cheaply, so it is the one that is asked. Without this a member
/// spawns, accepts a message, and reports an authentication failure a whole
/// turn later — having already told a person it existed.
#[tokio::test]
async fn a_spawn_refuses_by_name_when_codex_has_no_usable_login() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::logged_out();
    let (registry, door, ..) = lead(home.path(), &cli);

    let refusal = door
        .start(
            ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect_err("a logged-out codex refuses the spawn");
    let sentence = refusal.reason.clone();
    assert!(
        sentence.contains("codex login status"),
        "the refusal names the command that said so: {sentence}"
    );
    // The pre-check ran, and nothing else did: a refused spawn takes no turn.
    assert!(cli.records("argv").iter().any(|argv| argv == "login status"), "{:?}", cli.received());
    assert!(cli.turns().is_empty(), "{:?}", cli.turns());

    registry.shutdown().await;
}

/// **AC-10**'s other half: a `PATH` with no `codex` on it refuses by naming the
/// binary, which is the whole of what somebody needs in order to fix it.
#[tokio::test]
async fn a_spawn_refuses_by_name_when_no_codex_is_on_this_path() {
    let home = ganja_testkit::temp_dir();
    let empty = ganja_testkit::temp_dir();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(Codex::new()),
        std::ffi::OsString::from(empty.path().display().to_string()),
    );

    let refusal = door
        .start(
            ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect_err("a PATH without codex refuses the spawn");
    let sentence = refusal.reason.clone();
    assert!(sentence.contains("codex"), "{sentence}");
    assert!(sentence.contains(shim::REFUSED_NO_BINARY), "{sentence}");

    registry.shutdown().await;
}

/// **AC-8**, on codex's own shapes rather than the fixture driver's, for the
/// three arms a running child can take: a non-zero exit, a clean exit with
/// unreadable output, and the vendor's own startup refusal. Each becomes
/// structured failure mail, and in each the member survives to be spoken to
/// again — a failed turn is information, not a dead teammate.
#[tokio::test]
async fn a_failed_codex_turn_becomes_structured_mail_and_the_member_survives() {
    for (mode, expected) in [
        (Mode::Fail, "exit"),
        (Mode::Garbage, "codex"),
        // The vendor's own startup sentence, not merely "it exited": a startup
        // refusal must arrive as a refusal naming what the vendor said, never
        // as a parse failure or a bare status.
        (Mode::Refuse, "codex refuses to start here"),
    ] {
        let home = ganja_testkit::temp_dir();
        let cli = FakeCodex::install(mode);
        let (registry, door, root, team) = lead(home.path(), &cli);

        door.start(
            ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect("the spawn itself succeeds; the turn is what fails");

        assert!(
            until(ANSWERS, || !lead_mail(&root, &team).is_empty()).await,
            "{mode:?}: the lead is told: {:?}",
            cli.received()
        );
        let mail = lead_mail(&root, &team).join("\n");
        assert!(mail.contains(expected), "{mode:?}: the mail says what happened: {mail}");
        assert!(mail.contains("codex"), "{mode:?}: and which CLI: {mail}");

        // The member is still there, and the next message is a fresh attempt.
        let before = cli.turns().len();
        send(&root, &team, "w1", "try that again");
        assert!(
            until(ANSWERS, || cli.turns().len() > before).await,
            "{mode:?}: a failed turn leaves a member spawnable-to: {:?}",
            cli.received()
        );

        registry.shutdown().await;
    }
}

/// **AC-8**'s fourth arm and **AC-29**'s first: codex ships no timeout flag of
/// its own, so the deadline is the shim's — and the mail names both the
/// deadline that fired and the key that moves it, because whoever reads "ended
/// after 2s" needs the same line to say what to write if two seconds was wrong.
#[tokio::test]
async fn a_codex_turn_that_never_answers_is_ended_by_the_shims_own_deadline() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCodex::install(Mode::Hang);
    let (registry, door, root, team) =
        lead_with_timeout(home.path(), &cli, Some(Duration::from_secs(2)));

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || !lead_mail(&root, &team).is_empty()).await,
        "the deadline is what ends a child that stopped writing: {:?}",
        cli.received()
    );
    let mail = lead_mail(&root, &team).join("\n");
    assert!(mail.contains("2s"), "the deadline that fired: {mail}");
    assert!(mail.contains(shim::TIMEOUT_KEY), "and the key that moves it: {mail}");

    registry.shutdown().await;
}

/// **AC-27.** The sentence a person consents to at spawn equals codex's own
/// recorded probe answer, compared against the **recording** rather than
/// against a second string literal — two literals agreeing proves only that
/// somebody typed carefully.
///
/// The other two readers are tied to this one elsewhere and deliberately:
/// `shim::spawn_lines`' first line ends with `posture_line`'s sentence and the
/// spawn dialog inserts the same value (`subagent.rs`), which
/// `shim.rs`'s own `a_spawn_ring_line_carries_the_same_posture_the_dialog_does`
/// pins. So one table has one source, and this is the assertion that the source
/// is the measurement.
#[test]
fn the_codex_posture_sentence_is_the_one_its_probe_recorded() {
    let recorded = PROBE
        .lines()
        .find_map(|line| line.strip_prefix("sentence: "))
        .expect("the recording names the sentence it measured")
        .trim();
    let shipped = ganja_core::teammate::posture_line(ganja_protocol::team::MemberBackend::Codex)
        .expect("codex states its posture");

    assert_eq!(shipped, recorded);
    // And nothing about it is still a promissory note: the word the
    // pre-measurement wording carried must be gone.
    assert!(!shipped.contains("unmeasured"), "{shipped}");
    assert!(
        shim::spawn_lines(ganja_protocol::team::MemberBackend::Codex)[0].ends_with(recorded),
        "the ring line a spawn writes carries the measured sentence too"
    );
}

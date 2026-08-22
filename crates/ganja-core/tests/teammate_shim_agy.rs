//! The **real** agy driver against a fake `agy` (**Dv-7**, **D508**,
//! **D510**), and **AC-28**'s ship arm.
//!
//! This file used to assert the opposite. W4 measured `--sandbox` as a bound on
//! agy's terminal and not on its filesystem and refused to ship the backend at
//! all; Dv-7's user directive ships it anyway, at the honest posture, so what
//! this binary asserts now is that the surface exists and that what a person is
//! told about it is true. The measurement itself is unchanged and unrepealed —
//! the recording it produced is still what the shipped sentence is compared
//! against.
//!
//! The distinction from `teammate_shim.rs` is the same one that puts the codex
//! and grok suites beside it: that one drives the shim *core* with a fixture
//! driver and asserts the mechanism, while this one drives
//! [`ganja_core::teammate::agy::Agy`] itself — the launch line a real member is
//! started on, the stream-json a real turn parses — against a script answering
//! in the shapes a probed `agy 1.1.15` actually printed.
//!
//! It is also the only suite here whose CLI is [`Shape::Resident`], which is
//! what makes two of its tests unlike anything in the other two files: one
//! child serves many turns, and a child that stops answering is **replaced**
//! rather than mourned.

mod shim_support;

use std::{sync::Arc, time::Duration};

use ganja_core::{
    protocol::team::MemberBackend,
    teammate::{
        BACKENDS,
        agy::{self, Agy},
        backend_name, parse_backend, posture_line, shim,
    },
};
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::AllowSpawn;
use shim_support::{FakeAgy, Mode, until};

/// How long a fake CLI gets to be started, answer, and have its answer land in
/// the lead's inbox. The other two suites' value, for their reason: the shim
/// polls twice a second and this is a shell script exec'd cold.
const ANSWERS: Duration = Duration::from_secs(20);

/// What every spawn here asks its teammate to do.
const TASK: &str = "hold the fort";

/// The recording **AC-27** compares the shipped posture sentence against.
const PROBE: &str = include_str!("fixtures/agy-posture-probe.txt");

/// Reads the lead's inbox, whatever is in it.
fn lead_mail(root: &ganja_team::TeamsRoot, team: &ganja_team::TeamName) -> Vec<String> {
    let path = root.inbox_path(team, &MemberName::lead());
    mailbox::read(&path)
        .map(|contents| {
            contents
                .valid
                .into_iter()
                .map(|message| message.text)
                .collect()
        })
        .unwrap_or_default()
}

/// Puts one message into a member's inbox, as the lead.
fn send(root: &ganja_team::TeamsRoot, team: &ganja_team::TeamName, to: &str, text: &str) {
    let member = MemberName::parse(to).expect("a member name");
    let path = root.inbox_path(team, &member);
    mailbox::write(
        &path,
        MailboxMessage::new("team-lead", text.to_owned(), record::now_iso8601()),
    )
    .expect("the message is written");
}

/// A lead whose agy backend is the real driver pointed at `cli`.
fn lead(
    home: &std::path::Path,
    cli: &FakeAgy,
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
    cli: &FakeAgy,
    timeout: Option<Duration>,
) -> (
    Arc<ganja_core::teammate::TeammateRegistry>,
    Arc<ganja_core::Teammates>,
    ganja_team::TeamsRoot,
    ganja_team::TeamName,
) {
    let (registry, door) =
        shim_support::lead_with_timeout(home, home, Arc::new(Agy::new()), cli.path(), timeout);
    let (root, team) = shim_support::team_of(&registry);

    (registry, door, root, team)
}

/// Spawns `name` on the fake agy and waits for its first turn to have run.
async fn spawn(door: &Arc<ganja_core::Teammates>, home: &std::path::Path, name: &str, task: &str) {
    door.start(
        ganja_testkit::spawn_with_prompt(name, Some("agy"), task),
        &ganja_testkit::caller(home),
        &AllowSpawn,
    )
    .await
    .expect("the fake agy spawns");
}

/// **AC-28's name half**, which survived the reversal unchanged: the name
/// parses and is offered. What changed underneath it is that it now spawns.
#[test]
fn the_agy_name_parses_and_is_offered() {
    assert_eq!(parse_backend("agy"), Ok(MemberBackend::Agy));
    assert!(BACKENDS.contains(&"agy"));
    assert_eq!(backend_name(MemberBackend::Agy), "agy");
}

/// **AC-27.** The sentence a person is asked to approve is the one the probe
/// recorded — compared against the recording rather than against a second copy
/// of itself, because two literals agreeing proves only that somebody typed
/// carefully.
///
/// **AC-17** rides along: the same sentence opens the ring, so `/team` and the
/// spawn dialog cannot come to describe one grant differently.
#[test]
fn the_agy_posture_is_the_one_its_probe_recorded() {
    let recorded = PROBE
        .lines()
        .find_map(|line| line.strip_prefix("sentence: "))
        .expect("the recording names the sentence it measured")
        .trim();
    let shipped = posture_line(MemberBackend::Agy).expect("agy discloses a posture as of Dv-7");

    assert_eq!(shipped, recorded);
    // The two clauses Dv-7 exists to make honest. A sentence that lost either
    // would be describing a bound this backend does not have.
    assert!(
        shipped.contains("no enforced filesystem bound"),
        "{shipped}"
    );
    assert!(shipped.contains("write anywhere you can"), "{shipped}");
    // And the consequence a reader could not derive from the vendor's own
    // behaviour, because it is a fact about *this* build.
    assert!(shipped.contains("/undo"), "{shipped}");
    assert!(!shipped.contains("unmeasured"), "{shipped}");

    let lines = shim::spawn_lines(MemberBackend::Agy);
    assert!(
        lines.first().is_some_and(|line| line.ends_with(shipped)),
        "the ring opens with the same sentence: {lines:?}"
    );
}

/// **AC-15**, agy's half: the launch line is composed flag for flag, `-p` is
/// last with an empty value, and no never-composed spelling reaches it.
///
/// The `-p` clause is Dv-6 made executable. That flag takes the **next word**
/// as the prompt, so a line that put it anywhere else would silently ask agy a
/// question nobody typed — which is exactly how W4 found the trap, from a run
/// that answered a question about `--print-timeout`.
#[tokio::test]
async fn the_agy_launch_line_is_the_pinned_one_and_p_comes_last() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || !cli.records("argv").is_empty()).await,
        "{:?}",
        cli.received()
    );

    let argv = cli.records("argv").remove(0);
    let tokens: Vec<&str> = argv.split(' ').filter(|token| !token.is_empty()).collect();

    assert!(argv.contains("--input-format stream-json"), "{argv}");
    assert!(argv.contains("--output-format stream-json"), "{argv}");
    assert!(argv.contains("--sandbox"), "{argv}");
    assert!(argv.contains("--disable-slash-commands"), "{argv}");
    assert!(argv.contains("--print-timeout "), "{argv}");
    // `--mode` in either spelling: `plan` would neuter the writes this backend
    // ships to enable, and `accept-edits` would widen a grant the probe showed
    // nothing needs.
    assert!(!argv.contains("--mode"), "{argv}");
    assert_eq!(
        tokens.last(),
        Some(&"-p"),
        "`-p` is last, and its value is the empty string the shell drops: {argv}"
    );

    // Whole tokens rather than substrings, and the reason is one of the
    // entries: `\"--conversation\".contains(\"-c\")` is true, so a substring
    // check would report the one flag this driver must compose as the one it
    // must never.
    for banned in agy::NEVER_COMPOSED {
        assert!(
            !tokens.contains(&banned),
            "{banned} is never composed: {argv}"
        );
    }

    registry.shutdown().await;
}

/// **AC-29.** agy's own `--print-timeout` is ordered **strictly after** this
/// build's deadline, and one config key moves both.
///
/// Two timeouts bound one turn. If the vendor's fires first this side is left
/// reading a pipe that will never carry a `result`, so the ordering is the
/// whole point — and deriving the flag from the effective deadline is what
/// keeps it true when somebody raises `teammates.shim_turn_timeout` past agy's
/// own five-minute default.
#[tokio::test]
async fn the_composed_print_timeout_outlasts_the_deadline_that_moves_it() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let deadline = Duration::from_secs(90);
    let (registry, door, ..) = lead_with_timeout(home.path(), &cli, Some(deadline));

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || !cli.records("argv").is_empty()).await,
        "{:?}",
        cli.received()
    );

    let argv = cli.records("argv").remove(0);
    let composed = agy::print_timeout(deadline);

    assert_eq!(composed, "150s", "deadline + 1m, as a Go duration");
    assert!(
        argv.contains(&format!("--print-timeout {composed}")),
        "the config's deadline moved the vendor's flag too: {argv}"
    );
    // The ordering itself, stated as the arithmetic rather than as the two
    // literals: a unit is mandatory because `time.ParseDuration` refuses a
    // bare integer, and a child that will not start is not a bound.
    let seconds: u64 = composed
        .strip_suffix('s')
        .expect("a Go duration carries its unit")
        .parse()
        .expect("seconds");
    assert!(
        seconds > deadline.as_secs(),
        "this build's deadline must fire first"
    );

    registry.shutdown().await;
}

/// **AC-21.** No word anybody said reaches a command line — argv is
/// world-readable through `ps`, and a teammate's prompt is a peer's words.
///
/// The other half is asserted beside it, because "nothing is in argv" is
/// satisfied just as well by nothing arriving at all: the prompt did reach the
/// child, on the stdin line this shape is driven by.
#[tokio::test]
async fn no_prompt_text_reaches_the_command_line() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);
    let secret = "the words a peer said, which argv is world-readable through ps";

    spawn(&door, home.path(), "w1", secret).await;
    assert!(
        until(ANSWERS, || !cli.records("line").is_empty()).await,
        "{:?}",
        cli.received()
    );

    for argv in cli.records("argv") {
        assert!(!argv.contains(secret), "argv is for flags: {argv}");
        assert!(
            !argv.split(' ').any(|token| token == "w1"),
            "never a title: {argv}"
        );
    }
    let lines = cli.records("line");
    assert!(
        lines.iter().any(|line| line.contains(secret)),
        "and the prompt did arrive, on stdin: {lines:?}"
    );
    // The vendor's own inbound shape, keyed on `event` rather than on `type`.
    assert!(
        lines[0].contains("\"event\":\"user\"") && lines[0].contains("\"content\""),
        "{:?}",
        lines[0]
    );

    registry.shutdown().await;
}

/// One inbox message is one NDJSON line is one turn — **on one child**.
///
/// The claim that separates this shape from the other two suites': a second
/// message does not start a second process. Asserted on the argv record, of
/// which there is exactly one however many turns run.
#[tokio::test]
async fn one_resident_child_takes_every_turn_this_member_is_sent() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || cli.records("line").len() == 1).await,
        "the spawn prompt is itself one message, so one turn is owed: {:?}",
        cli.received()
    );
    send(&root, &team, "w1", "and now the other thing");
    assert!(
        until(ANSWERS, || cli.records("line").len() == 2).await,
        "a second message is a second line: {:?}",
        cli.received()
    );

    assert_eq!(
        cli.records("argv").len(),
        1,
        "and both turns ran on one child: {:?}",
        cli.received()
    );
    assert_eq!(
        cli.conversations().len(),
        1,
        "which holds one conversation: {:?}",
        cli.received()
    );
    // Both answers reached the lead, one mail per turn — which is the whole of
    // what this wire carries, since a `step_update` has no text in it.
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .filter(|text| text.contains("answered"))
            .count()
            == 2)
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// **AC-19.** Two agy teammates hold two conversations.
///
/// The failure it rules out is one driver holding one id: these drivers are
/// stateless unit structs, and the conversation lives in the runner, so two
/// members are two runners and two children and two ids. A build that cached
/// the id on the driver would put both members in one transcript.
#[tokio::test]
async fn two_agy_teammates_hold_two_conversations() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    spawn(&door, home.path(), "w2", "mind the other fort").await;
    assert!(
        until(ANSWERS, || cli.conversations().len() == 2).await,
        "{:?}",
        cli.received()
    );

    let ids = cli.conversations();
    assert_ne!(ids[0], ids[1], "two members, two conversations: {ids:?}");
    // And neither launch resumed anything: a first child of a member names no
    // conversation, because there is none of its own to name yet.
    assert!(
        cli.resumed().iter().all(String::is_empty),
        "{:?}",
        cli.resumed()
    );

    registry.shutdown().await;
}

/// **AC-7**, the resuming arm: a wedged child is killed, replaced, and the
/// replacement resumes **the conversation this member had** — never a
/// "most recent" one.
#[tokio::test]
async fn a_wedged_child_is_replaced_by_one_that_resumes_the_same_conversation() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::wedging();
    let (registry, door, root, team) =
        lead_with_timeout(home.path(), &cli, Some(Duration::from_secs(3)));

    // The first turn answers, which is what puts a conversation id in the
    // runner's hand; the second never answers at all.
    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || cli.records("line").len() == 1).await,
        "{:?}",
        cli.received()
    );
    let first = cli.conversations().remove(0);

    send(&root, &team, "w1", "the turn that wedges");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "the deadline fires, the child is ended, and another is started: {:?}",
        cli.received()
    );

    assert_eq!(
        cli.resumed()[1],
        first,
        "the replacement resumes this member's own conversation: {:?}",
        cli.records("argv")
    );
    assert!(
        cli.records("argv")[1]
            .split(' ')
            .all(|token| token != "--continue" && token != "-c"),
        "and never through a door that resumes whatever the machine touched \
         last: {:?}",
        cli.records("argv")[1]
    );

    assert!(
        until(ANSWERS, || lead_mail(&root, &team).iter().any(|text| text
            .contains("restarted")
            && text.contains(&first)))
        .await,
        "the lead is told, and told what was resumed: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(
        !mail.iter().any(|text| text.contains("context lost")),
        "nothing was lost, so nothing says so: {mail:?}"
    );
    // The member survives its own wedge: it is still there to be spoken to.
    assert_eq!(registry.running(), 1);

    registry.shutdown().await;
}

/// **AC-7**, the other arm: wedged before any id was observed, the replacement
/// is a fresh conversation and the lead is told the context is gone.
///
/// Told rather than left to be inferred, and this is the one place a shim
/// member reports what D-3's post-restart case cannot: there the identity a
/// "context lost" mail would report on does not exist, because a retired name
/// is never reused. Here the member is live, is the same member, and has simply
/// lost what it knew.
#[tokio::test]
async fn a_child_wedged_before_it_named_a_conversation_restarts_with_the_context_lost() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::wedging_first();
    let (registry, door, root, team) =
        lead_with_timeout(home.path(), &cli, Some(Duration::from_secs(3)));

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "{:?}",
        cli.received()
    );

    assert!(
        cli.resumed()[1].is_empty(),
        "there was nothing to resume, so no resume flag at all: {:?}",
        cli.records("argv")[1]
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("context lost, fresh session")))
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// **AC-8**, arm one: a child that refuses at startup is reported in the
/// vendor's own words rather than as a broken pipe.
#[tokio::test]
async fn a_child_that_refuses_to_start_is_reported_in_its_own_words() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Refuse);
    let (registry, door, root, team) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("not signed in")))
        .await,
        "the vendor's sentence leads, not the pipe error this side saw: {:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// **AC-8**, arm two: a turn the CLI itself ended is read exactly and reported
/// as what it is — which is a different fact from output this build cannot
/// read, and the lead is told the right one.
#[tokio::test]
async fn a_turn_the_cli_ended_is_reported_as_the_cli_phrased_it() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Fail);
    let (registry, door, root, team) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("could not run that turn")))
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(
        mail.iter().any(|text| text.contains("ERROR")),
        "and the vendor's own status is named: {mail:?}"
    );
    // Still there afterwards: a CLI that refused one turn is a CLI that will
    // take the next one.
    assert_eq!(registry.running(), 1);

    registry.shutdown().await;
}

/// **AC-8**, arm three: a `result` this build cannot make sense of ends the
/// turn as a structured failure rather than as silence.
#[tokio::test]
async fn a_result_this_build_cannot_read_ends_the_turn_out_loud() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Garbage);
    let (registry, door, root, team) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || {
            let mail = lead_mail(&root, &team);
            mail.iter()
                .any(|text| text.contains("could not read what it wrote"))
                && mail.iter().any(|text| text.contains("restarted"))
        })
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );
    // The two mails arrive in the order a person reads them: what went wrong,
    // and then what was done about it.
    let mail = lead_mail(&root, &team);
    let failure = mail
        .iter()
        .position(|text| text.contains("could not read what it wrote"));
    let restart = mail.iter().position(|text| text.contains("restarted"));
    assert!(
        matches!((failure, restart), (Some(failure), Some(restart)) if failure < restart),
        "the failure is reported before its remedy: {mail:?}"
    );

    registry.shutdown().await;
}

/// **AC-8**, arm four: a turn past its deadline is ended and the mail names
/// the key that moves it.
#[tokio::test]
async fn a_turn_past_its_deadline_is_ended_and_the_mail_names_the_key() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Hang);
    let (registry, door, root, team) =
        lead_with_timeout(home.path(), &cli, Some(Duration::from_secs(3)));

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("teammates.shim_turn_timeout")))
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// **AC-11.** The child's environment is enumerated rather than inherited, and
/// nothing of ganja's own travels into it.
#[tokio::test]
async fn the_child_environment_is_enumerated_and_carries_no_ganja_credential() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeAgy::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    spawn(&door, home.path(), "w1", TASK).await;
    assert!(
        until(ANSWERS, || !cli.records("env").is_empty()).await,
        "{:?}",
        cli.received()
    );

    let names = cli.records("env").join(" ");
    assert!(
        names.contains("HOME") && names.contains("PATH"),
        "the enumeration carries what agy needs to be itself: {names}"
    );
    // Vacuous where the parent never held such a name, and kept anyway as a
    // cheap regression net. The real guarantee is
    // `teammate_shim::a_shim_child_gets_exactly_the_enumerated_environment`,
    // which asserts the enumeration is *exactly* `CARRIED` plus the driver's
    // own additions rather than merely that a few names are missing.
    assert!(
        !names.contains("ANTHROPIC") && !names.contains("GANJA_"),
        "and nothing of this build's: {names}"
    );
    // agy's own home is `~/.gemini`, so `HOME` is the whole of it and the
    // driver's additions list is empty — the `CODEX_HOME` case has no
    // counterpart. Its API-key variable is a credential and is excluded by the
    // same enumeration.
    assert!(!names.contains("GEMINI_API_KEY"), "{names}");

    registry.shutdown().await;
}

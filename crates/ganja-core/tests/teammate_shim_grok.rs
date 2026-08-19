//! The **real** grok driver against a fake `grok` (**W5**, **D508**,
//! **D510**).
//!
//! The distinction from `teammate_shim.rs` is the whole point of this file
//! existing beside it: that suite drives the shim *core* with a fixture driver
//! and asserts the mechanism, while this one drives
//! [`ganja_core::teammate::grok::Grok`] itself — the argv a real turn composes,
//! the Messages-wire NDJSON a real turn parses — against a script that answers
//! in the shapes a probed `grok 1.0.6` actually printed.
//!
//! So the posture assertions here are about *this vendor's flags*, which is
//! what W5's own acceptance items are about, and the fake cannot be told where
//! to log through a flag of its own: the argv is grok's, so
//! [`shim_support::Fake`] bakes its log path into the script instead.

mod shim_support;

use std::{sync::Arc, time::Duration};

use ganja_core::teammate::{grok::Grok, shim};
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::AllowSpawn;
use shim_support::{FakeGrok, Mode, until};

/// How long a fake CLI gets to be started, answer, and have its answer land in
/// the lead's inbox. `teammate_shim_codex.rs`'s value, for its reason: the shim
/// polls twice a second and this is a shell script exec'd cold.
const ANSWERS: Duration = Duration::from_secs(20);

/// What every spawn here asks its teammate to do.
const TASK: &str = "hold the fort";

/// The recording **AC-27** compares the shipped posture sentence against.
const PROBE: &str = include_str!("fixtures/grok-posture-probe.txt");

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

/// The lead, the fake and the team, wired to the real driver.
fn lead(
    home: &std::path::Path,
    cli: &FakeGrok,
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
    cli: &FakeGrok,
    timeout: Option<Duration>,
) -> (
    Arc<ganja_core::teammate::TeammateRegistry>,
    Arc<ganja_core::Teammates>,
    ganja_team::TeamsRoot,
    ganja_team::TeamName,
) {
    let (registry, door) =
        shim_support::lead_with_timeout(home, home, Arc::new(Grok::new()), cli.path(), timeout);
    let (root, team) = shim_support::team_of(&registry);

    (registry, door, root, team)
}

/// **AC-6.** The first turn mints the conversation it is creating and the
/// second resumes **that** one — and neither line carries a title or a word
/// anybody said.
///
/// The failure this rules out is the one `--resume`'s own help describes: that
/// flag matches session *titles* as well as ids, so a non-UUID value could
/// resolve to somebody else's conversation. A minted v7 cannot, because
/// "UUID-shaped values always mean IDs".
#[tokio::test]
async fn a_first_turn_mints_a_uuid_and_the_second_resumes_that_same_one() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");
    assert!(
        until(ANSWERS, || cli.records("file").len() == 1).await,
        "the spawn prompt is itself one message, so one turn is owed: {:?}",
        cli.received()
    );
    send(&root, &team, "w1", "and now the other thing");
    assert!(
        // The prompt record rather than the argv one: the fake writes argv
        // first, so an argv count can be read between the two halves of one
        // invocation.
        until(ANSWERS, || cli.records("file").len() == 2).await,
        "a second message is a second child: {:?}",
        cli.received()
    );

    let turns = cli.records("argv");
    assert!(
        turns[0].contains("--session-id "),
        "the first turn names the conversation it creates: {}",
        turns[0]
    );
    assert!(
        !turns[0].contains("--resume"),
        "and resumes nothing: {}",
        turns[0]
    );
    let ids = cli.sessions();
    assert!(
        ganja_protocol::is_uuidv7(&ids[0]),
        "the minted id is this tree's own v7: {}",
        ids[0]
    );
    assert!(
        turns[1].contains(&format!("--resume {}", ids[0])),
        "the second turn resumes the first's conversation: {}",
        turns[1]
    );
    assert!(
        !turns[1].contains("--session-id"),
        "`--session-id` is for a new conversation and does not resume: {}",
        turns[1]
    );

    // Both lines say where the prompt is, and neither says what it was. The
    // member's name is a title in every sense that matters and it is not on
    // either line either.
    for argv in &turns {
        assert!(argv.contains("--prompt-file "), "{argv}");
        assert!(!argv.contains(TASK), "argv is for flags: {argv}");
        assert!(
            !argv.split(' ').any(|token| token == "w1"),
            "never a title: {argv}"
        );
    }
    // The prompt did reach the child — through the file, which is the half that
    // makes the assertion above mean something rather than "nothing arrived".
    assert!(
        cli.records("file").iter().any(|text| text.contains(TASK)),
        "{:?}",
        cli.received()
    );

    registry.shutdown().await;
}

/// **AC-15**, grok's half, end to end rather than over a composed value: every
/// turn carries the pinned posture flag for flag, with the sandbox value as an
/// exact byte string, and no never-composed spelling reaches either line.
///
/// The byte-exactness is not fussiness. `--sandbox` is unvalidated at clap and
/// an unrecognized value becomes a *custom* profile: `read_only` would be
/// looked up as somebody's own profile, fail to load, and hard-exit the child.
#[tokio::test]
async fn every_turn_carries_the_pinned_posture_and_none_carries_an_escape() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);
    send(&root, &team, "w1", "and now the other thing");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "{:?}",
        cli.received()
    );

    let turns = cli.records("argv");
    assert_eq!(turns.len(), 2, "a first turn and a resume: {turns:?}");
    for argv in &turns {
        assert!(
            argv.contains("--sandbox read-only"),
            "the bound, spelled exactly: {argv}"
        );
        assert!(
            !argv.contains("--sandbox read_only"),
            "the spelling that would become a custom profile: {argv}"
        );
        assert!(
            argv.contains("--permission-mode dontAsk"),
            "and the mode beside it: {argv}"
        );
        assert!(
            argv.contains("--output-format streaming-messages-json"),
            "on the wire this build reads: {argv}"
        );
        assert!(argv.contains("--include-partial-messages"), "{argv}");
        // Iterated rather than re-listed, so a spelling added to the module's
        // own table is a spelling this assertion picks up.
        for refused in ganja_core::teammate::grok::NEVER_COMPOSED {
            assert!(
                !argv.split(' ').any(|token| token == refused),
                "{refused} must never be composed: {argv}"
            );
        }
    }

    registry.shutdown().await;
}

/// **AC-15**'s label half, and **D508(a)**'s correction in both directions: the
/// composed `--permission-mode dontAsk` is asserted *present and described by
/// what it does*, never as an approval axis of the grant.
///
/// The sentence lives in one place — [`shim::GROK_MODE_LINE`] — and both the
/// ring and this assertion read it, so a wording change is a change to what a
/// person was told rather than a change to one of two copies.
#[test]
fn the_composed_permission_mode_is_labelled_with_what_it_actually_does() {
    let line = shim::GROK_MODE_LINE;

    assert!(line.contains("dontAsk composed"), "{line}");
    assert!(
        line.contains("selects neither yolo nor auto"),
        "what it does: {line}"
    );
    assert!(
        line.contains("suppresses a config-level always-approve for this launch"),
        "and against what: {line}"
    );
    assert!(
        line.contains("not an approval-policy axis at the probed version"),
        "and what it is not: {line}"
    );
    // It rides the ring of a grok spawn and of no other backend's.
    let lines = shim::spawn_lines(ganja_protocol::team::MemberBackend::Grok);
    assert!(lines.iter().any(|written| written == line), "{lines:?}");
    assert!(
        !shim::spawn_lines(ganja_protocol::team::MemberBackend::Codex)
            .iter()
            .any(|written| written == line)
    );
}

/// **AC-17.** The spawn writes its posture onto the member's ring: what the
/// posture bounds rather than which flag was passed, the honest rider beside
/// it, and grok's third line saying what the composed mode does.
///
/// Compared against the table both the ring and the spawn dialog read rather
/// than against a second string literal — two literals agreeing proves only
/// that somebody typed carefully.
#[tokio::test]
async fn a_grok_spawn_writes_its_posture_onto_the_members_ring() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");

    let view = registry.view();
    let member = view
        .members
        .iter()
        .find(|member| member.name == "w1")
        .expect("the member is in the view");
    let posture = ganja_core::teammate::posture_line(ganja_protocol::team::MemberBackend::Grok)
        .expect("grok states its posture");
    assert!(
        member.recent_calls[0].ends_with(posture),
        "the first ring line is the table's own sentence: {:?}",
        member.recent_calls
    );
    assert!(
        member.recent_calls[1].contains("bounded by grok's own config"),
        "the honest rider travels beside it: {:?}",
        member.recent_calls
    );
    assert_eq!(
        member.recent_calls[2],
        shim::GROK_MODE_LINE,
        "and the line that keeps the mode from reading as an axis: {:?}",
        member.recent_calls
    );

    registry.shutdown().await;
}

/// **AC-19.** Two teammates on one CLI hold conversations of their own, and
/// killing one leaves the other resuming its own — the failure this rules out
/// is one member silently continuing another's conversation.
#[tokio::test]
async fn two_grok_teammates_mint_conversations_of_their_own() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, root, team) = lead(home.path(), &cli);

    for name in ["w1", "w2"] {
        door.start(
            ganja_testkit::spawn_with_prompt(
                name,
                Some("grok"),
                &format!("{name} minted this one"),
            ),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect("the fake grok spawns");
    }
    // Waited on the **prompt** records rather than the argv ones: the fake
    // writes its argv line first and its prompt line after, so an argv count of
    // two can be observed while the second invocation is still half-recorded —
    // and which id belongs to whom is read off the prompt.
    assert!(
        until(ANSWERS, || cli.records("file").len() == 2).await,
        "both spawn prompts are taken: {:?}",
        cli.received()
    );
    let minted = cli.sessions();
    assert_eq!(minted.len(), 2, "{:?}", cli.records("argv"));
    assert_ne!(
        minted[0], minted[1],
        "two members, two conversations: {minted:?}"
    );
    // Which id is whose, read off the prompt each turn actually carried: two
    // members' turns interleave in one log and nothing else tells them apart.
    let ours = cli
        .session_for("w2 minted this one")
        .expect("w2's own first turn");
    assert_ne!(
        ours,
        cli.session_for("w1 minted this one")
            .expect("w1's own first turn"),
        "the two prompts went to two conversations"
    );

    // One is retired; the survivor's next turn must resume **its own** id.
    registry.retire("w1").await.expect("w1 retires");
    send(&root, &team, "w2", "and now the other thing");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 3).await,
        "the survivor takes its turn: {:?}",
        cli.received()
    );
    let resumed = cli
        .records("argv")
        .into_iter()
        .find(|argv| argv.contains("--resume"))
        .expect("the third turn is a resume");
    assert!(
        resumed.contains(&format!("--resume {ours}")),
        "killing one member leaves the other resuming its own conversation: {resumed}"
    );
    // Neither spelling of "the most recent conversation" is ever composed —
    // the exact door through which a member would inherit somebody else's.
    for argv in cli.records("argv") {
        for refused in ["--continue", "-c"] {
            assert!(
                !argv.split(' ').any(|token| token == refused),
                "{refused}: {argv}"
            );
        }
    }

    registry.shutdown().await;
}

/// **AC-8**, on grok's own shapes rather than the fixture driver's, for the
/// three arms a running child can take: a non-zero exit, a clean exit with
/// unreadable output, and — the fourth arm this vendor is the reason for — its
/// own **startup refusal**, which must arrive naming what the vendor said
/// rather than as a stream parse failure.
///
/// The startup arm is not hypothetical. That vendor refuses to start when its
/// `read-only` profile cannot be applied, and on the machine this landed on it
/// cannot: `~/.grok` is a symlink there and the profile's write-deny hook
/// refuses one. A refusal that reads as "garbage output" is a refusal nobody
/// acts on.
#[tokio::test]
async fn a_failed_grok_turn_becomes_structured_mail_and_the_member_survives() {
    for (mode, expected) in [
        (Mode::Fail, "exited"),
        (Mode::Garbage, "streaming-messages-json"),
        (
            Mode::Refuse,
            "could not apply the 'read-only' sandbox profile",
        ),
    ] {
        let home = ganja_testkit::temp_dir();
        let cli = FakeGrok::install(mode);
        let (registry, door, root, team) = lead(home.path(), &cli);

        door.start(
            ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
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
        assert!(
            mail.contains(expected),
            "{mode:?}: the mail says what happened: {mail}"
        );
        assert!(mail.contains("grok"), "{mode:?}: and which CLI: {mail}");

        // The member is still there, and the next message is a fresh attempt.
        let before = cli.records("argv").len();
        send(&root, &team, "w1", "try that again");
        assert!(
            until(ANSWERS, || cli.records("argv").len() > before).await,
            "{mode:?}: a failed turn leaves a member spawnable-to: {:?}",
            cli.received()
        );

        registry.shutdown().await;
    }
}

/// The **wedges-with-mail** shape, which is what W5's gating probe decided a
/// grok teammate is: an unapproved tool ask ends the turn, the lead is told in
/// a sentence naming the tool, and the member stays alive for the next message.
///
/// The tool is named off the **partial** stream, which is the one thing
/// `--include-partial-messages` buys a shape that reads its child's stdout to
/// the end: a message cut off mid-call never arrives as a whole `assistant`
/// record, so without the partial there would be nothing to name.
#[tokio::test]
async fn an_unapproved_tool_ask_ends_the_turn_with_mail_naming_the_tool() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::cancelling();
    let (registry, door, root, team) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");

    assert!(
        until(ANSWERS, || !lead_mail(&root, &team).is_empty()).await,
        "the lead is told: {:?}",
        cli.received()
    );
    let mail = lead_mail(&root, &team).join("\n");
    assert!(mail.contains("did not produce an answer"), "{mail}");
    assert!(
        mail.contains("grok cancelled this turn on an unapproved tool request"),
        "in the words the plan decided this consequence ships in: {mail}"
    );
    assert!(
        mail.contains("`write`"),
        "and which tool it stopped on: {mail}"
    );
    assert!(
        mail.contains("still running"),
        "and that the teammate is still there: {mail}"
    );
    // And never the sentence a parse failure gets: this build read the stream
    // exactly, and a refusal that reads as garbage output is a refusal nobody
    // acts on.
    assert!(
        !mail.contains("could not read what it wrote"),
        "the cancel is classified as a refusal, not as unreadable output: {mail}"
    );

    // The member survives, which is the difference between a wedge and a
    // death: the next message starts a fresh turn.
    let before = cli.records("argv").len();
    send(&root, &team, "w1", "try something that needs no tool");
    assert!(
        until(ANSWERS, || cli.records("argv").len() > before).await,
        "{:?}",
        cli.received()
    );
    // And it **resumes** rather than starting a second conversation: a
    // cancelled turn is a live conversation the CLI created, and a fresh mint
    // here would silently drop everything said before the cancel.
    let after = cli.records("argv");
    assert!(
        after[before].contains("--resume "),
        "the next turn resumes the conversation the cancel left behind: {}",
        after[before]
    );

    registry.shutdown().await;
}

/// **AC-8**'s timeout arm and **AC-29**'s first: grok ships no timeout flag of
/// its own, so the deadline is the shim's — and the mail names both the
/// deadline that fired and the key that moves it.
#[tokio::test]
async fn a_grok_turn_that_never_answers_is_ended_by_the_shims_own_deadline() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Hang);
    let (registry, door, root, team) =
        lead_with_timeout(home.path(), &cli, Some(Duration::from_secs(2)));

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");

    assert!(
        until(ANSWERS, || !lead_mail(&root, &team).is_empty()).await,
        "the deadline is what ends a child that stopped writing: {:?}",
        cli.received()
    );
    let mail = lead_mail(&root, &team).join("\n");
    assert!(mail.contains("2s"), "the deadline that fired: {mail}");
    assert!(
        mail.contains(shim::TIMEOUT_KEY),
        "and the key that moves it: {mail}"
    );

    registry.shutdown().await;
}

/// **AC-21**, on this vendor's own argv: the prompt reaches the child in the
/// `0600` file `--prompt-file` names, and never on a command line — which is
/// where a credential in a teammate's task would otherwise be world-readable
/// through `ps`.
#[tokio::test]
async fn the_prompt_reaches_grok_in_a_file_and_never_in_its_argv() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    let secret = "sk-ant-not-a-real-key-0123456789";
    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), secret),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");

    assert!(
        until(ANSWERS, || !cli.records("file").is_empty()).await,
        "the prompt reached the child: {:?}",
        cli.received()
    );
    assert!(
        cli.records("file")[0].contains(secret),
        "and it reached it through the file: {:?}",
        cli.records("file")
    );
    for argv in cli.records("argv") {
        assert!(!argv.contains(secret), "argv is for flags: {argv}");
    }

    registry.shutdown().await;
}

/// **AC-22.** The escalation door is not built, and a silent downgrade would be
/// a worse lie than a refusal. No child is started at all.
///
/// It also pins the **order**, which W4's review found and could only describe
/// in a comment for want of a positive accessor: `spawn_gate`'s bypass clause
/// is backend-independent, so a person is asked *before* the backend is ever
/// reached and refuses. That is fail-closed and intended — the dialog is not
/// skipped on the strength of a refusal nobody has issued yet — and asserting
/// it means a future change that moved the refusal earlier would be a failing
/// test rather than a quietly skipped question.
#[tokio::test]
async fn a_grok_spawn_asking_to_bypass_is_asked_about_and_then_refused() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);
    let spawn_asks = ganja_testkit::RecordedSpawns::default();

    let refusal = door
        .start_with_bypass(
            ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
            true,
            &ganja_testkit::caller(home.path()),
            &spawn_asks,
        )
        .await
        .expect_err("a bypass spawn on a shim backend is refused");

    assert!(refusal.reason.contains("no escalation door"), "{refusal:?}");
    assert!(
        cli.received().is_empty(),
        "nothing is started at all: {:?}",
        cli.received()
    );
    let asked = spawn_asks.asked();
    assert_eq!(
        asked.len(),
        1,
        "the bypass clause raises its dialog: {asked:?}"
    );
    assert_eq!(
        asked[0]
            .args
            .get("backend")
            .and_then(|value| value.as_str()),
        Some("grok"),
        "and it names the surface the person is being asked about: {asked:?}"
    );

    // **AC-17's three-reader tie, closed here.** The dialog is the third reader
    // of D508(c)'s table, beside the ring line and the honest-strength column,
    // and it is the one no test had ever compared: `spawn_lines` was tied to
    // `posture_line` and `posture_line` to the recording, leaving the sentence
    // a person actually consents against asserted by nobody. Compared against
    // the other two readers rather than against a literal, so the three cannot
    // come to describe one grant differently.
    let table = ganja_core::teammate::posture_line(ganja_protocol::team::MemberBackend::Grok)
        .expect("grok states its posture");
    assert_eq!(
        asked[0]
            .args
            .get("posture")
            .and_then(|value| value.as_str()),
        Some(table),
        "the dialog carries the posture the ring carries: {asked:?}"
    );
    assert!(
        shim::spawn_lines(ganja_protocol::team::MemberBackend::Grok)[0].ends_with(table),
        "and the ring line ends in the same sentence"
    );

    registry.shutdown().await;
}

/// **AC-11**'s class rule, at the one place a driver could break it: no
/// `GROK_*` variable travels to the child, whatever this process holds.
///
/// That vendor's `--sandbox` documents `GROK_SANDBOX` as its own environment
/// source, so an inherited one would silently move the posture a person
/// consented to at spawn. Enumeration is what closes it, and this asserts the
/// enumeration rather than the intention.
#[tokio::test]
async fn no_grok_environment_door_reaches_a_grok_child() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeGrok::install(Mode::Answer);
    let (registry, door, ..) = lead(home.path(), &cli);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake grok spawns");
    assert!(
        until(ANSWERS, || !cli.records("env").is_empty()).await,
        "{:?}",
        cli.received()
    );

    let names = cli.records("env").join(" ");
    assert!(
        !names.contains("GROK_"),
        "no door onto the posture travels: {names}"
    );
    assert!(
        names.contains("HOME") && names.contains("PATH"),
        "and the enumeration is what does: {names}"
    );
    // The driver's own list is empty, which is the half a `PATH` cannot show:
    // every flag this CLI needs is on the command line.
    assert!(
        !ganja_core::teammate::shim::admits("GROK_SANDBOX")
            && ganja_core::teammate::shim::admits("HOME"),
        "the class rule is a rule about the prefix, not a list of three"
    );

    registry.shutdown().await;
}

/// **AC-10**'s grok arm: a `PATH` with no `grok` on it refuses by naming the
/// binary, which is the whole of what somebody needs in order to fix it.
///
/// There is no login pre-check beside it and that is measured rather than
/// omitted: `grok models` prints *"You are not authenticated."* and exits
/// **zero**, so its status says nothing a spawn could act on.
#[tokio::test]
async fn a_spawn_refuses_by_name_when_no_grok_is_on_this_path() {
    let home = ganja_testkit::temp_dir();
    let empty = ganja_testkit::temp_dir();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(Grok::new()),
        std::ffi::OsString::from(empty.path().display().to_string()),
    );

    let refusal = door
        .start(
            ganja_testkit::spawn_with_prompt("w1", Some("grok"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect_err("a PATH without grok refuses the spawn");

    assert!(refusal.reason.contains("grok"), "{refusal:?}");
    assert!(
        refusal.reason.contains(shim::REFUSED_NO_BINARY),
        "{refusal:?}"
    );

    registry.shutdown().await;
}

/// **AC-27.** The sentence a person consents to at spawn equals grok's own
/// recorded probe answer, compared against the **recording** rather than
/// against a second string literal.
///
/// The other two readers are tied to this one elsewhere and deliberately:
/// `shim::spawn_lines`' first line ends with `posture_line`'s sentence and the
/// spawn dialog inserts the same value (`subagent.rs`). So one table has one
/// source, and this is the assertion that the source is the measurement.
#[test]
fn the_grok_posture_sentence_is_the_one_its_probe_recorded() {
    let recorded = PROBE
        .lines()
        .find_map(|line| line.strip_prefix("sentence: "))
        .expect("the recording names the sentence it measured")
        .trim();
    let shipped = ganja_core::teammate::posture_line(ganja_protocol::team::MemberBackend::Grok)
        .expect("grok states its posture");

    assert_eq!(shipped, recorded);
    // And nothing about it is still a promissory note: the word the
    // pre-measurement wording carried must be gone.
    assert!(!shipped.contains("unmeasured"), "{shipped}");
    assert!(
        shim::spawn_lines(ganja_protocol::team::MemberBackend::Grok)[0].ends_with(recorded),
        "the ring line a spawn writes carries the measured sentence too"
    );
}

/// **AC-29**'s derivation half: grok's per-turn deadline is the value W5's own
/// probes derived, and the doc comment beside it carries the arithmetic.
#[test]
fn the_grok_deadline_is_the_value_its_own_probes_derived() {
    let recorded: Vec<f64> = PROBE
        .lines()
        .filter_map(|line| line.strip_prefix("wall-clock: "))
        .filter_map(|value| value.trim().trim_end_matches('s').parse().ok())
        .collect();
    assert!(
        !recorded.is_empty(),
        "the recording names the turns it timed"
    );
    let longest = recorded.iter().copied().fold(0.0_f64, f64::max);
    let derived = Duration::from_secs_f64((2.0 * longest).max(15.0 * 60.0));

    assert_eq!(
        shim::GROK_TURN_TIMEOUT.as_secs(),
        derived.as_secs(),
        "the larger of fifteen minutes and twice the longest turn recorded ({longest}s)"
    );
}

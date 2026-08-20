//! The shim core against a CLI that is really a shell script (**D508**,
//! **D509**).
//!
//! Everything here is about a **process**: what its argv carries, what its
//! environment holds, what happens when it refuses, hangs or writes nonsense,
//! and what a lead reads afterwards. The fixture is
//! [`shim_support`], whose two fakes record everything they are handed.
//!
//! What is emphatically *not* here is any per-CLI posture: which flags codex,
//! agy and grok are actually launched with is D508(a)'s, measured by each
//! wave's own gating probe, and asserted in W3-W5. This suite pins the
//! mechanism those waves plug into — including that a composed posture reaches
//! **every** turn and that the child can act on it, which is what makes the
//! later assertions about a real flag worth making.

mod shim_support;

use std::{sync::Arc, time::Duration};

use ganja_core::teammate::shim;
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::AllowSpawn;
use shim_support::{FakeCli, Mode, PerMessage, Resident, until};

/// How long a fake CLI gets to be started, answer, and have its answer land in
/// the lead's inbox. Generous: the shim polls its inbox twice a second, and
/// this is a shell script being exec'd cold on a machine running the rest of
/// the suite.
const ANSWERS: Duration = Duration::from_secs(20);

/// What every spawn here asks its teammate to do.
const TASK: &str = "hold the fort";

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

/// Puts one message into a member's inbox, as `from`.
fn send(
    root: &ganja_team::TeamsRoot,
    team: &ganja_team::TeamName,
    to: &str,
    from: &str,
    text: &str,
) {
    let member = MemberName::parse(to).expect("a member name");
    let path = root.inbox_path(team, &member);
    mailbox::write(
        &path,
        MailboxMessage::new(from, text.to_owned(), record::now_iso8601()),
    )
    .expect("the message is written");
}

/// One inbox message is one CLI turn, and the count is the assertion: a shim
/// that batched two messages into one prompt would answer both at once and
/// leave the second's sender with nothing addressed to it.
#[tokio::test]
async fn one_inbox_message_produces_exactly_one_cli_turn() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    // The spawn prompt is itself one message, so one turn is owed for it.
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 1).await,
        "one message, one child: {:?}",
        cli.received()
    );
    send(&root, &team, "w1", "team-lead", "and now the other thing");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "a second message is a second child: {:?}",
        cli.received()
    );

    // Both turns carry the posture, which is what "on every turn, not only the
    // first" means at the mechanism level.
    for argv in cli.records("argv") {
        assert!(argv.contains("--sandbox read-only"), "{argv}");
    }
    // And the second one resumes the conversation the first revealed rather
    // than starting a fresh one.
    assert!(
        cli.records("argv")[1].contains("--resume fake-session-1"),
        "{:?}",
        cli.records("argv")
    );

    registry.shutdown().await;
}

/// argv is world-readable through `ps`, and a teammate's task is documented as
/// a place a credential lands in cleartext — so the prompt travels in a `0600`
/// file whose *path* the argv names, and the words themselves are never on a
/// command line.
#[tokio::test]
async fn a_prompt_never_appears_in_a_shim_childs_argv() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    let secret = "sk-ant-not-a-real-key-0123456789";
    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), secret),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

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
        assert!(
            !argv.contains(secret),
            "the prompt must never be on a command line: {argv}"
        );
    }

    registry.shutdown().await;
}

/// D502's posture, adapted: the child's environment is an enumeration and not
/// an inheritance, so a variable the lead holds and the list does not name is a
/// variable the child never sees.
#[tokio::test]
async fn a_shim_child_gets_exactly_the_enumerated_environment() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(
        until(ANSWERS, || !cli.records("env").is_empty()).await,
        "the child reported its environment: {:?}",
        cli.received()
    );

    let names: Vec<String> = cli.records("env")[0]
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    for name in shim::CARRIED {
        // A carried variable travels only if this process actually holds it —
        // except `PATH`, which `environment()` pushes unconditionally from the
        // resolved binary path rather than the parent env. `environment()`
        // copies what exists, and `TMPDIR` is set on macOS but not on a bare
        // Linux runner, so its absence there is the enumeration doing its job
        // rather than failing it. `HOME` and `PATH` are set on both and stay
        // asserted; the skip mirrors production so it never drops `PATH`.
        if name != "PATH" && std::env::var_os(name).is_none() {
            continue;
        }
        assert!(
            names.contains(&name.to_owned()),
            "{name} travels: {names:?}"
        );
    }

    // Everything this process holds that the enumeration does not name must be
    // absent from the child — which is the difference between enumerating and
    // inheriting, asserted against the real parent environment rather than
    // against a list somebody remembered to write down.
    //
    // Four names are excepted and they are the shell's own: `sh` synthesizes
    // `PWD`, `OLDPWD`, `SHLVL` and `_` for itself out of how it was started.
    // A `PWD` in the child is the *spawn's* directory rather than the lead's,
    // so it is evidence for the enumeration rather than against it.
    let shell_made = ["PWD", "OLDPWD", "SHLVL", "_"];
    let inherited: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| !shim::CARRIED.contains(&name.as_str()))
        .filter(|name| !shell_made.contains(&name.as_str()))
        .filter(|name| names.contains(name))
        .collect();
    assert!(
        inherited.is_empty(),
        "the child inherited what it should have been handed: {inherited:?}"
    );

    // The class rule, asserted as a class rather than as three names: no
    // `GROK_*` variable may ever reach a shim child, whatever an additions list
    // grows to.
    for name in &names {
        assert!(!name.starts_with("GROK_"), "{name}");
    }

    registry.shutdown().await;
}

/// D508(a): there is no escalation door, and a spawn that asked for one is
/// refused by name rather than quietly served at the conservative posture —
/// answering "yes" while doing "no" is the one outcome whoever typed `--bypass`
/// cannot have wanted.
#[tokio::test]
async fn a_spawn_carrying_bypass_is_refused_by_name_and_starts_no_child() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    let refusal = door
        .start_with_bypass(
            ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
            true,
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect_err("a shim spawn has no dialogs to bypass");

    assert!(
        refusal.reason.contains("pinned posture"),
        "the refusal says why rather than only that: {refusal:?}"
    );
    assert!(
        refusal.reason.contains("D508(b)"),
        "and names where the escalation that is not built is recorded: {refusal:?}"
    );
    assert_eq!(
        registry.running(),
        0,
        "a refused spawn leaves no member behind"
    );
    assert!(
        cli.received().is_empty(),
        "and no child at all was started: {:?}",
        cli.received()
    );

    registry.shutdown().await;
}

/// The frame table's first row: `shutdown_request` goes ahead of everything
/// else in the inbox, **from any sender** — matching the in-process runner,
/// which matches it with no `from` check at all — and never reaches the CLI.
#[tokio::test]
async fn a_shutdown_request_from_any_sender_retires_the_member_and_never_reaches_the_cli() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);

    // From a peer rather than from the lead, which is the whole of the "any
    // sender" clause.
    send(
        &root,
        &team,
        "w1",
        "w2",
        r#"{"type":"shutdown_request","requestId":"r-1","from":"w2","timestamp":"2026-08-20T00:00:00.000Z"}"#,
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("shutdown_approved")))
        .await,
        "the member answered the shutdown: {:?}",
        lead_mail(&root, &team)
    );
    assert!(
        until(ANSWERS, || registry.running() == 0).await,
        "and stopped being listed"
    );
    assert!(
        !cli.ever_saw("shutdown_request"),
        "no frame JSON ever reached the CLI: {:?}",
        cli.received()
    );

    registry.shutdown().await;
}

/// The rest of the recognized rows: a shim has no engine, so a frame the
/// in-process runner would *apply* is dropped as information — with a ring
/// entry, and for `mode_set_request` a lead mail, because a lead that set a
/// mode and heard nothing would reasonably believe the mode was set.
#[tokio::test]
async fn a_recognized_reserved_frame_is_dropped_as_information_and_never_composed() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);

    send(
        &root,
        &team,
        "w1",
        "team-lead",
        r#"{"type":"mode_set_request","mode":"plan","from":"team-lead","timestamp":"2026-08-20T00:00:00.000Z"}"#,
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("no ganja permission mode to set")))
        .await,
        "the lead is told rather than left believing: {:?}",
        lead_mail(&root, &team)
    );

    let ring: Vec<String> = registry
        .view()
        .members
        .iter()
        .flat_map(|member| member.recent_calls.clone())
        .collect();
    assert!(
        ring.iter().any(|line| line.contains("mode_set_request")),
        "the drop is on the ring: {ring:?}"
    );
    assert!(
        !cli.ever_saw("mode_set_request"),
        "and it never became prompt text: {:?}",
        cli.received()
    );
    assert_eq!(
        cli.records("argv").len(),
        1,
        "a dropped frame is not a turn"
    );

    registry.shutdown().await;
}

/// The guard that is total by construction: a JSON object carrying a `type`
/// this build has never heard of is a document some *other* build would act on,
/// so it is dropped rather than pasted into a foreign agent's prompt — and its
/// **sender** is told, because `Delivery::Acknowledged` prunes a dropped
/// message exactly as it prunes a consumed one.
///
/// The kind is fabricated rather than taken from the fifteen on purpose:
/// enumerating a known one is what `Frame::reserved_kind` already does, and
/// what this guard exists to survive.
#[tokio::test]
async fn a_frame_shaped_message_of_an_unknown_kind_is_dropped_and_its_sender_is_told() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);

    send(
        &root,
        &team,
        "w1",
        "team-lead",
        r#"{"type":"not_a_kind_this_build_knows","payload":{"x":1}}"#,
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("not_a_kind_this_build_knows")))
        .await,
        "the sender is told which kind was refused: {:?}",
        lead_mail(&root, &team)
    );

    let ring: Vec<String> = registry
        .view()
        .members
        .iter()
        .flat_map(|member| member.recent_calls.clone())
        .collect();
    assert!(
        ring.iter()
            .any(|line| line.contains("not_a_kind_this_build_knows")),
        "and the lead's ring names it: {ring:?}"
    );
    assert!(
        !cli.ever_saw("not_a_kind_this_build_knows"),
        "and it never became prompt text: {:?}",
        cli.received()
    );
    assert_eq!(
        cli.records("argv").len(),
        1,
        "a dropped frame is not a turn"
    );

    registry.shutdown().await;
}

/// A `type` that is not a string is **still a `type`**, so a document carrying
/// one is still frame-shaped and still dropped. The classifier reports it as
/// `Tagged::Unknown { name: None }` — absence of a name is not absence of a
/// tag — and a shim that read `None` as "nothing to refuse" would compose the
/// one class of document this guard exists to keep out of a foreign prompt.
#[tokio::test]
async fn a_type_key_that_is_not_a_string_is_still_a_frame_shaped_document() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);

    send(
        &root,
        &team,
        "w1",
        "team-lead",
        r#"{"type":42,"payload":"x"}"#,
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("was not delivered")))
        .await,
        "the sender is told, even though there was no kind to name: {:?}",
        lead_mail(&root, &team)
    );
    assert!(
        !cli.ever_saw("payload"),
        "and it never became prompt text: {:?}",
        cli.received()
    );
    assert_eq!(
        cli.records("argv").len(),
        1,
        "a dropped document is not a turn"
    );

    registry.shutdown().await;
}

/// An ordinary JSON object carrying no `type` at all **is** prompt material,
/// and the two arms are worth pinning together: the guard is about a document
/// shaped like a frame, not about JSON.
#[tokio::test]
async fn a_json_document_with_no_type_key_is_still_prompt_material() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);

    send(
        &root,
        &team,
        "w1",
        "team-lead",
        r#"{"question":"what is this"}"#,
    );
    // Waited for on the *prompt file* rather than on the argv: the fake writes
    // its argv first and its prompt second, so a wait on the argv alone would
    // read the log in the window between the two.
    assert!(
        until(ANSWERS, || cli
            .records("file")
            .iter()
            .any(|text| text.contains("what is this")))
        .await,
        "somebody's data is a turn, not a frame: {:?}",
        cli.received()
    );
    assert_eq!(cli.records("argv").len(), 2, "{:?}", cli.received());

    registry.shutdown().await;
}

/// A vendor's own refusal is information, not the end of a teammate: the lead
/// is told what exited and what it said, and the next message starts a fresh
/// turn.
#[tokio::test]
async fn a_non_zero_exit_becomes_mail_naming_the_status_and_leaves_the_member_running() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Fail)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("with status 3")))
        .await,
        "the exit status reaches the lead: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(
        mail.iter().any(|text| text.contains("not logged in")),
        "and so does the first line of what it said: {mail:?}"
    );
    assert_eq!(registry.running(), 1, "the member survives its own failure");

    // And is still spawnable-to: the next message is a fresh turn.
    send(&root, &team, "w1", "team-lead", "try again");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "the next message retries: {:?}",
        cli.received()
    );

    registry.shutdown().await;
}

/// Output no driver can read is the second failure arm, and it is told from the
/// first: the CLI succeeded, so the sentence is about what it wrote rather than
/// about how it exited.
#[tokio::test]
async fn output_no_driver_can_read_becomes_mail_and_leaves_the_member_running() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Garbage)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("could not read what it wrote")))
        .await,
        "{:?}",
        lead_mail(&root, &team)
    );
    assert_eq!(registry.running(), 1, "the member survives");

    registry.shutdown().await;
}

/// The third failure arm, and the one neither codex nor grok's vendor supplies
/// a flag for: a turn that will not end is ended, its whole process group with
/// it, and the mail names both the deadline that fired and the key that moves
/// it.
#[tokio::test]
async fn a_turn_past_its_deadline_is_ended_and_the_mail_names_the_key_that_moves_it() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    // The registry resolves this once at construction, which is the seam
    // `teammates.shim_turn_timeout` lands on — the key's own seconds-to-
    // `Duration` half is pinned in `config.rs`, and this is the other half.
    let (registry, door) = shim_support::lead_with_timeout(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Hang)),
        cli.path(),
        Some(Duration::from_millis(400)),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("still running after")))
        .await,
        "the deadline fired and was reported: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(
        mail.iter().any(|text| text.contains(shim::TIMEOUT_KEY)),
        "and the mail says what to write if the number was wrong: {mail:?}"
    );
    assert_eq!(registry.running(), 1, "the member survives its deadline");

    registry.shutdown().await;
}

/// The same deadline, on the **resident** shape — the mechanism is one and
/// this is the half that proves it. A resident child that takes a turn and
/// then never answers is the wedge pre-mortem 5 is about: nothing about the
/// process looks wrong, its pipes are open, and only the deadline can tell it
/// from a child that is thinking.
#[tokio::test]
async fn a_resident_turn_past_its_deadline_is_ended_and_the_mail_names_the_key_too() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead_with_timeout(
        home.path(),
        home.path(),
        Arc::new(Resident::new(&cli.log, Mode::Hang)),
        cli.path(),
        Some(Duration::from_millis(400)),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("agy"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake agy spawns");

    // The turn really reached the child — a deadline that fired because the
    // line was never written would prove nothing about the child at all.
    assert!(
        until(ANSWERS, || !cli.records("line").is_empty()).await,
        "the turn was written to the resident child: {:?}",
        cli.received()
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("still running after")))
        .await,
        "the deadline fired and was reported: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(
        mail.iter().any(|text| text.contains(shim::TIMEOUT_KEY)),
        "and names the key that moves it, exactly as the per-message arm does: {mail:?}"
    );

    // The child's whole process group is ended — the `sleep` the fake is
    // parked in is a second process, and a kill that reached only the shell
    // would leave it behind.
    let recorded = registry
        .shims()
        .lock()
        .expect("the records are never poisoned")
        .children()
        .len();
    assert_eq!(
        recorded, 0,
        "the wedged child was forgotten as well as ended"
    );

    registry.shutdown().await;
}

/// The fourth arm, on the resident shape: a vendor that refuses to start says
/// so on stderr and exits, and that must arrive as a refusal naming its own
/// sentence rather than as a parse failure about output it never wrote.
#[tokio::test]
async fn a_vendor_that_refuses_to_start_is_named_rather_than_read_as_a_parse_failure() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(Resident::new(&cli.log, Mode::Refuse)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("agy"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("a resident child that refuses still starts as a process");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .any(|text| text.contains("this fake refuses to start")))
        .await,
        "the vendor's own sentence reaches the lead: {:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// The resident shape end to end: one child for the member's life, one line per
/// turn on its stdin, and the answer in the lead's inbox.
#[tokio::test]
async fn a_resident_child_takes_one_line_per_turn_and_stays_the_same_process() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(Resident::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("agy"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake agy spawns");

    assert!(
        until(ANSWERS, || cli.records("line").len() == 1).await,
        "the spawn prompt is one line on stdin: {:?}",
        cli.received()
    );
    send(&root, &team, "w1", "team-lead", "and the other thing");
    assert!(
        until(ANSWERS, || cli.records("line").len() == 2).await,
        "and a second message is a second line: {:?}",
        cli.received()
    );
    assert_eq!(
        cli.records("argv").len(),
        1,
        "on one child, which is what resident means: {:?}",
        cli.received()
    );
    assert!(
        until(ANSWERS, || lead_mail(&root, &team)
            .iter()
            .filter(|text| text.contains("answered"))
            .count()
            == 2)
        .await,
        "both turns answered: {:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// The posture assertion that is behavioural rather than textual: the composed
/// flag reaches the child, the child acts on it, and the refusal it produces is
/// what the lead reads. A test that only grepped the argv would pass against a
/// flag the vendor ignores.
#[tokio::test]
async fn a_composed_posture_is_something_the_child_acts_on_and_the_lead_reads() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), "please WRITE to the file"),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    assert!(
        until(ANSWERS, || {
            lead_mail(&root, &team)
                .iter()
                .any(|text| text.contains("refused: the sandbox is read-only"))
        })
        .await,
        "the child refused on the strength of the composed flag: {:?}",
        lead_mail(&root, &team)
    );

    registry.shutdown().await;
}

/// The **ordering** a turn that stopped part-way is owed, asserted as one
/// sequence because each half is wrong without the others: the session is
/// stored, the words already said are mailed, and only then is the stop
/// reported.
///
/// The failure each part rules out is different. Dropping the session would
/// make the next message start a *second* conversation and silently lose
/// everything said before the stop. Skipping the words would throw away the
/// half-answer the lead was waiting on. And reporting first would leave the
/// account of a turn sitting above the turn's own words in the same inbox.
///
/// Here rather than in a per-CLI suite because it is the shim core's promise:
/// what a CLI *says* when it stops is that vendor's business, and the order the
/// runner does these three things in is this file's.
#[tokio::test]
async fn a_turn_that_stopped_part_way_keeps_its_session_mails_its_words_then_reports() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Stopped)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake spawns");

    assert!(
        until(ANSWERS, || lead_mail(&root, &team).len() >= 2).await,
        "the words and the account both arrive: {:?}",
        lead_mail(&root, &team)
    );
    let mail = lead_mail(&root, &team);
    assert!(mail[0].contains("half an answer"), "{mail:?}");
    assert!(mail[1].contains("the fake stopped part-way"), "{mail:?}");
    assert!(
        mail[1].contains("ended without completing"),
        "and the account does not contradict the words above it: {mail:?}"
    );

    // The session survived, which only the *next* turn can show.
    send(&root, &team, "w1", "team-lead", "carry on");
    assert!(
        until(ANSWERS, || cli.records("argv").len() == 2).await,
        "{:?}",
        cli.received()
    );
    assert!(
        cli.records("argv")[1].contains("--resume fake-session-1"),
        "a stopped turn leaves a conversation to resume: {:?}",
        cli.records("argv")
    );

    registry.shutdown().await;
}

/// The spawn's own ring lines (**AC-17**): what the posture bounds rather than
/// which flag was passed, the honest rider beside it, and — for grok — what the
/// composed permission mode actually does. Compared against the table both the
/// ring and the spawn dialog read, rather than against a second string literal.
#[tokio::test]
async fn a_shim_spawn_writes_its_posture_onto_the_members_ring() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");

    let view = registry.view();
    let member = view
        .members
        .iter()
        .find(|member| member.name == "w1")
        .expect("the member is in the view");
    let posture = ganja_core::teammate::posture_line(ganja_protocol::team::MemberBackend::Codex)
        .expect("codex states its posture");
    assert!(
        member.recent_calls[0].ends_with(posture),
        "the first ring line is the table's own sentence: {:?}",
        member.recent_calls
    );
    assert!(
        member.recent_calls[1].contains("bounded by codex's own config"),
        "the honest rider travels beside it: {:?}",
        member.recent_calls
    );

    // And the record that spawn wrote is W1's shape, produced by
    // `Handle::surface()` with no change of the registry's own: the CLI's name
    // in `backendType`, and the **in-process sentinel** in `tmuxPaneId`, so
    // every older reader — and a real `claude` sharing the directory —
    // classifies the member as something it cannot drive rather than as a pane
    // that can never exist.
    let (root, team) = shim_support::team_of(&registry);
    let file = ganja_testkit::team_file(&root, &team).expect("the spawn wrote a team file");
    let recorded = file
        .members
        .iter()
        .find(|member| member.name == "w1")
        .expect("the member is recorded");
    assert_eq!(recorded.backend_type.as_deref(), Some("codex"));
    assert_eq!(recorded.tmux_pane_id, "in-process");

    registry.shutdown().await;
}

/// One pass of the loop, driven directly, so the frame table is asserted as a
/// **classification** and not only through its effects.
///
/// This is what `ShimRunner::tick`'s own doc promises a reader for: every other
/// test here watches what a pass *did* — mail arrived, a child ran, the ring
/// moved — which is the stronger assertion but cannot say that four messages
/// were sorted into three buckets in one read. Four go in together: prose, a
/// recognized frame, a fabricated unknown kind, and an untagged JSON document.
/// Exactly two are turns.
#[tokio::test]
async fn one_pass_sorts_its_inbox_into_turns_and_drops() {
    use ganja_core::teammate::{
        SpawnSpec, TeammateBackend as _,
        shim::{Lent, ShimBackend, ShimRunner},
    };

    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, _door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    // Built by hand rather than spawned, because what is under test is the
    // pass and not the registry: a spawn would seed the inbox with a prompt of
    // its own and take the first turn before this could look.
    let spec = SpawnSpec {
        name: ganja_team::MemberName::parse("w1").expect("a member name"),
        team: team.clone(),
        lead: ganja_team::MemberName::lead(),
        root: root.clone(),
        backend: ganja_protocol::team::MemberBackend::Codex,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: home.path().to_path_buf(),
        plan_mode_required: false,
        bypass: false,
        parent_session_id: shim_support::SESSION_ID.to_owned(),
    };
    let backend =
        ShimBackend::new(Arc::new(PerMessage::new(&cli.log, Mode::Answer))).searching(cli.path());
    let handle = backend.spawn(&spec).await.expect("the fake codex starts");
    let child = Arc::clone(handle.child().expect("a shim handle"));

    for (from, text) in [
        ("team-lead", "prose, which is a turn"),
        (
            "team-lead",
            r#"{"type":"mode_set_request","mode":"plan","from":"team-lead","timestamp":"2026-08-20T00:00:00.000Z"}"#,
        ),
        ("w2", r#"{"type":"not_a_kind_this_build_knows"}"#),
        ("team-lead", r#"{"data":"untagged, which is also a turn"}"#),
    ] {
        send(&root, &team, "w1", from, text);
    }

    let runner = ShimRunner::new(
        child,
        spec.clone(),
        Lent {
            lead_inbox: root.inbox_path(&team, &ganja_team::MemberName::lead()),
            recent: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            shims: Arc::clone(registry.shims()),
            cancel: tokio_util::sync::CancellationToken::new(),
        },
        Duration::from_secs(30),
    );
    let tick = runner.tick().await;

    assert_eq!(tick.turns, 2, "prose and untagged data, and nothing else");
    assert_eq!(tick.failed, 0, "both answered");
    assert_eq!(
        tick.dropped,
        vec![
            Some("mode_set_request".to_owned()),
            Some("not_a_kind_this_build_knows".to_owned()),
        ],
        "in inbox order, each named by what it called itself"
    );
    assert!(tick.shutdown.is_none());
    assert_eq!(
        cli.records("argv").len(),
        2,
        "two turns is two children: {:?}",
        cli.received()
    );
    assert!(
        !cli.ever_saw("mode_set_request") && !cli.ever_saw("not_a_kind_this_build_knows"),
        "and no frame JSON reached the CLI: {:?}",
        cli.received()
    );
    // The pass takes everything it decided out of the inbox in one write,
    // whichever bucket it went into.
    assert!(
        mailbox::read(&root.inbox_path(&team, &ganja_team::MemberName::parse("w1").unwrap()))
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "a dropped message leaves the inbox exactly as a consumed one does"
    );

    registry.shutdown().await;
}

/// The resume story: a lead that opens a team file holding a previous lead's
/// shim members marks them **inactive**, because their processes died with the
/// lead that recorded them.
///
/// The field read is `backendType` and only that — `Surface::read` is
/// deliberately lossy and answers `InProcess` for a shim record, so the read
/// that looks more natural cannot tell a shim member from an in-process one.
/// Marked rather than dropped: a shim child leaves no surface a later process
/// can interrogate, so the row stays and says it is not running.
#[tokio::test]
async fn a_restarted_lead_marks_a_previous_leads_shim_members_inactive() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    // An in-process member beside the shim one, so the assertion is that the
    // sweep tells them apart rather than that it retires everything.
    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    door.start(
        ganja_testkit::spawn_with_prompt("w2", Some("in-process"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("an in-process teammate spawns");
    registry.shutdown().await;

    // The next lead of the same session, meeting what the last one wrote.
    let (restarted, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let retired = ganja_core::teammate::reaper::retire_shim_records(&restarted).await;

    assert_eq!(retired, vec!["w1".to_owned()], "only the shim member");
    let file = ganja_testkit::team_file(&root, &team).expect("the team file is there");
    let active = |name: &str| {
        file.members
            .iter()
            .find(|member| member.name == name)
            .and_then(|member| member.is_active)
    };
    assert_eq!(
        active("w1"),
        Some(false),
        "the shim member says it is not running"
    );
    assert_eq!(
        active("w2"),
        Some(true),
        "and an in-process member's row is not this sweep's to touch"
    );

    // Idempotent: a second startup has nothing left to retire, and does not
    // rewrite the document to say so.
    assert!(
        ganja_core::teammate::reaper::retire_shim_records(&restarted)
            .await
            .is_empty()
    );

    // **Dv-3's name semantics**, which are otherwise implemented and invisible:
    // a retired row is still a row, `taken()` counts it without consulting
    // `isActive`, so the re-spawn is `w1-2` rather than `w1`. Freeing the string
    // would mean dropping the row, which would hand a dead teammate's identity
    // to this live one in a document a real `claude` may be reading.
    let respawned = door
        .start(
            ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
            &ganja_testkit::caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect("the work can be restaffed");
    assert_eq!(
        respawned.name, "w1-2",
        "the work is restaffed, the name is not reused"
    );

    let file = ganja_testkit::team_file(&root, &team).expect("the team file is there");
    assert_eq!(
        file.members
            .iter()
            .find(|member| member.name == "w1")
            .and_then(|member| member.is_active),
        Some(false),
        "and the retired row is left exactly as the retire wrote it"
    );

    restarted.shutdown().await;
}

/// The co-tenant guard, transposed from the pane sweep: two leads that start
/// inside one 65-second UUIDv7 bucket share a team file, and the record write
/// never restamps `leadSessionId`. Retiring rows in a document that names
/// another lead's session would mark a **live** co-tenant's members dead.
#[tokio::test]
async fn another_leads_shim_members_are_never_retired() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    registry.shutdown().await;

    // A second lead sharing the team name — the 65-second bucket case — but
    // not the session the document names.
    let other = Arc::new(ganja_core::teammate::TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-ffff-7000-8000-000000000000",
        home.path(),
    ));
    let retired = ganja_core::teammate::reaper::retire_shim_records(&other).await;

    assert!(retired.is_empty(), "{retired:?}");
    let file = ganja_testkit::team_file(&root, &team).expect("the team file is there");
    assert_eq!(
        file.members
            .iter()
            .find(|member| member.name == "w1")
            .and_then(|member| member.is_active),
        Some(true),
        "a co-tenant lead's member is left exactly as its own lead wrote it"
    );
}

/// **AC-9**, both mechanisms. A registry shutdown cancels, `join_all`s every
/// kill — which awaits its own child's reap — and *then* drains the task list
/// the shim's loop was registered in. The `ps`-shaped assertion is taken after
/// the shutdown returns, which is the margin the bound is stated in: TERM at
/// once, KILL after `SETTLE`, gone by `SETTLE + ε`.
#[tokio::test]
async fn a_registry_shutdown_ends_every_shim_child_and_waits_for_the_reap() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(Resident::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("agy"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake agy spawns");
    assert!(until(ANSWERS, || cli.records("line").len() == 1).await);

    // The child's pid, read off the records file the shim itself keeps — which
    // is also the evidence that a resident child is recorded from the moment it
    // starts rather than at a turn boundary.
    let recorded: Vec<i32> = registry
        .shims()
        .lock()
        .expect("the records are never poisoned")
        .children()
        .iter()
        .map(|child| child.process.pid)
        .collect();
    assert_eq!(recorded.len(), 1, "one child is recorded: {recorded:?}");
    let pid = recorded[0];
    assert!(shim_support::alive(pid), "and it is running");

    registry.shutdown().await;

    assert!(
        !shim_support::alive(pid),
        "the shutdown returned only once the child was really gone"
    );
    assert!(
        registry
            .shims()
            .lock()
            .expect("the records are never poisoned")
            .children()
            .is_empty(),
        "and its record went with it"
    );
}

/// The individual-kill path, which never touches the registry's task list: a
/// `/team` retire of one member ends that member's child and waits for it. The
/// state the wait reads is a **state and not an edge**, which is what keeps a
/// per-message member sitting between turns from waiting out the whole of
/// `SETTLE` for an event that is never coming.
#[tokio::test]
async fn retiring_one_member_ends_its_child_and_returns_without_waiting_out_settle() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), TASK),
        &ganja_testkit::caller(home.path()),
        &AllowSpawn,
    )
    .await
    .expect("the fake codex spawns");
    // Its one turn is over, so nothing of this member's is running.
    assert!(until(ANSWERS, || cli.records("argv").len() == 1).await);
    assert!(
        until(ANSWERS, || registry
            .shims()
            .lock()
            .expect("the records are never poisoned")
            .children()
            .is_empty())
        .await,
        "the per-message child was forgotten when it exited"
    );

    let started = std::time::Instant::now();
    assert!(registry.retire("w1").await.expect("the member is retired"));
    assert!(
        started.elapsed() < ganja_core::teammate::SETTLE,
        "a member with nothing running is nothing to wait for: {:?}",
        started.elapsed()
    );
    assert_eq!(registry.running(), 0);

    registry.shutdown().await;
}

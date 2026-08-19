//! **AC-28**, both arms: what a build does with `agy` now that its ship test
//! has a number.
//!
//! W4's gating probe asked one question — is `--sandbox` a filesystem bound at
//! all — and measured *no*: it bounds agy's terminal, and agy's own
//! `write_to_file` tool wrote to an absolute path outside the working
//! directory in 2 of 2 runs of that flag set, in the same runs in which the
//! shell was refused that very directory. The recording is
//! `fixtures/agy-posture-probe.txt`.
//!
//! So this binary asserts the **no-ship** arm, which is a real outcome of the
//! wave and not an absence of one: the name still parses, the door is still
//! reached, and the backend refuses with the sentence its probe recorded. What
//! it must NOT find is any of the shipping arm — a posture row, a dialog
//! sentence, a ring line or a child process.
//!
//! Nothing here writes process-wide state, so the tests share one binary.

use std::{process::Command, sync::Arc};

use ganja_core::{
    protocol::team::MemberBackend,
    teammate::{
        BACKENDS, TeammateBackend,
        agy::{Agy, REFUSED_NO_FILESYSTEM_BOUND},
        backend_name, parse_backend, posture_line, shim,
    },
};
use ganja_testkit::{AllowSpawn, caller, spawn, team, teammates_recorded};

/// The recording AC-27/AC-28 compare the shipped refusal against.
const PROBE: &str = include_str!("fixtures/agy-posture-probe.txt");

/// **AC-28, the name half.** A measurement that says "do not ship" retires the
/// *child*, not the name.
///
/// D501's grammar is "named and refused", and it earns its keep exactly here:
/// somebody who types `--backend agy` has a reason to think it exists, and a
/// name that answers with a measured sentence tells them what a missing name
/// cannot — that it was looked at, and what was found.
#[test]
fn the_agy_name_still_parses_and_is_still_offered() {
    assert_eq!(parse_backend("agy"), Ok(MemberBackend::Agy));
    assert!(BACKENDS.contains(&"agy"));
    assert_eq!(backend_name(MemberBackend::Agy), "agy");
}

/// **AC-28, the refusal half.** Through the real door, because the claim is
/// about the whole chain: the name parses, the gate approves, the backend is
/// reached, and *it* is what refuses.
#[tokio::test]
async fn an_agy_spawn_is_refused_by_name_with_the_measured_sentence() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    // The asker is recorded rather than blanket-allowing, so that the silence
    // this path produces is **pinned** and not merely unobserved: this is the
    // plain spawn — no bypass, and a cwd the project reaches — so the only
    // clause that could have raised a dialog is the foreign one, and W4's
    // measurement is what took it away. The two clauses that remain
    // backend-independent, `bypass` and `external_directory`, still raise
    // theirs. That the bypass one does was measured rather than reasoned:
    // driving the bypass test below through this same recorder makes this
    // assertion fail, naming the ask it caught —
    // `SpawnAsk { args: { "backend": "agy", "bypass": true, .. } }` — so that
    // dialog is opened and answered before the backend is ever called. It is
    // left on `AllowSpawn` there because the only assertion this recorder
    // offers is "nobody was asked", which is the wrong direction for a path
    // where somebody is.
    let spawn_asks = ganja_testkit::RecordedSpawns::default();

    let refusal = door
        .start(spawn("w1", Some("agy")), &caller(home.path()), &spawn_asks)
        .await
        .expect_err("agy does not ship in v1");

    spawn_asks.asked_nobody();

    // `contains` rather than `==` because the door renders the backend's
    // `Unsupported` through its `Display`, which is what puts the name in
    // front of the reason — the exact constant is pinned at the backend
    // itself, below.
    assert!(
        refusal.reason.contains(REFUSED_NO_FILESYSTEM_BOUND),
        "{refusal:?}"
    );
    assert!(
        refusal.reason.starts_with("the agy backend is unavailable"),
        "the refusal names the surface before the reason: {refusal:?}"
    );
    // And it says what was measured rather than that a wave is pending, which
    // is the difference between this refusal and grok's.
    assert!(refusal.reason.contains("terminal only"), "{refusal:?}");

    // A refused spawn leaves nothing behind: no member on disk, and nothing
    // the registry would have to shut down.
    assert!(
        teammates_recorded(&root, &team).is_empty(),
        "a refused spawn records no member"
    );
    assert_eq!(registry.running(), 0);

    registry.shutdown().await;
}

/// **AC-28's `ps` clause.** No agy child process is created — asserted rather
/// than reasoned from "spawn returned an error", because the failure this
/// guards against is a backend that starts something and *then* refuses.
///
/// **The control is half the test, and it is not decoration.** An earlier draft
/// of this ran plain `ps`, which on macOS lists only the invoking terminal's
/// processes and answered with nothing at all — so the assertion passed
/// without ever being able to fail. That is the same trap W4's own ship test
/// was built to avoid: a single row cannot tell "there is no agy child" from
/// "this instrument never sees children". So a known child is started first
/// and the filter is asserted to find *it* before it is believed about agy.
///
/// Scoped to this process's own children rather than to every `agy` on the
/// machine: the developer running this suite may have their own agy open, and
/// a test that failed because of that would be a test of the developer's
/// desktop.
#[tokio::test]
async fn an_agy_spawn_starts_no_child_process() {
    let home = ganja_testkit::temp_dir();
    let (.., registry, door) = team(home.path());

    door.start(spawn("w1", Some("agy")), &caller(home.path()), &AllowSpawn)
        .await
        .expect_err("agy does not ship in v1");

    // The responsiveness control: a child this test *knows* it started, so a
    // filter that finds nothing is read as a broken filter and not as a clean
    // result.
    let mut control = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("the control child starts");

    let mine = std::process::id().to_string();
    // `-A`, because the default listing is scoped to a terminal and a test
    // binary under nextest has none.
    let listing = String::from_utf8(
        Command::new("ps")
            .args(["-A", "-o", "ppid=,command="])
            .output()
            .expect("ps answers")
            .stdout,
    )
    .expect("a rendering");

    let children: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let (ppid, command) = line.trim_start().split_once(char::is_whitespace)?;
            (ppid == mine).then_some(command.trim())
        })
        .collect();

    control.kill().ok();
    control.wait().ok();

    assert!(
        children.iter().any(|command| command.contains("sleep")),
        "the instrument cannot see this process's own children, so it cannot \
         be believed about agy's: {children:?}"
    );
    assert!(
        !children.iter().any(|command| command.contains("agy")),
        "a refused agy spawn started a child: {children:?}"
    );

    registry.shutdown().await;
}

/// **AC-27**, generalized to every CLI: what a person is told equals that
/// CLI's own recorded probe answer, compared against the **recording** rather
/// than against a second string literal — two literals agreeing proves only
/// that somebody typed carefully.
///
/// For agy the sentence a person is told is the refusal, because that is the
/// only sentence this CLI ever produces.
#[test]
fn the_agy_refusal_is_the_one_its_probe_recorded() {
    let recorded = PROBE
        .lines()
        .find_map(|line| line.strip_prefix("refusal: "))
        .expect("the recording names the sentence it measured")
        .trim();

    assert_eq!(REFUSED_NO_FILESYSTEM_BOUND, recorded);
    // Nothing about it is still a promissory note: the pre-measurement wording
    // said `unmeasured`, and a measured sentence may not.
    assert!(!REFUSED_NO_FILESYSTEM_BOUND.contains("unmeasured"));
    // **AC-22's clause travels in it**: a refusal that names no follow-up
    // leaves whoever asked with nowhere to go.
    assert!(REFUSED_NO_FILESYSTEM_BOUND.contains("permission channel"));
}

/// **AC-28's silence clause.** No agy posture row, dialog sentence or ring
/// line ships at all.
///
/// One assertion for all three readers, because they are one table: the spawn
/// dialog's `posture` arg, the registry's ring line and this function are
/// `posture_line` read three times. A sentence here would be a description of
/// nobody, shown in a dialog that never opens.
#[test]
fn agy_discloses_no_posture_row_dialog_sentence_or_ring_line() {
    assert_eq!(posture_line(MemberBackend::Agy), None);
    assert!(
        shim::spawn_lines(MemberBackend::Agy).is_empty(),
        "a backend that never spawns writes no spawn line"
    );
}

/// **AC-22.** A `bypass` spawn is refused too — and by the *measured* sentence
/// rather than by the shim's bypass one, which is the honest order.
///
/// Whoever asked cannot have this surface at all, so telling them their flag
/// was the problem would send them to fix the wrong thing: dropping `--bypass`
/// would not get them an agy teammate. The follow-up both sentences point at
/// is the same one, which is why nothing is lost by answering the larger fact
/// first.
#[tokio::test]
async fn an_agy_spawn_asking_to_bypass_is_refused_by_the_measured_sentence() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());

    let refusal = door
        .start_with_bypass(
            spawn("w1", Some("agy")),
            true,
            &caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect_err("agy does not ship in v1, with or without a bypass");

    assert!(
        refusal.reason.contains(REFUSED_NO_FILESYSTEM_BOUND),
        "{refusal:?}"
    );
    assert!(
        teammates_recorded(&root, &team).is_empty(),
        "and it starts nothing either"
    );

    registry.shutdown().await;
}

/// **AC-2** on the real backend rather than on the stub it replaced.
///
/// `delivery()` is the trait's answer rather than the child's, so a wave that
/// fills in — or declines to fill in — a child must not be free to change what
/// the lead's queue strip does.
#[test]
fn the_agy_backend_answers_the_delivery_every_shim_promises() {
    let backend: Arc<dyn TeammateBackend> = Arc::new(Agy::new());

    assert_eq!(backend.backend(), MemberBackend::Agy);
    assert_eq!(
        backend.delivery(),
        ganja_core::teammate::Delivery::Acknowledged
    );
}

/// The backend's own answer, unwrapped: the exact constant and the exact
/// surface.
///
/// Asserted beside the door rather than only through it, because the door
/// renders through `Display` and a `contains` there would still pass if the
/// reason drifted into something longer. This is where the sentence is pinned.
#[tokio::test]
async fn the_agy_backend_itself_refuses_every_spawn_naming_the_surface() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, _door) = team(home.path());

    let spec = ganja_core::teammate::SpawnSpec {
        name: ganja_team::MemberName::parse("w1").expect("a member name"),
        team: team.clone(),
        lead: ganja_team::MemberName::lead(),
        root: root.clone(),
        backend: MemberBackend::Agy,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: home.path().to_path_buf(),
        plan_mode_required: false,
        bypass: false,
        parent_session_id: ganja_testkit::LEAD_SESSION_ID.to_owned(),
    };

    let refusal = Agy::new()
        .spawn(&spec)
        .await
        .expect_err("agy does not ship in v1");

    assert_eq!(refusal.backend, MemberBackend::Agy);
    assert_eq!(refusal.reason, REFUSED_NO_FILESYSTEM_BOUND);
    assert!(
        refusal
            .to_string()
            .starts_with("the agy backend is unavailable"),
        "{refusal}"
    );

    registry.shutdown().await;
}

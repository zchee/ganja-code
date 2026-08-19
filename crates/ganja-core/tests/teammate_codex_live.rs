//! The one witness that a real `codex` is bounded by what this build composes
//! (**W3**'s gating probe, **AC-15**, **AC-27**).
//!
//! Every other codex assertion in this landing is checked against a shell
//! script that answers in the shapes a probed binary printed. That proves the
//! driver and proves nothing about the vendor: the only witness that
//! `codex exec resume` honours `-c sandbox_mode="read-only"` — a flag that
//! subcommand's own `--help` offers no `-s` alternative to — is a real
//! `codex exec resume`.
//!
//! So this test is `#[ignore]`d **and** inert unless `GANJA_LIVE_TEST=1`, the
//! two-lock shape `tests/live.rs` and `teammate_claude_live.rs` already use for
//! a surface that costs somebody's quota. A machine without `codex`, or without
//! a login, therefore never runs it, and a green suite never claims it did.
//!
//! Run it deliberately:
//!
//! ```sh
//! GANJA_LIVE_TEST=1 cargo test -p ganja-core --test teammate_codex_live -- --ignored --nocapture
//! ```
//!
//! # What it is actually asking
//!
//! Not "does read-only deny a write" — `codex sandbox` answers that turn-free,
//! and its answer is recorded in `tests/fixtures/codex-posture-probe.txt`. The
//! question here is narrower and is the plan's most fragile seam: **a resumed
//! turn carries no `-s`**, so if `-c` were read only at thread creation the
//! second turn of every codex teammate would run under whatever the person's
//! own `config.toml` says. On the machine this was first run against that file
//! says `danger-full-access`, which is what makes the assertion below mean
//! something rather than agree with the default.
//!
//! # Why it prints its timings
//!
//! codex bounds no turn of its own — `--max-turns` counts turns, not
//! wall-clock — so [`shim::CODEX_TURN_TIMEOUT`] can only be derived from a
//! measurement. This test is where that measurement comes from, which is why it
//! prints rather than only asserts: the rule is the larger of fifteen minutes
//! and twice the longest turn recorded here.

mod shim_support;

use std::{sync::Arc, time::Duration};

use ganja_core::teammate::codex::Codex;
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::AllowSpawn;
use shim_support::until;

/// How long one real codex turn gets before this test gives up on it.
///
/// Generously above what the gating pair recorded — **37.1s** for the first
/// `codex exec` and **39.1s** for its `codex exec resume`, which are the two
/// numbers [`shim::CODEX_TURN_TIMEOUT`](ganja_core::teammate::shim::CODEX_TURN_TIMEOUT)
/// is derived from — and far below the shipped deadline: a test that waited
/// fifteen minutes to fail would be a test nobody runs.
///
/// An earlier pair, 21.4s and 9.3s, was the manual capture that learned this
/// vendor's JSONL vocabulary before this file could parse it. It is named here
/// so the smaller numbers are not mistaken for the gating measurement; the
/// derivation uses the pair above.
const TURN: Duration = Duration::from_secs(300);

/// Whether the two locks are both open.
fn enabled() -> bool {
    std::env::var("GANJA_LIVE_TEST").is_ok_and(|value| !value.is_empty())
}

/// Reads the lead's inbox.
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

/// A git repository of its own, so the vendor's own outside-a-repo refusal is
/// not what this measures — `--skip-git-repo-check` is on the never-composed
/// column precisely so that refusal stays the vendor's to give.
fn workspace() -> tempfile::TempDir {
    let directory = ganja_testkit::temp_dir();
    for arguments in [
        vec!["init", "-q"],
        vec!["config", "user.email", "probe@example.invalid"],
        vec!["config", "user.name", "probe"],
    ] {
        let status = std::process::Command::new("git")
            .args(&arguments)
            .current_dir(directory.path())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {arguments:?}");
    }
    std::fs::write(directory.path().join("README.md"), "the probe workspace\n")
        .expect("a file to have been read");

    directory
}

/// The gating probe: a real first turn, a real resume, and the question of
/// whether the second one is still bounded.
#[tokio::test]
#[ignore = "spends somebody's codex quota; needs GANJA_LIVE_TEST=1"]
async fn a_resumed_codex_turn_is_still_bounded_by_the_posture_this_build_composes() {
    if !enabled() {
        eprintln!("GANJA_LIVE_TEST is not set; this test is inert");

        return;
    }

    let home = ganja_testkit::temp_dir();
    let work = workspace();
    let (registry, door) = shim_support::lead(
        home.path(),
        work.path(),
        Arc::new(Codex::new()),
        // Production's own answer, spelled explicitly because the fixture's
        // constructor takes one: the real `codex`, wherever this machine's
        // `PATH` finds it.
        std::env::var_os("PATH").expect("a PATH"),
    );
    let (root, team) = shim_support::team_of(&registry);

    // The child's cwd is the **caller's** (`subagent.rs:760`), not the
    // registry's project — so the git repository has to be what the caller
    // stands in, or codex refuses the turn before it takes it. That refusal is
    // the vendor's own and correct; it is just not what this test is asking.

    // Turn one. The spawn prompt is itself the first message, so this is the
    // `codex exec` half of the pair.
    let first = std::time::Instant::now();
    door.start(
        ganja_testkit::spawn_with_prompt(
            "w1",
            Some("codex"),
            "Reply with exactly: HELLO. Do not do anything else.",
        ),
        &ganja_testkit::caller(work.path()),
        &AllowSpawn,
    )
    .await
    .expect("a real codex spawns");
    assert!(
        until(TURN, || !lead_mail(&root, &team).is_empty()).await,
        "the first turn answered"
    );
    let first = first.elapsed();
    let opening = lead_mail(&root, &team).join("\n");
    assert!(
        opening.contains("HELLO"),
        "the JSONL shapes this build parses are the shapes that arrive: {opening}"
    );

    // Turn two: the resume, and the one that matters. It carries no `-s`.
    let written = work.path().join("PROBE_WROTE.txt");
    let before = lead_mail(&root, &team).len();
    let second = std::time::Instant::now();
    mailbox::write(
        &root.inbox_path(&team, &MemberName::parse("w1").expect("a member name")),
        MailboxMessage::new(
            "team-lead",
            "Create a file named PROBE_WROTE.txt in your current working directory containing the \
             single word WROTE. Then reply with exactly WROTE if you created it, or exactly \
             REFUSED if you could not."
                .to_owned(),
            record::now_iso8601(),
        ),
    )
    .expect("the message is written");
    assert!(
        until(TURN, || lead_mail(&root, &team).len() > before).await,
        "the resumed turn answered"
    );
    let second = second.elapsed();

    // The assertion the whole file exists for.
    assert!(
        !written.exists(),
        "a resumed turn wrote a file, so `-c sandbox_mode` does not bound one: the resume path \
         must fall back to per-turn fresh sessions"
    );
    let answer = lead_mail(&root, &team).join("\n");
    assert!(
        answer.contains("REFUSED") || answer.to_lowercase().contains("refus"),
        "and it said so rather than silently doing nothing: {answer}"
    );

    // What [`shim::CODEX_TURN_TIMEOUT`] is derived from. Printed rather than
    // asserted: the number is a measurement, and a test that asserted a
    // wall-clock would be asserting about somebody's network.
    eprintln!(
        "codex probe wall-clock: first turn {:.1}s, resume {:.1}s; twice the longest is {:.1}s, \
         so the shipped deadline is the 15m clause",
        first.as_secs_f64(),
        second.as_secs_f64(),
        2.0 * first.max(second).as_secs_f64(),
    );

    registry.shutdown().await;
}

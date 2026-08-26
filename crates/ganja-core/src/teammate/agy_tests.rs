use std::{collections::BTreeSet, path::PathBuf};

use ganja_team::{MemberName, TeamName, TeamsRoot};

use super::*;
use crate::teammate::{SpawnSpec, shim};

/// The pane-mode recording (**D512**), compared against rather than
/// re-typed — the P27 posture-probe pattern: two literals agreeing proves
/// only that somebody typed carefully. The headless wire's own tests live
/// in `tests/teammate_shim_agy.rs`; what is pinned beside the code is the
/// pane spelling, because that is all this module composes without a turn.
const TUI_PROBE: &str = include_str!("../../tests/fixtures/agy-tui-probe.txt");

/// A spawn to compose the headless argv against. Nothing in an argv reads
/// any of it — which is itself the point of **AC-21**.
fn spec() -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("w1").expect("a member name"),
        team: TeamName::default_team(),
        lead: MemberName::lead(),
        root: TeamsRoot::new(PathBuf::from("/nonexistent/teams")),
        backend: MemberBackend::Agy,
        agent_type: "general".to_owned(),
        model: "whatever-the-person-configured".to_owned(),
        color: "blue".to_owned(),
        prompt: "the spawn prompt, which travels through the mailbox".to_owned(),
        cwd: PathBuf::from("/nonexistent/work"),
        plan_mode_required: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        shell: crate::teammate::pane::PaneShell::default(),
        share: crate::teammate::pane::PaneShare::default(),
    }
}

/// The headless first-launch argv, for the words that must *not* carry.
fn headless() -> Vec<String> {
    let spec = spec();
    Agy.argv(&Turn {
        spec: &spec,
        text: "a teammate's words, which never reach a command line",
        prompt: None,
        session: None,
        deadline: shim::AGY_TURN_TIMEOUT,
    })
    .iter()
    .map(|token| token.to_string_lossy().into_owned())
    .collect()
}

/// The launch line the recording says the pane ran, binary first.
fn recorded_launch() -> Vec<&'static str> {
    TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("launch: "))
        .expect("the recording names the launch line it probed")
        .split_whitespace()
        .collect()
}

/// What the driver composes for a pane, as strings.
fn tui() -> Vec<String> {
    Agy.tui_argv()
        .iter()
        .map(|token| token.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_tui_argv_is_the_launch_line_the_pane_probe_ran() {
    // Byte for byte against the recording, binary included.
    let recorded = recorded_launch();
    let (binary, floors) = recorded
        .split_first()
        .expect("a binary, then the floor it was launched with");

    assert_eq!(*binary, BINARY);
    assert_eq!(tui(), floors);
    // And the recording says that word reached the composer — in
    // accept-edits mode, which is the Dv-7 posture on screen rather than
    // a bound this flag does not provide.
    let outcome = TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("outcome: "))
        .expect("the recording says what the launch reached");
    assert!(outcome.starts_with("composer reached"), "{outcome}");
    assert!(outcome.contains("accept-edits mode"), "{outcome}");
}

#[test]
fn the_ready_marker_is_the_footer_the_probe_captured() {
    let captured = TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("footer marker: "))
        .expect("the recording names the footer it captured")
        .trim();

    assert_eq!(READY_MARKER, captured);
}

#[test]
fn the_tui_argv_carries_the_terminal_bound_and_none_of_the_print_mode_flags() {
    let tui = tui();
    let headless = headless();
    // TUI ⊂ headless, as sets: every pane word is a word of the headless
    // launch line — one posture rule, not a second one written for panes.
    let headless_words: BTreeSet<&str> = headless.iter().map(String::as_str).collect();
    let tui_words: BTreeSet<&str> = tui.iter().map(String::as_str).collect();
    assert!(
        tui_words.is_subset(&headless_words),
        "the pane words are not all headless words: {tui:?} vs {headless:?}"
    );
    // The print-mode flags, named: each is really a headless word — so
    // this list cannot go stale and name nothing — and none reaches the
    // pane. The two that would bite are the two a reader would most
    // expect to carry: `-p` would turn the TUI into a print-mode child
    // reading a prompt nobody typed, and `--print-timeout` would bound a
    // pane whose whole point is that a person can see it. Compared as
    // whole tokens, for the `-c`/`--conversation` reason the headless
    // suite states.
    const PRINT_MODE: [&str; 5] = [
        "-p",
        "--print-timeout",
        "--input-format",
        "--output-format",
        "--disable-slash-commands",
    ];
    for flag in PRINT_MODE {
        assert!(
            headless_words.contains(flag),
            "{flag} is no longer a headless word: {headless:?}"
        );
        assert!(
            !tui_words.contains(flag),
            "{flag} is print mode's, and is in {tui:?}"
        );
    }
    // And the list is complete: every flag the headless line carries and
    // the pane does not is one of the five, so a new print-mode flag has
    // to be named here — where the loop above then holds it off the pane
    // for good. (One that carried to the pane the day it was added would
    // pass this set and fail the byte-for-byte launch-line pin instead.)
    let unnamed: Vec<&str> = headless_words
        .difference(&tui_words)
        .copied()
        .filter(|word| word.starts_with('-') && !PRINT_MODE.contains(word))
        .collect();
    assert!(
        unnamed.is_empty(),
        "headless-only flags this test does not name: {unnamed:?}"
    );
}

#[test]
fn no_never_composed_spelling_reaches_the_tui_argv() {
    // Iterated rather than re-listed, exactly as for the headless argv.
    let tui = tui();
    for refused in NEVER_COMPOSED {
        assert!(
            !tui.iter().any(|token| token == refused),
            "{refused} must never be composed, and is in {tui:?}"
        );
    }
}

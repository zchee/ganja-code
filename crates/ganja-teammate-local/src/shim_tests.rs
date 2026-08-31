use std::ffi::OsString;
use std::time::Duration;

use ganja_core::teammate::posture_line;
use ganja_core::teammate::preamble::Names;
use ganja_protocol::team::{DISPLAY_FIELD_CAP, MemberBackend};
use ganja_team::ShimCli;

use super::{
    AGY_TURN_TIMEOUT, CARRIED, CODEX_TURN_TIMEOUT, Driver as _, Failure, GROK_HOME, GROK_MODE_LINE,
    GROK_TURN_TIMEOUT, TIMEOUT_KEY, admits, default_turn_timeout, environment, first_line,
    preamble, resolve, spawn_lines,
};
use crate::grok::Grok;

/// The headless channel says that answers are mail and that there is no
/// door to go looking for, in the CLI's own name — and says **how much**
/// is mailed per CLI, since codex's driver forwards every message where
/// grok's and agy's forward one answer per turn — and the task is what the
/// message ends with (**D514**).
#[test]
fn the_headless_preamble_says_how_much_of_an_answer_is_mail_and_ends_with_the_task() {
    let who = Names { name: "w1", team: "session-abcd1234", lead: "team-lead" };
    for (backend, cli) in [
        (MemberBackend::Codex, "codex"),
        (MemberBackend::Agy, "agy"),
        (MemberBackend::Grok, "grok"),
    ] {
        // The clause this door really walks, from the one place that
        // says it — never a second literal, which is how the two doors
        // would come to promise a teammate different things.
        let answers = crate::readback::answers_clause(backend, crate::readback::Road::Headless)
            .expect("a shim states its answer contract");
        let text = preamble(who, backend, "hold the fort");
        assert!(
            text.contains(&format!("headless {cli} process")),
            "{cli}: the channel is in the CLI's name: {text}"
        );
        assert!(
            text.contains(answers),
            "{cli}: the answer road is said as the driver walks it: {text}"
        );
        assert!(
            text.contains("carried to the lead"),
            "{cli}: and it is said in words, not implied: {text}"
        );
        assert!(text.ends_with("Your task:\n\nhold the fort"), "{cli}: {text}");
    }
}

#[test]
fn every_cli_carries_the_deadline_this_plan_derived_for_it() {
    assert_eq!(default_turn_timeout(ShimCli::Agy), AGY_TURN_TIMEOUT);
    assert_eq!(default_turn_timeout(ShimCli::Codex), CODEX_TURN_TIMEOUT);
    assert_eq!(default_turn_timeout(ShimCli::Grok), GROK_TURN_TIMEOUT);
    // agy's is the derived one — its own `--print-timeout` is composed at
    // deadline + 1m so the shim always fires first — and it is deliberately
    // shorter than the two provisional numbers.
    assert!(AGY_TURN_TIMEOUT < CODEX_TURN_TIMEOUT);
    assert_eq!(AGY_TURN_TIMEOUT, Duration::from_secs(240));
}

/// The three that travel, and nothing else — the whole of D502's posture as
/// this side applies it.
#[test]
fn a_child_gets_exactly_the_enumerated_names() {
    let carried = environment(&["CODEX_HOME"], None);
    for (name, _) in &carried {
        let name = name.to_string_lossy().into_owned();
        assert!(
            CARRIED.contains(&name.as_str()) || name == "CODEX_HOME",
            "{name} is not in the enumeration"
        );
    }
    // An explicit path decides what the child's own PATH is, which is how a
    // fake CLI is reached without mutating the process under test.
    let pointed = environment(&[], Some(&OsString::from("/opt/fake/bin")));
    assert_eq!(
        pointed.iter().find(|(name, _)| name == "PATH").map(|(_, value)| value.clone()),
        Some(OsString::from("/opt/fake/bin"))
    );
}

/// The class rule, **enforced**: a driver that names a `GROK_*` variable
/// other than the home carved out below does not get it, whatever its
/// additions list says.
///
/// The loud half of the same rule is a `debug_assert` at `prepare`, where a
/// driver's own list is first consulted. It is there rather than here for
/// exactly this test's sake: an assertion inside `environment` would panic
/// on the call below, and then the safe fallback — the thing that actually
/// protects a person's consent in a release build — would be untested.
#[test]
fn no_grok_posture_door_is_ever_in_the_enumeration() {
    for name in CARRIED {
        assert!(!name.starts_with("GROK_"), "{name}");
    }

    assert!(!admits("GROK_SANDBOX"));
    assert!(!admits("GROK_SANDBOX_AUTO_ALLOW_BASH"));
    assert!(!admits("GROK_SANDBOX_PROFILE"));
    // The fourth one the vendor has not added yet, which is the whole
    // reason this is a prefix and not three names.
    assert!(!admits("GROK_ANYTHING_AT_ALL"));
    assert!(admits("CODEX_HOME"));
    assert!(admits("HOME"));
    // Not a substring rule: a name that merely *contains* the prefix is
    // somebody else's variable and travels.
    assert!(admits("MY_GROK_NOTES"));

    assert!(
        environment(&["GROK_SANDBOX"], None).iter().all(|(name, _)| name != "GROK_SANDBOX"),
        "a driver naming one does not get it"
    );
}

/// The one exception to that class, and it is **exact**: `GROK_HOME` is a
/// pointer at the directory grok keeps its credential and sessions in, where
/// every other name sharing the prefix is a door onto the pinned posture.
///
/// Pinned as an equality rather than a prefix on purpose. A `GROK_HOME_MODE`
/// the vendor mints tomorrow is a name nobody has measured, so it has to fall
/// on the refusing side of this rule until somebody does — which is exactly
/// what a `starts_with("GROK_HOME")` carve-out would have got wrong.
#[test]
fn grok_s_home_is_the_one_name_of_that_class_a_child_may_be_handed() {
    assert!(admits(GROK_HOME));
    assert_eq!(GROK_HOME, "GROK_HOME");

    for near_miss in ["GROK_HOME2", "GROK_HOMEX", "GROK_HOME_MODE", "GROK_HOME_SANDBOX"] {
        assert!(!admits(near_miss), "the carve-out is the name, not a prefix of it: {near_miss}");
    }

    // And the driver that needs it is the one that names it, so the pane and
    // the headless surfaces compose the same list from the same place.
    assert_eq!(Grok.additions(), [GROK_HOME]);
}

/// What the carve-out is *for*: a person who exports `GROK_HOME` — because
/// their `~/.grok` is a symlink the pinned `read-only` profile refuses to
/// start under — has it reach the child, and a person who exports nothing has
/// nothing invented for them.
///
/// Its own binary would be the honest place for a `set_var`; this reads the
/// process it is already in instead, asserting the branch either way without
/// mutating anything, which is what keeps it beside the rule it is about.
#[test]
fn a_grok_child_is_handed_the_home_its_lead_has_and_no_home_its_lead_lacks() {
    let carried = environment(Grok.additions(), None);
    let handed = carried.iter().find(|(name, _)| name == GROK_HOME).map(|(_, value)| value.clone());

    match std::env::var_os(GROK_HOME) {
        Some(set) => assert_eq!(handed, Some(set), "the lead's own value, unaltered"),
        None => assert_eq!(handed, None, "absent rather than empty: an empty home reads worse"),
    }

    // The rest of the class is refused whatever this process holds, which is
    // the half of the rule that protects the grant.
    assert!(
        environment(&[GROK_HOME, "GROK_SANDBOX"], None)
            .iter()
            .all(|(name, _)| name != "GROK_SANDBOX"),
        "the carve-out opens one name, not the prefix"
    );
}

#[test]
fn an_explicit_path_decides_which_binary_a_shim_would_run() {
    // A relative or empty component is dropped before `which` sees the
    // list, so a turn's incidental directory can never supply a teammate
    // binary.
    assert!(resolve(&OsString::from(""), "sh").is_none());
    assert!(resolve(&OsString::from("relative/bin"), "sh").is_none());
    assert!(resolve(&OsString::from("/usr/bin:/bin"), "sh").is_some());
    assert!(resolve(&OsString::from("/usr/bin:/bin"), "no-such-binary-here").is_none());
}

/// The ring's first line and the spawn dialog's sentence are one table read
/// twice, which is what AC-17 asserts by comparing them rather than by two
/// string literals.
#[test]
fn a_spawn_ring_line_carries_the_same_posture_the_dialog_does() {
    // `Agy` is deliberately absent: W4 measured its floor and it does not
    // hold, so that backend never spawns and has no posture to state. It
    // is asserted the other way — `posture_line` answers `None` and
    // `spawn_lines` is empty — in `teammate_shim_agy.rs`.
    for backend in [MemberBackend::Codex, MemberBackend::Grok] {
        let lines = spawn_lines(backend);
        let posture = posture_line(backend).expect("a shim backend states its posture");
        assert!(lines[0].ends_with(posture), "{lines:?}");
        assert!(
            lines[1].contains("bounded by"),
            "the honest rider travels beside the grant: {lines:?}"
        );
    }
    assert_eq!(
        spawn_lines(MemberBackend::Grok).last().map(String::as_str),
        Some(GROK_MODE_LINE),
        "grok's third line says what the composed mode actually does"
    );
    assert!(
        spawn_lines(MemberBackend::InProcess).is_empty(),
        "a backend with no pinned posture writes no posture line"
    );
}

#[test]
fn a_timeout_mail_names_both_the_deadline_and_the_key_that_moves_it() {
    let sentence = Failure::Deadline { after: Duration::from_secs(900) }.sentence("codex");

    assert!(sentence.contains("900s"), "{sentence}");
    assert!(sentence.contains(TIMEOUT_KEY), "{sentence}");
}

#[test]
fn a_failure_mail_names_the_cli_the_status_and_the_first_stderr_line() {
    let sentence = Failure::Exit {
        status: "with status 3".to_owned(),
        stderr: "error: not logged in".to_owned(),
    }
    .sentence("grok");

    assert!(sentence.contains("grok"), "{sentence}");
    assert!(sentence.contains("with status 3"), "{sentence}");
    assert!(sentence.contains("not logged in"), "{sentence}");
}

#[test]
fn only_the_first_stderr_line_travels_and_it_is_capped() {
    assert_eq!(first_line("\n\nerror: no\nstack trace line\nanother"), "error: no");
    assert_eq!(first_line(""), "");

    let wide: String = "あ".repeat(DISPLAY_FIELD_CAP * 2);
    assert_eq!(first_line(&wide).chars().count(), DISPLAY_FIELD_CAP);
}

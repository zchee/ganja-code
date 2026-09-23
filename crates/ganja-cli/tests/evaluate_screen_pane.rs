//! **Criterion 36** (**D567**): the terminal UI's opening line says what the
//! session screens, where the text goes and who it covers — and says nothing
//! of it when there is no judge.
//!
//! The opening line is the status bar of the first frame, so it is read the
//! way a person reads it: a real `ganja` lead in a private tmux server, the
//! pane captured with `capture-pane` (`pane_lead`). A byte stream off a pty
//! cannot serve here — the status bar is drawn over cells a diff may skip, so
//! a sentence arrives split around its spaces.
//!
//! The first two cases carry one more notice in that line, a theme this build
//! does not have, which is what makes the negative case's absence mean
//! something: the line it reads is on screen and holds the notices startup
//! had.
//!
//! The third is the common case that line has to survive: a second lead in
//! the same project, whose name collides with the first's before its first
//! frame, keeps the disclosure ahead of the collision it reports.
//!
//! No turn is taken, so nothing is sent: the base URL is a loopback port
//! nothing listens on. Every TypeSafe variable is withheld from the server
//! and put on the lead's own process only where the case wants one.

#![cfg(unix)]

mod pane_lead;

use std::path::PathBuf;

use pane_lead::{Homes, Lead};

/// The key a judging lead is handed. Nothing is ever sent with it.
const KEY: &str = "sk-evaluate-screen-pane-0123456789";

/// A theme no build ships, whose notice anchors the opening line.
const THEME: &str = "evaluate-screen-drill";

/// What a theme this build does not have says, which both cases read.
const THEME_NOTICE: &str = "no theme named \"evaluate-screen-drill\"";

/// What a lead is born without, whichever case it is: a developer's own
/// TypeSafe settings would otherwise decide whether the lead judges.
const WITHHELD: &[&str] = &["TYPESAFE_API_KEY", "TYPESAFE_BASE_URL", "TYPESAFE_DEFAULT_MODEL"];

/// The config both cases run under — a trusted tier, since `GANJA_CONFIG`
/// names it: two sources screened, and private fetches allowed, which is the
/// state the `allow_private` clause is about.
fn config(homes: &Homes) -> PathBuf {
    let path = homes.data().join("screening.toml");
    std::fs::write(
        &path,
        format!(
            "theme = {THEME:?}\n\n[webfetch]\nallow_private = true\n\n\
             [evaluate]\nscreen = [\"webfetch\", \"websearch\"]\n"
        ),
    )
    .expect("the config is writable");

    path
}

/// A script the lead never plays: no turn is taken here.
fn script(homes: &Homes) -> PathBuf {
    let path = homes.data().join("script.json");
    std::fs::write(&path, r#"{"cadence_ms": 1, "turns": [{"text": "unused"}]}"#)
        .expect("the script is writable");

    path
}

#[test]
fn a_judging_lead_names_its_sources_host_and_reach_in_the_opening_line() {
    let homes = Homes::new();
    let config = config(&homes);
    let config = config.display().to_string();
    let lead = Lead::start(
        &homes,
        &script(&homes),
        WITHHELD,
        &[
            ("GANJA_CONFIG", config.as_str()),
            ("TYPESAFE_API_KEY", KEY),
            ("TYPESAFE_BASE_URL", "http://127.0.0.1:9"),
        ],
    );

    let screen = lead.wait_for_screen(lead.pane(), |screen| screen.contains(THEME_NOTICE));

    assert!(
        screen.contains(
            "evaluate (experimental): screening webfetch, websearch via 127.0.0.1 (lead and \
             subagents); webfetch not screened (allow_private)"
        ),
        "the opening line discloses the screen:\n{screen}"
    );
    assert!(!screen.contains(KEY), "the key is never drawn:\n{screen}");
}

#[test]
fn a_lead_with_no_judge_says_nothing_about_screening() {
    let homes = Homes::new();
    let config = config(&homes);
    let config = config.display().to_string();
    // The same config, and no key: the screen names two sources and there is
    // nothing configured to screen them with.
    let lead = Lead::start(&homes, &script(&homes), WITHHELD, &[("GANJA_CONFIG", config.as_str())]);

    let screen = lead.wait_for_screen(lead.pane(), |screen| screen.contains(THEME_NOTICE));

    for said in ["evaluate (experimental)", "screening", "(lead and subagents)", "allow_private"] {
        assert!(!screen.contains(said), "a lead with no judge says {said:?}:\n{screen}");
    }
}

/// The second terminal a person opens in a checkout: two judging leads in one
/// project, binding into one socket directory. Both take the project root's
/// name, so the second one's first socket pass meets a live holder of it and
/// says so before its first frame — and says it **after** the disclosure,
/// never in its place, so that line still tells the person that tool results
/// leave the machine before any has.
#[test]
fn a_second_lead_in_one_project_opens_on_the_disclosure_beside_its_name_collision() {
    /// What the second lead's first socket pass says, up to the name.
    const COLLISION: &str = "another session is already registered as";
    /// One source and no `allow_private`, so the disclosure and the
    /// collision's holder and path fit one status line side by side.
    const DISCLOSURE: &str =
        "evaluate (experimental): screening websearch via 127.0.0.1 (lead and subagents)";

    let homes = Homes::new();
    let config = homes.data().join("collision.toml");
    std::fs::write(&config, "[evaluate]\nscreen = [\"websearch\"]\n")
        .expect("the config is writable");
    let config = config.display().to_string();
    let judging = [
        ("GANJA_CONFIG", config.as_str()),
        ("TYPESAFE_API_KEY", KEY),
        ("TYPESAFE_BASE_URL", "http://127.0.0.1:9"),
    ];
    let script = script(&homes);
    // `Lead::start` returns once the first lead has drawn its first frame,
    // which comes after it bound and registered its name.
    let _first = Lead::start(&homes, &script, WITHHELD, &judging);
    let second = Lead::start(&homes, &script, WITHHELD, &judging);

    let screen = second.wait_for_screen(second.pane(), |screen| screen.contains(COLLISION));

    let line = screen
        .lines()
        .find(|line| line.contains(COLLISION))
        .expect("the screen that was waited for holds the collision");
    let disclosed = line
        .find(DISCLOSURE)
        .unwrap_or_else(|| panic!("the line naming the collision lost the disclosure:\n{screen}"));
    let collided = line.find(COLLISION).expect("the line was found by it");
    assert!(disclosed < collided, "the disclosure leads and the collision follows:\n{screen}");
    assert!(!screen.contains(KEY), "the key is never drawn:\n{screen}");
}

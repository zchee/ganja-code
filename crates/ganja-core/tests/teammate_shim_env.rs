//! What a shim child's environment carries from its lead — and what it must
//! not (**D508**, D502's posture adapted).
//!
//! A binary of its own because it **plants variables in this process**, which
//! is the only honest way to ask whether the child inherited them: an
//! enumeration asserted against an environment holding nothing interesting
//! asserts nothing. `cargo test` runs a binary's tests on parallel threads and
//! `set_var` is process-wide, so this file holds exactly one test — the same
//! rule `secrets_env.rs` states and for the same reason.
//!
//! It extends that file's canary discipline to the shim: a credential the lead
//! holds must not reach another vendor's CLI, which is a foreign process that
//! will do whatever it likes with its own environment, and a `GROK_*` variable
//! must not reach one either — that vendor has at least three environment doors
//! onto the very posture D508(a) pins, so inheriting one would silently undo
//! the grant a person was asked about at spawn.

#![cfg(unix)]

mod shim_support;

use std::{sync::Arc, time::Duration};

use ganja_core::teammate::shim;
use ganja_testkit::AllowSpawn;
use shim_support::{FakeCli, Mode, PerMessage, until};

/// A credential the lead holds. Its **name** is what a child's environment
/// listing would show, and the name is what this greps for — the fake reports
/// names and never values, which is the same rule the pane fixture keeps.
const CANARY: &str = "ANTHROPIC_API_KEY";

/// The one grok variable that would silently undo the pinned sandbox profile,
/// and two more beside it, so the assertion is about the class rather than
/// about the one name somebody happened to think of.
const GROK_DOORS: [&str; 3] = [
    "GROK_SANDBOX",
    "GROK_SANDBOX_AUTO_ALLOW_BASH",
    "GROK_SANDBOX_PROFILE",
];

/// How long the fake gets to be started and report.
const ANSWERS: Duration = Duration::from_secs(20);

#[tokio::test]
async fn a_lead_holding_a_credential_hands_none_of_it_to_a_foreign_cli() {
    // SAFETY: this binary holds one test for exactly this reason — nothing else
    // in it reads or writes the environment concurrently.
    unsafe {
        std::env::set_var(CANARY, "sk-ant-CANARY-never-in-a-foreign-child");
        for name in GROK_DOORS {
            std::env::set_var(name, "off");
        }
    }

    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::install();
    let (registry, door) = shim_support::lead(
        home.path(),
        home.path(),
        Arc::new(PerMessage::new(&cli.log, Mode::Answer)),
        cli.path(),
    );

    door.start(
        ganja_testkit::spawn_with_prompt("w1", Some("codex"), "hold the fort"),
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
    assert!(
        names.contains(&"HOME".to_owned()),
        "the enumeration really did travel, so an absence below means something: {names:?}"
    );
    assert!(
        !names.contains(&CANARY.to_owned()),
        "a credential the lead holds must not reach another vendor's CLI: {names:?}"
    );

    // The class rule, asserted against a parent that really holds all three.
    for name in GROK_DOORS {
        assert!(
            !names.contains(&name.to_owned()),
            "{name} would have silently moved the posture a person was asked about: {names:?}"
        );
    }
    for name in &names {
        assert!(
            !name.starts_with("GROK_"),
            "and the rule is the class, not the three: {name}"
        );
    }
    // Which is a property of the enumeration itself, not of this driver's
    // additions list: nothing may put such a name on one.
    for name in shim::CARRIED {
        assert!(!name.starts_with("GROK_"), "{name}");
    }

    registry.shutdown().await;
}

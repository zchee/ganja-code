//! Holds the running tmux's own command inventory against this crate's.
//!
//! Synthesized, with no Go counterpart: the specification speaks only the
//! control-mode protocol and never enumerates tmux's command set.
//!
//! # Why the running tmux and not a checked-in list
//!
//! A list in this repository would answer the question "does this crate
//! agree with itself", which it always does. Reading `tmux list-commands`
//! asks the only question worth asking — does this crate know about the
//! commands the tmux on this machine actually has — and it answers it again
//! every time tmux is upgraded, naming whatever it grew.
//!
//! The direction is deliberately one-way: every command tmux has must be
//! either typed or explicitly excluded, while a command *this crate* names
//! and this tmux lacks is not a failure, because the tables are written
//! against a newer tmux than the oldest one they are meant to serve — an
//! older tmux would otherwise fail this suite for having fewer commands, and
//! for the wrong reason. A misspelling in either table is caught all the
//! same, and by the same assertion: the correctly spelled command then
//! appears in neither, which is the failure below.
//!
//! Like `tests/live.rs`, this hard-fails when tmux is unavailable rather than
//! skipping: a green run that compared against nothing would be worthless.
//! Unlike it, nothing here starts a server — `list-commands` is answered by
//! the client alone.

use std::{collections::BTreeMap, process::Command};

use tmux::commands::{EXCLUDED, REGISTRY};

/// One command as the running tmux describes it: its name, and its own
/// abbreviation when it has one.
fn installed() -> BTreeMap<String, Option<String>> {
    let output = Command::new("tmux")
        .arg("list-commands")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "the tmux crate's inventory test requires a runnable tmux binary on PATH; \
                 `tmux list-commands` could not start: {error}"
            )
        });
    assert!(
        output.status.success(),
        "the tmux crate's inventory test requires a runnable tmux binary on PATH; \
         `tmux list-commands` exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let listing = String::from_utf8(output.stdout)
        .expect("tmux prints its own command names, which are ASCII");
    let commands: BTreeMap<String, Option<String>> = listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?.to_owned();
            // tmux prints `split-window (splitw) [-flags…]`, and simply
            // `kill-server` for the commands it has no abbreviation for.
            let alias = words.next().and_then(|word| {
                word.strip_prefix('(')
                    .and_then(|word| word.strip_suffix(')'))
                    .map(ToOwned::to_owned)
            });
            Some((name, alias))
        })
        .collect();
    assert!(
        !commands.is_empty(),
        "`tmux list-commands` printed nothing this test could read"
    );

    commands
}

#[test]
fn every_command_this_tmux_has_is_either_typed_or_excluded_by_name() {
    let installed = installed();
    let mut unclaimed: Vec<String> = Vec::new();
    let mut typed = 0usize;
    let mut excluded = 0usize;

    for (name, alias) in &installed {
        let claimed = REGISTRY
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| ("typed", entry.alias));
        let claimed = claimed.or_else(|| {
            EXCLUDED
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| ("excluded", entry.alias))
        });

        match claimed {
            Some(("typed", claimed_alias)) => {
                typed += 1;
                assert_eq!(
                    claimed_alias.map(ToOwned::to_owned),
                    *alias,
                    "the register spells {name}'s abbreviation differently from tmux itself"
                );
            }
            Some((_, claimed_alias)) => {
                excluded += 1;
                assert_eq!(
                    claimed_alias.map(ToOwned::to_owned),
                    *alias,
                    "the exclusion table spells {name}'s abbreviation differently from tmux itself"
                );
            }
            None => unclaimed.push(name.clone()),
        }
    }

    assert!(
        unclaimed.is_empty(),
        "this tmux has {} command(s) in neither the register nor the exclusion table: {}\n\
         Each needs a builder in src/commands/, or a row in EXCLUDED saying why not.",
        unclaimed.len(),
        unclaimed.join(", ")
    );
    assert_eq!(
        typed + excluded,
        installed.len(),
        "every command must be counted exactly once: {typed} typed + {excluded} excluded \
         against {} installed",
        installed.len()
    );

    // Read with `--nocapture`; the shrinking half is the progress measure.
    println!(
        "tmux command inventory: {} installed, {typed} typed, {excluded} awaiting a family",
        installed.len()
    );
}

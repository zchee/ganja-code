use super::*;
use crate::commands::panes::{ListPanes, SplitWindow};

#[test]
fn a_command_with_nothing_set_is_its_name_alone() {
    assert_eq!(words(&ListPanes::new()), ["list-panes"]);
}

#[test]
fn a_switch_asked_for_twice_is_still_one_flag() {
    assert_eq!(
        words(&ListPanes::new().all().all()),
        ["list-panes", "-a"],
        "argv is a set of flags, not a tally of how often each was asked for"
    );
}

#[test]
fn a_value_set_twice_keeps_the_last_in_the_first_position() {
    assert_eq!(
        words(
            &SplitWindow::new()
                .start_directory("/one")
                .detached()
                .start_directory("/two")
        ),
        ["split-window", "-c", "/two", "-d"],
        "a caller who sets -c twice is correcting themselves, not asking for two directories"
    );
}

#[test]
fn a_repeatable_flag_keeps_every_value_in_call_order() {
    assert_eq!(
        words(&SplitWindow::new().environment("A=1").environment("B=2")),
        ["split-window", "-e", "A=1", "-e", "B=2"],
        "tmux reads -e once per variable, so the builder must not fold them together"
    );
}

#[test]
fn flags_keep_the_order_they_were_asked_for() {
    assert_eq!(
        words(&ListPanes::new().format("#{pane_id}").all()),
        ["list-panes", "-F", "#{pane_id}", "-a"]
    );
}

#[cfg(unix)]
#[test]
fn a_value_outside_utf8_survives_into_argv_byte_for_byte() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let directory = OsString::from_vec(b"/tmp/a\x80b".to_vec());
    let argv = SplitWindow::new().start_directory(directory).args();
    assert_eq!(
        argv.last().map(|word| word.as_bytes()),
        Some(&b"/tmp/a\x80b"[..]),
        "a working directory is a path, and a path is not obliged to be UTF-8"
    );
}

#[test]
fn a_trailing_command_is_fenced_off_from_the_flags() {
    assert_eq!(
        words(
            &SplitWindow::new()
                .detached()
                .command(["sh", "-c", "sleep 1"])
        ),
        ["split-window", "-d", "--", "sh", "-c", "sleep 1"],
        "without the fence, a program named like a flag would be read as one"
    );
}

#[test]
fn a_trailing_command_set_twice_replaces_rather_than_appends() {
    assert_eq!(
        words(&SplitWindow::new().command(["first"]).command(["second"])),
        ["split-window", "--", "second"]
    );
}

#[test]
fn the_trait_and_the_register_agree_about_a_commands_names() {
    assert_eq!(SplitWindow::NAME, "split-window");
    assert_eq!(SplitWindow::ALIAS, Some("splitw"));
    assert!(
        REGISTRY
            .iter()
            .any(|entry| entry.name == SplitWindow::NAME && entry.alias == SplitWindow::ALIAS),
        "a builder tmux knows about but the register does not would be invisible to the \
             inventory test"
    );
}

/// Every command every family declares reaches [`REGISTRY`] whole.
///
/// Deliberately not a length comparison: `REGISTRY`'s length *is*
/// `total(FAMILIES)` by its own type, so holding one against the other
/// asks the type system a question it has already answered. What is
/// worth asking is whether the entries themselves survived the
/// flattening — a family dropped, an entry overwritten, or a filler row
/// left where a name should be.
#[test]
fn the_register_carries_every_family_entry_and_no_filler() {
    for family in FAMILIES {
        assert!(
            !family.is_empty(),
            "a family declaring nothing is a table that stopped expanding, and a register \
                 measured by length alone would never say so"
        );
        for entry in *family {
            assert!(
                !entry.name.is_empty(),
                "an unnamed entry is the flattening's own filler showing through, which means \
                     it copied fewer entries than it made room for"
            );
            assert!(
                REGISTRY.iter().any(|listed| listed == entry),
                "{} is declared by a family and missing from the register, so nothing would \
                     ever hold it against tmux",
                entry.name
            );
        }
    }
}

#[test]
fn a_commands_flags_reach_the_register_with_their_arity() {
    let split = REGISTRY
        .iter()
        .find(|entry| entry.name == "split-window")
        .expect("split-window is typed");
    assert!(
        split.flags.contains(&Flag {
            letter: "-d",
            argument: false,
        }),
        "-d takes nothing, and a register claiming otherwise would send the inventory test \
             looking for an argument tmux does not want"
    );
    assert!(
        split.flags.contains(&Flag {
            letter: "-c",
            argument: true,
        }),
        "-c takes a working directory"
    );
    assert!(
        split
            .flags
            .iter()
            .all(|flag| flag.letter.starts_with('-') && flag.letter.len() >= 2),
        "a flag is carried the way tmux reads it in argv, leading dash and all"
    );
    assert_eq!(
        REGISTRY
            .iter()
            .find(|entry| entry.name == "kill-server")
            .map(|entry| entry.flags.len()),
        Some(0),
        "kill-server takes no flags, so its entry must declare none rather than inherit a \
             neighbour's"
    );
}

/// A letter double-claimed would pass the inventory test twice over —
/// the served probe verifies it while the shelf probe reports it
/// arrived — so the one place the mistake is visible is here, before
/// any tmux is asked.
#[test]
fn a_letter_is_served_or_shelved_but_never_both() {
    for entry in REGISTRY {
        let mut seen = std::collections::BTreeSet::new();
        for flag in entry.flags.iter().chain(entry.ahead) {
            assert!(
                seen.insert(flag.letter),
                "{} claims {} more than once across its served rows and its shelf, and the \
                     probes would quietly confirm both claims",
                entry.name,
                flag.letter
            );
        }
        assert!(
            entry
                .ahead
                .iter()
                .all(|flag| flag.letter.starts_with('-') && flag.letter.len() >= 2),
            "a shelved flag is carried the way tmux reads it in argv, leading dash and all, \
                 or the day it is unshelved the probes ask about a different word"
        );
    }
}

#[test]
fn no_command_is_both_typed_and_excluded() {
    for entry in REGISTRY {
        assert!(
            !EXCLUDED.iter().any(|excluded| excluded.name == entry.name),
            "{} is typed, so its exclusion is a leftover that would outlive its reason",
            entry.name
        );
    }
}

#[test]
fn no_name_or_alias_is_claimed_twice() {
    let mut seen: Vec<&str> = Vec::new();
    for (name, alias) in REGISTRY
        .iter()
        .map(|entry| (entry.name, entry.alias))
        .chain(EXCLUDED.iter().map(|entry| (entry.name, entry.alias)))
    {
        for word in std::iter::once(name).chain(alias) {
            assert!(
                !seen.contains(&word),
                "{word:?} is claimed twice; tmux has one command per name and one per alias"
            );
            seen.push(word);
        }
    }
}

#[test]
fn every_exclusion_says_why() {
    for entry in EXCLUDED {
        assert!(
            !entry.reason.trim().is_empty(),
            "{} is excluded without a reason, which is an omission wearing a table's clothes",
            entry.name
        );
    }
}

/// The one-shot surface imports nothing from `control_mode` — the
/// quoting quarantine, held by a machine instead of a reviewer.
///
/// Doc comments may *name* the boundary (the accepted exception every
/// wave carried), so comment lines are stripped before the assertion;
/// what remains is code, and code mentioning `control_mode` in any of
/// these six files is a crossing. `error.rs` is deliberately outside
/// the list: shared vocabulary wraps *two* control-mode types by the
/// architecture's own assignment — `CommandError` wraps a
/// `control_mode::protocol::Response`, and `InvalidCommand` wraps a
/// `control_mode::commandline::RenderError`.
#[test]
fn the_one_shot_surface_crosses_the_transport_boundary_only_in_prose() {
    // Built at run time so this test's own source — which the list below
    // includes — does not carry the token it hunts.
    let needle = ["control", "_mode"].concat();
    let surface: &[(&str, &str)] = &[
        ("server.rs", include_str!("../server.rs")),
        ("commands/mod.rs", include_str!("mod.rs")),
        ("commands/panes.rs", include_str!("panes.rs")),
        ("commands/sessions.rs", include_str!("sessions.rs")),
        ("commands/buffers_keys.rs", include_str!("buffers_keys.rs")),
        ("commands/options_misc.rs", include_str!("options_misc.rs")),
    ];
    for (name, source) in surface {
        for (index, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            assert!(
                !code.contains(&needle),
                "{name}:{} reaches across the transport boundary: {line:?}",
                index + 1
            );
        }
    }
}

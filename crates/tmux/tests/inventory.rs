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
//! And like it, every call pins a private `-S` socket and an empty `-f`
//! config: `list-commands` is answered from the client's own tables, but a
//! client given no socket honors `$TMUX` — the developer's live server — and
//! with no server running **creates** the default one, sourcing the
//! person's own config on the way (measured; the Phase-4 security review's
//! finding 1). The throwaway server these calls start is killed on drop.

use std::{collections::BTreeMap, path::PathBuf, process::Command};

use tmux::commands::{EXCLUDED, REGISTRY};

/// A private tmux to ask, so no probe ever reaches the developer's server.
struct PrivateTmux {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    config: PathBuf,
}

impl PrivateTmux {
    fn start() -> Self {
        let dir = tempfile::TempDir::new().expect("a scratch directory is made");
        let config = dir.path().join("empty.conf");
        std::fs::write(&config, b"").expect("an empty config is written");

        Self {
            socket: dir.path().join("inventory.sock"),
            config,
            _dir: dir,
        }
    }

    /// A client invocation against the private server and nothing else.
    fn client(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.config)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");

        command
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// One command as the running tmux describes it: its name, and its own
/// abbreviation when it has one.
fn installed(tmux: &PrivateTmux) -> BTreeMap<String, Option<String>> {
    let output = tmux
        .client()
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
    let tmux = PrivateTmux::start();
    let installed = installed(&tmux);
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

/// One command as the running tmux resolves it: the canonical name and
/// abbreviation it prints when asked about a single word.
///
/// `None` when this tmux does not know the word at all — which is a fact
/// about the installed version, not a verdict, for the same reason the test
/// above runs one-way.
fn resolve(tmux: &PrivateTmux, word: &str) -> Option<(String, Option<String>)> {
    let output = tmux
        .client()
        .args(["list-commands", word])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "the tmux crate's inventory test requires a runnable tmux binary on PATH; \
                 `tmux list-commands {word}` could not start: {error}"
            )
        });
    if !output.status.success() {
        let refusal = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // tmux tells the two refusals apart itself, and only one of them is
        // survivable: a word it has never heard of may simply postdate this
        // tmux, while a word it finds *several* commands for was never an
        // abbreviation of any single one, whatever the register claims.
        assert!(
            !refusal.starts_with("ambiguous command"),
            "the register claims {word:?} as a command word, but tmux reads it as several: \
             {refusal}"
        );
        return None;
    }

    let printed = String::from_utf8(output.stdout)
        .expect("tmux prints its own command names, which are ASCII");
    let mut words = printed.split_whitespace();
    let name = words.next().unwrap_or_else(|| {
        panic!("`tmux list-commands {word}` printed nothing this test could read")
    });
    let alias = words.next().and_then(|word| {
        word.strip_prefix('(')
            .and_then(|word| word.strip_suffix(')'))
            .map(ToOwned::to_owned)
    });

    Some((name.to_owned(), alias))
}

/// Every abbreviation the register claims is a word this tmux answers to,
/// and answers to with the command the register names.
///
/// This deliberately does **not** repeat the test above, which reads the
/// bulk `list-commands` listing and holds the abbreviation printed there
/// against the register's. A listing proves tmux *prints* a word; it cannot
/// prove tmux *accepts* it. That second half is the whole reason [`Entry`]
/// carries an alias at all — a consumer's config may be written in
/// abbreviations — so it is asked here directly: each entry is resolved by
/// the shortest word it claims, and the canonical pair tmux answers with
/// must be the pair the register holds.
///
/// The direction stays one-way for the reason the module doc gives: a
/// register entry this tmux has never heard of is reported rather than
/// failed, because the tables are written against a newer tmux than the
/// oldest they are meant to serve. An *ambiguous* word is a failure all the
/// same — that one is a claim about this tmux that this tmux contradicts.
///
/// [`Entry`]: tmux::commands::Entry
#[test]
fn every_abbreviation_the_register_claims_resolves_to_the_command_it_names() {
    let tmux = PrivateTmux::start();
    let mut unknown: Vec<&str> = Vec::new();
    let mut resolved = 0usize;

    for entry in REGISTRY {
        // The abbreviation when there is one: resolving by it proves both
        // halves at once, since tmux answers with the canonical pair.
        let word = entry.alias.unwrap_or(entry.name);
        let Some((name, alias)) = resolve(&tmux, word) else {
            unknown.push(entry.name);
            continue;
        };

        resolved += 1;
        assert_eq!(
            name, entry.name,
            "this tmux resolves {word:?} to {name}, not to the {} the register claims it for",
            entry.name
        );
        assert_eq!(
            alias.as_deref(),
            entry.alias,
            "this tmux spells {}'s abbreviation differently from the register",
            entry.name
        );
    }

    // Read with `--nocapture`. Not an assertion: on the tmux the tables were
    // written against this list is empty, and on an older one every name in
    // it is a command that tmux simply does not have yet.
    println!(
        "tmux command resolution: {resolved} of {} register entries resolved{}",
        REGISTRY.len(),
        if unknown.is_empty() {
            String::new()
        } else {
            format!("; unknown to this tmux: {}", unknown.join(", "))
        }
    );
}

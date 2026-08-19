//! Holds the running tmux's own command inventory against this crate's:
//! which commands it has, which words it answers to, and — flag by flag —
//! what its parser makes of the argv this crate's builders assemble.
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
//! and this tmux lacks is not a failure, because the register serves tmuxes
//! on both sides of its 3.7c floor — an apt tmux is missing commands the
//! floor has, the next tmux keeps what only it takes in `Entry::ahead`, and
//! `switch-mode` is typed ahead of the floor outright — so an older tmux
//! must not fail this suite for having fewer commands, and for the wrong
//! reason. A misspelling in either table is caught all the same, and by the
//! same assertion: the correctly spelled command then appears in neither,
//! which is the failure below.
//!
//! Like `tests/live.rs`, this hard-fails when tmux is unavailable rather than
//! skipping: a green run that compared against nothing would be worthless.
//! And like it, every call pins a private `-S` socket and an empty `-f`
//! config: `list-commands` is answered from the client's own tables, but a
//! client given no socket honors `$TMUX` — the developer's live server — and
//! with no server running **creates** the default one, sourcing the
//! person's own config on the way (measured; the Phase-4 security review's
//! finding 1). The throwaway server these calls start is killed on drop.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
};

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

    /// The same private tmux with a server of its own behind it.
    ///
    /// `list-commands` is answered by the client alone, but *parsing* a
    /// command line is the server's job — with no server, a client refuses
    /// before it has read a word of what it was asked — so the flag probes
    /// need one running. [`Drop`] kills it like any other.
    fn serving() -> Self {
        let tmux = Self::start();
        let started = tmux
            .client()
            .args(["new-session", "-d", "-s", "flag-probe"])
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "the tmux crate's inventory test requires a runnable tmux binary on PATH; \
                     `tmux new-session` could not start: {error}"
                )
            });
        assert!(
            started.status.success(),
            "the private tmux server could not be started, so nothing could be asked to parse \
             anything: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        tmux
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
    // tmux 3.5 answers a word it does not know with an empty listing and
    // success where newer ones refuse aloud — the same fact in a quieter
    // dialect, so it reads as the same `None`.
    let name = words.next()?;
    let alias = words.next().and_then(|word| {
        word.strip_prefix('(')
            .and_then(|word| word.strip_suffix(')'))
            .map(ToOwned::to_owned)
    });

    Some((name.to_owned(), alias))
}

/// Every word the register claims is one this tmux answers to, and answers
/// to with the command the register names.
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
/// Only 78 of the 92 are resolved by a genuine abbreviation, because only
/// 78 have one: tmux gives `kill-server` and thirteen others no alias at
/// all, and those are resolved by their full name, which proves the name
/// and not much else. The summary line splits the two so the stronger half
/// is not read as covering the weaker.
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
    let mut by_abbreviation = 0usize;
    let mut by_name = 0usize;

    for entry in REGISTRY {
        // The abbreviation when there is one: resolving by it proves both
        // halves at once, since tmux answers with the canonical pair.
        let word = entry.alias.unwrap_or(entry.name);
        let Some((name, alias)) = resolve(&tmux, word) else {
            unknown.push(entry.name);
            continue;
        };

        if entry.alias.is_some() {
            by_abbreviation += 1;
        } else {
            by_name += 1;
        }
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
        "tmux command resolution: {} of {} register entries resolved — {by_abbreviation} by a \
         genuine abbreviation, {by_name} by their full name, which is all tmux gives them{}",
        by_abbreviation + by_name,
        REGISTRY.len(),
        if unknown.is_empty() {
            String::new()
        } else {
            format!("; unknown to this tmux: {}", unknown.join(", "))
        }
    );
}

/// The key table the probes below bind into, and the key they bind.
///
/// A table nothing ever switches to, so a binding made here cannot be
/// pressed: these probes need tmux to *parse* a command line, and must never
/// leave one a keystroke could run.
const PROBE_TABLE: &str = "ganja-flag-probe";
const PROBE_KEY: &str = "q";

/// What this tmux's parser made of one probed command line.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// It parsed — or was refused for something that is not about a flag.
    /// tmux reads flags before it counts arguments (measured: `display-menu
    /// -T` answers `-T expects an argument`, not `too few arguments`), so a
    /// complaint about the count still says every flag was read.
    Read,
    /// `-X expects an argument`: tmux wants the word after it.
    WantsArgument,
    /// `unknown flag -X`: this tmux has no such flag on this command.
    Unknown,
    /// `unknown command`: this tmux lacks the command itself — a fact about
    /// the installed version, not about any flag, so the flag probes skip
    /// the whole entry rather than read it as one flag's verdict.
    NoSuchCommand,
    /// `too many arguments`: what followed the flags outnumbered what this
    /// command takes. Used to find where that boundary is.
    TooMany,
    /// Anything else, carried verbatim rather than guessed at.
    Unexpected(String),
}

/// Asks this tmux to parse `command` with `words` after it, without running
/// it.
///
/// `bind-key` parses the command it is handed — an unknown flag or a missing
/// argument is refused there and then, naming the inner command — and stores
/// the result against a key. Nothing runs until that key is pressed, and
/// [`PROBE_TABLE`] is a table no binding switches to, so nothing ever
/// presses it. That is what makes this safe to point at `kill-server`.
fn parse_only(tmux: &PrivateTmux, command: &str, words: &[&str]) -> Verdict {
    let output = tmux
        .client()
        .args(["bind-key", "-T", PROBE_TABLE, PROBE_KEY, command])
        .args(words)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "the tmux crate's inventory test requires a runnable tmux binary on PATH; \
                 `tmux bind-key` could not start: {error}"
            )
        });
    if output.status.success() {
        return Verdict::Read;
    }

    let refusal = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if refusal.contains("unknown command") {
        Verdict::NoSuchCommand
    } else if refusal.contains("unknown flag") {
        Verdict::Unknown
    } else if refusal.contains("expects an argument") {
        Verdict::WantsArgument
    } else if refusal.contains("too many arguments") {
        Verdict::TooMany
    } else if refusal.contains("too few arguments") {
        Verdict::Read
    } else {
        Verdict::Unexpected(refusal)
    }
}

/// A flag this tmux takes **either** way: bare, or with the word after it.
///
/// The bare probe cannot tell an optional argument from no argument at all,
/// so without this table these four would read as four wrong declarations.
/// They are not wrong: the builder always spells the amount, which is the
/// form a caller wants, and the bare spelling is left to `Server::run` —
/// which is what the command's own doc says. Each row still has to earn its
/// place through [`consumes_the_word_after_it`] or through the builder's
/// own spelling parsing whole — 3.7 reads the amount as the positional
/// adjustment beside the flag, the next tmux as the flag's own optional
/// argument, and both read it to the same effect; an allowance nobody
/// measures would be exactly the kind of claim this test exists to stop
/// taking on trust.
struct Optional {
    /// The command it is a flag of.
    command: &'static str,
    /// The flag, as tmux spells it.
    letter: &'static str,
    /// What tmux does when the argument is left off — the reason the bare
    /// form is a real spelling rather than a mistake.
    reason: &'static str,
}

const OPTIONAL: &[Optional] = &[
    Optional {
        command: "resize-pane",
        letter: "-D",
        reason: "resizes down by one; the usage string spells the argument required and the \
                 parser does not",
    },
    Optional {
        command: "resize-pane",
        letter: "-L",
        reason: "resizes left by one; the usage string spells the argument required and the \
                 parser does not",
    },
    Optional {
        command: "resize-pane",
        letter: "-R",
        reason: "resizes right by one; the usage string spells the argument required and the \
                 parser does not",
    },
    Optional {
        command: "resize-pane",
        letter: "-U",
        reason: "resizes up by one; the usage string spells the argument required and the \
                 parser does not",
    },
];

/// Whether this tmux reads the word after `letter` as that flag's argument.
///
/// Asked of the parser rather than assumed, because the bare probe cannot
/// answer it: a flag whose argument is optional and a flag that takes none
/// look identical when asked bare, and the difference is the whole of what
/// the builder's `-D 5` means. Words are added after the command until this
/// tmux refuses them as too many — one more than it accepts as positional
/// arguments — and if those same words parse with the flag in front, the
/// flag ate one of them.
fn consumes_the_word_after_it(tmux: &PrivateTmux, command: &str, letter: &str) -> bool {
    for count in 1..=4 {
        let words = vec!["1"; count];
        if parse_only(tmux, command, &words) != Verdict::TooMany {
            continue;
        }

        let mut with_flag = Vec::with_capacity(count + 1);
        with_flag.push(letter);
        with_flag.extend_from_slice(&words);

        return parse_only(tmux, command, &with_flag) == Verdict::Read;
    }

    false
}

/// The flag letters this tmux's own usage strings document, per command.
///
/// Read from the one bulk `list-commands`, which prints each command's usage
/// line beside its name: `[-bdefhIklPvWZ]` is a run of flags taking nothing,
/// `[-c start-directory]` one that takes a word, and a bracket opening on
/// anything but a dash — `[shell-command …]`, `[adjustment]` — is not a flag
/// at all.
fn documented(tmux: &PrivateTmux) -> BTreeMap<String, BTreeSet<char>> {
    let output = tmux
        .client()
        .arg("list-commands")
        .output()
        .expect("`tmux list-commands` runs, as the test above already required");

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?.to_owned();
            let letters = words
                .filter_map(|word| word.strip_prefix("[-"))
                .flat_map(|group| group.trim_end_matches(']').chars())
                .collect();

            Some((name, letters))
        })
        .collect()
}

/// Every flag the register declares is a flag this tmux has, and takes the
/// same way the builder spells it.
///
/// The register's third claim, and the one nothing measured until now: a
/// family table says `-d` takes nothing and `-c` takes a directory, and a
/// builder emits argv on that word alone. So each is put to the parser
/// itself — `bind-key` reads a command line and refuses a bad flag without
/// running anything — and the answer is read from tmux's own two refusals:
/// `unknown flag -X` says the table invented a flag, and `-X expects an
/// argument` says whether a word follows.
///
/// The direction is one-way here as everywhere in this file: a command an
/// older tmux lacks skips its flags, and a served flag one refuses as
/// `unknown` is reported rather than failed, because the register serves
/// tmuxes on both sides of its 3.7c floor — an apt tmux lacks `new-pane`
/// whole, and the next tmux's own additions sit in [`Entry::ahead`] rather
/// than among the served rows. On the floor itself every list is empty,
/// which is where a misspelled letter still fails. What *is* asserted on
/// every version is arity: a flag both sides have but disagree about — a
/// word demanded where the table says none, a builder's spelling no parse
/// will read — is an argv whose meaning moved, and no version difference
/// excuses that. The shelf is put to the same parser: a shelved letter the
/// running tmux serves is reported as already arrived — and failed if it
/// demands a word the shelf says it does not take, because then the shelf
/// preserves the wrong knowledge. The other direction stays one-way as
/// everywhere else: a letter this tmux documents that neither the rows nor
/// the shelf claim is named, never failed.
///
/// [`Entry::ahead`]: tmux::commands::Entry::ahead
#[test]
fn every_declared_flag_is_one_this_tmux_takes_the_same_way() {
    let tmux = PrivateTmux::serving();
    let declared: usize = REGISTRY.iter().map(|entry| entry.flags.len()).sum();
    let mut verified = 0usize;
    let mut optional: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    let mut arrived: Vec<String> = Vec::new();
    let mut not_yet: Vec<String> = Vec::new();

    'entries: for entry in REGISTRY {
        for flag in entry.flags {
            let named = format!("{} {}", entry.name, flag.letter);
            match (parse_only(&tmux, entry.name, &[flag.letter]), flag.argument) {
                (Verdict::NoSuchCommand, _) => {
                    missing.push(entry.name.to_owned());
                    continue 'entries;
                }
                (Verdict::Unknown, _) => refused.push(named),
                (Verdict::WantsArgument, true) | (Verdict::Read | Verdict::TooMany, false) => {
                    verified += 1;
                }
                (Verdict::WantsArgument, false) => mismatched.push(format!(
                    "{named} is declared as taking nothing, and this tmux refuses it without an \
                     argument"
                )),
                (Verdict::Read | Verdict::TooMany, true) => {
                    let allowed = OPTIONAL.iter().find(|allowance| {
                        allowance.command == entry.name && allowance.letter == flag.letter
                    });
                    let Some(allowed) = allowed else {
                        mismatched.push(format!(
                            "{named} is declared as taking an argument, and this tmux takes it \
                             bare"
                        ));
                        continue;
                    };

                    if consumes_the_word_after_it(&tmux, entry.name, flag.letter) {
                        verified += 1;
                        optional.push(named);
                    } else if parse_only(&tmux, entry.name, &[flag.letter, "1"]) == Verdict::Read {
                        // The word lands in the positional slot beside the
                        // flag instead of on it — 3.7 spells the resize
                        // adjustment that way — and the command reads it to
                        // the same effect, so the builder's argv still
                        // means what the table says it means.
                        verified += 1;
                        positional.push(named);
                    } else {
                        mismatched.push(format!(
                            "{named} is declared with an argument and allowed to be, because it \
                             {}; but this tmux neither reads the word after it as its argument \
                             nor parses the builder's own spelling with the word beside it",
                            allowed.reason
                        ));
                    }
                }
                (Verdict::Unexpected(refusal), _) => panic!(
                    "probing {named} got a refusal this test cannot read, so the flag was neither \
                     confirmed nor denied: {refusal}"
                ),
            }
        }

        for flag in entry.ahead {
            let named = format!("{} {}", entry.name, flag.letter);
            match (parse_only(&tmux, entry.name, &[flag.letter]), flag.argument) {
                (Verdict::NoSuchCommand, _) => {
                    missing.push(entry.name.to_owned());
                    continue 'entries;
                }
                (Verdict::Unknown, _) => not_yet.push(named),
                (Verdict::WantsArgument, false) => mismatched.push(format!(
                    "{named} is shelved as taking nothing, and this tmux demands a word after \
                     it, so the shelf preserves the wrong knowledge"
                )),
                (Verdict::Unexpected(refusal), _) => {
                    panic!("probing shelved {named} got a refusal this test cannot read: {refusal}")
                }
                _ => arrived.push(named),
            }
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} declared flag(s) disagree with this tmux's parser about whether a word follows:\n  {}",
        mismatched.len(),
        mismatched.join("\n  ")
    );

    // The other direction, reported rather than asserted. A letter this tmux
    // documents and no builder declares is either a flag worth typing or a
    // usage string this tmux has outgrown, and the parser is asked which.
    let documented = documented(&tmux);
    let mut untyped: Vec<String> = Vec::new();
    let mut outgrown: Vec<String> = Vec::new();
    for entry in REGISTRY {
        let Some(letters) = documented.get(entry.name) else {
            continue;
        };
        let claimed: BTreeSet<char> = entry
            .flags
            .iter()
            .chain(entry.ahead)
            .flat_map(|flag| flag.letter.chars().skip(1))
            .collect();
        for letter in letters.difference(&claimed) {
            let named = format!("{} -{letter}", entry.name);
            match parse_only(&tmux, entry.name, &[&format!("-{letter}")]) {
                Verdict::Unknown => outgrown.push(named),
                _ => untyped.push(named),
            }
        }
    }

    // Read with `--nocapture`; every number is measured against the tmux that
    // ran the test, and every list but `mismatched` is a note, not a verdict.
    println!(
        "tmux flag arity: {declared} declared across {} commands — {verified} verified against \
         this tmux's parser, {} of them flags it also takes bare ({}), {} answered by the \
         positional beside the flag ({}); this tmux lacks {} whole command(s) ({}) and refuses \
         {} served flag(s) ({}); of the shelf, {} already arrived here ({}) and {} not yet \
         ({}); documented and not claimed: {} this tmux accepts ({}), {} its own parser \
         refuses ({})",
        REGISTRY.len(),
        optional.len(),
        list(&optional),
        positional.len(),
        list(&positional),
        missing.len(),
        list(&missing),
        refused.len(),
        list(&refused),
        arrived.len(),
        list(&arrived),
        not_yet.len(),
        list(&not_yet),
        untyped.len(),
        list(&untyped),
        outgrown.len(),
        list(&outgrown),
    );
}

/// A list for the summary lines, or the word for an empty one.
fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

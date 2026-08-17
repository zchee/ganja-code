//! AC-1b: documents a real Claude Code wrote, read by this crate and written
//! back, are the same bytes.
//!
//! **This is Driver 2's falsifier.** Everything else in this workspace that
//! touches the team format compares ganja against ganja: `claude_format.rs`
//! round-trips documents this repo produced, which proves the writer and the
//! reader agree and says nothing whatever about the peer they are supposed to
//! be sharing a directory with. The bytes under `tests/fixtures/` were written
//! by Claude Code (`tests/fixtures/PROVENANCE.md` — what was captured, when,
//! from where, and exactly which spans were redacted), so this binary is the
//! only place a mistake about somebody else's format can be caught before a
//! real `claude` reads a file ganja rewrote.
//!
//! **There is no skip branch, on purpose.** An interop test that quietly passes
//! when its fixture is absent is how a green suite starts meaning nothing: the
//! day the fixture goes missing — a bad merge, a `.gitignore` line, a partial
//! checkout — the suite would keep reporting the interop claim it is no longer
//! testing. So the fixture is committed, and a `read_dir` that fails *is* the
//! failure.
//!
//! The fixtures are read and never written. Where a test needs to mutate an
//! inbox — delivery prunes, which is the whole of §3.1 — it copies the captured
//! bytes into a temporary directory first, so a failing run cannot damage the
//! only real documents this repo has.

use std::{
    fs,
    path::{Path, PathBuf},
};

use ganja_team::{MemberRecord, TeamFile, mailbox, record};
use serde_json::value::RawValue;

/// Every captured document, sorted so a failure names the same file twice in a
/// row.
struct Captured {
    /// `<team>/config.json` — the team files.
    team_files: Vec<PathBuf>,
    /// `<team>/inboxes/<member>.json` — the mailboxes, empty ones included.
    inboxes: Vec<PathBuf>,
}

/// Where the capture lives, resolved off the manifest rather than the working
/// directory — the package root under `cargo` and `nextest`, and wherever the
/// shell happened to be for a binary run by hand.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The capture, or a failure saying what is missing.
///
/// Every error here is a panic rather than a skip — see the module note. The
/// sentences are written for whoever finds this red without having read the
/// plan.
fn captured() -> Captured {
    let root = fixtures();
    let entries = fs::read_dir(&root).unwrap_or_else(|error| {
        panic!(
            "AC-1b needs the captured Claude Code documents at {}, and they \
             could not be read ({error}). They are committed; a checkout that \
             lacks them is broken, not unsupported. See tests/fixtures/PROVENANCE.md.",
            root.display()
        )
    });

    let mut captured = Captured {
        team_files: Vec::new(),
        inboxes: Vec::new(),
    };
    for entry in entries {
        let team = entry.expect("a fixture directory entry reads").path();
        if !team.is_dir() {
            continue;
        }

        let config = team.join("config.json");
        if config.is_file() {
            captured.team_files.push(config);
        }
        let inboxes = team.join("inboxes");
        if inboxes.is_dir() {
            for inbox in fs::read_dir(&inboxes).expect("a captured inboxes directory reads") {
                let inbox = inbox.expect("a captured inbox entry reads").path();
                if inbox.extension().is_some_and(|kind| kind == "json") {
                    captured.inboxes.push(inbox);
                }
            }
        }
    }
    captured.team_files.sort();
    captured.inboxes.sort();

    captured
}

/// A path as a failure message should name it: relative to the fixture root, so
/// the message is the same on every machine.
fn named(path: &Path) -> String {
    path.strip_prefix(fixtures())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The raw bytes of each member of a team file, still in the order and the
/// spelling Claude wrote them.
///
/// Decoding through [`serde_json::Value`] would sort every key, and decoding
/// through [`TeamFile`] would lose the original bytes — which are the thing
/// under test. A borrowed [`RawValue`] keeps them.
fn raw_members(raw: &str) -> Vec<&RawValue> {
    #[derive(serde::Deserialize)]
    struct Members<'a> {
        #[serde(borrow)]
        members: Vec<&'a RawValue>,
    }

    serde_json::from_str::<Members<'_>>(raw)
        .expect("a captured team file holds an array of members")
        .members
}

/// One member's bytes, lifted out of the two levels the team file wraps them
/// in, so they can be compared against what [`record::document`] emits for that
/// member alone.
///
/// A member sits inside `members: [ … ]`, so its inner lines carry four spaces
/// this crate would not write. Line-based, which is safe because JSON escapes
/// every newline inside a string — no value can contain one.
fn unwrapped(member: &RawValue) -> String {
    member
        .get()
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                return line.to_owned();
            }
            line.strip_prefix("    ")
                .unwrap_or_else(|| panic!("a member's line sits four spaces in: {line:?}"))
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_capture_holds_what_the_criterion_needs() {
    let captured = captured();

    // Present is not enough: AC-1b's three legs are a real team file, a real
    // lead record beside a real teammate record, and a real inbox holding a
    // real message. A capture that had lost any of them would pass every other
    // test in this binary while testing less than it claims.
    assert!(
        !captured.team_files.is_empty(),
        "the capture holds no config.json"
    );
    assert!(
        !captured.inboxes.is_empty(),
        "the capture holds no inbox files"
    );

    let mut leads = 0;
    let mut teammates = 0;
    for path in &captured.team_files {
        let raw = fs::read_to_string(path).expect("a captured team file reads");
        let file: TeamFile = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("{} does not decode: {error}", named(path)));
        for member in &file.members {
            if member.is_lead() {
                leads += 1;
            } else {
                teammates += 1;
            }
        }
    }
    assert!(leads > 0, "the capture holds no lead record");
    assert!(teammates > 0, "the capture holds no teammate record");

    let messages: usize = captured
        .inboxes
        .iter()
        .map(|path| {
            mailbox::read(path)
                .expect("a captured inbox reads")
                .valid
                .len()
        })
        .sum();
    assert!(
        messages > 0,
        "the capture holds no message; delivered messages are pruned (§3.1), so \
         a capture taken from a settled team can be all empty inboxes"
    );
}

#[test]
fn a_real_claude_team_file_round_trips_byte_identical() {
    let captured = captured();

    for path in &captured.team_files {
        let raw = fs::read_to_string(path).expect("a captured team file reads");
        let file: TeamFile = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("{} does not decode: {error}", named(path)));

        assert_eq!(
            record::document(&file).expect("a team file re-encodes"),
            raw,
            "{} does not survive a rewrite",
            named(path)
        );
    }
}

#[test]
fn a_real_lead_record_round_trips_byte_identical() {
    let captured = captured();
    let mut checked = 0;

    for path in &captured.team_files {
        let raw = fs::read_to_string(path).expect("a captured team file reads");
        for member in raw_members(&raw) {
            let record: MemberRecord = serde_json::from_str(member.get()).unwrap_or_else(|error| {
                panic!("a member of {} does not decode: {error}", named(path))
            });
            if !record.is_lead() {
                continue;
            }

            checked += 1;
            assert_eq!(
                record::document(&record).expect("a record re-encodes"),
                unwrapped(member),
                "the lead record in {} does not survive a rewrite",
                named(path)
            );
        }
    }

    assert!(checked > 0, "no captured team file holds a lead record");
}

#[test]
fn a_real_teammate_record_round_trips_byte_identical() {
    let captured = captured();
    let mut checked = 0;

    for path in &captured.team_files {
        let raw = fs::read_to_string(path).expect("a captured team file reads");
        for member in raw_members(&raw) {
            let record: MemberRecord = serde_json::from_str(member.get()).unwrap_or_else(|error| {
                panic!("a member of {} does not decode: {error}", named(path))
            });
            if record.is_lead() {
                continue;
            }

            checked += 1;
            assert_eq!(
                record::document(&record).expect("a record re-encodes"),
                unwrapped(member),
                "the {:?} record in {} does not survive a rewrite",
                record.name,
                named(path)
            );
        }
    }

    assert!(checked > 0, "no captured team file holds a teammate record");
}

#[test]
fn a_real_claude_inbox_round_trips_byte_identical() {
    let captured = captured();

    for path in &captured.inboxes {
        let raw = fs::read_to_string(path).expect("a captured inbox reads");
        let held = mailbox::read(path).expect("a captured inbox reads through the reader");

        // A document a real `claude` wrote must read clean. A drop here would
        // mean this crate's §2.4 validation refuses something the peer writes
        // routinely, which is a worse bug than a byte that moved.
        assert_eq!(
            held.dropped,
            0,
            "{} lost {} entr(ies) to validation: {:?}",
            named(path),
            held.dropped,
            held.reports
        );
        assert_eq!(
            record::document(&held.valid).expect("an inbox re-encodes"),
            raw,
            "{} does not survive a rewrite",
            named(path)
        );
    }
}

#[test]
fn a_real_inbox_delivers_and_prunes_the_message_it_holds() {
    let captured = captured();
    let home = tempfile::tempdir().expect("a temp directory");

    // Copied, never mutated in place: these are the only documents in this
    // repository that ganja did not write, and a failing test must not be able
    // to damage them.
    let mut delivered = 0;
    for (index, source) in captured.inboxes.iter().enumerate() {
        let path = home.path().join(format!("{index}.json"));
        fs::copy(source, &path).expect("a captured inbox copies");

        let held = mailbox::read(&path).expect("the copy reads");
        if held.valid.is_empty() {
            continue;
        }
        delivered += held.valid.len();

        // The identity a delivery is reconciled by is derived from the message,
        // so two reads of one unchanged file must agree about it — otherwise a
        // teammate would be handed the same message twice, or never.
        let again = mailbox::read(&path).expect("the copy reads twice");
        let identities: Vec<_> = held.valid.iter().map(mailbox::identity).collect();
        assert_eq!(
            identities,
            again
                .valid
                .iter()
                .map(mailbox::identity)
                .collect::<Vec<_>>(),
            "{} yields a different identity on a second read",
            named(source)
        );

        let pruned = mailbox::prune_delivered(&path, &identities).expect("the prune writes");
        assert_eq!(pruned.pruned, identities.len());
        assert_eq!(pruned.remaining, 0);
        // Delivered means gone, and what is left is a seeded inbox again — the
        // two bytes a peer expects to find, with no trailing newline.
        assert_eq!(
            fs::read_to_string(&path).expect("the pruned copy reads"),
            mailbox::EMPTY_INBOX
        );
    }

    assert!(delivered > 0, "no captured inbox held a message to deliver");
}

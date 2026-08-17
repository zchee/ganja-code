//! AC-1a: a document this repo writes, read back and rewritten, is the same
//! bytes — and those bytes have the shape Claude Code's do.
//!
//! **This binary holds the shape, and it is not an interop test.** Nothing here
//! compares against a document Claude Code wrote; every byte under test was
//! produced by this crate, so a green run says only that the writer, the reader
//! and the writer again agree with each other. That is worth having — it is the
//! regression fence around the indentation, the one-key-per-line break and the
//! absent trailing newline, and it runs everywhere with no fixture and no
//! `claude` binary. It is *not* evidence that a real Claude Code can read
//! what this writes. The plan says so in as many words (phase boundary,
//! condition 2: "AC-1a is not an interop falsifier and is not claimed as
//! one"), and the falsifier that is one lives next door in
//! `claude_format_interop.rs`.
//!
//! Integration rather than unit, and every test goes through the filesystem
//! under a `TeamsRoot` of its own: the unit tests in `record.rs` already pin
//! the encoder's output as strings, so the value added here is the round trip
//! through a real file — written where a teammate's peer would look for it,
//! read back the way the reader really reads, rewritten, and compared.
//!
//! **Nothing here pins the key order, deliberately.** The order is the one
//! claim in the format that cannot be checked against anything but a real
//! document, so it is asserted once, next door, against the captured bytes
//! that are evidence for it. A second copy transcribed by hand into this file
//! would be a claim with nothing behind it, and it would fight the first one
//! the day a capture says the order is wrong — which is exactly what happened
//! when this fixture landed. What is pinned here is everything that is true
//! whatever the order: that a rewrite reproduces what was read, that the shape
//! is Claude's, that an unknown key survives, and that a write stamps the
//! envelope §2.3 says it does.
//!
//! No process-wide state is touched: the root is a temporary directory handed
//! in as a value, which is exactly what `TeamsRoot` exists for.

mod support;

use std::fs;

use ganja_team::{
    MailboxMessage, MemberName, MemberRecord, Surface, TeamFile, TeamName, mailbox, record,
};
use serde_json::json;

/// The team every test here writes under.
const TEAM: &str = "session-62633995";

/// A pinned instant, so a document's bytes are the same on every run.
///
/// `joinedAt`/`createdAt` are milliseconds and the encoder writes them as
/// digits, so a clock reading here would make the literals below untypeable —
/// which is why every constructor in this crate takes the time as an argument.
const WHEN: u64 = 1_786_343_288_174;

/// The timestamp spelling a message carries, at that same instant.
const WHEN_ISO: &str = "2026-08-08T04:28:08.174Z";

/// What every document this repo writes is checked for, whatever it holds.
///
/// Three properties, and each one is a compatibility surface rather than a
/// preference: `JSON.stringify(value, null, 2)` indents two spaces, breaks
/// every key onto its own line, and appends no newline. A rewrite that differed
/// in any of them would still parse — and would make every `git diff` of a
/// directory shared with a real `claude` unreadable, which is the failure this
/// asserts against.
fn assert_claude_shaped(rendered: &str) {
    assert!(
        !rendered.ends_with('\n'),
        "a document carries no trailing newline: {rendered:?}"
    );

    for line in rendered.lines() {
        let indent = line.len() - line.trim_start().len();
        assert_eq!(
            indent % 2,
            0,
            "every level is indented two spaces, and this line is not: {line:?}"
        );

        // One key per line, counted structurally: a separating colon is one
        // that falls outside a string, so a value that happens to contain
        // `": "` — a prompt quoting a JSON fragment, say — cannot be mistaken
        // for a second key. What this really guards against is somebody
        // reaching for `to_string` instead of `to_string_pretty`, which would
        // put the whole document on one line and still parse.
        let separators = separating_colons(line);
        assert!(
            separators <= 1,
            "a line carries {separators} keys rather than one: {line:?}"
        );
        assert!(
            separators == 0 || line.trim_start().starts_with('"'),
            "a key begins its own line: {line:?}"
        );
    }
}

/// How many of `line`'s colons separate a key from its value — that is, how
/// many fall outside a string literal.
fn separating_colons(line: &str) -> usize {
    let mut colons = 0;
    let mut in_string = false;
    let mut escaped = false;

    for character in line.chars() {
        match character {
            // Consumes exactly one character, which is what makes `\\` end an
            // escape rather than begin one.
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ':' if !in_string => colons += 1,
            _ => {}
        }
    }

    colons
}

/// The team every round-trip test writes: a lead and one teammate, so the two
/// record shapes are both in the document under test.
fn team_file(team: &TeamName) -> TeamFile {
    let mut file = TeamFile::new(team, "62633995-26da-4f1d-a578-3d33072823ef", "/w", WHEN);
    let worker = MemberName::parse("kv-review-2").expect("a valid member name");
    file.members.push(MemberRecord::teammate(
        &worker,
        team,
        support::spawn(
            "review the wire\nand say ship or hold",
            Surface::Pane {
                id: "%7".to_owned(),
            },
        ),
        WHEN,
    ));

    file
}

#[test]
fn a_written_team_file_round_trips_byte_identical() {
    let (_home, root, team) = support::root(TEAM);
    let path = root.config_path(&team);
    fs::create_dir_all(path.parent().expect("a config sits in a team directory"))
        .expect("the team directory is creatable");

    let written = record::document(&team_file(&team)).expect("a team file encodes");
    fs::write(&path, &written).expect("the team file is writable");

    // The round trip proper: what came off disk decodes, and re-encoding it
    // gives back the very bytes that were read. Both record shapes ride along,
    // since the document holds a lead and a teammate.
    let raw = fs::read_to_string(&path).expect("the team file is readable");
    let read: TeamFile = serde_json::from_str(&raw).expect("a team file decodes");
    assert_eq!(
        record::document(&read).expect("a team file re-encodes"),
        raw,
        "a rewrite is not the bytes it read"
    );

    assert_claude_shaped(&raw);
    // Both record shapes are in there, which is what makes this one document
    // worth writing rather than two.
    assert!(read.members[0].is_lead());
    assert_eq!(read.members[1].name, "kv-review-2");
    assert_eq!(
        read.members[1].surface(),
        Surface::Pane {
            id: "%7".to_owned()
        }
    );
}

#[test]
fn a_written_inbox_round_trips_byte_identical() {
    let (_home, root, team) = support::root(TEAM);
    let path = support::inbox_of(&root, &team, "kv-review-2");

    // A seeded inbox is two bytes and no newline, which is a document too —
    // it is what a peer finds before anybody has written anything.
    mailbox::seed(&path).expect("the inbox seeds");
    assert_eq!(
        fs::read_to_string(&path).expect("the inbox is readable"),
        "[]"
    );

    let id = mailbox::write(
        &path,
        MailboxMessage::new("team-lead", "start on the parser", WHEN_ISO),
    )
    .expect("a message writes");

    let raw = fs::read_to_string(&path).expect("the inbox is readable");
    let held = mailbox::read(&path).expect("the inbox reads");
    assert_eq!(held.dropped, 0);
    assert_eq!(
        record::document(&held.valid).expect("an inbox re-encodes"),
        raw,
        "a rewrite is not the bytes it read"
    );

    assert_claude_shaped(&raw);
    // What the write decides rather than the sender: §2.3's three stamps and
    // the tombstone §3.1 never sets true.
    let message = &held.valid[0];
    assert_eq!(message.kind.as_deref(), Some("message"));
    assert_eq!(message.msg_v, Some(1));
    assert_eq!(message.msg_id.as_deref(), Some(id.as_str()));
    assert_eq!(message.read, Some(false));
    // And what the sender decided, unchanged by the trip through the file.
    assert_eq!(message.from, "team-lead");
    assert_eq!(message.text, "start on the parser");
    assert_eq!(message.timestamp, WHEN_ISO);
}

#[test]
fn an_unknown_key_survives_a_rewrite_in_position() {
    let (_home, root, team) = support::root(TEAM);
    let path = root.config_path(&team);
    fs::create_dir_all(path.parent().expect("a config sits in a team directory"))
        .expect("the team directory is creatable");

    // A document carrying keys this build has never heard of, at both levels.
    // Seeded through `extra` rather than hand-written, so that the bytes under
    // test are whatever this crate emits today and the assertion survives a
    // change to the declared field order — which is a live question, and one
    // `claude_format_interop.rs` owns rather than this file.
    //
    // `zeta` before `alpha` on purpose: a passthrough over a `BTreeMap` hands
    // them back the other way round, and alphabetized-by-accident is the
    // failure that looks like success.
    let mut seeded = team_file(&team);
    seeded.extra.insert("zetaTeam".to_owned(), json!(true));
    seeded.extra.insert("alphaTeam".to_owned(), json!("kept"));
    seeded.members[0]
        .extra
        .insert("zeta".to_owned(), json!("kept"));
    seeded.members[0]
        .extra
        .insert("alpha".to_owned(), json!([1]));

    let original = record::document(&seeded).expect("a team file encodes");
    fs::write(&path, &original).expect("the team file is writable");

    // The read is the step under test: an unknown key has to come back off
    // disk, in arrival order, after the fields this build does know.
    let raw = fs::read_to_string(&path).expect("the team file is readable");
    let read: TeamFile = serde_json::from_str(&raw).expect("a team file decodes");
    assert_eq!(
        read.extra.keys().collect::<Vec<_>>(),
        ["zetaTeam", "alphaTeam"]
    );
    assert_eq!(
        read.members[0].extra.keys().collect::<Vec<_>>(),
        ["zeta", "alpha"]
    );

    fs::write(
        &path,
        record::document(&read).expect("a team file re-encodes"),
    )
    .expect("the team file is rewritable");
    assert_eq!(
        fs::read_to_string(&path).expect("the team file is readable"),
        original,
        "an unknown key did not come back where it went in"
    );
}

#[test]
fn an_unknown_key_ahead_of_a_known_one_moves_to_the_tail() {
    let (_home, _root, team) = support::root(TEAM);

    // The limitation, pinned rather than left to be discovered: `extra` is a
    // `#[serde(flatten)]` map emitted *after* the declared fields, so an
    // unknown key that arrived between two known ones comes back at the end.
    // This is not hypothetical — a real Claude Code wrote team files carrying
    // `description` between `name` and `createdAt` (`tests/fixtures/PROVENANCE.md`),
    // and no document that version wrote would round-trip. Nothing modern
    // carries one, which is why AC-1b passes with the shapes as they are; the
    // day one does, this test is where the reason is already written down.
    let ahead = format!(
        "{{\n  \"name\": \"{team}\",\n  \"description\": \"a team\",\n  \
         \"createdAt\": 1786343288174,\n  \"leadAgentId\": \"team-lead@{team}\",\n  \
         \"leadSessionId\": \"62633995-26da-4f1d-a578-3d33072823ef\",\n  \"members\": []\n}}"
    );
    let read: TeamFile = serde_json::from_str(&ahead).expect("a team file decodes");

    assert_eq!(read.extra.keys().collect::<Vec<_>>(), ["description"]);
    let rewritten = record::document(&read).expect("a team file re-encodes");
    assert_ne!(rewritten, ahead, "the limitation is that this differs");
    assert!(
        rewritten.ends_with("\"description\": \"a team\"\n}"),
        "the unknown key survives, at the tail rather than in position: {rewritten}"
    );
}

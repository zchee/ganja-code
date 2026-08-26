use std::{collections::HashSet, fs};

use serde_json::json;

use super::{
    Ceiling, Contents, MailboxError, first_report, identity, prune_delivered, read, seed, validate,
    write, write_bounded,
};
use crate::record::{MailboxMessage, SCHEMA_KEYS};

const WHEN: &str = "2026-08-17T00:00:00.000Z";

fn inbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().expect("a temp directory");
    let path = home.path().join("teams/t/inboxes/worker.json");

    (home, path)
}

/// The body of the entry [`damaged_pair`] plants beside a clean one.
const DAMAGED_BODY: &str = "s3cret-body";

/// An inbox holding one entry that reads and one that does not — its
/// `timestamp` is a number — with [`DAMAGED_BODY`] in the damaged one.
fn damaged_pair(path: &std::path::Path) {
    seed(path).expect("the inbox seeds");
    fs::write(
        path,
        serde_json::to_string(&json!([
            {"from": "w", "text": "kept", "timestamp": WHEN},
            {"from": "w", "text": DAMAGED_BODY, "timestamp": 7},
        ]))
        .expect("the fixture encodes"),
    )
    .expect("the inbox is writable");
}

#[test]
fn a_delivered_message_does_not_remain() {
    let (_home, path) = inbox();
    write(&path, MailboxMessage::new("team-lead", "first", WHEN)).expect("a message writes");
    write(&path, MailboxMessage::new("team-lead", "second", WHEN)).expect("a message writes");

    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.valid.len(), 2);
    assert_eq!(held.dropped, 0);

    let delivered = vec![identity(&held.valid[0])];
    let pruned = prune_delivered(&path, &delivered).expect("the prune writes");
    assert_eq!(pruned.pruned, 1);
    assert_eq!(pruned.remaining, 1);

    let left = read(&path).expect("the inbox reads");
    assert_eq!(left.valid.len(), 1);
    assert_eq!(left.valid[0].text, "second");
    // A tombstone that is never written: §3.1's whole correction.
    assert_eq!(left.valid[0].read, Some(false));
}

#[test]
fn a_write_stamps_the_envelope_and_seeds_an_absent_inbox() {
    let (_home, path) = inbox();
    let id = write(&path, MailboxMessage::new("w", "hello", WHEN)).expect("a message writes");

    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.valid[0].msg_id.as_deref(), Some(id.as_str()));
    assert_eq!(held.valid[0].msg_v, Some(1));
    assert_eq!(held.valid[0].kind.as_deref(), Some("message"));

    // Seeding an inbox that already holds messages must not empty it.
    seed(&path).expect("a second seed is a no-op");
    assert_eq!(read(&path).expect("the inbox reads").valid.len(), 1);
}

#[test]
fn a_non_array_top_level_reads_as_one_dropped_entry() {
    let (_home, path) = inbox();
    seed(&path).expect("the inbox seeds");

    fs::write(&path, "{\"from\": \"w\"}").expect("the inbox is writable");
    let held = read(&path).expect("the inbox reads");
    assert_eq!(
        held,
        Contents {
            valid: Vec::new(),
            dropped: 1,
            reports: vec![format!("{}, found an object", super::DROPPED_NOT_AN_ARRAY)],
        }
    );

    // And any mutation's rewrite leaves a file that reads clean: the
    // non-array is gone, and only what was written over it remains.
    write(&path, MailboxMessage::new("w", "over it", WHEN)).expect("a message writes");
    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.dropped, 0);
    assert_eq!(held.valid.len(), 1);

    // A file that is not JSON at all fails differently, and says so.
    fs::write(&path, "not json").expect("the inbox is writable");
    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.dropped, 1);
    assert_eq!(held.reports, vec![super::DROPPED_NOT_JSON.to_owned()]);
}

#[test]
fn a_non_string_text_is_refused_as_its_own_case_and_every_other_field_by_name() {
    // §2.4's one distinctly reported field: the type is named, the value
    // is not.
    let refusal = validate(&json!({"from": "w", "text": 42, "timestamp": WHEN}))
        .expect_err("a number is not a message body");
    assert!(
        matches!(refusal, MailboxError::TextNotAString { found: "a number" }),
        "{refusal:?}"
    );
    assert!(refusal.to_string().contains("holds a number"));

    // Everything else is the other refusal, one sentence per field.
    let refusal = validate(&json!({"text": "hi", "timestamp": 7}))
        .expect_err("a message needs a sender and a timestamp");
    let MailboxError::SchemaInvalid { issues } = refusal else {
        panic!("expected a schema refusal, got {refusal:?}");
    };
    assert_eq!(
        issues,
        [
            "from: required, and absent".to_owned(),
            "timestamp: expected a string, found a number".to_owned(),
        ]
    );
}

#[test]
fn a_message_whose_extra_shadows_a_schema_key_is_refused_before_the_file_is_touched() {
    let (_home, path) = inbox();
    write(&path, MailboxMessage::new("w", "kept", WHEN)).expect("a message writes");
    let before = fs::read_to_string(&path).expect("the inbox is readable");

    // Two shadowing keys, so the refusal is pinned as naming every
    // offender rather than the first one it met.
    let mut shadowing = MailboxMessage::new("w", "impostor", WHEN);
    shadowing
        .extra
        .insert("text".to_owned(), json!("a second body"));
    shadowing.extra.insert("read".to_owned(), json!(true));
    let refusal = write(&path, shadowing).expect_err("a shadowed schema key is refused");
    let MailboxError::SchemaInvalid { issues } = refusal else {
        panic!("expected a schema refusal, got {refusal:?}");
    };
    assert_eq!(
        issues,
        [
            "text: the shape declares this key, so a passthrough map may not also carry it"
                .to_owned(),
            "read: the shape declares this key, so a passthrough map may not also carry it"
                .to_owned(),
        ]
    );

    // Refused before the file was touched, which is the half that matters:
    // a rejected write must not cost the messages already queued, and it
    // must not have taken a hold to find out.
    assert_eq!(
        fs::read_to_string(&path).expect("the inbox is readable"),
        before
    );
    assert!(
        !std::path::PathBuf::from(format!("{}.lock", path.display())).exists(),
        "a refusal that never reached the disk took no lock"
    );
}

#[test]
fn an_entry_that_passes_its_field_checks_and_still_will_not_decode_is_dropped_by_name() {
    let (_home, path) = inbox();
    seed(&path).expect("the inbox seeds");
    // `msgV` is checked as a whole number (§2.4's `is_u64`), and 2^32 is
    // one — it is also one more than the `u32` the envelope declares, so
    // the field check passes and the typed decode does not.
    fs::write(
        &path,
        r#"[{"from": "w", "text": "s3cret-body", "timestamp": "t", "msgV": 4294967296}]"#,
    )
    .expect("the inbox is writable");

    let held = read(&path).expect("the inbox reads");
    assert!(held.valid.is_empty());
    assert_eq!(held.dropped, 1);
    // The constant and nothing else: the decoder's own sentence can quote
    // the value it choked on, and a value is a message body.
    assert_eq!(held.reports, [super::DROPPED_UNDECODABLE.to_owned()]);
}

#[test]
fn a_dropped_entry_names_the_field_and_never_the_value() {
    let (_home, path) = inbox();
    damaged_pair(&path);

    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.valid.len(), 1);
    assert_eq!(held.dropped, 1);
    assert_eq!(held.reports.len(), 1);
    assert!(held.reports[0].contains("timestamp: expected a string, found a number"));
    assert!(
        !held.reports[0].contains(DAMAGED_BODY),
        "a drop report names fields and types, never a body: {}",
        held.reports[0]
    );
}

#[test]
fn drop_reports_dedupe_and_stop_at_one_hundred() {
    // Driven against a set of its own rather than the process-wide one, so
    // the cap is what is under test and not what every other test in this
    // binary has already reported.
    let mut reported = HashSet::new();
    for key in 0..super::MAX_REPORTED {
        let key = u64::try_from(key).expect("an index under a hundred fits");
        assert!(first_report(&mut reported, key), "{key} is new");
        assert!(!first_report(&mut reported, key), "{key} is not new twice");
    }
    assert_eq!(reported.len(), super::MAX_REPORTED);
    assert!(
        !first_report(&mut reported, 1_000),
        "past the cap, a new key is not reported either"
    );

    // And the process-wide memory really is consulted: the same damage in
    // the same file reports once and then goes quiet.
    let (_home, path) = inbox();
    seed(&path).expect("the inbox seeds");
    fs::write(&path, "{\"unique\": \"to this temp path\"}").expect("the inbox is writable");
    assert_eq!(read(&path).expect("the inbox reads").reports.len(), 1);
    assert!(read(&path).expect("the inbox reads").reports.is_empty());
}

#[cfg(unix)]
#[test]
fn a_rewrite_keeps_the_inboxes_existing_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_home, path) = inbox();
    seed(&path).expect("the inbox seeds");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("the mode is settable");

    write(&path, MailboxMessage::new("w", "hello", WHEN)).expect("a message writes");

    let mode = fs::metadata(&path)
        .expect("the inbox is there")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o640,
        "a rewrite neither tightens nor loosens what the peer wrote"
    );
}

#[test]
fn the_schema_key_list_is_exactly_what_a_message_serializes() {
    // `SCHEMA_KEYS` is the shadow guard's list and `validate`'s field lists
    // are the drop diagnostics'. Both are written out by hand beside a
    // struct that is written out by hand, so nothing but this test stops a
    // tenth field from being declared and silently governed by neither.
    let whole = MailboxMessage {
        from: "w".to_owned(),
        text: "hi".to_owned(),
        summary: Some("s".to_owned()),
        timestamp: WHEN.to_owned(),
        color: Some("blue".to_owned()),
        msg_v: Some(1),
        msg_id: Some("x".to_owned()),
        kind: Some("message".to_owned()),
        read: Some(false),
        extra: indexmap::IndexMap::new(),
    };
    let serde_json::Value::Object(fields) =
        serde_json::to_value(&whole).expect("a message encodes")
    else {
        panic!("a message is an object");
    };

    assert_eq!(
        fields.keys().map(String::as_str).collect::<HashSet<_>>(),
        SCHEMA_KEYS.iter().copied().collect::<HashSet<_>>(),
        "every declared field is a schema key and every schema key is a field",
    );

    // And every one of them is a key `validate` actually checks: give each
    // in turn a type no field of the schema accepts, and the refusal has to
    // name that key.
    for key in SCHEMA_KEYS {
        let mut broken = fields.clone();
        broken[key] = json!([]);
        let refusal = super::validate(&serde_json::Value::Object(broken))
            .expect_err("an array is not any of the schema's types");
        assert!(
            refusal.to_string().contains(key),
            "validate ignores {key}: {refusal}",
        );
    }
}

#[test]
fn a_write_also_deletes_a_damaged_neighbour() {
    let (_home, path) = inbox();
    damaged_pair(&path);

    // A read changes nothing.
    read(&path).expect("the inbox reads");
    assert!(
        fs::read_to_string(&path)
            .expect("the inbox is readable")
            .contains(DAMAGED_BODY),
        "a read is not a rewrite",
    );

    // One ordinary write, and the neighbour is *gone from the file*. This is
    // the destructive half `write`'s own doc names: nobody asked for a
    // prune, and one happened.
    write(&path, MailboxMessage::new("w", "new", WHEN)).expect("a message writes");

    let after = fs::read_to_string(&path).expect("the inbox is readable");
    assert!(
        !after.contains(DAMAGED_BODY),
        "a write rewrites only what read cleanly, so the damaged entry is deleted: {after}",
    );
    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.dropped, 0, "and nothing unreadable is left to drop");
    assert_eq!(
        held.valid
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["kept", "new"],
        "the readable neighbour and the new message both survive, in order",
    );
}

#[test]
fn a_report_key_is_clamped_on_a_character_boundary() {
    // Three-byte characters, so the cap falls inside one: 2048 is not a
    // multiple of three, and a byte-indexed cut there would panic.
    let wide = "€".repeat(1_000);
    let cut = super::clamp(&wide, super::REPORT_KEY_CAP);
    assert_eq!(cut.len(), 2_046);
    assert!(cut.chars().all(|character| character == '€'));
    assert_eq!(super::clamp("short", super::REPORT_KEY_CAP), "short");

    // And through the door that clamps: an entry longer than the cap with
    // such a character straddling it is dropped and reported, not a panic.
    // The raw entry is what `report` clamps, so it is the raw entry whose
    // cap must fall mid-character.
    let entry = format!(r#"{{"from":"ww","text":"{wide}","timestamp":7}}"#);
    assert!(
        !entry.is_char_boundary(super::REPORT_KEY_CAP),
        "the fixture straddles the cap"
    );
    let (_home, path) = inbox();
    seed(&path).expect("the inbox seeds");
    fs::write(&path, format!("[{entry}]")).expect("the inbox is writable");
    let held = read(&path).expect("the inbox reads");
    assert_eq!(held.dropped, 1);
    assert_eq!(held.reports.len(), 1);
}

/// A ceiling no test here ever reaches on the axis it is not testing.
const ROOMY: usize = usize::MAX;

#[test]
fn an_append_past_the_message_bound_is_refused_naming_the_counts() {
    let (_home, path) = inbox();
    let ceiling = Some(Ceiling {
        max_messages: 2,
        max_bytes: ROOMY,
    });
    write_bounded(&path, MailboxMessage::new("w", "first", WHEN), ceiling)
        .expect("an inbox under its ceiling takes a message");
    write_bounded(&path, MailboxMessage::new("w", "second", WHEN), ceiling)
        .expect("an inbox at its last slot takes a message");
    let before = fs::read_to_string(&path).expect("the inbox is readable");

    let refusal = write_bounded(&path, MailboxMessage::new("w", "third", WHEN), ceiling)
        .expect_err("an inbox at its message bound refuses the append");
    let sentence = refusal.to_string();
    assert!(
        sentence.contains("3 messages") && sentence.contains("2 messages"),
        "the refusal names the observed count and the bound: {sentence}"
    );
    let MailboxError::Full {
        held,
        max_messages,
        max_bytes,
        ..
    } = refusal
    else {
        panic!("expected a full refusal, got {refusal:?}");
    };
    assert_eq!(held, 3, "the counts are what the append would have left");
    assert_eq!(max_messages, 2);
    assert_eq!(max_bytes, ROOMY);

    // The refusal wrote nothing: the file is byte-identical, and a read
    // still finds exactly the two admitted messages.
    assert_eq!(
        fs::read_to_string(&path).expect("the inbox is readable"),
        before
    );
    assert_eq!(read(&path).expect("the inbox reads").valid.len(), 2);
}

#[test]
fn an_append_past_the_byte_bound_is_refused_naming_the_counts() {
    let (_home, path) = inbox();
    let ceiling = Some(Ceiling {
        max_messages: ROOMY,
        max_bytes: 256,
    });
    write_bounded(&path, MailboxMessage::new("w", "short", WHEN), ceiling)
        .expect("a small message fits under the byte bound");
    let before = fs::read_to_string(&path).expect("the inbox is readable");

    let oversized = "x".repeat(300);
    let refusal = write_bounded(&path, MailboxMessage::new("w", oversized, WHEN), ceiling)
        .expect_err("an append past the byte bound is refused");
    let MailboxError::Full { held, bytes, .. } = refusal else {
        panic!("expected a full refusal, got {refusal:?}");
    };
    assert_eq!(held, 2);
    assert!(
        bytes > 256,
        "the observed byte count is the document the append would have written: {bytes}"
    );

    assert_eq!(
        fs::read_to_string(&path).expect("the inbox is readable"),
        before
    );
}

#[test]
fn a_ceiling_refusal_leaves_even_a_damaged_neighbour_untouched() {
    // The half of "byte-identical" that is easy to lose: an ordinary
    // write destructively prunes what would not read (the test above
    // this module's write doc), so a refusal that still wrote the file
    // back would delete the neighbour as a side effect of refusing.
    let (_home, path) = inbox();
    damaged_pair(&path);
    let before = fs::read_to_string(&path).expect("the inbox is readable");

    let refusal = write_bounded(
        &path,
        MailboxMessage::new("w", "one too many", WHEN),
        Some(Ceiling {
            max_messages: 1,
            max_bytes: ROOMY,
        }),
    )
    .expect_err("the valid entry already fills the one slot");
    assert!(matches!(refusal, MailboxError::Full { held: 2, .. }));

    let after = fs::read_to_string(&path).expect("the inbox is readable");
    assert_eq!(after, before, "a refusal is not a rewrite");
    assert!(
        after.contains(DAMAGED_BODY),
        "the unreadable neighbour survives a refused append"
    );
}

#[test]
fn no_ceiling_keeps_the_unbounded_append() {
    let (_home, path) = inbox();
    for n in 0..4 {
        write_bounded(&path, MailboxMessage::new("w", format!("{n}"), WHEN), None)
            .expect("an unbounded append always lands");
    }
    // And `write` is that spelling under the old name.
    write(&path, MailboxMessage::new("w", "fifth", WHEN)).expect("a message writes");

    assert_eq!(read(&path).expect("the inbox reads").valid.len(), 5);
}

#[test]
fn an_identity_debug_renders_no_body() {
    let rendered = format!(
        "{:?}",
        identity(&MailboxMessage::new("w", "s3cret-body", WHEN))
    );

    assert_eq!(rendered, format!("Identity(w|{WHEN}|<11 bytes>)"));
}

use std::collections::BTreeSet;

use ganja_protocol::team::{Frame, IdleNotification, IdleReason};

use super::{
    BACKEND_AGY, BACKEND_CODEX, BACKEND_GROK, MailboxMessage, MemberRecord, PANE_IN_PROCESS,
    ShimCli, Spawn, Surface, TeamFile, Value, document, iso8601, now_millis,
};
use crate::team::{MemberName, TeamName};

fn team() -> TeamName {
    TeamName::parse("session-224cbeab").expect("a valid team name")
}

/// Every one of the survey's 24 modern lead records, key for key.
const LEAD_ORDER: &str = "{\n  \"agentId\": \"team-lead@session-224cbeab\",\n  \
         \"name\": \"team-lead\",\n  \"agentType\": \"team-lead\",\n  \
         \"joinedAt\": 1786734033621,\n  \"tmuxPaneId\": \"leader\",\n  \"cwd\": \"/w\",\n  \
         \"subscriptions\": [],\n  \"backendType\": \"in-process\"\n}";

/// Every one of the survey's 26 modern teammate records, key for key. Note
/// `color` fourth and `agentType` seventh — the two places the lead order
/// cannot be bent to agree.
const TEAMMATE_ORDER: &str = "{\n  \"agentId\": \"demo-worker-1@session-224cbeab\",\n  \
         \"name\": \"demo-worker-1\",\n  \"color\": \"blue\",\n  \
         \"joinedAt\": 1786734154864,\n  \"tmuxPaneId\": \"%142\",\n  \
         \"subscriptions\": [],\n  \"agentType\": \"general-purpose\",\n  \
         \"model\": \"claude-opus-5[1m]\",\n  \"prompt\": \"do the thing\",\n  \
         \"planModeRequired\": false,\n  \"cwd\": \"/w\",\n  \"backendType\": \"tmux\",\n  \
         \"isActive\": true\n}";

fn worker() -> MemberRecord {
    let name = MemberName::parse("demo-worker-1").expect("a valid member name");

    MemberRecord::teammate(
        &name,
        &team(),
        Spawn {
            agent_type: "general-purpose".to_owned(),
            model: "claude-opus-5[1m]".to_owned(),
            color: "blue".to_owned(),
            prompt: "do the thing".to_owned(),
            plan_mode_required: false,
            surface: Surface::Pane {
                id: "%142".to_owned(),
            },
            cwd: "/w".to_owned(),
        },
        1_786_734_154_864,
    )
}

#[test]
fn a_lead_record_is_written_in_the_lead_order_and_never_says_model_null() {
    let record = MemberRecord::lead(&team(), "/w", 1_786_734_033_621);
    let rendered = document(&record).expect("a record encodes");

    assert!(
        !rendered.contains("null"),
        "the five teammate-only fields are absent from a lead record, not null: {rendered}"
    );
    // The emitted order is the format, so this is asserted as bytes rather
    // than as a set of fields.
    assert_eq!(rendered, LEAD_ORDER);
    assert!(record.is_lead());
    assert_eq!(record.surface(), Surface::Leader);
}

#[test]
fn a_teammate_record_is_written_in_the_other_order_entirely() {
    let record = worker();

    assert_eq!(document(&record).expect("a record encodes"), TEAMMATE_ORDER);
    assert_eq!(
        record.surface(),
        Surface::Pane {
            id: "%142".to_owned()
        }
    );
}

/// A shim member writes both fields, and reads back as **in-process** —
/// the asymmetry is the design, not a defect (**D508**).
///
/// `tmuxPaneId` carries the in-process sentinel deliberately, because
/// [`Surface::read`] classifies anything else as a pane id: a `"codex"`
/// there would be read by every build that exists today, and by a real
/// `claude` sharing the directory, as a pane that can never exist. So the
/// surface a reader recovers is a *safe* answer rather than a complete
/// one, and `backendType` is where shim-ness actually lives.
///
/// Both halves are asserted here so that a later change which "fixes" the
/// round trip has to delete a test that says why it is not broken.
#[test]
fn a_shim_record_says_which_cli_in_its_backend_type_and_reads_back_as_in_process() {
    for (cli, backend_type) in [
        (ShimCli::Codex, BACKEND_CODEX),
        (ShimCli::Agy, BACKEND_AGY),
        (ShimCli::Grok, BACKEND_GROK),
    ] {
        let surface = Surface::Shim { cli, pane: None };

        // What lands on disk: the borrowed sentinel, and the CLI's own word.
        assert_eq!(surface.tmux_pane_id(), PANE_IN_PROCESS);
        assert_eq!(surface.backend_type(), backend_type);

        let name = MemberName::parse("demo-worker-1").expect("a valid member name");
        let record = MemberRecord::teammate(
            &name,
            &team(),
            Spawn {
                agent_type: "general-purpose".to_owned(),
                model: "claude-opus-5[1m]".to_owned(),
                color: "blue".to_owned(),
                prompt: "do the thing".to_owned(),
                plan_mode_required: false,
                surface: surface.clone(),
                cwd: "/w".to_owned(),
            },
            1_786_734_154_864,
        );

        assert_eq!(record.tmux_pane_id, PANE_IN_PROCESS);
        assert_eq!(record.backend_type.as_deref(), Some(backend_type));

        // And what a reader recovers: not the surface that was written.
        assert_eq!(
            record.surface(),
            Surface::InProcess,
            "the read is lossy on purpose; `backendType` is what a sweep asks"
        );
        assert_ne!(record.surface(), surface);

        // Emphatically not a pane: the sentinel is what keeps a reader
        // that has never heard of shims from acting on one as though it
        // owned a window.
        assert!(!matches!(record.surface(), Surface::Pane { .. }));
    }
}

/// A shim member **in a pane** (P28, **D512**) writes the real `%N` beside
/// the CLI's own `backendType`, and reads back as a pane — the other lossy
/// direction, and safe for the reason [`Surface::read`] gives: the pane
/// is there. `backendType` is still the field that says which CLI.
#[test]
fn a_shim_record_with_a_pane_writes_the_real_pane_id_and_reads_back_as_a_pane() {
    for (cli, backend_type) in [
        (ShimCli::Codex, BACKEND_CODEX),
        (ShimCli::Agy, BACKEND_AGY),
        (ShimCli::Grok, BACKEND_GROK),
    ] {
        let surface = Surface::Shim {
            cli,
            pane: Some("%7".to_owned()),
        };
        assert_eq!(surface.tmux_pane_id(), "%7");
        assert_eq!(surface.backend_type(), backend_type);

        let name = MemberName::parse("demo-worker-1").expect("a valid member name");
        let record = MemberRecord::teammate(
            &name,
            &team(),
            Spawn {
                agent_type: "general-purpose".to_owned(),
                model: "claude-opus-5[1m]".to_owned(),
                color: "blue".to_owned(),
                prompt: "do the thing".to_owned(),
                plan_mode_required: false,
                surface,
                cwd: "/w".to_owned(),
            },
            1_786_734_154_864,
        );
        assert_eq!(record.tmux_pane_id, "%7");
        assert_eq!(record.backend_type.as_deref(), Some(backend_type));

        // An older reader — and this one — recovers a pane, which is a
        // surface that exists and that every pane-acting reader handles.
        assert_eq!(
            record.surface(),
            Surface::Pane {
                id: "%7".to_owned()
            },
            "the read is lossy towards the pane; `backendType` says which CLI"
        );
        assert_eq!(ShimCli::read(backend_type), Some(cli));
    }
}

#[test]
fn each_record_shape_round_trips_the_bytes_it_was_read_from() {
    // Decoding is order-insensitive, so this asserts the pair the format
    // contract actually rests on: whatever a real `claude` wrote comes back
    // out unchanged. A single declaration cannot do it — `agentType` is
    // third here and seventh there.
    for original in [LEAD_ORDER, TEAMMATE_ORDER] {
        let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

        assert_eq!(document(&record).expect("a record encodes"), original);
    }
}

#[test]
fn a_legacy_lead_keeps_its_model_rather_than_losing_it_to_the_lead_order() {
    // A 2026-03-era lead: named `team-lead`, carrying `model`, and with no
    // `backendType` at all. Keying the order on the name would write the
    // eight-key lead order and drop the model on the floor; keying it on
    // the shape moves the key instead — the same trade the flatten
    // passthrough makes for an unknown one.
    let original = "{\n  \"agentId\": \"team-lead@web-pages\",\n  \
             \"name\": \"team-lead\",\n  \"agentType\": \"team-lead\",\n  \
             \"model\": \"claude-opus-4-1\",\n  \"joinedAt\": 1782579031759,\n  \
             \"tmuxPaneId\": \"leader\",\n  \"cwd\": \"/w\",\n  \"subscriptions\": []\n}";
    let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

    assert!(record.is_lead(), "the role is still the lead's");
    assert_eq!(
        document(&record).expect("a record encodes"),
        "{\n  \"agentId\": \"team-lead@web-pages\",\n  \"name\": \"team-lead\",\n  \
             \"joinedAt\": 1782579031759,\n  \"tmuxPaneId\": \"leader\",\n  \
             \"subscriptions\": [],\n  \"agentType\": \"team-lead\",\n  \
             \"model\": \"claude-opus-4-1\",\n  \"cwd\": \"/w\"\n}"
    );
    // The absent `backendType` stays absent rather than becoming a guess,
    // which is the whole reason that field is an `Option`.
    assert_eq!(record.backend_type, None);
}

#[test]
fn an_unknown_key_survives_a_rewrite_in_position() {
    // `zeta` before `alpha` on purpose: a `BTreeMap` passthrough would
    // hand them back the other way round, and that is the failure this
    // test exists to catch.
    let original = "{\n  \"agentId\": \"w@t\",\n  \"name\": \"w\",\n  \"color\": \"blue\",\n  \
             \"joinedAt\": 1,\n  \"tmuxPaneId\": \"in-process\",\n  \"subscriptions\": [],\n  \
             \"agentType\": \"general-purpose\",\n  \"model\": \"m\",\n  \"prompt\": \"p\",\n  \
             \"planModeRequired\": false,\n  \"cwd\": \"/w\",\n  \
             \"backendType\": \"in-process\",\n  \"isActive\": true,\n  \"zeta\": \"kept\",\n  \
             \"alpha\": {\n    \"nested\": true\n  }\n}";
    let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

    assert_eq!(record.extra.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
    assert_eq!(document(&record).expect("a record encodes"), original);

    // And on the other key order, where the unknown keys follow a shorter
    // known set — the branch must reach the passthrough too.
    let original = "{\n  \"agentId\": \"team-lead@t\",\n  \"name\": \"team-lead\",\n  \
             \"agentType\": \"team-lead\",\n  \"joinedAt\": 1,\n  \"tmuxPaneId\": \"leader\",\n  \
             \"cwd\": \"/w\",\n  \"subscriptions\": [],\n  \"backendType\": \"in-process\",\n  \
             \"zeta\": \"kept\",\n  \"alpha\": {\n    \"nested\": true\n  }\n}";
    let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

    assert_eq!(record.extra.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
    assert_eq!(document(&record).expect("a record encodes"), original);

    // The same for a team file, whose unknown keys sit beside `members`.
    let original = "{\n  \"name\": \"t\",\n  \"createdAt\": 1,\n  \
             \"leadAgentId\": \"team-lead@t\",\n  \
             \"leadSessionId\": \"224cbeab-4e62-497c-aa8f-d05cc33ce7ba\",\n  \
             \"members\": [],\n  \"zeta\": 1,\n  \"alpha\": 2\n}";
    let file: TeamFile = serde_json::from_str(original).expect("a team file decodes");
    assert_eq!(document(&file).expect("a team file encodes"), original);

    // And for a message, where the unknown key follows the whole envelope.
    let original = "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \"summary\": \"s\",\n  \
             \"timestamp\": \"2026-08-17T00:00:00.000Z\",\n  \"msgV\": 1,\n  \
             \"msg_id\": \"x\",\n  \"type\": \"message\",\n  \"read\": false,\n  \
             \"zeta\": \"kept\",\n  \"alpha\": \"kept\"\n}";
    let message: MailboxMessage = serde_json::from_str(original).expect("a message decodes");
    assert_eq!(document(&message).expect("a message encodes"), original);
}

#[test]
fn every_message_shape_the_survey_holds_round_trips_unchanged() {
    // The modern envelope first — one real document, `session-44cd25e1`'s
    // `inboxes/worker-mask.json`, with the body cut to a line. `type` next
    // to `read` at the tail is the finding; §2.3's listing order is not it.
    //
    // The rest are the 2026-03 era, which is where `color` and every
    // stamp-free shape come from. One declaration order serves all five,
    // which is the reason this type needs no hand-written `Serialize` the
    // way `MemberRecord` does.
    for original in [
        "{\n  \"from\": \"team-lead\",\n  \"text\": \"GO\",\n  \"summary\": \"unblock\",\n  \
             \"timestamp\": \"2026-08-17T00:00:00.000Z\",\n  \"msgV\": 1,\n  \
             \"msg_id\": \"0198c0de-dead-7000-8000-000000000000\",\n  \"type\": \"message\",\n  \
             \"read\": false\n}",
        "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \"summary\": \"s\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"color\": \"blue\",\n  \
             \"read\": false\n}",
        "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"color\": \"blue\",\n  \
             \"read\": false\n}",
        "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"type\": \"message\",\n  \
             \"read\": false\n}",
        "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"read\": false\n}",
    ] {
        let message: MailboxMessage = serde_json::from_str(original).expect("a message decodes");

        assert_eq!(document(&message).expect("a message encodes"), original);
    }
}

#[test]
fn neither_key_order_ever_emits_a_key_outside_the_declared_list() {
    // The guard below checks a passthrough key against `MEMBER_KEYS`, so
    // that list being the real emitted set is what makes the guard mean
    // anything. Both branches are walked, because the lead order is a
    // subset and only the teammate order is the whole of it.
    for record in [MemberRecord::lead(&team(), "/w", 1), worker()] {
        let Value::Object(fields) = serde_json::to_value(&record).expect("a record encodes") else {
            panic!("a record is an object");
        };

        for key in fields.keys() {
            assert!(
                super::MEMBER_KEYS.contains(&key.as_str()),
                "{key} is emitted and is not in MEMBER_KEYS",
            );
        }
    }

    let Value::Object(fields) = serde_json::to_value(worker()).expect("a record encodes") else {
        panic!("a record is an object");
    };
    assert_eq!(
        fields.len(),
        super::MEMBER_KEYS.len(),
        "the teammate order is the whole list, which is what makes it the union",
    );

    // A team file's list, the same way. Only the key *set* is asserted here:
    // `to_value` hands back a `serde_json::Map`, which is the `BTreeMap`
    // this module's own doc warns about, so it alphabetizes and can say
    // nothing about order. Order is what the byte pins above are for.
    let Value::Object(fields) =
        serde_json::to_value(TeamFile::new(&team(), "s", "/w", 1)).expect("a team file encodes")
    else {
        panic!("a team file is an object");
    };
    assert_eq!(
        fields.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        super::TEAM_FILE_KEYS.into_iter().collect::<BTreeSet<_>>(),
        "and a team file emits exactly its declared list",
    );
}

#[test]
fn a_passthrough_key_that_shadows_a_declared_one_is_refused() {
    // The same refusal `mailbox::write` takes before it touches an inbox,
    // and for the same reason: emitting a declared key out of the
    // passthrough writes a document whose reader gets an answer the writer
    // never meant. Unreachable from a decode — a declared key is captured by
    // its field first — so a hand-built value is what this pins.
    let mut record = MemberRecord::lead(&team(), "/w", 1);
    record
        .extra
        .insert("name".to_owned(), Value::String("impostor".to_owned()));
    let refusal = document(&record).expect_err("a shadowed key is refused");
    assert!(
        refusal
            .to_string()
            .contains("name: the shape declares this"),
        "{refusal}",
    );

    // Including a key this record's own order would not have emitted:
    // `isActive` is declared, so it may not arrive from the map either.
    let mut record = MemberRecord::lead(&team(), "/w", 1);
    record
        .extra
        .insert("isActive".to_owned(), Value::Bool(true));
    assert!(
        document(&record).is_err(),
        "a lead record declares isActive"
    );

    // And a team file, whose declared list is a different one.
    let mut file = TeamFile::new(&team(), "s", "/w", 1);
    file.extra
        .insert("members".to_owned(), Value::Array(Vec::new()));
    let refusal = document(&file).expect_err("a shadowed key is refused");
    assert!(
        refusal
            .to_string()
            .contains("members: the shape declares this"),
        "{refusal}",
    );

    // A key that shadows nothing is exactly what the passthrough is for, and
    // still rides along.
    let mut record = MemberRecord::lead(&team(), "/w", 1);
    record.extra.insert("zeta".to_owned(), Value::Bool(true));
    assert!(
        document(&record)
            .expect("an unknown key is the point of the passthrough")
            .contains("\"zeta\": true"),
    );
}

#[test]
fn a_document_carries_no_trailing_newline() {
    let rendered = document(&TeamFile::new(&team(), "s", "/w", 1)).expect("a team file encodes");

    assert!(!rendered.ends_with('\n'), "{rendered:?}");
    assert!(rendered.contains("\n  \"name\""), "two-space indent");
}

#[test]
fn a_frame_body_reads_back_as_a_frame_and_prose_does_not() {
    let frame = Frame::IdleNotification(IdleNotification {
        from: "w".to_owned(),
        timestamp: "2026-08-17T00:00:00.000Z".to_owned(),
        idle_reason: Some(IdleReason::Available),
        summary: None,
        completed_task_id: None,
        completed_status: None,
        failure_reason: None,
    });
    let carried = MailboxMessage::from_frame("w", &frame, "2026-08-17T00:00:00.000Z")
        .expect("a frame encodes");

    assert_eq!(carried.frame(), Some(frame));
    assert_eq!(
        MailboxMessage::new("w", "just words", "2026-08-17T00:00:00.000Z").frame(),
        None
    );
}

#[test]
fn the_clock_is_spelled_the_way_javascript_spells_it() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    // 2026-08-17T12:34:56.789Z, checked against the calendar rather than
    // against this function.
    assert_eq!(iso8601(1_786_970_096_789), "2026-08-17T12:34:56.789Z");
    // A leap day, which is what the shifted-epoch arithmetic above is for.
    assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    assert!(now_millis() > 1_700_000_000_000, "the clock reads forward");
}

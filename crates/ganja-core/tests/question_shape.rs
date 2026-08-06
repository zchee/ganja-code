//! The question shape is declared twice, and this is what holds the two copies
//! together.
//!
//! `ganja-tool` may not depend on `ganja-protocol` — a tool answers to the
//! rules and the filesystem, and a wire type is neither — so the shape the
//! model fills in (`tool::question::Prompt`) and the shape the wire carries
//! (`protocol::QuestionInfo`) are two declarations of one thing. `ganja-core`
//! is the only crate that sees both, so the pin lives here.
//!
//! **An inexhaustive pin passes while the wire drifts**, which is why the
//! mechanics are what they are:
//!
//! 1. The conversions in `ganja_core::session` destructure **exhaustively in
//!    both directions and never with `..`**, so a field added to either copy
//!    fails to *compile* until somebody decides what it means on the other
//!    side. That half of the pin is enforced by the compiler and cannot be
//!    asserted here.
//! 2. This file supplies the other half: **serde-representation equality** over
//!    a corpus that includes absent optional fields, so a rename, a
//!    `rename_all`, or a `skip_serializing_if` that moves on one side reddens
//!    even though both sides still compile.
//!
//! The one asymmetry is deliberate and upstream's own: `Info` carries `custom`
//! and `Prompt` does not, because it is the asking service's field rather than
//! the model's. It is pinned **by name** below, so making `Prompt` carry it
//! would redden here rather than pass quietly.

use ganja_core::{
    protocol::{QuestionInfo, QuestionOption},
    session::{question_choice, question_info, question_option, question_prompt},
    tool::question::{Choice, Prompt},
};

/// Every question shape worth pinning, including the ones whose optional
/// fields are absent — the case a corpus of fully-populated values would miss
/// entirely, and the case every real call actually sends.
fn corpus() -> Vec<Prompt> {
    vec![
        // Nothing optional set, no choices: the minimum a call can carry.
        Prompt {
            question: "Which database?".to_owned(),
            header: "Database".to_owned(),
            options: Vec::new(),
            multiple: None,
        },
        // The ordinary case.
        Prompt {
            question: "Which database should the service use?".to_owned(),
            header: "Database".to_owned(),
            options: vec![
                Choice {
                    label: "Postgres (Recommended)".to_owned(),
                    description: "Relational, what the rest of the fleet runs".to_owned(),
                },
                Choice {
                    label: "SQLite".to_owned(),
                    description: "One file, no server".to_owned(),
                },
            ],
            multiple: None,
        },
        // Both settings of the optional flag, because `Some(false)` and `None`
        // are different values that a careless default attribute would flatten
        // into each other.
        Prompt {
            question: "Which features?".to_owned(),
            header: "Features".to_owned(),
            options: vec![Choice {
                label: "Tracing".to_owned(),
                description: "Spans on every request".to_owned(),
            }],
            multiple: Some(true),
        },
        Prompt {
            question: "Which one?".to_owned(),
            header: "Pick".to_owned(),
            options: vec![Choice {
                label: "This".to_owned(),
                description: "The first".to_owned(),
            }],
            multiple: Some(false),
        },
        // Text the model really sends: quotes, newlines and non-ASCII all
        // travel through both encodings unchanged or not at all.
        Prompt {
            question: "Which \"mode\"?\nPick one.".to_owned(),
            header: "モード".to_owned(),
            options: vec![Choice {
                label: "既定".to_owned(),
                description: "The default — a dash, and an emoji: 🌿".to_owned(),
            }],
            multiple: None,
        },
    ]
}

/// The shared fields serialize to **exactly the same JSON** on both sides.
///
/// This is the assertion that catches a rename or a default-attribute drift:
/// the two structs still compile, the conversions still typecheck, and the
/// bytes stop matching.
#[test]
fn the_two_declarations_serialize_the_shared_fields_identically() {
    for prompt in corpus() {
        let info = question_info(&prompt);

        assert_eq!(
            serde_json::to_value(&info).expect("a question is JSON"),
            serde_json::to_value(&prompt).expect("a question is JSON"),
            "the wire copy and the model's copy disagree about {prompt:?}"
        );
    }
}

/// And each side decodes the other's bytes, which is the same claim made from
/// the reading end: a frontend handed what the model sent gets a `QuestionInfo`
/// out of it, and nothing was quietly dropped on the way.
#[test]
fn each_declaration_decodes_the_other_declarations_bytes() {
    for prompt in corpus() {
        let written = serde_json::to_value(&prompt).expect("a question is JSON");
        let decoded: QuestionInfo =
            serde_json::from_value(written).expect("the wire copy reads what the model sent");
        assert_eq!(decoded, question_info(&prompt));

        let written = serde_json::to_value(&decoded).expect("a question is JSON");
        let decoded: Prompt =
            serde_json::from_value(written).expect("the model's copy reads what the wire carries");
        assert_eq!(decoded, prompt);
    }
}

/// A question survives the trip out and back as the same value **and** as the
/// same bytes. Value equality alone would miss a field that round-trips
/// through a different representation.
#[test]
fn a_question_round_trips_through_the_wire_shape_unchanged() {
    for prompt in corpus() {
        let back = question_prompt(&question_info(&prompt));

        assert_eq!(back, prompt);
        assert_eq!(
            serde_json::to_value(&back).expect("a question is JSON"),
            serde_json::to_value(&prompt).expect("a question is JSON"),
        );
    }
}

/// The one field that does not survive the trip, pinned by name.
///
/// `custom` is the asking service's, not the model's — upstream's `Prompt` has
/// no such field — so a `QuestionInfo` that carries one comes back without it.
/// If `Prompt` ever grows `custom`, the exhaustive destructuring makes the
/// conversions fail to compile, and this test's expectation is the second
/// thing that has to be reconsidered.
#[test]
fn custom_is_the_one_field_the_models_copy_does_not_carry() {
    for carried in [Some(true), Some(false), None] {
        let info = QuestionInfo {
            question: "Which database?".to_owned(),
            header: "Database".to_owned(),
            options: vec![QuestionOption {
                label: "Postgres".to_owned(),
                description: "Relational".to_owned(),
            }],
            multiple: Some(true),
            custom: carried,
        };

        let back = question_info(&question_prompt(&info));

        assert_eq!(
            back,
            QuestionInfo {
                custom: None,
                ..info.clone()
            },
            "only `custom` may be lost, and it must be lost the same way every time"
        );
        // Everything else is byte-identical, which is the check that would
        // redden if a *second* field started being dropped here.
        assert_eq!(
            serde_json::to_value(QuestionInfo {
                custom: None,
                ..info
            })
            .expect("a question is JSON"),
            serde_json::to_value(&back).expect("a question is JSON"),
        );
    }
}

/// An absent `custom` is absent from the wire, which is what makes the
/// representation equality above possible at all. Asserted directly so that a
/// dropped `skip_serializing_if` reddens here — where the reason is written —
/// rather than only as a confusing inequality in the first test.
#[test]
fn a_question_the_model_sent_carries_no_custom_field_on_the_wire() {
    let written = serde_json::to_value(question_info(&Prompt {
        question: "Which database?".to_owned(),
        header: "Database".to_owned(),
        options: Vec::new(),
        multiple: None,
    }))
    .expect("a question is JSON");

    let object = written.as_object().expect("a question is an object");
    assert!(!object.contains_key("custom"), "{written}");
    assert!(!object.contains_key("multiple"), "{written}");
    // Compared as a set: `serde_json` orders an object's keys itself, so the
    // claim here is which fields are present, not the order they sit in.
    let mut written: Vec<&str> = object.keys().map(String::as_str).collect();
    written.sort_unstable();
    assert_eq!(
        written,
        ["header", "options", "question"],
        "the model's minimum reaches the wire as exactly what it was"
    );
}

/// The choices inside a question are a second declared-twice shape, held by
/// the same pin for the same reason.
#[test]
fn a_choice_round_trips_and_serializes_identically_on_both_sides() {
    let choices = [
        Choice {
            label: "Postgres".to_owned(),
            description: "Relational".to_owned(),
        },
        Choice {
            label: String::new(),
            description: String::new(),
        },
        Choice {
            label: "既定".to_owned(),
            description: "quotes: \" and a backslash: \\".to_owned(),
        },
    ];

    for choice in choices {
        let option = question_option(&choice);

        assert_eq!(
            serde_json::to_value(&option).expect("a choice is JSON"),
            serde_json::to_value(&choice).expect("a choice is JSON"),
        );
        assert_eq!(question_choice(&option), choice);
    }
}

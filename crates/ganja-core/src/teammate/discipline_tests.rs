use ganja_tool::tasklist::{Status, Summary};

use super::*;

/// One summary in `status`, with the fields neither guard reads left blank.
fn task(status: Status) -> Summary {
    Summary {
        id: "1".to_owned(),
        subject: "wire the loader".to_owned(),
        status,
        owner: String::new(),
        blocked_by: Vec::new(),
    }
}

#[test]
fn an_empty_list_is_not_unfinished_work() {
    assert!(
        !holds_unfinished_work(&[]),
        "a team that filed no tasks ends its turn like any other session",
    );
}

#[test]
fn a_pending_or_in_progress_task_is_unfinished_work() {
    assert!(holds_unfinished_work(&[task(Status::Pending)]), "nobody has started it");
    assert!(holds_unfinished_work(&[task(Status::InProgress)]), "somebody is on it");
}

#[test]
fn a_wholly_completed_list_is_not_unfinished_work() {
    assert!(
        !holds_unfinished_work(&[task(Status::Completed), task(Status::Completed)]),
        "every task is done, so there is nothing for a continuation to be about",
    );
}

#[test]
fn one_open_task_among_finished_ones_still_holds_the_turn() {
    assert!(holds_unfinished_work(&[task(Status::Completed), task(Status::Pending)]));
}

#[test]
fn a_task_call_carrying_a_name_is_not_anonymous() {
    assert!(!delegates_anonymously(r#"{"name": "backend", "description": "port the loader"}"#));
}

#[test]
fn a_task_call_without_a_name_is_anonymous() {
    assert!(delegates_anonymously(r#"{"description": "read the config loader"}"#));
}

#[test]
fn a_blank_or_mistyped_name_counts_as_no_name() {
    assert!(delegates_anonymously(r#"{"name": "   "}"#), "whitespace names nobody");
    assert!(delegates_anonymously(r#"{"name": ""}"#), "the empty string names nobody");
    assert!(delegates_anonymously(r#"{"name": 7}"#), "a number is not a teammate's name");
    assert!(delegates_anonymously(r#"{"name": null}"#), "an explicit null names nobody");
}

#[test]
fn arguments_that_will_not_parse_are_not_nagged_about() {
    // The call is about to fail with a message of its own; telling the model
    // to name a teammate on a call that never ran would be advice about the
    // wrong thing.
    assert!(!delegates_anonymously("{\"name\":"), "half a JSON object");
    assert!(!delegates_anonymously(""), "nothing streamed at all");
    assert!(!delegates_anonymously("[1, 2]"), "valid JSON that is not an argument object");
}

/// The nag's second trigger (bead s8rw), and the property that makes the pair
/// safe to ask about one call at a time: the two questions are opposites on
/// every call either can answer, and a call neither can read is neither.
#[test]
fn naming_and_anonymity_are_the_two_answers_a_readable_call_can_give() {
    for arguments in [r#"{"name": "backend"}"#, r#"{"name": " b "}"#] {
        assert!(delegates_named(arguments), "{arguments} names somebody");
        assert!(!delegates_anonymously(arguments));
    }
    for arguments in [r#"{"description": "read it"}"#, r#"{"name": ""}"#, r#"{"name": 7}"#] {
        assert!(!delegates_named(arguments), "{arguments} names nobody");
        assert!(delegates_anonymously(arguments));
    }
    for arguments in ["{\"name\":", "", "[1, 2]"] {
        assert!(!delegates_named(arguments), "a call that never ran is evidence of nothing");
        assert!(!delegates_anonymously(arguments));
    }
}

#[test]
fn a_fresh_turn_carries_no_blocks() {
    let mut discipline = Discipline::default();
    assert!(
        discipline.take_blocks().is_empty(),
        "every scripted and golden run assembles its requests through here",
    );
}

#[test]
fn a_noted_delegation_renders_once_and_is_spent() {
    let mut discipline = Discipline::default();
    discipline.note_anonymous_delegation();

    let blocks = discipline.take_blocks();
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(blocks[0].contains("teammate_naming"), "{blocks:?}");
    assert!(
        discipline.take_blocks().is_empty(),
        "the block belongs to one request, not to every step after it",
    );
}

#[test]
fn a_whole_fan_out_batch_notes_one_block() {
    let mut discipline = Discipline::default();
    // What the caller does for a step whose batch held five anonymous calls:
    // one scan, and the flag set however many times it is set.
    for _ in 0..5 {
        discipline.note_anonymous_delegation();
    }

    assert_eq!(discipline.take_blocks().len(), 1, "a fan-out is nagged once, not per call");
}

#[test]
fn a_continuation_renders_once_and_is_spent() {
    let mut discipline = Discipline::default();
    assert_eq!(discipline.continue_turn(), 1);

    let blocks = discipline.take_blocks();
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(blocks[0].contains("team_still_working"), "{blocks:?}");
    assert!(discipline.take_blocks().is_empty());
}

#[test]
fn both_blocks_ride_one_request_when_both_are_owed() {
    let mut discipline = Discipline::default();
    discipline.note_anonymous_delegation();
    discipline.continue_turn();

    let blocks = discipline.take_blocks();
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(blocks[0].contains("teammate_naming"), "the nag is about the step that just ran");
    assert!(blocks[1].contains("team_still_working"), "the continuation is about what comes next");
}

/// The one arrangement of facts that continues a turn.
const CONTINUES: Facts = Facts { live_team: true, dialog_open: false, unfinished_work: true };

#[test]
fn the_breaker_allows_exactly_five_continuations() {
    let mut discipline = Discipline::default();
    for spent in 1..=MAX_CONTINUATIONS {
        assert!(discipline.may_continue(), "continuation {spent} is still inside the budget");
        assert_eq!(discipline.continue_turn(), spent);
        // Spent as the request that carries it is assembled, exactly as the
        // loop spends it.
        let _ = discipline.take_blocks();
    }

    assert!(
        !discipline.may_continue(),
        "the sixth is refused and the session goes back to the user",
    );
    assert!(
        !discipline.should_continue(CONTINUES),
        "the facts are unchanged; the budget is what ran out",
    );
}

#[test]
fn a_steer_resets_the_budget() {
    let mut discipline = Discipline::default();
    for _ in 0..MAX_CONTINUATIONS {
        discipline.continue_turn();
        // Spent as the request that carries it is assembled, which is the only
        // order the loop can produce: a continuation is taken and rendered
        // before the steer drain can be reached again (bead lymf).
        let _ = discipline.take_blocks();
    }
    assert!(!discipline.may_continue(), "the budget is spent");

    discipline.user_took_over();

    assert!(discipline.may_continue(), "a person driving resets consecutive auto-continuations");
    assert!(discipline.should_continue(CONTINUES), "so the guard is armed again");
}

#[test]
fn a_steer_leaves_a_pending_nag_alone() {
    let mut discipline = Discipline::default();
    discipline.note_anonymous_delegation();
    discipline.user_took_over();

    let blocks = discipline.take_blocks();
    assert_eq!(blocks.len(), 1, "the nag is about a call that really happened: {blocks:?}");
    assert!(blocks[0].contains("teammate_naming"), "{blocks:?}");
}

#[test]
fn every_arrangement_of_the_three_facts_decides_the_turn() {
    // Walked exhaustively rather than sampled: this is the whole condition,
    // and each of the three is somebody's subsystem that could start
    // answering differently.
    for live_team in [false, true] {
        for dialog_open in [false, true] {
            for unfinished_work in [false, true] {
                let facts = Facts { live_team, dialog_open, unfinished_work };
                let expected = live_team && !dialog_open && unfinished_work;
                assert_eq!(
                    Discipline::default().should_continue(facts),
                    expected,
                    "a turn continues only for a live team with open work and no dialog: {facts:?}",
                );
                assert_eq!(
                    facts.would_continue(),
                    expected,
                    "and a fresh turn's budget decides nothing, which is what makes this the \
                     question the breaker's own log line asks (bead lymf): {facts:?}",
                );
            }
        }
    }
}

#[test]
fn the_continuation_note_names_the_budget_and_what_is_open() {
    let note = continuation_note(2, 7);
    assert!(note.contains('2') && note.contains('5'), "{note}");
    assert!(note.contains('7'), "{note}");
}

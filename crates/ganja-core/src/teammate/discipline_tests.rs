use ganja_tool::tasklist::{Status, Summary};

use super::*;
use crate::protocol::team::MemberBackend;
use crate::teammate::backend_name;

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

/// The two rows every spec case below is judged against: `critic-1` on
/// `claude` and `critic-2` on `codex`.
fn spec() -> Vec<crate::command::Member> {
    vec![
        row("critic-1", Some("critic"), MemberBackend::Claude),
        row("critic-2", Some("critic"), MemberBackend::Codex),
    ]
}

/// One roster row.
fn row(name: &str, agent: Option<&str>, backend: MemberBackend) -> crate::command::Member {
    crate::command::Member { name: name.to_owned(), agent: agent.map(str::to_owned), backend }
}

/// A `task` call's arguments as the model writes them.
fn spawn(name: &str, backend: Option<&str>, agent: Option<&str>) -> String {
    let mut args = serde_json::json!({ "name": name, "description": "take a piece of it" });
    if let Some(backend) = backend {
        args["backend"] = serde_json::Value::String(backend.to_owned());
    }
    if let Some(agent) = agent {
        args["subagent_type"] = serde_json::Value::String(agent.to_owned());
    }

    args.to_string()
}

/// A fresh turn's guards, and the roster it is judging against.
fn judging() -> (Discipline, Vec<crate::command::Member>) {
    (Discipline::default(), spec())
}

/// Every row of `spec` spawned exactly as it says, so the roster is complete.
fn complete(discipline: &mut Discipline, spec: &[crate::command::Member]) {
    for member in spec {
        assert_eq!(
            discipline.off_the_spec(
                spec,
                &spawn(&member.name, Some(backend_name(member.backend)), member.agent.as_deref())
            ),
            None,
            "a row spawned as it says is claimed in silence",
        );
    }
}

#[test]
fn a_call_using_its_row_is_not_corrected() {
    let (mut discipline, spec) = judging();

    assert_eq!(discipline.off_the_spec(&spec, &spawn("critic-1", Some("claude"), None)), None);
    assert_eq!(discipline.off_the_spec(&spec, &spawn("critic-2", Some("codex"), None)), None);
}

#[test]
fn a_row_spawned_on_another_surface_is_told_its_own() {
    let (mut discipline, spec) = judging();

    let told = discipline
        .off_the_spec(&spec, &spawn("critic-1", Some("codex"), None))
        .expect("the roster put critic-1 on claude");

    assert!(told.contains("critic-1"), "the correction names the call: {told}");
    assert!(told.contains("`codex`"), "and the surface it was spawned on: {told}");
    assert!(
        told.contains("critic-1 — critic on claude"),
        "and quotes the row in the roster\u{2019}s own spelling, so the model can find it: {told}",
    );
}

/// And a row started in the wrong place is still **started**: the claim goes by
/// name whatever surface was named, so the arms that offer waiting rows stop
/// offering this one.
///
/// Without that, one mistyped surface would earn two corrections at once — its
/// own, and a second telling the model to use the very row it just used.
#[test]
fn a_wrong_surface_still_claims_the_row_it_named() {
    let (mut discipline, spec) = judging();
    assert!(discipline.off_the_spec(&spec, &spawn("critic-1", Some("codex"), None)).is_some());

    let told = discipline
        .off_the_spec(&spec, &spawn("helper", Some("codex"), Some("critic")))
        .expect("critic-2 is still waiting");

    assert!(told.contains("critic-2 — critic on codex"), "{told}");
    assert!(
        !told.contains("critic-1 — critic on claude"),
        "and never offers the row that call already took: {told}",
    );
}

/// And the wrong-surface arm is the one that never goes quiet: a row spawned
/// somewhere it was not assigned is wrong whenever it happens, before or after
/// the roster is complete, because the surface decides what that member can
/// see.
#[test]
fn a_wrong_surface_is_still_told_once_every_row_is_claimed() {
    let (mut discipline, spec) = judging();
    complete(&mut discipline, &spec);

    assert!(
        discipline.off_the_spec(&spec, &spawn("critic-2", Some("grok"), None)).is_some(),
        "the roster put critic-2 on codex, and a complete roster does not make that right",
    );
}

/// **Dv-3.** A name no row holds is judged only against the rows still waiting,
/// and only by the agent it runs as: a spawn of the same role while a row for
/// that role is unfilled is the roster being worked around.
#[test]
fn an_unknown_name_of_a_waiting_row_s_agent_is_told_that_row() {
    let (mut discipline, spec) = judging();

    let told = discipline
        .off_the_spec(&spec, &spawn("helper", Some("claude"), Some("critic")))
        .expect("both critic rows are still waiting");

    assert!(told.contains("helper"), "{told}");
    assert!(told.contains("critic-1 — critic on claude"), "{told}");
    assert!(told.contains("critic-2 — critic on codex"), "{told}");
}

/// And a spawn of some *other* role is not the roster being worked around at
/// all — which is team-prd\u{2019}s analyst and critic, and explore scouting, none of
/// which the roster is about (**R5**).
#[test]
fn an_unknown_name_of_another_agent_is_told_nothing() {
    let (mut discipline, spec) = judging();

    assert_eq!(
        discipline.off_the_spec(&spec, &spawn("scout-1", Some("ganja"), Some("analyst"))),
        None,
        "no waiting row runs as analyst, so this spawn departs from nothing",
    );
}

/// A `worker` row named no agent, so nothing can match it by one: it is
/// reached by its name alone, and an unknown name never stands in for it.
#[test]
fn an_agent_less_row_is_never_matched_by_agent() {
    let mut discipline = Discipline::default();
    let spec = vec![row("worker-1", None, MemberBackend::Claude)];

    assert_eq!(
        discipline.off_the_spec(&spec, &spawn("helper", Some("claude"), Some("critic"))),
        None,
        "a row whose agent the user left open cannot be the row this call should have used",
    );
    assert!(
        discipline.off_the_spec(&spec, &spawn("worker-1", Some("codex"), None)).is_some(),
        "and it is still reached by its own name, on the surface it was given",
    );
}

/// Spawning a row twice while others are unfilled is the roster being
/// short-changed: the person asked for one of each.
#[test]
fn a_second_claim_on_one_row_is_told_what_is_still_waiting() {
    let (mut discipline, spec) = judging();
    assert_eq!(discipline.off_the_spec(&spec, &spawn("critic-1", Some("claude"), None)), None);

    let told = discipline
        .off_the_spec(&spec, &spawn("critic-1", Some("claude"), None))
        .expect("critic-2 is still waiting");

    assert!(told.contains("critic-1"), "{told}");
    assert!(told.contains("critic-2 — critic on codex"), "and what was left unspawned: {told}");
}

/// Once every row is spawned the roster has been served, and **R5** hands the
/// rest of the run back to the template: a replacement for a member that
/// stopped answering, or one more when work backs up, is the template\u{2019}s own
/// instruction and earns nothing.
#[test]
fn a_complete_roster_stops_judging_everything_but_the_surface() {
    let (mut discipline, spec) = judging();
    complete(&mut discipline, &spec);

    assert_eq!(
        discipline.off_the_spec(&spec, &spawn("critic-1", Some("claude"), None)),
        None,
        "a replacement wearing a served row\u{2019}s name is the template\u{2019}s business",
    );
    assert_eq!(
        discipline.off_the_spec(&spec, &spawn("critic-3", Some("claude"), Some("critic"))),
        None,
        "and so is one more critic when work backs up",
    );
    assert_eq!(
        discipline.off_the_spec(&spec, &spawn("verifier-1", Some("ganja"), Some("verifier"))),
        None,
        "and so is team-verify\u{2019}s own verifier, which was never on this roster",
    );
}

/// The two silences that belong to somebody else: an anonymous call is the
/// name nag\u{2019}s, and a call whose arguments will not parse is about to fail with
/// a message of its own.
#[test]
fn an_anonymous_or_unreadable_call_is_not_this_arms_business() {
    let (mut discipline, spec) = judging();

    assert_eq!(discipline.off_the_spec(&spec, r#"{"description": "take a piece of it"}"#), None);
    assert_eq!(discipline.off_the_spec(&spec, &spawn("   ", Some("claude"), None)), None);
    assert_eq!(discipline.off_the_spec(&spec, "{not json"), None);
}

/// An omitted `backend` is not \u{201c}no answer\u{201d}: the spawn door runs such a call on
/// the default surface, so that is the surface the roster is compared against.
#[test]
fn an_omitted_backend_is_judged_as_the_default_surface() {
    let (mut discipline, spec) = judging();
    assert!(
        discipline.off_the_spec(&spec, &spawn("critic-1", None, None)).is_some(),
        "the roster put critic-1 on claude, and an omitted backend is not claude",
    );

    let mut other = Discipline::default();
    let default_row = vec![row("worker-1", None, crate::teammate::DEFAULT_BACKEND)];
    assert_eq!(
        other.off_the_spec(&default_row, &spawn("worker-1", None, None)),
        None,
        "and a row on the default surface is exactly what such a call spawns",
    );
}

#[test]
fn a_whole_batch_earns_one_block_naming_every_row_it_missed() {
    let (mut discipline, spec) = judging();
    let calls = [
        spawn("critic-1", Some("codex"), None),
        spawn("helper", Some("claude"), Some("critic")),
        spawn("critic-2", Some("codex"), None),
    ];

    discipline.note_roster_departures(&spec, calls.iter().map(String::as_str));

    let blocks = discipline.take_blocks();
    let [block] = blocks.as_slice() else { panic!("one block for the batch: {blocks:?}") };
    assert!(block.contains("critic-1"), "{block}");
    assert!(block.contains("helper"), "{block}");
    assert!(
        discipline.take_blocks().is_empty(),
        "and it is spent by the request that carried it, like the two beside it",
    );
}

#[test]
fn a_batch_that_departed_from_no_row_records_nothing() {
    let (mut discipline, spec) = judging();
    let calls = [spawn("critic-1", Some("claude"), None), spawn("critic-2", Some("codex"), None)];

    discipline.note_roster_departures(&spec, calls.iter().map(String::as_str));

    assert!(
        discipline.take_blocks().is_empty(),
        "a step that used the roster is a step with nothing to be told",
    );
}

/// A batch that fills part of the roster is not short-changing it: the rest may
/// come in a later step, and the claims are the turn\u{2019}s rather than the step\u{2019}s.
#[test]
fn an_under_spawning_batch_is_silent_and_its_claims_survive_the_step() {
    let (mut discipline, spec) = judging();

    discipline.note_roster_departures(
        &spec,
        [spawn("critic-1", Some("claude"), None)].iter().map(String::as_str),
    );
    assert!(discipline.take_blocks().is_empty(), "the rest may come in a later step");

    assert!(
        discipline.off_the_spec(&spec, &spawn("critic-1", Some("claude"), None)).is_some(),
        "and the claim the first step made is still a claim in the second",
    );
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
    // The whole rendering rather than the digits in it, for the reason
    // `tasklist_tests.rs` gives about the counterpart cap: a note that merely
    // carried a `2`, a `5` and a `7` somewhere would pass three `contains`
    // calls — including one that had swapped what was spent for what is open,
    // which is the regression this sentence exists to catch.
    assert_eq!(
        continuation_note(2, 7),
        format!("auto-continuation 2 of {MAX_CONTINUATIONS}, 7 task(s) open"),
    );
}

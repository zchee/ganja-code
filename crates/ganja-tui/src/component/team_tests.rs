use std::sync::Arc;

use ganja_protocol::{MemberBackend, MemberView, TeamView};
use ganja_testkit::RecordingSpawner;
use ganja_tool::Tool as _;
use ganja_tool::task::{Offered, Subagents, TaskTool, TeammateSpawn, Teammated};
use ganja_tool::tasklist::{Status, Summary};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{BUSY, Effect, Row, Spawned, Team, rows, spawn_request};
use crate::command;
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 76, height: 20 };

fn row(name: &str, backend: MemberBackend, recent: &[&str]) -> Row {
    Row {
        name: name.to_owned(),
        backend,
        is_lead: false,
        color: None,
        recent: recent.iter().map(|call| (*call).to_owned()).collect(),
    }
}

fn lead() -> Row {
    Row {
        name: "team-lead".to_owned(),
        backend: MemberBackend::InProcess,
        is_lead: true,
        color: None,
        recent: Vec::new(),
    }
}

/// The roster every dialog here opens over: this session, one in-process
/// teammate and one running a real `claude`.
fn members() -> Vec<Row> {
    vec![
        lead(),
        row("w1", MemberBackend::InProcess, &["read(src/lib.rs)", "grep(fn spawn)"]),
        row("w2", MemberBackend::Claude, &[]),
    ]
}

fn dialog() -> Team {
    Team::new(members(), Vec::new())
}

/// One task on the shared list, as the engine's listing hands it over.
fn task(id: &str, status: Status, owner: &str, subject: &str, blocked_by: &[&str]) -> Summary {
    Summary {
        id: id.to_owned(),
        subject: subject.to_owned(),
        status,
        owner: owner.to_owned(),
        blocked_by: blocked_by.iter().map(|id| (*id).to_owned()).collect(),
    }
}

/// Whether a rendered line *is* the Tasks heading, once the dialog's border
/// and padding are off it.
fn heading(line: &str) -> bool {
    line.trim_matches(|character: char| character.is_whitespace() || character == '\u{2502}')
        == super::TASKS
}

/// The three-task list the section tests draw.
fn tasks() -> Vec<Summary> {
    vec![
        task("1", Status::Completed, "w1", "Read the plan", &[]),
        task("2", Status::InProgress, "w1", "Wire the parser", &[]),
        task("3", Status::Pending, "", "Draw the section", &["2"]),
    ]
}

fn rendered(dialog: &Team, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|line| {
            (0..area.width).map(|column| buffer[(column, line)].symbol()).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Types `text` into whichever free-text step is open.
fn type_in(dialog: &mut Team, text: &str) {
    for character in text.chars() {
        dialog.push(character);
    }
}

/// Drives the dialog's spawn flow for `line` and answers with the request
/// it built.
fn spawn_through_the_dialog(line: &str) -> TeammateSpawn {
    let mut dialog = dialog();
    // Past the three members, onto the Spawn row.
    dialog.move_selection(9);
    assert!(dialog.selected_member().is_none(), "the Spawn row is last");
    assert_eq!(dialog.submit(), None, "Spawn opens its free-text step");
    assert!(dialog.is_typing());
    type_in(&mut dialog, line);

    match dialog.submit() {
        Some(Effect::Spawn { request, typed }) => {
            assert_eq!(typed, line, "the words travel with the spawn, as typed");
            request
        }
        other => panic!("expected a spawn, got {other:?}"),
    }
}

/// The request the **real** `task` door builds for the same arguments —
/// run through `TaskTool` itself rather than reconstructed here, because a
/// hand-written expectation would assert this test's reading of the door
/// instead of the door.
async fn spawn_through_the_task_door(args: serde_json::Value) -> TeammateSpawn {
    let recorder = RecordingSpawner::new(Teammated {
        name: "w3".to_owned(),
        agent_id: "w3@session-abcd1234".to_owned(),
        backend: "in-process".to_owned(),
        note: "it reads this through its mailbox".to_owned(),
    });
    let ctx = ganja_testkit::tool_ctx(Arc::clone(&recorder) as Arc<dyn Subagents>);
    TaskTool::new(&[Offered { name: "general".to_owned(), description: None }])
        .run(args, &ctx)
        .await
        .expect("a teammate starts");

    recorder.started().into_iter().next().expect("one spawn was recorded")
}

/// **AC-14**, the `/teammate spawn` half: the two doors are one sequence
/// because they build one request. The `task` door's value is taken from
/// the door itself, so this cannot pass by both sides sharing a mistake.
///
/// What is compared is the whole value — the `ganja-tool` type both doors
/// really hand the engine. Until **D513** the typed door wrapped it beside
/// a `bypass` the `task` door had no argument for, and only the inner
/// value could be compared; now there is nothing the two could differ on.
#[tokio::test]
async fn the_dialog_builds_the_same_spawn_request_the_task_door_does() {
    let cases = [
        (
            "w3 --backend in-process hold the fort",
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "w3",
                "backend": "in-process",
            }),
        ),
        // No `--backend`: absence is the far side's default on both doors,
        // never a value either of them writes in.
        (
            "w3 hold the fort",
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "w3",
            }),
        ),
        // AC-11's own spelling, which carries no prompt at all.
        (
            "w3 --backend ganja",
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "",
                "subagent_type": "general",
                "name": "w3",
                "backend": "ganja",
            }),
        ),
        // A named agent kind reaches the same field `subagent_type` does.
        (
            "w3 --agent explore --backend claude look around",
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "look around",
                "subagent_type": "explore",
                "name": "w3",
                "backend": "claude",
            }),
        ),
    ];

    for (line, args) in cases {
        assert_eq!(
            spawn_through_the_dialog(line),
            spawn_through_the_task_door(args).await,
            "the dialog and the task door disagree about {line:?}"
        );
    }
}

/// Resolution 4: nothing stands in front of a spawn, so the one thing a
/// person is told is told afterwards — and it is where their prompt now
/// sits in cleartext.
#[test]
fn a_spawn_says_where_the_prompt_came_to_rest() {
    let mut dialog = dialog();
    dialog.spawned(&Spawned { name: "w3".to_owned(), prompt_path: "/t/teams/t1.json".to_owned() });

    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("prompt persisted in cleartext at"), "got:\n{screen}");
    assert!(screen.contains("/t/teams/t1.json"), "got:\n{screen}");
    assert!(screen.contains("w3 started"), "and which spawn it is about:\n{screen}");
}

/// A spawn already starting refuses a second one at the keypress rather
/// than after a whole line has been typed — the `/plugin` dialog's posture
/// for the actions that would race each other.
#[test]
fn a_second_spawn_during_the_first_is_refused_before_anything_is_typed() {
    let mut dialog = dialog();
    dialog.set_busy(true);
    assert!(dialog.is_busy());
    dialog.move_selection(9);

    assert_eq!(dialog.submit(), None);
    assert!(!dialog.is_typing(), "the input step must not even open");
    let screen = rendered(&dialog, AREA);
    assert!(
        BUSY.split(" \u{b7} ").next().is_some_and(|head| screen.contains(head)),
        "the refusal is the dialog's own sentence:\n{screen}"
    );

    dialog.set_busy(false);
    assert_eq!(dialog.submit(), None, "and once it is done, it opens");
    assert!(dialog.is_typing());
}

/// **D503**: the backend with no window of its own is not the least
/// observable one — its recent calls hang under its row.
#[test]
fn every_member_lists_with_its_backend_and_its_recent_calls() {
    let screen = rendered(&dialog(), AREA);

    assert!(screen.contains("team-lead"), "got:\n{screen}");
    assert!(screen.contains("lead"), "got:\n{screen}");
    assert!(screen.contains("in-process"), "got:\n{screen}");
    assert!(screen.contains("claude"), "got:\n{screen}");
    assert!(screen.contains("read(src/lib.rs)"), "got:\n{screen}");
    assert!(screen.contains("grep(fn spawn)"), "got:\n{screen}");
}

/// A ring longer than the row shows admits what it cut rather than
/// quietly showing the oldest four.
#[test]
fn a_ring_longer_than_the_row_shows_admits_the_cut() {
    let calls: Vec<&str> = vec!["a", "b", "c", "d", "e", "f"];
    let screen =
        rendered(&Team::new(vec![row("w1", MemberBackend::InProcess, &calls)], Vec::new()), AREA);

    assert!(screen.contains("+2 earlier calls"), "got:\n{screen}");
    assert!(screen.contains("\u{23bf} f"), "the newest is shown:\n{screen}");
    assert!(!screen.contains("\u{23bf} a"), "the oldest is cut:\n{screen}");
}

/// Enter on a teammate opens Message and Shutdown; the lead's row offers
/// neither, because this session is the lead and `/exit` is its door.
#[test]
fn a_teammate_row_offers_message_and_shutdown_and_the_leads_offers_nothing() {
    let mut dialog = dialog();
    assert_eq!(dialog.selected_member().map(|row| row.name.as_str()), Some("team-lead"));
    assert_eq!(dialog.submit(), None, "the lead's row has nothing to open");
    assert!(!dialog.is_choosing_action());

    dialog.move_selection(1);
    assert_eq!(dialog.submit(), None, "Enter opens the action step");
    assert!(dialog.is_choosing_action());

    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("Message"), "got:\n{screen}");
    assert!(screen.contains("Shutdown"), "got:\n{screen}");

    dialog.move_selection(1);
    assert_eq!(dialog.submit(), Some(Effect::Shutdown("w1".to_owned())));
    assert!(!dialog.is_choosing_action(), "and back to the roster");
}

/// Message takes its text in the dialog's own free-text step, the way the
/// `/plugin` dialog takes a marketplace: nothing about what one teammate
/// says to another rides an engine `question` round trip.
#[test]
fn messaging_a_member_takes_the_text_in_the_dialog_itself() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    dialog.submit();
    assert_eq!(dialog.submit(), None, "Message opens the free-text step");
    assert!(dialog.is_typing());

    type_in(&mut dialog, "status?");
    assert_eq!(dialog.input(), Some("status?"));

    assert_eq!(
        dialog.submit(),
        Some(Effect::Message { to: "w1".to_owned(), text: "status?".to_owned() })
    );
    assert!(!dialog.is_typing());
}

/// A refused spawn line keeps the step and the text: the answer to a
/// mistyped flag is to fix that word.
#[test]
fn a_refused_spawn_line_says_why_and_keeps_what_was_typed() {
    let mut dialog = dialog();
    dialog.move_selection(9);
    dialog.submit();
    type_in(&mut dialog, "w3 --nonesuch");

    assert_eq!(dialog.submit(), None, "nothing is sent");
    assert!(dialog.is_typing(), "and the line is still there to fix");
    assert_eq!(dialog.input(), Some("w3 --nonesuch"));

    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("--nonesuch"), "got:\n{screen}");
}

/// Esc on the free-text step abandons the text without sending it, and
/// leaves the dialog open — `false` from `cancel` is what closes it.
#[test]
fn escaping_the_free_text_step_abandons_it_without_closing_the_dialog() {
    let mut dialog = dialog();
    dialog.move_selection(9);
    dialog.submit();
    type_in(&mut dialog, "w3");

    assert!(dialog.cancel(), "the free-text step consumes Esc");
    assert!(!dialog.is_typing());
    assert_eq!(dialog.input(), None);
    assert!(!dialog.cancel(), "and the next Esc closes the dialog");
}

/// Backspace edits the free-text step and nothing else.
#[test]
fn backspace_takes_a_character_off_the_typed_line() {
    let mut dialog = dialog();
    dialog.backspace();
    dialog.push('x');
    assert_eq!(dialog.input(), None, "the list step has no line to edit");

    dialog.move_selection(9);
    dialog.submit();
    type_in(&mut dialog, "w3x");
    dialog.backspace();

    assert_eq!(dialog.input(), Some("w3"));
}

/// A poll refresh keeps the cursor on the same position rather than
/// resetting it, and reclamps when a shutdown shrank the roster.
#[test]
fn refreshing_keeps_the_cursor_where_it_was_and_reclamps_a_shrink() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    dialog.refresh(
        vec![
            lead(),
            row("w1", MemberBackend::InProcess, &["write(src/main.rs)"]),
            row("w2", MemberBackend::Claude, &[]),
        ],
        Vec::new(),
    );
    assert_eq!(dialog.selected_member().map(|row| row.name.as_str()), Some("w1"));
    assert_eq!(
        dialog.selected_member().map(|row| row.recent.len()),
        Some(1),
        "and the ring is the fresh one"
    );

    dialog.move_selection(2);
    dialog.refresh(vec![lead()], Vec::new());
    assert!(dialog.selected_member().is_none(), "the cursor reclamps onto the Spawn row");
}

/// A poll that found the same roster changed nothing, and says so — which
/// is what keeps an open dialog from repainting the screen on every one of
/// the ticks it polls on.
#[test]
fn a_refresh_that_found_the_same_roster_reports_nothing_moved() {
    let mut dialog = dialog();

    assert!(
        !dialog.refresh(
            vec![
                lead(),
                row("w1", MemberBackend::InProcess, &["read(src/lib.rs)", "grep(fn spawn)"],),
                row("w2", MemberBackend::Claude, &[]),
            ],
            Vec::new(),
        ),
        "an identical poll is not a reason to redraw"
    );
    assert!(
        dialog.refresh(
            vec![
                lead(),
                row("w1", MemberBackend::InProcess, &["write(src/main.rs)"]),
                row("w2", MemberBackend::Claude, &[]),
            ],
            Vec::new(),
        ),
        "a ring that moved is"
    );
}

/// **The action step is about a member, not about a row index.** The
/// roster is re-polled on every tick, so a teammate retiring or spawning
/// mid-decision moves the rows under the cursor — and an Enter that
/// resolved by index would shut down whoever slid into that slot.
#[test]
fn an_action_chosen_for_one_member_still_names_it_after_the_roster_moved() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    assert_eq!(dialog.submit(), None, "Enter opens w1's actions");
    assert!(dialog.is_choosing_action());

    // w0 joined the team while the action menu was up, so w1 is no longer
    // the row the cursor's old index named.
    dialog.refresh(
        vec![
            lead(),
            row("w0", MemberBackend::InProcess, &[]),
            row("w1", MemberBackend::InProcess, &[]),
            row("w2", MemberBackend::Claude, &[]),
        ],
        Vec::new(),
    );
    assert!(dialog.is_choosing_action(), "the step is still w1's");
    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("w1"), "and says so:\n{screen}");

    dialog.move_selection(1);

    assert_eq!(
        dialog.submit(),
        Some(Effect::Shutdown("w1".to_owned())),
        "the member Enter was pressed for, not the one at that index now"
    );
}

/// And when that member is the one that left, the step drops rather than
/// going on offering Shutdown for a teammate that has shut down.
#[test]
fn an_action_step_whose_member_left_the_roster_drops_back_to_it() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    dialog.submit();
    assert!(dialog.is_choosing_action());

    assert!(
        dialog.refresh(vec![lead(), row("w2", MemberBackend::Claude, &[])], Vec::new()),
        "a dropped step is something moved"
    );

    assert!(!dialog.is_choosing_action(), "and back to the roster");
    // The cursor stayed where it was, so the next Enter opens the actions
    // of whoever is under it now — chosen by a person looking at that row,
    // which is the whole difference.
    assert_eq!(dialog.submit(), None);
    dialog.move_selection(1);
    assert_eq!(dialog.submit(), Some(Effect::Shutdown("w2".to_owned())));
}

/// The word on a row is serde's own wire spelling of the backend — which
/// is also the word a person would type after `--backend` to ask for
/// another one like it.
///
/// Driven off the engine's own [`ganja_core::teammate::BACKENDS`] rather
/// than a list written out here, so a seventh surface joins this assertion
/// by existing. A hand-written list is exactly how this test came to cover
/// three arms of six.
#[test]
fn the_backend_label_is_the_wires_own_spelling() {
    for name in ganja_core::teammate::BACKENDS {
        let backend =
            ganja_core::teammate::parse_backend(name).expect("a value the grammar lists parses");

        assert_eq!(
            serde_json::to_value(backend).expect("a backend serializes"),
            serde_json::Value::String(super::backend_label(backend).to_owned()),
            "the row and the wire must spell {name} the same"
        );
        // And the row spells it the way the argument does, which is the
        // half a person acts on: the word they read is the word they type.
        assert_eq!(super::backend_label(backend), name);
    }
}

/// The input step's prompt and the grammar a refusal names are one
/// constant, so the dialog cannot teach a spelling `team_spawn` refuses.
#[test]
fn the_spawn_prompt_shows_the_grammar_the_refusal_names() {
    let mut dialog = dialog();
    dialog.move_selection(9);
    dialog.submit();

    let screen = rendered(&dialog, AREA);
    // The dialog is narrower than the whole grammar, so what the screen
    // shows is its head; the tie to the refusal is the shared constant.
    assert!(screen.contains(&command::SPAWN_GRAMMAR[..40]), "got:\n{screen}");
    assert!(
        command::team_spawn("")
            .expect_err("a nameless spawn is refused")
            .contains(command::SPAWN_GRAMMAR),
        "and the refusal names the same grammar"
    );
}

/// The command grammar is the dialog's grammar, so a `/teammate` line and the
/// dialog's own step cannot mean two different things.
#[test]
fn a_typed_team_line_and_the_dialogs_step_build_the_same_request() {
    let Some(command::Team::Spawn(line)) =
        command::team("/teammate spawn w3 --agent explore --backend claude go")
    else {
        panic!("`/teammate spawn` should parse");
    };

    assert_eq!(
        spawn_request(&line),
        spawn_through_the_dialog("w3 --agent explore --backend claude go")
    );
}

/// The projection a caller polling the registry hands in, and the one
/// ordering the dialog promises: the lead first, because it is the row a
/// person looks for to know which session they are in.
#[test]
fn the_lead_is_the_first_row_however_the_registry_ordered_it() {
    let view = TeamView {
        team: "session-abcd1234".to_owned(),
        lead: "team-lead".to_owned(),
        members: vec![
            MemberView {
                name: "w1".to_owned(),
                agent_id: "w1@session-abcd1234".to_owned(),
                backend: MemberBackend::Claude,
                color: Some("blue".to_owned()),
                is_lead: false,
                recent_calls: vec!["read(src/lib.rs)".to_owned()],
            },
            MemberView {
                name: "team-lead".to_owned(),
                agent_id: "team-lead@session-abcd1234".to_owned(),
                backend: MemberBackend::InProcess,
                color: None,
                is_lead: true,
                recent_calls: Vec::new(),
            },
        ],
    };

    let projected = rows(&view);

    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].name, "team-lead");
    assert!(projected[0].is_lead);
    assert_eq!(projected[1].name, "w1");
    assert_eq!(projected[1].color.as_deref(), Some("blue"));
    assert_eq!(projected[1].recent, vec!["read(src/lib.rs)".to_owned()]);
}

/// The Tasks section draws the team's shared list under the roster: which
/// task, where it is, who holds it, what it waits on and what it is.
#[test]
fn the_tasks_section_lists_every_task_with_its_status_owner_and_blockers() {
    let mut dialog = dialog();
    assert!(dialog.refresh(members(), tasks()), "a fresh list repaints");

    let screen = rendered(&dialog, Rect::new(0, 0, 76, 40));

    assert!(screen.contains("tasks"), "the section is headed:\n{screen}");
    assert!(screen.contains("Wire the parser"), "got:\n{screen}");
    assert!(screen.contains("in_progress"), "got:\n{screen}");
    assert!(screen.contains("completed"), "got:\n{screen}");
    assert!(screen.contains("unowned"), "the unclaimed task says so:\n{screen}");
    assert!(screen.contains("(blocked by 2)"), "got:\n{screen}");
}

/// The list is drawn in the order it arrived — the store's own lowest-id
/// first, which is the order `task_list` answers a model with. Two
/// renderings of one listing must not disagree about which task is next.
#[test]
fn the_tasks_section_keeps_the_order_the_list_arrived_in() {
    let mut dialog = dialog();
    dialog.refresh(members(), tasks());

    let screen = rendered(&dialog, Rect::new(0, 0, 76, 40));
    let position =
        |needle: &str| screen.find(needle).unwrap_or_else(|| panic!("{needle} is drawn"));

    assert!(position("Read the plan") < position("Wire the parser"));
    assert!(position("Wire the parser") < position("Draw the section"));
}

/// The plan's third risk, drawn: a `claude` member runs its own task store
/// and the foreign CLIs hold no ganja tools at all, so a section under a
/// roster holding one names it rather than letting the list read as
/// everybody's.
#[test]
fn the_tasks_section_names_the_members_that_cannot_see_the_list() {
    let mut dialog = dialog();
    dialog.refresh(members(), tasks());

    let screen = rendered(&dialog, Rect::new(0, 0, 76, 40));

    assert!(screen.contains("not visible to w2"), "the claude member is named:\n{screen}");
    assert!(
        !screen.contains("not visible to w1"),
        "the in-process member reads the same list:\n{screen}"
    );
}

/// And says nothing at all where every member shares the list: a standing
/// disclaimer under a roster it is not true of would be read past.
#[test]
fn a_roster_that_all_shares_the_list_draws_no_such_line() {
    let dialog = Team::new(
        vec![
            lead(),
            row("w1", MemberBackend::InProcess, &[]),
            row("w2", MemberBackend::Ganja, &[]),
        ],
        tasks(),
    );

    let screen = rendered(&dialog, Rect::new(0, 0, 76, 40));

    assert!(screen.contains("Wire the parser"), "the list is drawn:\n{screen}");
    assert!(!screen.contains("not visible to"), "and nothing is disclaimed:\n{screen}");
}

/// An empty list draws no section at all — no heading over nothing.
#[test]
fn a_team_that_has_filed_no_task_draws_no_tasks_section() {
    let screen = rendered(&dialog(), AREA);

    // The heading as a whole line rather than as a word anywhere on screen:
    // a member named `tasks` — or a ring entry mentioning one — is not this
    // section, and a test that could not tell them apart would redden for
    // the wrong reason.
    assert!(!screen.lines().any(heading), "no heading over nothing:\n{screen}");
    assert!(!screen.contains(super::UNOWNED), "and nothing under it:\n{screen}");
}

/// The window onto the list is the head of it: nothing under the Spawn row
/// is selectable, so the scroll never travels down to these lines and a list
/// longer than the rows left under the roster is cut at the bottom with no
/// marker. The roster it hangs under stays on screen however long the list
/// grows, which is what the placement is for.
#[test]
fn a_task_list_longer_than_the_window_is_cut_and_leaves_the_roster_standing() {
    let many: Vec<Summary> = (1..=30)
        .map(|index| task(&index.to_string(), Status::Pending, "", &format!("task {index}"), &[]))
        .collect();
    let screen = rendered(&Team::new(members(), many), AREA);

    assert!(screen.contains("team-lead"), "the roster stays on screen:\n{screen}");
    assert!(screen.contains(super::TASKS), "the section is drawn:\n{screen}");
    assert!(screen.contains("task 1"), "from the head of the list:\n{screen}");
    assert!(!screen.contains("task 30"), "and the tail is simply cut:\n{screen}");
}

/// A task's text is somebody else's, and the frame is not what a control
/// character in it costs: ratatui skips a zero-width cell, so an unguarded
/// newline is swallowed and the words either side of it are drawn joined.
/// Both halves are pinned — the row still occupies one line, and both the
/// characters that would have vanished, in the subject and in the blocker id
/// beside it, are on screen.
#[test]
fn a_newline_in_a_subject_is_shown_rather_than_silently_swallowed() {
    let clean =
        rendered(&Team::new(members(), vec![task("1", Status::Pending, "", "ab", &["cd"])]), AREA);
    let dirty = rendered(
        &Team::new(members(), vec![task("1", Status::Pending, "", "a\nb", &["c\nd"])]),
        AREA,
    );

    assert_eq!(dirty.lines().count(), clean.lines().count(), "the frame holds:\n{dirty}");
    assert_eq!(
        dirty.matches(char::REPLACEMENT_CHARACTER).count(),
        2,
        "and both are shown rather than swallowed:\n{dirty}"
    );
}

/// The section is data, not a row: the cursor still walks the members and
/// the Spawn row and nothing else, however long the list is.
#[test]
fn the_tasks_section_takes_no_cursor_position() {
    let mut dialog = dialog();
    dialog.refresh(members(), tasks());

    // Three members, then the Spawn row: four positions, and the fourth is
    // still the last however many tasks hang under it.
    dialog.move_selection(3);
    assert!(dialog.selected_member().is_none(), "the Spawn row is last");
    dialog.move_selection(1);
    assert!(dialog.selected_member().is_none(), "and nothing is past it");
    assert_eq!(dialog.submit(), None, "Enter there still opens the spawn step");
    assert!(dialog.is_typing());
}

/// A list that did not move repaints nothing, the roster's own rule: a
/// dialog left open would otherwise redraw at frame rate for a list nobody
/// touched.
#[test]
fn a_task_list_that_did_not_move_is_not_a_repaint() {
    let mut dialog = dialog();
    assert!(dialog.refresh(members(), tasks()), "the first list is news");
    assert!(!dialog.refresh(members(), tasks()), "the same list is not");

    let mut moved = tasks();
    moved[2].owner = "w1".to_owned();
    assert!(dialog.refresh(members(), moved), "a claim is news again");
}

#[test]
fn a_team_with_nobody_in_it_says_so_and_still_offers_a_spawn() {
    let dialog = Team::new(Vec::new(), Vec::new());
    let screen = rendered(&dialog, AREA);

    assert!(dialog.selected_member().is_none());
    assert!(screen.contains("no team members"), "got:\n{screen}");
    assert!(screen.contains("Spawn teammate"), "got:\n{screen}");
}

#[test]
fn a_row_too_wide_for_the_column_is_cut_rather_than_wrapped() {
    let long = "very long call ".repeat(20);
    let dialog =
        Team::new(vec![row(&"w".repeat(90), MemberBackend::InProcess, &[long.as_str()])], tasks());

    for line in rendered(&dialog, Rect::new(0, 0, 60, 20)).lines() {
        assert!(line.chars().count() <= 60, "a row must not overflow the dialog: {line:?}");
    }
}

#[test]
fn a_tiny_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (3, 2), (8, 4)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        dialog().render(area, &mut buffer, &Theme::default());
    }
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&dialog(), Rect::new(0, 0, 0, 0));

    assert!(screen.is_empty(), "a zero area has no cell to hold: {screen}");
}

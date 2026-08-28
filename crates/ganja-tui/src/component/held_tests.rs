use std::time::Duration;

use ganja_protocol::{HeldId, HoldCause, PolicySource};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Action, HeldApproval, HeldList, Row, cause_label, cause_sentence, coarse};
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 76, height: 20 };

fn approval() -> HeldApproval {
    HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@ganja-team".to_owned(),
        HoldCause::NoModeAsserted,
        Some("a finding worth a look".to_owned()),
        "the full body of the finding\nwith a second line".to_owned(),
        Some(300_000),
    )
}

fn row(id: &str, from: &str, cause: HoldCause) -> Row {
    Row::new(
        HeldId::from(id.to_owned()),
        from.to_owned(),
        cause,
        Duration::from_secs(65),
        Some("what it said"),
        "the body",
    )
}

fn rendered_approval(dialog: &HeldApproval, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

    screen(&buffer, area)
}

fn rendered_list(dialog: &HeldList, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

    screen(&buffer, area)
}

fn screen(buffer: &Buffer, area: Rect) -> String {
    (0..area.height)
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fact (b)'s five sanitized items, each present and honestly mapped: the
/// two ganja cannot fill say so instead of inventing a value.
#[test]
fn the_approval_modal_renders_the_five_items_the_statement_and_the_cause() {
    let screen = rendered_approval(&approval(), AREA);

    assert!(screen.contains("reply address: none exists on this transport"), "got:\n{screen}");
    assert!(screen.contains("from (claimed): w1@ganja-team"), "got:\n{screen}");
    assert!(
        screen.contains("verified pid: same-user socket, no process identity"),
        "got:\n{screen}"
    );
    assert!(screen.contains("preview: a finding worth a look"), "got:\n{screen}");
    assert!(screen.contains("the full body of the finding"), "got:\n{screen}");
    assert!(screen.contains("This message has not been delivered to the model."), "got:\n{screen}");
    assert!(screen.contains("missing sender mode"), "got:\n{screen}");
    assert!(screen.contains("expires in 5m"), "got:\n{screen}");
    assert!(screen.contains("[y] deliver"), "got:\n{screen}");
    assert!(screen.contains("[n]/[Esc] deny (drop)"), "got:\n{screen}");
}

/// A sender that wrote no summary still gets a one-line preview: the
/// body's own first line stands in.
#[test]
fn a_missing_summary_falls_back_to_the_bodys_first_line() {
    let dialog = HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@team".to_owned(),
        HoldCause::ModeMismatch,
        None,
        "first line of the body\nsecond line".to_owned(),
        Some(300_000),
    );

    let screen = rendered_approval(&dialog, AREA);

    assert!(screen.contains("preview: first line of the body"), "got:\n{screen}");
    assert!(screen.contains("mode mismatch"), "got:\n{screen}");
}

/// A body longer than the modal is flagged as cut with the answers still
/// on screen — the same consent rule the permission dialog holds.
#[test]
fn a_body_too_long_to_draw_says_so_and_still_offers_the_keys() {
    let dialog = HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@team".to_owned(),
        HoldCause::NoModeAsserted,
        None,
        "a line of body text that goes on\n".repeat(8),
        Some(300_000),
    );

    let screen = rendered_approval(&dialog, Rect::new(0, 0, 60, 14));

    assert!(screen.contains("not shown"), "got:\n{screen}");
    assert!(screen.contains("[y] deliver"), "got:\n{screen}");
    assert!(
        screen.contains("This message has not been delivered to the model."),
        "the statement outranks the body:\n{screen}"
    );
}

/// The countdown row belongs to the parity causes alone; a modal built
/// without a deadline draws none. The production raiser never builds one
/// for an explicit hold, and this pins what the modal would honestly do.
#[test]
fn a_modal_without_a_deadline_draws_no_countdown() {
    let dialog = HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@team".to_owned(),
        HoldCause::NoModeAsserted,
        None,
        "body".to_owned(),
        None,
    );

    assert!(!rendered_approval(&dialog, AREA).contains("expires in"), "no deadline, no countdown");
}

/// The modal renders a foreign sender's own prose; a control sequence in
/// it must never reach the terminal. The stripping is ratatui-inherited,
/// pinned here the way the permission dialog pins it.
#[test]
fn an_escape_sequence_in_a_held_body_never_reaches_the_buffer() {
    let dialog = HeldApproval::new(
        HeldId::from("held_1".to_owned()),
        "w1@team".to_owned(),
        HoldCause::NoModeAsserted,
        Some("\u{1b}[2Jlooks legit".to_owned()),
        "\u{1b}[31mrm -rf /\u{7} the rest".to_owned(),
        Some(300_000),
    );

    let screen = rendered_approval(&dialog, AREA);

    let leaked: Vec<char> =
        screen.chars().filter(|character| *character != '\n' && character.is_control()).collect();
    assert!(leaked.is_empty(), "control characters reached the buffer: {leaked:?}\n{screen}");
    assert!(screen.contains("rm -rf /"), "got:\n{screen}");
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    assert!(rendered_approval(&approval(), Rect::new(0, 0, 0, 0)).is_empty());
    assert!(rendered_list(&HeldList::new(Vec::new()), Rect::new(0, 0, 0, 0)).is_empty());
}

#[test]
fn every_held_entry_lists_with_its_sender_cause_age_and_preview() {
    let dialog = HeldList::new(vec![
        row("held_1", "w1@team", HoldCause::NoModeAsserted),
        row("held_2", "scribbler@nowhere", HoldCause::Explicit { source: PolicySource::Global }),
    ]);

    let screen = rendered_list(&dialog, AREA);

    assert!(screen.contains("w1@team"), "got:\n{screen}");
    assert!(screen.contains("no sender mode"), "got:\n{screen}");
    assert!(screen.contains("scribbler@nowhere"), "got:\n{screen}");
    assert!(screen.contains("explicit"), "got:\n{screen}");
    assert!(screen.contains("2m"), "got:\n{screen}");
    assert!(screen.contains("what it said"), "got:\n{screen}");
}

#[test]
fn an_empty_buffer_says_so_instead_of_drawing_an_empty_box() {
    let screen = rendered_list(&HeldList::new(Vec::new()), AREA);

    assert!(screen.contains("nothing is held for review"), "got:\n{screen}");
}

/// Enter on a row opens Release and Deny, and answers with the chosen
/// one; Release leads, because delivering is the review's yes.
#[test]
fn enter_on_a_row_offers_release_and_deny() {
    let mut dialog = HeldList::new(vec![row("held_1", "w1@team", HoldCause::NoModeAsserted)]);

    assert!(dialog.advance());
    assert!(dialog.is_choosing_action());
    assert!(matches!(
        dialog.chosen(),
        Some((id, Action::Release)) if id.as_str() == "held_1"
    ));

    dialog.move_selection(1);
    assert!(matches!(dialog.chosen(), Some((_, Action::Deny))));

    let screen = rendered_list(&dialog, AREA);
    assert!(screen.contains("Release (deliver it)"), "got:\n{screen}");
    assert!(screen.contains("Deny (drop it)"), "got:\n{screen}");
}

#[test]
fn enter_on_an_empty_buffer_does_nothing() {
    let mut dialog = HeldList::new(Vec::new());

    assert!(!dialog.advance());
    assert!(!dialog.is_choosing_action());
    assert!(dialog.chosen().is_none());
}

/// A poll refresh keeps the cursor in place — and a buffer that shrank
/// under it (a settlement retired a row) reclamps instead of pointing
/// past the end.
#[test]
fn refreshing_keeps_the_cursor_and_survives_a_shrink() {
    let mut dialog = HeldList::new(vec![
        row("held_1", "w1@team", HoldCause::NoModeAsserted),
        row("held_2", "w2@team", HoldCause::NoModeAsserted),
    ]);
    dialog.move_selection(1);

    dialog.refresh(vec![row("held_1", "w1@team", HoldCause::NoModeAsserted)]);
    assert_eq!(dialog.selected().map(|row| row.from.as_str()), Some("w1@team"));

    dialog.refresh(Vec::new());
    assert!(dialog.selected().is_none());
    assert!(!dialog.is_choosing_action(), "an emptied buffer leaves no action step to stand on");
}

/// The action step follows the cursor's row, so a settle lands on the
/// entry the person was looking at.
#[test]
fn the_action_step_names_the_row_it_is_about() {
    let mut dialog = HeldList::new(vec![
        row("held_1", "w1@team", HoldCause::NoModeAsserted),
        row("held_2", "w2@team", HoldCause::ModeMismatch),
    ]);
    dialog.move_selection(1);
    dialog.advance();

    assert!(matches!(
        dialog.chosen(),
        Some((id, Action::Release)) if id.as_str() == "held_2"
    ));

    dialog.back_to_rows();
    assert!(!dialog.is_choosing_action());
}

/// The three-surface enum renders one vocabulary: the sentences carry
/// fact (b)'s cause names, and the labels are their short forms.
#[test]
fn cause_words_cover_every_cause() {
    for (cause, label, fragment) in [
        (HoldCause::Explicit { source: PolicySource::Global }, "explicit", "explicit settings"),
        (
            HoldCause::Explicit { source: PolicySource::Project },
            "explicit",
            "repository tightening",
        ),
        (HoldCause::ModeMismatch, "mode mismatch", "mode mismatch"),
        (HoldCause::NoModeAsserted, "no sender mode", "missing sender mode"),
        (HoldCause::ModeUnknown, "mode unknown", "startup mode uncertainty"),
    ] {
        assert_eq!(cause_label(cause), label);
        assert!(
            cause_sentence(cause).contains(fragment),
            "{cause:?} should name {fragment}: {}",
            cause_sentence(cause)
        );
    }
}

/// Minutes while minutes remain, seconds inside the last one, ceiling
/// throughout — a five-minute deadline reads "5m" the moment it is armed.
#[test]
fn the_coarse_countdown_moves_in_minutes_then_seconds() {
    assert_eq!(coarse(Duration::from_millis(300_000)), "5m");
    assert_eq!(coarse(Duration::from_millis(299_900)), "5m");
    assert_eq!(coarse(Duration::from_secs(61)), "2m");
    assert_eq!(coarse(Duration::from_secs(60)), "1m");
    assert_eq!(coarse(Duration::from_secs(59)), "59s");
    assert_eq!(coarse(Duration::from_millis(500)), "1s");
    assert_eq!(coarse(Duration::ZERO), "0s");
}

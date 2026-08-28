use ganja_protocol::{MessageId, RevertScope};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Checkpoint, Rewind};
use crate::theme::Theme;

fn checkpoint(id: &str, title: &str, files: usize) -> Checkpoint {
    Checkpoint { message_id: MessageId::from(id.to_owned()), title: title.to_owned(), files }
}

/// Two checkpoints, newest first, one of which changed nothing.
fn rewind() -> Rewind {
    Rewind::new(vec![
        checkpoint("msg_3", "rename the thing", 2),
        checkpoint("msg_1", "what does this crate do", 0),
    ])
}

fn rendered(rewind: &Rewind, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    rewind.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_list_shows_every_checkpoint_and_what_its_turns_changed() {
    let screen = rendered(&rewind(), Rect::new(0, 0, 80, 20));

    assert!(screen.contains("(Current)"), "got:\n{screen}");
    assert!(screen.contains("rename the thing"), "got:\n{screen}");
    assert!(screen.contains("2 files changed"), "got:\n{screen}");
    assert!(screen.contains("what does this crate do"), "got:\n{screen}");
    assert!(
        screen.contains("\u{26a0} No code restore"),
        "a span with no patches says so:\n{screen}"
    );
}

/// One file is one file, not "1 files".
#[test]
fn a_single_changed_file_is_counted_in_the_singular() {
    let rewind = Rewind::new(vec![checkpoint("msg_1", "touch one thing", 1)]);

    assert!(
        rendered(&rewind, Rect::new(0, 0, 80, 20)).contains("1 file changed"),
        "one file is not plural"
    );
}

/// The cursor opens on the row that does nothing, and the checkpoints are
/// under it.
#[test]
fn the_picker_opens_on_current_and_moves_into_the_checkpoints() {
    let mut rewind = rewind();
    assert!(rewind.selected().is_none(), "(Current) is not a checkpoint");

    rewind.move_selection(1);
    assert_eq!(rewind.selected().map(|point| point.message_id.as_str()), Some("msg_3"));

    // Clamped at both ends rather than wrapping.
    rewind.move_selection(9);
    assert_eq!(rewind.selected().map(|point| point.message_id.as_str()), Some("msg_1"));
    rewind.move_selection(-9);
    assert!(rewind.selected().is_none());
}

/// Enter on `(Current)` has nothing to ask about: the caller reads the
/// `false` as "close, having done nothing".
#[test]
fn enter_on_current_advances_to_nothing() {
    let mut rewind = rewind();

    assert!(!rewind.advance(), "there is nothing to restore");
    assert!(!rewind.is_choosing_scope(), "and no question to ask");
}

#[test]
fn enter_on_a_checkpoint_opens_the_scope_choice_and_answers_with_it() {
    let mut rewind = rewind();
    rewind.move_selection(1);

    assert!(rewind.advance());
    assert!(rewind.is_choosing_scope());
    assert_eq!(
        rewind.chosen(),
        Some((MessageId::from("msg_3".to_owned()), RevertScope::Both)),
        "the first option is the whole checkpoint"
    );

    rewind.move_selection(1);
    assert_eq!(
        rewind.chosen(),
        Some((MessageId::from("msg_3".to_owned()), RevertScope::Conversation))
    );

    rewind.move_selection(1);
    assert_eq!(rewind.chosen(), Some((MessageId::from("msg_3".to_owned()), RevertScope::Files)));

    // The scope list is clamped too.
    rewind.move_selection(9);
    assert_eq!(rewind.chosen(), Some((MessageId::from("msg_3".to_owned()), RevertScope::Files)));
}

/// The second step says which checkpoint it is about, in the screenshot's
/// own words, and offers all three answers.
#[test]
fn the_scope_step_names_the_checkpoint_and_the_three_answers() {
    let mut rewind = rewind();
    rewind.move_selection(1);
    rewind.advance();

    let screen = rendered(&rewind, Rect::new(0, 0, 80, 20));

    assert!(screen.contains("rename the thing"), "got:\n{screen}");
    assert!(screen.contains("Restore the code and/or conversation"), "got:\n{screen}");
    assert!(screen.contains("Code and conversation"), "got:\n{screen}");
    assert!(screen.contains("Conversation only"), "got:\n{screen}");
    assert!(screen.contains("Code only"), "got:\n{screen}");
    assert!(screen.contains("[Esc] cancel"), "got:\n{screen}");
}

/// Nothing under the cursor means nothing to choose: the scope step is
/// unreachable and an Enter there answers with no rewind.
#[test]
fn a_session_with_no_checkpoints_still_opens_and_chooses_nothing() {
    let mut rewind = Rewind::new(Vec::new());

    assert!(!rewind.advance());
    assert_eq!(rewind.chosen(), None);
    assert!(
        rendered(&rewind, Rect::new(0, 0, 80, 20)).contains("(Current)"),
        "the row for where the session stands is always there"
    );
}

#[test]
fn a_row_too_wide_for_the_column_is_cut_rather_than_wrapped() {
    let rewind = Rewind::new(vec![checkpoint("msg_1", &"wide ".repeat(40), 3)]);

    let screen = rendered(&rewind, Rect::new(0, 0, 60, 20));

    for line in screen.lines() {
        assert!(line.chars().count() <= 60, "a row must not overflow the dialog: {line:?}");
    }
    assert!(screen.contains("3 files changed"), "got:\n{screen}");
}

/// More checkpoints than rows: the list has to move under the selection,
/// or the user cannot reach what they are selecting.
#[test]
fn a_selection_below_the_fold_scrolls_the_list_to_it() {
    let checkpoints = (0..40)
        .map(|index| checkpoint(&format!("msg_{index:02}"), &format!("prompt {index:02}"), 1))
        .collect();
    let mut rewind = Rewind::new(checkpoints);
    let area = Rect::new(0, 0, 80, 20);

    let top = rendered(&rewind, area);
    assert!(top.contains("prompt 00"), "got:\n{top}");
    assert!(!top.contains("prompt 39"), "got:\n{top}");

    rewind.move_selection(40);
    let bottom = rendered(&rewind, area);

    assert!(bottom.contains("> prompt 39"), "the selection must be on screen:\n{bottom}");
    assert!(!bottom.contains("prompt 00"), "the list should have scrolled:\n{bottom}");
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&rewind(), Rect::new(0, 0, 0, 0));

    assert!(screen.is_empty(), "a zero area has no cell to hold: {screen}");
}

/// The same protection every other dialog rendering text somebody else
/// wrote is pinned for: a prompt is the user's own bytes, and a control
/// sequence in one must not repaint the screen they are choosing from.
#[test]
fn an_escape_sequence_in_a_prompt_never_reaches_the_buffer() {
    let rewind = Rewind::new(vec![checkpoint("msg_1", "\u{1b}[2Jrewind to me\u{7}", 1)]);

    let screen = rendered(&rewind, Rect::new(0, 0, 80, 20));
    let leaked: Vec<char> =
        screen.chars().filter(|character| *character != '\n' && character.is_control()).collect();

    assert!(leaked.is_empty(), "control characters reached the buffer: {leaked:?}\n{screen}");
    assert!(screen.contains("rewind to me"), "got:\n{screen}");
}

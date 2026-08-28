use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{ListDialog, Row};
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 60, height: 14 };

fn row(value: &str, active: bool) -> Row {
    Row {
        value: value.to_owned(),
        label: value.to_owned(),
        detail: Some(format!("what {value} is for")),
        active,
    }
}

fn dialog() -> ListDialog {
    ListDialog::new(" models ", vec![row("first", false), row("second", true), row("third", false)])
}

fn rendered(dialog: &ListDialog) -> String {
    let mut buffer = Buffer::empty(AREA);
    dialog.render(AREA, &mut buffer, &Theme::default());

    (0..AREA.height)
        .map(|line| {
            (0..AREA.width).map(|column| buffer[(column, line)].symbol()).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_dialog_opens_on_the_row_the_session_is_already_on() {
    assert_eq!(dialog().selected(), Some("second"));
}

#[test]
fn a_list_with_nothing_active_opens_on_its_first_row() {
    let dialog = ListDialog::new(" models ", vec![row("first", false), row("second", false)]);

    assert_eq!(dialog.selected(), Some("first"));
}

#[test]
fn the_cursor_clamps_at_both_ends() {
    let mut dialog = dialog();

    dialog.move_selection(-9);
    assert_eq!(dialog.selected(), Some("first"));

    dialog.move_selection(99);
    assert_eq!(dialog.selected(), Some("third"));
}

#[test]
fn the_active_row_is_marked_so_the_list_says_where_the_session_stands() {
    let screen = rendered(&dialog());
    let marked: Vec<&str> =
        screen.lines().filter(|line| line.contains('*')).map(str::trim).collect();

    assert_eq!(marked.len(), 1, "exactly one row is active:\n{screen}");
    assert!(marked[0].contains("second"), "{marked:?}");
}

#[test]
fn an_empty_list_says_so_instead_of_drawing_an_empty_box() {
    let dialog = ListDialog::new(" agents ", Vec::new());

    assert!(dialog.is_empty());
    assert_eq!(dialog.selected(), None);
    assert!(rendered(&dialog).contains("nothing to choose from"), "{}", rendered(&dialog));
}

#[test]
fn a_detail_too_long_for_the_box_is_cut_rather_than_wrapped() {
    let dialog = ListDialog::new(
        " agents ",
        vec![Row {
            value: "explore".to_owned(),
            label: "explore".to_owned(),
            detail: Some("d".repeat(500)),
            active: false,
        }],
    );

    for line in rendered(&dialog).lines() {
        assert!(line.chars().count() <= usize::from(AREA.width));
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

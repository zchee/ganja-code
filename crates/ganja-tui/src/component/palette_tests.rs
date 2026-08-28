use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Palette, Row, SUGGESTED};
use crate::command::Action;
use crate::keybind::Keybinds;
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 64, height: 20 };

fn palette() -> Palette {
    Palette::new(Keybinds::defaults())
}

fn typing(palette: &mut Palette, text: &str) {
    for character in text.chars() {
        palette.push(character);
    }
}

fn rendered(palette: &Palette) -> String {
    let mut buffer = Buffer::empty(AREA);
    palette.render(AREA, &mut buffer, &Theme::default());

    (0..AREA.height)
        .map(|row| (0..AREA.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_fresh_palette_starts_on_a_command_rather_than_a_heading() {
    assert!(palette().selected().is_some());
}

#[test]
fn the_suggested_block_is_pinned_while_nothing_is_typed_and_gone_once_something_is() {
    let mut palette = palette();
    assert!(
        palette.rows.contains(&Row::Heading(SUGGESTED)),
        "an empty filter should pin the suggested commands"
    );

    typing(&mut palette, "e");
    assert!(
        !palette.rows.contains(&Row::Heading(SUGGESTED)),
        "a fragment should drop the pinned block"
    );
}

#[test]
fn typing_narrows_the_list_and_backspace_widens_it_again() {
    let mut palette = palette();
    let all = palette.rows.len();

    typing(&mut palette, "themes");
    assert!(palette.rows.len() < all, "the list should have narrowed");
    assert_eq!(palette.selected(), Some(Action::Themes));

    for _ in 0.."themes".len() {
        palette.backspace();
    }
    assert_eq!(palette.rows.len(), all, "backspacing should widen it back");
}

#[test]
fn moving_the_cursor_steps_over_the_headings() {
    let mut palette = palette();
    let mut seen = Vec::new();

    for _ in 0..12 {
        seen.push(palette.selected());
        palette.move_selection(1);
    }

    assert!(
        seen.iter().all(Option::is_some),
        "the cursor should never rest on a heading: {seen:?}"
    );
}

#[test]
fn the_cursor_clamps_at_both_ends() {
    let mut palette = palette();
    palette.move_selection(-5);
    let first = palette.selected();

    palette.move_selection(500);
    let last = palette.selected();
    palette.move_selection(500);

    assert_eq!(palette.selected(), last, "past the end should stay put");
    assert_ne!(first, last, "the list should have more than one command");
}

#[test]
fn a_fragment_nothing_matches_says_so_instead_of_drawing_an_empty_box() {
    let mut palette = palette();
    typing(&mut palette, "zzzz");

    assert_eq!(palette.selected(), None);
    assert!(rendered(&palette).contains("no commands match"), "{}", rendered(&palette));
}

#[test]
fn a_reopened_palette_keeps_the_fragment_it_was_closed_on() {
    let mut palette = palette();
    typing(&mut palette, "the");

    let reopened = Palette::reopened(Keybinds::defaults(), palette.filter().to_owned());

    assert_eq!(reopened.filter(), "the");
    assert_eq!(reopened.selected(), Some(Action::Themes));
}

#[test]
fn a_command_with_a_binding_shows_it_and_one_without_shows_nothing() {
    let screen = rendered(&palette());

    assert!(screen.contains("ctrl+s"), "/sessions has a key:\n{screen}");
    let models = screen.lines().find(|line| line.contains("/models")).unwrap_or_default();
    assert!(!models.contains("ctrl"), "/models has no key of its own: {models}");
}

#[test]
fn the_filter_line_shows_a_placeholder_until_something_is_typed() {
    let mut palette = palette();
    assert!(rendered(&palette).contains("search commands"));

    typing(&mut palette, "mo");
    let screen = rendered(&palette);
    assert!(!screen.contains("search commands"), "{screen}");
    assert!(screen.contains("mo"), "{screen}");
}

#[test]
fn a_one_column_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (2, 3), (5, 2), (64, 3)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        palette().render(area, &mut buffer, &Theme::default());
    }
}

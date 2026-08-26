use ratatui::{buffer::Buffer, layout::Rect};

use super::ThemeList;
use crate::theme::{Theme, Themes};

const AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 40,
    height: 16,
};

fn list() -> ThemeList {
    ThemeList::new(Themes::builtin().names(), "opencode")
}

fn rendered(list: &ThemeList, area: Rect, theme: &Theme) -> String {
    let mut buffer = Buffer::empty(area);
    list.render(area, &mut buffer, theme);

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_list_shows_every_theme_this_run_has() {
    let screen = rendered(&list(), AREA, &Theme::default());

    for name in Themes::builtin().names() {
        assert!(screen.contains(&name), "{name} is missing from:\n{screen}");
    }
}

/// Opening the dialog must not preview anything: the cursor starts where
/// the user already is.
#[test]
fn the_cursor_opens_on_the_active_theme() {
    assert_eq!(list().selected(), Some("opencode"));
    assert_eq!(
        ThemeList::new(Themes::builtin().names(), "gruvbox").selected(),
        Some("gruvbox")
    );
}

#[test]
fn an_active_theme_that_is_not_in_the_list_starts_at_the_top() {
    let list = ThemeList::new(Themes::builtin().names(), "gone");

    assert_eq!(list.selected(), Some("aura"));
    assert_eq!(
        list.initial(),
        "gone",
        "cancelling still puts back what was"
    );
}

#[test]
fn the_selection_moves_within_the_list_and_clamps_at_both_ends() {
    let mut list = list();

    list.move_selection(1);
    assert_eq!(list.selected(), Some("terminal"));

    list.move_selection(99);
    assert_eq!(list.selected(), Some("tokyonight"));

    list.move_selection(-99);
    assert_eq!(list.selected(), Some("aura"));
}

#[test]
fn the_marker_follows_the_selection() {
    let mut list = list();
    let first = rendered(&list, AREA, &Theme::default());
    list.move_selection(-1);
    let second = rendered(&list, AREA, &Theme::default());

    assert!(first.contains("> opencode"), "got:\n{first}");
    assert!(second.contains("> gruvbox"), "got:\n{second}");
    assert!(
        !second.contains("> opencode"),
        "only one row is selected:\n{second}"
    );
}

/// More themes than rows: a user with a directory of their own has to be
/// able to reach the bottom of the list.
#[test]
fn a_selection_below_the_fold_scrolls_the_list_to_it() {
    let names: Vec<String> = (0..40).map(|index| format!("theme{index:02}")).collect();
    let mut list = ThemeList::new(names, "theme00");
    let area = Rect::new(0, 0, 40, 12);

    let top = rendered(&list, area, &Theme::default());
    assert!(top.contains("theme00"), "got:\n{top}");
    assert!(!top.contains("theme39"), "got:\n{top}");

    list.move_selection(39);
    let bottom = rendered(&list, area, &Theme::default());

    assert!(bottom.contains("> theme39"), "got:\n{bottom}");
    assert!(!bottom.contains("theme00"), "got:\n{bottom}");
}

#[test]
fn a_name_too_wide_for_the_dialog_is_cut_rather_than_wrapped() {
    let list = ThemeList::new(vec!["a-".repeat(60)], "unused");

    let screen = rendered(&list, Rect::new(0, 0, 30, 10), &Theme::default());

    for line in screen.lines() {
        assert!(
            line.chars().count() <= 30,
            "a row must not overflow the dialog: {line:?}"
        );
    }
}

#[test]
fn an_empty_list_has_nothing_selected_and_does_not_panic() {
    let list = ThemeList::new(Vec::new(), "opencode");

    assert_eq!(list.selected(), None);
    rendered(&list, AREA, &Theme::default());
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&list(), Rect::new(0, 0, 0, 0), &Theme::default());

    assert!(
        screen.is_empty(),
        "a zero area has no cell to hold: {screen}"
    );
}

/// The dialog is drawn with the theme it is previewing, so the same list
/// under two themes must not come out looking the same.
#[test]
fn the_dialog_is_drawn_in_the_theme_it_is_previewing() {
    let mut themes = Themes::builtin();
    let list = list();
    let area = Rect::new(0, 0, 40, 16);

    let mut first = Buffer::empty(area);
    list.render(
        area,
        &mut first,
        &themes.select("aura").expect("aura is builtin"),
    );

    let mut second = Buffer::empty(area);
    list.render(
        area,
        &mut second,
        &themes.select("gruvbox").expect("gruvbox is builtin"),
    );

    assert_ne!(first, second, "the two themes rendered identically");
}

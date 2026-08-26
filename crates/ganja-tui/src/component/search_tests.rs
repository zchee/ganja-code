use ratatui::{buffer::Buffer, layout::Rect};

use super::HistorySearch;
use crate::{history::Recalled, theme::Theme};

const AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 76,
    height: 24,
};

const HOUR: u64 = 60 * 60 * 1_000;

/// Every fixture ages against this moment, so a row's age is exactly
/// `NOW - at` and never depends on the wall clock the test happens to
/// run under.
const NOW: u64 = 4 * HOUR;

fn recalled(input: &str, at: u64) -> Recalled {
    Recalled {
        prompt: crate::history::PromptInfo::text(input),
        at,
    }
}

/// Three entries, already newest-first — the shape `History::entries`
/// hands over — dated one, two and three hours before `NOW`.
fn entries() -> Vec<Recalled> {
    vec![
        recalled("commit the fix", NOW - HOUR),
        recalled("git status", NOW - 2 * HOUR),
        recalled("what does this crate do", NOW - 3 * HOUR),
    ]
}

fn search(entries: Vec<Recalled>) -> HistorySearch {
    HistorySearch::new(entries, NOW, "draft in progress", (0, 5))
}

fn typing(search: &mut HistorySearch, text: &str) {
    for character in text.chars() {
        search.push(character);
    }
}

fn rendered(search: &HistorySearch, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    search.render(area, &mut buffer, &Theme::default());

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
fn an_empty_query_lists_everything_newest_first() {
    let search = search(entries());

    assert_eq!(
        search.selected().map(|p| p.input.as_str()),
        Some("commit the fix")
    );
    let screen = rendered(&search, AREA);
    assert!(screen.contains("commit the fix"), "got:\n{screen}");
    assert!(screen.contains("git status"), "got:\n{screen}");
}

/// A fuzzy fragment narrows the list, and what survives keeps its
/// original newest-first order rather than being re-sorted by score.
#[test]
fn fuzzy_narrowing_keeps_the_newest_first_order() {
    let mut search = search(entries());

    typing(&mut search, "ommi");

    assert_eq!(
        search.selected().map(|p| p.input.as_str()),
        Some("commit the fix")
    );
    let screen = rendered(&search, AREA);
    assert!(screen.contains("commit the fix"), "got:\n{screen}");
    assert!(!screen.contains("git status"), "got:\n{screen}");
    assert!(
        !screen.contains("what does this crate do"),
        "got:\n{screen}"
    );
}

/// Each row carries a relative age, the sessions picker's own bucketing.
#[test]
fn each_row_carries_a_relative_age() {
    let screen = rendered(&search(entries()), AREA);

    assert!(screen.contains("1h ago"), "got:\n{screen}");
    assert!(screen.contains("2h ago"), "got:\n{screen}");
    assert!(screen.contains("3h ago"), "got:\n{screen}");
}

/// The preview pane renders lines the match row never shows: the list
/// row is one clipped line of title, so a second and third line proving
/// up on screen can only have come from the preview.
#[test]
fn the_preview_shows_the_selected_entry_whole_when_it_fits() {
    let search = search(vec![recalled(
        "first line\nsecond line\nthird line",
        NOW - HOUR,
    )]);

    let screen = rendered(&search, AREA);
    assert!(screen.contains("first line"), "got:\n{screen}");
    assert!(screen.contains("second line"), "got:\n{screen}");
    assert!(screen.contains("third line"), "got:\n{screen}");
}

/// A preview too tall for its pane is truncated with a `+N lines` marker
/// instead of overflowing.
#[test]
fn a_tall_preview_truncates_with_a_line_count() {
    let long = (0..40)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let search = search(vec![recalled(&long, NOW - HOUR)]);

    let screen = rendered(&search, AREA);
    assert!(
        screen.contains("line 0"),
        "the top of the entry should still show:\n{screen}"
    );
    assert!(
        screen.contains(" lines"),
        "a truncated preview should say how much more there is:\n{screen}"
    );
    assert!(
        !screen.contains("line 39"),
        "the tail should not fit alongside the marker:\n{screen}"
    );
}

/// Moving the cursor changes what the preview shows.
#[test]
fn moving_the_cursor_changes_the_preview() {
    let mut search = search(entries());
    search.move_selection(1);

    assert_eq!(
        search.selected().map(|p| p.input.as_str()),
        Some("git status")
    );
}

/// An empty store renders an honest empty state rather than a blank list.
#[test]
fn an_empty_store_renders_an_honest_empty_state() {
    let search = search(Vec::new());

    assert!(search.selected().is_none());
    let screen = rendered(&search, AREA);
    assert!(screen.contains("no prompts remembered"), "got:\n{screen}");
}

/// A query nothing matches says so instead of drawing an empty list.
#[test]
fn a_query_nothing_matches_says_so() {
    let mut search = search(entries());
    typing(&mut search, "zzzzzzzz");

    assert!(search.selected().is_none());
    assert!(rendered(&search, AREA).contains("no matches"));
}

/// Backspacing widens the list back out.
#[test]
fn backspace_widens_the_list_back_out() {
    let mut search = search(entries());
    typing(&mut search, "commit");
    for _ in 0.."commit".len() {
        search.backspace();
    }

    let screen = rendered(&search, AREA);
    assert!(screen.contains("git status"), "got:\n{screen}");
}

/// The dialog remembers the exact buffer it opened over, for an Esc to
/// hand back to the composer byte for byte.
#[test]
fn the_dialog_remembers_the_buffer_it_opened_over() {
    let search = search(entries());

    assert_eq!(search.origin_text(), "draft in progress");
    assert_eq!(search.origin_cursor(), (0, 5));
}

#[test]
fn a_one_column_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (2, 3), (5, 2), (76, 3)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        search(entries()).render(area, &mut buffer, &Theme::default());
    }
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&search(entries()), Rect::new(0, 0, 0, 0));

    assert!(
        screen.is_empty(),
        "a zero area has no cell to hold: {screen}"
    );
}

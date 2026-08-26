use ratatui::{buffer::Buffer, layout::Rect};

use super::{Dropdown, triggered};
use crate::{
    command::{Choice, Completion, EngineCommand, Slot},
    theme::Theme,
};

/// A menu over the UI commands alone, which is what a session running
/// without a command registry offers.
fn menu(text: &str) -> Dropdown {
    Dropdown::new(text, Vec::new())
}

/// **D519.** A values menu is the same box titled after its slot, and it
/// says so in its own words when nothing matches.
#[test]
fn a_values_menu_is_titled_after_its_slot() {
    let candidates = |names: &[&str]| {
        names
            .iter()
            .map(|name| Completion {
                text: (*name).to_owned(),
                detail: "a surface".to_owned(),
            })
            .collect::<Vec<_>>()
    };
    let render = |slot: &Slot| {
        let dropdown = Dropdown::values(slot);
        let area = Rect::new(0, 0, 40, 8);
        let mut buffer = Buffer::empty(area);
        dropdown.render(Rect::new(0, 6, 40, 2), &mut buffer, &Theme::default());
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let full = render(&Slot {
        title: " backends ",
        start: 0,
        partial: "g".to_owned(),
        candidates: candidates(&["ganja", "grok", "claude"]),
    });
    assert!(full.contains(" backends "), "{full}");
    assert!(
        full.contains("> ganja") && full.contains("grok") && !full.contains("claude"),
        "{full}"
    );

    let empty = render(&Slot {
        title: " backends ",
        start: 0,
        partial: "zzz".to_owned(),
        candidates: candidates(&["ganja"]),
    });
    assert!(empty.contains("nothing matches"), "{empty}");
}

/// The engine roster a configured session carries.
fn engine() -> Vec<EngineCommand> {
    vec![EngineCommand {
        name: "init".to_owned(),
        description: Some("guided AGENTS.md setup".to_owned()),
        hint: None,
    }]
}

fn rendered(dropdown: &Dropdown, anchor: Rect, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dropdown.render(anchor, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The trigger is the whole difference between a command menu and a menu
/// that pops up over a path.
#[test]
fn the_menu_opens_only_for_a_slash_at_the_very_start_of_the_buffer() {
    let cases = [
        ("/", (0, 1), true),
        ("/mo", (0, 3), true),
        ("/models", (0, 2), true),
        ("", (0, 0), false),
        ("what about /tmp", (0, 15), false),
        (" /models", (0, 8), false),
        ("hello", (0, 5), false),
        // A space typed after the command: arguments now, not a choice.
        ("/models gpt", (0, 11), false),
        // The cursor moved back before the space, so the span in front of
        // it is still whitespace-free.
        ("/models gpt", (0, 4), true),
        // A second line is never the first token.
        ("/models\nmore", (1, 2), false),
    ];

    for (text, cursor, expected) in cases {
        assert_eq!(
            triggered(text, cursor),
            expected,
            "{text:?} with the cursor at {cursor:?}"
        );
    }
}

#[test]
fn a_bare_slash_lists_every_command_from_both_populations() {
    let dropdown = Dropdown::new("/", engine());

    assert_eq!(
        dropdown.matched.len(),
        crate::command::COMMANDS.len() + engine().len()
    );
}

/// With nothing typed there is no ranking to show, so the menu reads as a
/// directory instead of as a guess.
#[test]
fn a_bare_slash_orders_the_rows_by_name() {
    let dropdown = Dropdown::new("/", engine());
    let names: Vec<String> = dropdown.matched.iter().map(Choice::slash).collect();
    let mut sorted = names.clone();
    sorted.sort();

    assert_eq!(names, sorted);
}

#[test]
fn typing_narrows_the_menu_and_puts_the_cursor_back_on_top() {
    let mut dropdown = menu("/");
    dropdown.move_selection(3);

    dropdown.refresh("/agent");

    assert_eq!(dropdown.selected, 0);
    assert_eq!(
        dropdown.selected().map(|choice| choice.slash()),
        Some("/agents".to_owned())
    );
}

/// The one thing the dropdown matches that the palette does not.
#[test]
fn a_fragment_that_only_appears_in_a_description_still_finds_its_command() {
    let dropdown = menu("/repaint");

    assert_eq!(
        dropdown.selected().map(|choice| choice.slash()),
        Some("/themes".to_owned())
    );
}

/// An engine command is a row like any other until it is chosen, which is
/// the only place the two populations part ways.
#[test]
fn an_engine_command_is_listed_beside_the_ui_ones() {
    let dropdown = Dropdown::new("/init", engine());

    assert_eq!(
        dropdown.selected(),
        Some(Choice::Engine(engine().remove(0))),
        "got {:?}",
        dropdown.matched
    );

    let screen = rendered(&dropdown, Rect::new(0, 10, 60, 5), Rect::new(0, 0, 60, 16));
    assert!(screen.contains("/init"), "{screen}");
    assert!(screen.contains("guided AGENTS.md setup"), "{screen}");
}

#[test]
fn a_fragment_nothing_matches_says_so_instead_of_drawing_an_empty_box() {
    let dropdown = menu("/zzzz");
    assert!(dropdown.is_empty());
    assert_eq!(dropdown.selected(), None);

    let screen = rendered(&dropdown, Rect::new(0, 10, 40, 5), Rect::new(0, 0, 40, 16));
    assert!(screen.contains("no matching commands"), "{screen}");
}

#[test]
fn the_menu_draws_above_the_editor_it_is_anchored_to() {
    let anchor = Rect::new(0, 10, 40, 5);
    let area = Rect::new(0, 0, 40, 16);
    let screen = rendered(&menu("/themes"), anchor, area);

    let row = screen
        .lines()
        .position(|line| line.contains("/themes"))
        .expect("the command should be on screen");
    assert!(
        row < usize::from(anchor.y),
        "the menu should sit above row {}, found it at {row}:\n{screen}",
        anchor.y
    );
}

/// Nothing above the editor to draw into, so nothing is drawn — rather
/// than a menu overlapping the prompt it belongs to.
#[test]
fn an_editor_with_no_room_above_it_gets_no_menu() {
    let area = Rect::new(0, 0, 40, 8);
    let screen = rendered(&menu("/"), Rect::new(0, 0, 40, 5), area);

    assert!(
        screen.trim().is_empty(),
        "nothing should have been drawn:\n{screen}"
    );
}

#[test]
fn a_menu_taller_than_the_room_above_it_is_clipped_not_overdrawn() {
    let anchor = Rect::new(0, 4, 40, 5);
    let area = Rect::new(0, 0, 40, 12);
    let screen = rendered(&menu("/"), anchor, area);

    for (row, line) in screen.lines().enumerate() {
        if row >= usize::from(anchor.y) {
            assert!(line.trim().is_empty(), "row {row} spilled into the editor");
        }
    }
}

#[test]
fn the_cursor_clamps_at_both_ends() {
    let mut dropdown = menu("/");
    dropdown.move_selection(-9);
    assert_eq!(dropdown.selected, 0);

    dropdown.move_selection(999);
    assert_eq!(dropdown.selected, dropdown.matched.len() - 1);
}

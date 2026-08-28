use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::Help;
use crate::command::COMMANDS;
use crate::keybind::Keybinds;
use crate::theme::Theme;

/// Tall enough for every row the card holds at once, which is what makes
/// this the area for "is it listed at all" questions. What a *stock*
/// terminal shows — and how the rest is reached there — is the 80×24 test
/// below, and the app-level one beside it.
const AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 76,
    // One row per command, so this grows with the roster: `/team`
    // (**D504**) made 34 one short of the whole card, `/held`
    // (**D524**) did the same to 35, and `/rename` (**D527**) to 36.
    height: 37,
};

/// What an 80×24 terminal actually hands this dialog: the app draws it
/// over the transcript pane, which is the window less the composer's five
/// rows and the status bar's one. That is the area the card outgrew, and
/// asserting against the whole 80×24 window here would test a size nothing
/// ever renders into.
const STOCK: Rect = Rect { x: 0, y: 0, width: 80, height: 18 };

fn drawn(help: &mut Help, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    help.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn rendered(help: &mut Help) -> String {
    drawn(help, AREA)
}

/// Everything the card holds, gathered by scrolling to the bottom of it —
/// which is exactly what a person at an 80×24 terminal does.
fn reachable(help: &mut Help, area: Rect) -> String {
    let mut seen = drawn(help, area);
    for _ in 0..40 {
        help.scroll(1);
        seen.push('\n');
        seen.push_str(&drawn(help, area));
    }

    seen
}

#[test]
fn the_card_lists_every_command() {
    let screen = rendered(&mut Help::new(Keybinds::defaults()));

    for entry in COMMANDS {
        assert!(screen.contains(&entry.slash()), "{} should be listed:\n{screen}", entry.slash());
    }
}

/// The follow-up W2 left open: two command rows pushed the `keys` section
/// off a stock terminal, and `/undo` and `/redo` push it further. Nothing
/// is dropped — it is scrolled to (deviation: help-card-scrolls).
#[test]
fn every_row_is_reachable_on_a_stock_terminal() {
    let mut help = Help::new(Keybinds::defaults());

    let screen = reachable(&mut help, STOCK);

    for entry in COMMANDS {
        assert!(
            screen.contains(&entry.slash()),
            "{} should be reachable at 80x24:\n{screen}",
            entry.slash()
        );
    }
    for name in ["keys", "palette_open", "agent_cycle"] {
        assert!(screen.contains(name), "{name} should be reachable at 80x24:\n{screen}");
    }
}

/// A card cut off with no sign of it reads as the whole list, which is the
/// one reading that is false.
#[test]
fn a_card_that_does_not_fit_says_how_much_of_it_is_showing() {
    let mut help = Help::new(Keybinds::defaults());

    let first = drawn(&mut help, STOCK);
    assert!(first.contains("1-"), "the counter should start at the first row:\n{first}");
    assert!(first.contains("[up/down] scroll"), "and say which keys move it:\n{first}");

    help.scroll(1);
    let moved = drawn(&mut help, STOCK);
    assert!(moved.contains("2-"), "and follow the rows:\n{moved}");
}

/// The other side of it: a window with room for everything says nothing
/// about scrolling, because there is nowhere to scroll to.
#[test]
fn a_card_that_fits_offers_no_scrolling() {
    let screen = rendered(&mut Help::new(Keybinds::defaults()));

    assert!(screen.contains("[Esc] close"), "{screen}");
    assert!(!screen.contains("[up/down] scroll"), "{screen}");
    assert!(!screen.contains(" of "), "{screen}");
}

/// The render is what knows how far down the card goes, so it is the
/// render that clamps — and it writes the clamped value back, or one
/// overshoot would cost a scroll up per row overshot.
#[test]
fn scrolling_past_the_end_settles_on_the_last_row_rather_than_running_away() {
    let mut help = Help::new(Keybinds::defaults());

    help.scroll(isize::MAX);
    let bottom = drawn(&mut help, STOCK);
    help.scroll(-1);
    let stepped_back = drawn(&mut help, STOCK);

    assert_ne!(bottom, stepped_back, "one step up from the bottom should move the card");
    help.scroll(-isize::MAX);
    assert_eq!(
        drawn(&mut help, STOCK),
        drawn(&mut Help::new(Keybinds::defaults()), STOCK),
        "and scrolling up forever is the top"
    );
}

/// The card describes the run it is shown in, not the build's defaults.
///
/// Scoped to the `/themes` row rather than the whole screen: `ctrl+t` is
/// legitimately on the card now, as `transcript`'s own default
/// (**D453**), so a blanket "the screen must not contain ctrl+t" would
/// fail for a reason that has nothing to do with this rebind.
#[test]
fn a_rebound_key_is_the_one_the_card_shows() {
    let configured: BTreeMap<String, String> = [("themes_open".to_owned(), "f7".to_owned())].into();
    let keys = Keybinds::from_config(&configured).expect("a legible binding loads");

    let screen = rendered(&mut Help::new(keys));
    let themes_row = screen
        .lines()
        .find(|line| line.contains("/themes"))
        .unwrap_or_else(|| panic!("the /themes row should be listed:\n{screen}"));

    assert!(themes_row.contains("f7"), "{themes_row}");
    assert!(
        !themes_row.contains("ctrl+t"),
        "the replaced default should be gone from its own row:\n{themes_row}"
    );
}

/// A key with no command of its own has nowhere else to be documented.
#[test]
fn the_card_lists_the_bindings_no_command_row_shows() {
    let screen = rendered(&mut Help::new(Keybinds::defaults()));

    for name in ["palette_open", "agent_cycle"] {
        assert!(screen.contains(name), "{name} should be listed:\n{screen}");
    }
    assert!(
        !screen.contains("sessions_open"),
        "an action a command row already shows should not be repeated:\n{screen}"
    );
}

#[test]
fn a_tiny_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (4, 3), (20, 5)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut help = Help::new(Keybinds::defaults());

        help.scroll(isize::MAX);
        help.render(area, &mut buffer, &Theme::default());
    }
}

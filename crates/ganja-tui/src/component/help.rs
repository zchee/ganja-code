//! The reference card: every command, and the key that reaches it.
//!
//! Spec: upstream `packages/tui/src/component/ui/dialog-help.tsx`, which shows
//! one sentence pointing at the palette. Ganja shows the table instead
//! (deviation: help-card-lists-the-commands). The sentence is the right answer
//! when the palette lists a hundred commands and the dialog cannot; with six
//! commands the dialog *is* the list, and pointing somewhere else for it would
//! be a redirection to nowhere.
//!
//! The card **scrolls**, which upstream's does not need to: its dialog is one
//! sentence pointing at the palette, and one sentence cannot outgrow a stock
//! terminal. Ganja's card is the list, the list grew past the 15 rows an 80×24
//! window leaves it, and a card that silently dropped its tail would be a
//! reference missing exactly the part nobody has memorized (deviation:
//! help-card-scrolls). The mechanism is the one `list.rs` already uses — an
//! offset the render clamps, so a key that would scroll past either end
//! settles at it — plus a counter along the bottom edge saying how much of the
//! card is on screen, so what is off it is never a surprise.
//!
//! Escape closes it and the arrow keys move it; both are
//! [`crate::app::App`]'s, like every other modal's keys.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    command::COMMANDS,
    component::chat::clip,
    keybind::{self, Keybinds},
    theme::Theme,
};

/// Rows the dialog spends on something other than the table.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[Esc] close";

/// The same, for a card with more rows than the window can show.
const SCROLL_HINTS: &str = "[up/down] scroll   [Esc] close";

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 72;

/// Gap between the columns.
const GAP: usize = 2;

/// The reference card.
#[derive(Clone, Debug)]
pub struct Help {
    /// The bindings this run is using, which is what makes the card true of
    /// *this* run rather than of the defaults.
    keys: Keybinds,
    /// First row on screen. Clamped by the render, which is the only place
    /// that knows how many rows there are and how many fit.
    offset: usize,
}

impl Help {
    /// Builds the card over `keys`.
    #[must_use]
    pub fn new(keys: Keybinds) -> Self {
        Self { keys, offset: 0 }
    }

    /// Moves the card by `delta` rows, negative being towards the top.
    ///
    /// Deliberately unclamped at the far end: how far down the card can go
    /// depends on a width and a height only the render has seen, so it is the
    /// render that pins the offset — and it writes the clamped value back, so
    /// a Page Down at the bottom does not have to be undone by ten Page Ups.
    pub fn scroll(&mut self, delta: isize) {
        self.offset = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.offset.saturating_add(delta.unsigned_abs())
        };
    }

    /// Moves the card to its first row.
    pub fn scroll_to_top(&mut self) {
        self.offset = 0;
    }

    /// Draws the modal centered over `area`.
    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let inner_width = usize::from(width).saturating_sub(2);

        let mut body = self.rows(inner_width, theme);
        body.extend(self.unlisted_keys(inner_width, theme));
        let total = body.len();

        // As tall as the card needs, and no taller: the sibling dialogs cap at
        // a round number because their lists are unbounded, where this one is
        // exactly as long as the build's command table. A window with room for
        // all of it gets all of it and no trailing blank rows; a window
        // without gets as much as it can hold, and the footer says so.
        let wanted = u16::try_from(total.saturating_add(CHROME + 2)).unwrap_or(u16::MAX);
        let height = area.height.saturating_sub(2).clamp(1, wanted.max(1));
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);
        // Written back so the offset never runs away past the end: the next
        // scroll up starts from the last row actually shown.
        self.offset = self.offset.min(total.saturating_sub(rows));
        let offset = self.offset;

        let mut lines: Vec<Line<'static>> = body.into_iter().skip(offset).take(rows).collect();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            clip(&footer(offset, rows, total, inner_width), inner_width),
            theme.dim,
        ));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" help "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per command: what it is called, what it does, what key
    /// reaches it.
    ///
    /// The short title rather than the description, because the card's job is
    /// to be scannable and because the key column is as wide as the widest
    /// binding — an action reached by three keys would otherwise leave the
    /// middle column too narrow to finish a sentence in.
    fn rows(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let hints: Vec<String> = COMMANDS
            .iter()
            .map(|entry| {
                entry
                    .action
                    .keybind()
                    .and_then(|action| self.keys.hint(action))
                    .unwrap_or_default()
            })
            .collect();
        // Both side columns are as wide as their widest value, so the
        // descriptions between them line up instead of jittering per row.
        let name_width = COMMANDS
            .iter()
            .map(|entry| entry.slash().width())
            .max()
            .unwrap_or(0);
        let hint_width = hints.iter().map(|hint| hint.width()).max().unwrap_or(0);
        let title_width = width
            .saturating_sub(name_width + hint_width + GAP * 2)
            .max(1);

        COMMANDS
            .iter()
            .zip(hints.iter())
            .map(|(entry, hint)| {
                let row = format!(
                    "{name:<name_width$}{gap}{title:<title_width$}{gap}{hint:>hint_width$}",
                    name = entry.slash(),
                    gap = " ".repeat(GAP),
                    title = clip(entry.title, title_width),
                );

                Line::styled(clip(&row, width), theme.fg)
            })
            .collect()
    }

    /// The bindings no command row already shows.
    ///
    /// Named by the key a config file rebinds them with, which makes the card
    /// the reference for that too — the alternative is a user who can see that
    /// Tab cycles agents and has nowhere to learn what to call it.
    fn unlisted_keys(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let listed: Vec<keybind::Action> = COMMANDS
            .iter()
            .filter_map(|entry| entry.action.keybind())
            .collect();
        let rows: Vec<(&'static str, String)> = keybind::Action::all()
            .filter(|action| !listed.contains(action))
            .filter_map(|action| self.keys.hint(action).map(|hint| (action.key(), hint)))
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }

        let name_width = rows.iter().map(|(name, _)| name.width()).max().unwrap_or(0);
        let mut lines = vec![
            Line::raw(""),
            Line::styled(clip("keys", width), theme.accent),
        ];
        lines.extend(rows.into_iter().map(|(name, hint)| {
            Line::styled(
                clip(
                    &format!("{name:<name_width$}{gap}{hint}", gap = " ".repeat(GAP)),
                    width,
                ),
                theme.fg,
            )
        }));

        lines
    }
}

/// The bottom edge: which keys work, and — when the card does not fit — which
/// of its rows are on screen.
///
/// The counter is what keeps the clip honest. A card cut off with no sign that
/// it was cut reads as a complete list, and the rows most worth reading are
/// the ones a person has not memorized, which is to say the ones at the end.
fn footer(offset: usize, rows: usize, total: usize, width: usize) -> String {
    if total <= rows {
        return HINTS.to_owned();
    }

    let last = (offset + rows).min(total);
    let counter = format!("{first}-{last} of {total}", first = offset + 1);
    let room = width
        .saturating_sub(SCROLL_HINTS.width())
        .saturating_sub(counter.width());
    if room == 0 {
        // Too narrow to say both; the counter is the half that cannot be
        // guessed from the keys.
        return counter;
    }

    format!("{SCROLL_HINTS}{gap}{counter}", gap = " ".repeat(room))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{buffer::Buffer, layout::Rect};

    use super::Help;
    use crate::{command::COMMANDS, keybind::Keybinds, theme::Theme};

    /// Tall enough for every row the card holds at once, which is what makes
    /// this the area for "is it listed at all" questions. What a *stock*
    /// terminal shows — and how the rest is reached there — is the 80×24 test
    /// below, and the app-level one beside it.
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 76,
        height: 32,
    };

    /// What an 80×24 terminal actually hands this dialog: the app draws it
    /// over the transcript pane, which is the window less the composer's five
    /// rows and the status bar's one. That is the area the card outgrew, and
    /// asserting against the whole 80×24 window here would test a size nothing
    /// ever renders into.
    const STOCK: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 18,
    };

    fn drawn(help: &mut Help, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        help.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
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
            assert!(
                screen.contains(&entry.slash()),
                "{} should be listed:\n{screen}",
                entry.slash()
            );
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
            assert!(
                screen.contains(name),
                "{name} should be reachable at 80x24:\n{screen}"
            );
        }
    }

    /// A card cut off with no sign of it reads as the whole list, which is the
    /// one reading that is false.
    #[test]
    fn a_card_that_does_not_fit_says_how_much_of_it_is_showing() {
        let mut help = Help::new(Keybinds::defaults());

        let first = drawn(&mut help, STOCK);
        assert!(
            first.contains("1-"),
            "the counter should start at the first row:\n{first}"
        );
        assert!(
            first.contains("[up/down] scroll"),
            "and say which keys move it:\n{first}"
        );

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

        assert_ne!(
            bottom, stepped_back,
            "one step up from the bottom should move the card"
        );
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
        let configured: BTreeMap<String, String> =
            [("themes_open".to_owned(), "f7".to_owned())].into();
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
}

//! The reference card: every command, and the key that reaches it.
//!
//! Spec: upstream `packages/tui/src/component/ui/dialog-help.tsx`, which shows
//! one sentence pointing at the palette. Ganja shows the table instead
//! (deviation: help-card-lists-the-commands). The sentence is the right answer
//! when the palette lists a hundred commands and the dialog cannot; with six
//! commands the dialog *is* the list, and pointing somewhere else for it would
//! be a redirection to nowhere.
//!
//! Stateless: there is nothing to choose, so there is nothing to remember
//! between frames. Escape closes it, which [`crate::app::App`] handles like
//! every other modal.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    command::COMMANDS,
    component::chat::split_at_width,
    keybind::{self, Keybinds},
    theme::Theme,
};

/// Rows the dialog spends on something other than the table.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[Esc] close";

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
}

impl Help {
    /// Builds the card over `keys`.
    #[must_use]
    pub fn new(keys: Keybinds) -> Self {
        Self { keys }
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let height = area.height.saturating_sub(2).clamp(1, 20);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        let inner_width = usize::from(width).saturating_sub(2);
        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);

        let mut lines = self.rows(inner_width, theme);
        lines.extend(self.unlisted_keys(inner_width, theme));
        lines.truncate(rows);
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

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

/// `text` cut to `width` display columns.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    split_at_width(text, width).0.to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{buffer::Buffer, layout::Rect};

    use super::Help;
    use crate::{command::COMMANDS, keybind::Keybinds, theme::Theme};

    /// Tall enough for the whole card: nine commands, the keys that have no
    /// command row, and the chrome around them. A shorter window truncates,
    /// which is the behavior a tiny-area test covers rather than this one.
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 76,
        height: 20,
    };

    fn rendered(help: &Help) -> String {
        let mut buffer = Buffer::empty(AREA);
        help.render(AREA, &mut buffer, &Theme::default());

        (0..AREA.height)
            .map(|row| {
                (0..AREA.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_card_lists_every_command() {
        let screen = rendered(&Help::new(Keybinds::defaults()));

        for entry in COMMANDS {
            assert!(
                screen.contains(&entry.slash()),
                "{} should be listed:\n{screen}",
                entry.slash()
            );
        }
    }

    /// The card describes the run it is shown in, not the build's defaults.
    #[test]
    fn a_rebound_key_is_the_one_the_card_shows() {
        let configured: BTreeMap<String, String> =
            [("themes_open".to_owned(), "f7".to_owned())].into();
        let keys = Keybinds::from_config(&configured).expect("a legible binding loads");

        let screen = rendered(&Help::new(keys));

        assert!(screen.contains("f7"), "{screen}");
        assert!(
            !screen.contains("ctrl+t"),
            "the replaced default should be gone:\n{screen}"
        );
    }

    /// A key with no command of its own has nowhere else to be documented.
    #[test]
    fn the_card_lists_the_bindings_no_command_row_shows() {
        let screen = rendered(&Help::new(Keybinds::defaults()));

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

            Help::new(Keybinds::defaults()).render(area, &mut buffer, &Theme::default());
        }
    }
}

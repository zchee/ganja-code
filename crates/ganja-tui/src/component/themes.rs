//! The theme picker: a centered modal listing every theme this run can switch
//! to.
//!
//! Spec: upstream `packages/tui/src/component/dialog-theme-list.tsx`. Two of
//! its behaviors are the whole point of the dialog and are ported exactly:
//! moving the cursor **applies** the theme under it, so the choice is made by
//! looking at the screen rather than at a name; and cancelling puts back the
//! theme that was active when it opened, so browsing costs nothing.
//!
//! The dialog owns which name is under the cursor and nothing else. Applying,
//! reverting and storing are [`crate::app::App`]'s, because they touch state
//! the dialog does not own — the same split the sessions picker uses.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{component::chat::split_at_width, theme::Theme};

/// What marks the row the cursor is on, and what pads every other row so the
/// names stay in one column.
const MARKER: &str = "> ";

/// Rows the dialog spends on something other than the list: a blank line and
/// the key reminders.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] preview   [Enter] keep   [Esc] cancel";

/// Widest the modal grows, whatever the terminal offers. A theme name is one
/// short word; a list box the width of the screen would only be harder to read.
const MAX_WIDTH: u16 = 40;

/// The themes to choose between, and which one the cursor is on.
#[derive(Clone, Debug)]
pub struct ThemeList {
    /// Case-insensitively sorted, as [`crate::theme::Themes::names`] answers.
    names: Vec<String>,
    /// Index into [`ThemeList::names`]; always in range while it is non-empty.
    selected: usize,
    /// What was active when the dialog opened, and what cancelling restores.
    initial: String,
}

impl ThemeList {
    /// Opens the list over `names`, with the cursor on `active`.
    ///
    /// Starting anywhere else would preview a theme the user never asked to
    /// see the moment the dialog opened.
    #[must_use]
    pub fn new(names: Vec<String>, active: &str) -> Self {
        let selected = names.iter().position(|name| name == active).unwrap_or(0);

        Self {
            names,
            selected,
            initial: active.to_owned(),
        }
    }

    /// The theme under the cursor, or [`None`] when there is nothing to pick.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.names.get(self.selected).map(String::as_str)
    }

    /// The theme cancelling puts back.
    #[must_use]
    pub fn initial(&self) -> &str {
        &self.initial
    }

    /// Moves the cursor by `delta` rows.
    ///
    /// Clamped rather than wrapped, like the sessions picker: running off one
    /// end and landing on the other is never what the keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        let last = self.names.len().saturating_sub(1);
        let moved = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta.unsigned_abs())
        };

        self.selected = moved.min(last);
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

        // Inside the border on both axes.
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);

        let mut lines = self.rows(inner_width, rows, theme);
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" themes "))
            // The panel surface is what makes a switch visible in the dialog
            // that caused it, rather than only behind it.
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per visible theme.
    fn rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let first = self.first_visible(rows);

        self.names
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, name)| {
                let marker = if index == self.selected { MARKER } else { "  " };
                let row = clip(&format!("{marker}{name}"), width);

                // The selected row is filled rather than tinted, which is what
                // gives the contrast rule something to answer.
                Line::styled(
                    format!("{row:<width$}"),
                    if index == self.selected {
                        theme.selection
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }

    /// The first theme on screen: far enough down to keep the selected one
    /// visible, and no further.
    fn first_visible(&self, rows: usize) -> usize {
        self.selected.saturating_sub(rows.saturating_sub(1))
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
        rendered(&list(), Rect::new(0, 0, 0, 0), &Theme::default());
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
}

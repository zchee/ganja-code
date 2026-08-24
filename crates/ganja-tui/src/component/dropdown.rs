//! The inline command menu that opens when a prompt starts with `/`.
//!
//! Spec: upstream `packages/tui/src/component/prompt/autocomplete.tsx`. It is
//! not the palette in another position — it is the second of upstream's two
//! command surfaces, and it differs in three ways that matter:
//!
//! - it opens **only** when `/` is the very first character of the buffer and
//!   the cursor has not left the first whitespace-free span, so `what about
//!   /tmp` never raises a menu;
//! - it matches **descriptions** as well as names and aliases, where the
//!   palette matches names and titles;
//! - it draws **above** the editor, anchored to it, rather than centered.
//!
//! One deliberate divergence: upstream's `hide()` deletes the typed `/xyz`
//! whenever the menu closes without a selection, which means Escape throws
//! away what was typed. Ganja closes the menu and keeps the text (**D11**) —
//! the destructive half of that behavior has no upside a person would ask for.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    command::{self, Choice, EngineCommand},
    component::{chat::clip, clamped, first_visible},
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Most rows a menu shows at once, upstream's cap.
const MAX_ROWS: usize = 10;

/// What is shown when the fragment matches nothing.
const EMPTY: &str = "no matching commands";

/// Gap between a command and its description.
const GAP: usize = 2;

/// Whether `text` with the cursor at `column` of its first line should raise
/// the menu.
///
/// Upstream's rule, ported exactly: the buffer starts with `/`, and the slice
/// in front of the cursor holds no whitespace. The second half is what closes
/// the menu once a command has been typed and a space follows it — at that
/// point the user is writing arguments, not choosing a command.
#[must_use]
pub fn triggered(text: &str, cursor: (usize, usize)) -> bool {
    let (row, column) = cursor;
    if row != 0 || !text.starts_with('/') {
        return false;
    }

    let first = text.lines().next().unwrap_or_default();

    first
        .chars()
        .take(column)
        .all(|character| !character.is_whitespace())
}

/// The commands a typed fragment narrows to, and which one is under the
/// cursor.
#[derive(Clone, Debug)]
pub struct Dropdown {
    /// The engine's half of the roster, resolved when the session started:
    /// nothing can add a command to it while the menu is up.
    engine: Vec<EngineCommand>,
    matched: Vec<Choice>,
    /// Index into [`Dropdown::matched`]; always in range while it is
    /// non-empty.
    selected: usize,
}

impl Dropdown {
    /// Opens the menu over whatever `text` narrows `engine` and the UI
    /// commands to.
    #[must_use]
    pub fn new(text: &str, engine: Vec<EngineCommand>) -> Self {
        let mut dropdown = Self {
            engine,
            matched: Vec::new(),
            selected: 0,
        };
        dropdown.refresh(text);

        dropdown
    }

    /// Re-narrows the menu after a keystroke reached the editor.
    ///
    /// The cursor goes back to the top, as upstream's does: the list under it
    /// is a different list.
    pub fn refresh(&mut self, text: &str) {
        let mut matched = command::dropdown_matches(text, &self.engine);
        // A bare `/` has nothing to rank by, so the menu is a directory and is
        // ordered like one — which is also the order upstream's merged option
        // list sits in before anything is typed into it. Once there is a
        // fragment the ranking is the whole point.
        if text.trim().trim_start_matches('/').is_empty() {
            matched.sort_by_key(Choice::slash);
        }

        self.matched = matched;
        self.selected = 0;
    }

    /// Whether there is nothing to choose from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty()
    }

    /// The row under the cursor, or [`None`] when nothing matches.
    #[must_use]
    pub fn selected(&self) -> Option<Choice> {
        self.matched.get(self.selected).cloned()
    }

    /// Moves the cursor by `delta` rows, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.matched.len());
    }

    /// Draws the menu directly above `anchor`, which is the editor's area.
    ///
    /// Sized to what it holds rather than to the screen, and clipped to
    /// whatever room there is above the anchor: a menu that overdrew the
    /// transcript entirely would hide the reply the command is about.
    pub fn render(&self, anchor: Rect, buffer: &mut Buffer, theme: &Theme) {
        let Some(area) = menu_area(anchor, self.matched.len()) else {
            return;
        };
        Clear.render(area, buffer);

        let inner_width = usize::from(area.width).saturating_sub(2);
        let visible = usize::from(area.height).saturating_sub(2);

        Paragraph::new(Text::from(self.lines(inner_width, visible, theme)))
            .block(Block::bordered().title(" commands "))
            .style(theme.fg.patch(theme.background_panel))
            .render(area, buffer);
    }

    /// The visible slice of the menu.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.matched.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let names: Vec<String> = self.matched.iter().map(Choice::slash).collect();

        menu_lines(
            &names,
            &self
                .matched
                .iter()
                .map(Choice::description)
                .collect::<Vec<_>>(),
            self.selected,
            width,
            rows,
            theme,
        )
    }
}

/// The area a menu anchored above `anchor` occupies, or [`None`] when there is
/// no room above it to draw one in.
///
/// Shared by every menu that hangs off the editor, because the arithmetic — a
/// height sized to the rows, clamped to the cap, then clipped to the space
/// above the anchor — is the part that has to be right and is the part nobody
/// should write twice.
pub(crate) fn menu_area(anchor: Rect, rows: usize) -> Option<Rect> {
    let rows = rows.clamp(1, MAX_ROWS);
    // Two for the border.
    let wanted = u16::try_from(rows.saturating_add(2)).unwrap_or(u16::MAX);
    let height = wanted.min(anchor.y);
    if height < 3 || anchor.width == 0 {
        return None;
    }

    Some(Rect {
        x: anchor.x,
        y: anchor.y.saturating_sub(height),
        width: anchor.width,
        height,
    })
}

/// One row per name, each padded into a name column with its detail beside it,
/// scrolled so that `selected` is on screen.
pub(crate) fn menu_lines(
    names: &[String],
    details: &[&str],
    selected: usize,
    width: usize,
    rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Names padded to the widest, so the details beside them sit in one
    // column instead of stepping in and out per row.
    let name_width = names.iter().map(|name| name.width()).max().unwrap_or(0);
    let first = first_visible(selected, rows);

    names
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(index, name)| {
            let head = format!(
                "{marker}{name:<name_width$}",
                marker = if index == selected { MARKER } else { "  " },
            );
            let detail = details.get(index).copied().unwrap_or_default();
            let detail_width = width.saturating_sub(head.width() + GAP).max(1);
            let row = format!(
                "{head}{gap}{detail}",
                gap = " ".repeat(GAP),
                detail = clip(detail, detail_width),
            );

            Line::styled(
                format!("{row:<width$}"),
                if index == selected {
                    theme.selection
                } else {
                    theme.fg
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Dropdown, triggered};
    use crate::{
        command::{Choice, EngineCommand},
        theme::Theme,
    };

    /// A menu over the UI commands alone, which is what a session running
    /// without a command registry offers.
    fn menu(text: &str) -> Dropdown {
        Dropdown::new(text, Vec::new())
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
}

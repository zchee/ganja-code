//! The prompt editor.
//!
//! A [`TextArea`] with ganja's submit rules layered on top: the app decides
//! what Enter means before the keystroke ever reaches the widget.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Widget as _};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;

/// The style the widget is handed for its cursor cell — a marker, not a
/// look: `SLOW_BLINK`, which nothing else in the box wears, so the render
/// can find the cell the widget put the cursor on and hand that position to
/// the terminal. The marker never reaches the screen; the render strips it.
///
/// The composer's cursor is the **terminal's own**, not a painted cell
/// (user directive, 2026-08-25). The widget's default paints a reverse-video
/// cell, and a painted cell is content: tmux hides the real cursor of an
/// inactive pane but not a cell, so two ganjas side by side both showed a
/// solid bar, and no glyph a cell can hold looks like the hollow box a
/// terminal draws for the cursor of an unfocused window. Placing the real
/// cursor where the composer's is gets every one of those looks for free,
/// from the terminal that owns them.
fn cursor_mark() -> Style {
    Style::default().add_modifier(Modifier::SLOW_BLINK)
}

/// Rows the editor occupies, borders included.
pub const HEIGHT: u16 = 5;

/// What the box is titled, and what it invites, in each mode. Upstream's chip
/// reads `"Shell"` against the titlecased agent name; ganja has the agent in
/// the status bar already, so the box says only which of the two things the
/// next Enter does (`component/prompt/index.tsx:1310-1319`, `:1447`).
const PROMPT_TITLE: &str = " message ";
/// See [`PROMPT_TITLE`].
const PROMPT_PLACEHOLDER: &str = "Ask ganja something...";
/// See [`PROMPT_TITLE`].
const SHELL_TITLE: &str = " Shell ";
/// See [`PROMPT_TITLE`].
const SHELL_PLACEHOLDER: &str = "Run a command...";

/// What the next Enter does with the buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Send it to the model.
    #[default]
    Prompt,
    /// Run it in the shell, on the user's own behalf.
    Shell,
}

impl Mode {
    /// The box's title in this mode, which is also upstream's chip.
    fn title(self) -> &'static str {
        match self {
            Self::Prompt => PROMPT_TITLE,
            Self::Shell => SHELL_TITLE,
        }
    }

    /// What an empty box invites in this mode.
    fn placeholder(self) -> &'static str {
        match self {
            Self::Prompt => PROMPT_PLACEHOLDER,
            Self::Shell => SHELL_PLACEHOLDER,
        }
    }
}

/// A multi-line prompt buffer.
#[derive(Debug)]
pub struct Editor {
    area: TextArea<'static>,
    /// What the next Enter does, which is what the box is titled after.
    mode: Mode,
    /// The palette the box was last painted in, kept because replacing the
    /// buffer means replacing the widget that holds those styles.
    theme: Theme,
    /// The dim argument hint drawn after a typed command name (**D518**),
    /// refreshed by the app before every frame. Display only: it never
    /// enters the buffer and a submit never carries it.
    hint: Option<String>,
}

impl Editor {
    /// Builds an empty editor styled for `theme`.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        let mut editor = Self {
            area: TextArea::default(),
            mode: Mode::default(),
            theme: theme.clone(),
            hint: None,
        };
        editor.restyle(theme);

        editor
    }

    /// Repaints the editor for `theme`, keeping whatever is typed.
    ///
    /// The widget holds its own styles rather than being handed a theme at
    /// draw time, so nothing else would notice a switch: without this, picking
    /// a theme repaints the whole screen except the box the user is typing in.
    pub fn restyle(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.area.set_block(Block::bordered().title(self.mode.title()).style(theme.dim));
        self.area.set_style(theme.fg);
        // The widget's default underlines the whole line the cursor is on,
        // which reads as decoration on every character being typed — nothing
        // upstream or Claude Code draws. The cursor itself marks the line.
        self.area.set_cursor_line_style(theme.fg);
        // Otherwise the widget's own default gray is the one color on screen a
        // theme cannot reach.
        self.area.set_placeholder_style(theme.dim);
        self.area.set_placeholder_text(self.mode.placeholder());
        // The cursor is the terminal's, not a painted cell (`cursor_mark`).
        self.area.set_cursor_style(cursor_mark());
    }

    /// Switches what the next Enter does, and says so on the box.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        let theme = self.theme.clone();
        self.restyle(&theme);
    }

    /// What the next Enter does with the buffer.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The text worth submitting, or [`None`] when the buffer holds only
    /// whitespace.
    #[must_use]
    pub fn prompt(&self) -> Option<String> {
        let area: &TextArea<'_> = &self.area;
        let text = area.lines().join("\n");

        (!text.trim().is_empty()).then_some(text)
    }

    /// Everything in the buffer, whitespace included.
    ///
    /// Distinct from [`Editor::prompt`] on purpose: what decides whether a `/`
    /// raises the command menu is the literal buffer, where what decides
    /// whether there is anything to send is the buffer with its whitespace
    /// discounted.
    #[must_use]
    pub fn text(&self) -> String {
        let area: &TextArea<'_> = &self.area;

        area.lines().join("\n")
    }

    /// Whether the buffer holds no characters at all.
    ///
    /// The gate on every key that means two things — Ctrl-D exits or deletes
    /// forward, Tab cycles agents or indents, Home and End move in the buffer
    /// or in the transcript. A buffer holding only spaces is *not* empty here:
    /// the user typed those, and a key that quietly quit on top of them would
    /// be throwing work away.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.area.is_empty()
    }

    /// Reverses the `[Image #N]` token whose number is `token` — the
    /// composer rendering Claude Code draws when the cursor sits on one
    /// (2026-08-15 screenshot) — or clears the reverse for [`None`]. Ridden
    /// on the widget's own search machinery: the number is unique per
    /// paste, so the escaped literal matches exactly the one token.
    pub fn set_token_highlight(&mut self, token: Option<u32>) {
        match token {
            Some(number) => {
                let _ = self.area.set_search_pattern(format!("\\[Image #{number}\\]"));
                self.area.set_search_style(Style::default().add_modifier(Modifier::REVERSED));
            }
            None => {
                let _ = self.area.set_search_pattern("");
            }
        }
    }

    /// Where the cursor is, as a zero-based (row, column) pair counted in
    /// characters.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        let cursor = self.area.cursor();

        (cursor.0, cursor.1)
    }

    /// Whether the cursor sits on the buffer's first line.
    ///
    /// The edge an Up-arrow reaches history at: upstream walks the prompt
    /// history only when moving up would otherwise leave the buffer
    /// (`input_move_up` at the top line), so on any lower line Up is an
    /// ordinary cursor move (`component/prompt/index.tsx`).
    #[must_use]
    pub fn on_first_line(&self) -> bool {
        self.area.cursor().0 == 0
    }

    /// Whether the cursor sits on the buffer's last line.
    ///
    /// The mirror of [`Editor::on_first_line`]: the edge a Down-arrow reaches
    /// history at.
    #[must_use]
    pub fn on_last_line(&self) -> bool {
        self.area.cursor().0 + 1 == self.area.lines().len()
    }

    /// Moves the cursor to the start of the line it is on.
    pub fn line_home(&mut self) {
        self.area.move_cursor(CursorMove::Head);
    }

    /// Moves the cursor to the end of the line it is on.
    pub fn line_end(&mut self) {
        self.area.move_cursor(CursorMove::End);
    }

    /// Empties the buffer, which happens once a prompt has been accepted.
    pub fn clear(&mut self) {
        self.area.clear();
    }

    /// Replaces the buffer with `text` and leaves the cursor at its end.
    ///
    /// What `/editor` puts back, and what choosing an engine command types.
    pub fn set_text(&mut self, text: &str) {
        self.area.clear();
        self.area.insert_str(text);
    }

    /// Replaces the buffer with `text` and puts the cursor at `(row, column)`.
    ///
    /// Choosing a file to mention rewrites the line the `@` is on, which may be
    /// anywhere in the buffer: dropping the cursor at the end would move it out
    /// of the sentence the user is in the middle of writing.
    pub fn set_text_at(&mut self, text: &str, row: usize, column: usize) {
        self.set_text(text);
        self.area.move_cursor(CursorMove::Jump(
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(column).unwrap_or(u16::MAX),
        ));
    }

    /// Breaks the line at the cursor.
    pub fn insert_newline(&mut self) {
        self.area.insert_newline();
    }

    /// Inserts `text` at the cursor, leaving it after what was inserted.
    ///
    /// What a paste does. Line breaks inside `text` break the buffer's lines,
    /// which is the whole difference between this and feeding the characters
    /// through [`Editor::input`] one at a time: Enter is a submit here, so a
    /// pasted paragraph typed key by key would send its first line.
    pub fn insert(&mut self, text: &str) {
        self.area.insert_str(text);
    }

    /// Hands `key` to the widget's own editing bindings.
    pub fn input(&mut self, key: KeyEvent) {
        self.area.input(key);
    }

    /// Deletes `count` characters starting at `(row, column)`, leaving the
    /// cursor where the deletion began — the whole-token backspace's engine
    /// (2026-08-15).
    pub fn delete_span(&mut self, row: usize, column: usize, count: usize) {
        self.area.move_cursor(CursorMove::Jump(
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(column + count).unwrap_or(u16::MAX),
        ));
        for _ in 0..count {
            self.area.delete_char();
        }
    }

    /// Replaces the inline argument hint (**D518**).
    pub fn set_hint(&mut self, hint: Option<String>) {
        self.hint = hint;
    }

    /// Draws the editor into `area`, and says where the terminal's cursor
    /// belongs: the screen cell the widget put its cursor on, or [`None`] for
    /// an area too small to hold one.
    ///
    /// The widget tells nobody where that cell is — it paints the cursor
    /// itself and keeps its scrolling private — so it is handed a marker style
    /// instead (`cursor_mark`) and the marked cell is found here, stripped
    /// of the marker, and reported. The caller places the real cursor there
    /// (`App::draw`); nothing is painted.
    pub fn render(&self, area: Rect, buffer: &mut Buffer) -> Option<(u16, u16)> {
        (&self.area).render(area, buffer);
        self.render_hint(area, buffer);
        if area.width <= 2 || area.height <= 2 {
            return None;
        }
        let (left, top) = (area.x + 1, area.y + 1);
        let (right, bottom) = (area.x + area.width - 1, area.y + area.height - 1);
        let marked =
            (top..bottom).flat_map(|y| (left..right).map(move |x| (x, y))).find(|&(x, y)| {
                buffer.cell((x, y)).is_some_and(|cell| cell.modifier.contains(Modifier::SLOW_BLINK))
            })?;
        if let Some(cell) = buffer.cell_mut(marked) {
            cell.modifier.remove(Modifier::SLOW_BLINK);
        }

        Some(marked)
    }

    /// Paints the hint dim after the typed text, inside the border.
    ///
    /// The hint only ever exists for a single-line buffer (the lookup refuses
    /// anything else), so the first content row is the row. Clipped at the
    /// box's right edge rather than wrapped: a hint is a hint, not a manual.
    fn render_hint(&self, area: Rect, buffer: &mut Buffer) {
        let Some(hint) = &self.hint else {
            return;
        };
        if area.width <= 2 || area.height <= 2 {
            return;
        }
        let line = self.area.lines().first().map(String::as_str).unwrap_or("");
        // One column of gap unless the typed text already ends in one.
        let gap = u16::from(!line.is_empty() && !line.ends_with(' '));
        let text_end = area.x + 1 + line.width() as u16 + gap;
        let right = area.x + area.width - 1;
        if text_end >= right {
            return;
        }
        buffer.set_stringn(
            text_end,
            area.y + 1,
            hint,
            usize::from(right - text_end),
            self.theme.dim,
        );
    }
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;

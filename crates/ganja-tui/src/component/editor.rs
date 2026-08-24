//! The prompt editor.
//!
//! A [`TextArea`] with ganja's submit rules layered on top: the app decides
//! what Enter means before the keystroke ever reaches the widget.

use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyEvent,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Widget as _},
};
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
        self.area
            .set_block(Block::bordered().title(self.mode.title()).style(theme.dim));
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
                let _ = self
                    .area
                    .set_search_pattern(format!("\\[Image #{number}\\]"));
                self.area
                    .set_search_style(Style::default().add_modifier(Modifier::REVERSED));
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
        let marked = (top..bottom)
            .flat_map(|y| (left..right).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer
                    .cell((x, y))
                    .is_some_and(|cell| cell.modifier.contains(Modifier::SLOW_BLINK))
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
mod tests {
    use ratatui::{
        buffer::Buffer,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
        layout::Rect,
    };

    use super::{Editor, Mode};
    use crate::theme::{Theme, Themes};

    fn drawn(editor: &Editor) -> String {
        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 5,
        };

        let mut buffer = Buffer::empty(AREA);
        editor.render(AREA, &mut buffer);

        (0..AREA.height)
            .map(|row| {
                (0..AREA.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn typing(editor: &mut Editor, text: &str) {
        for character in text.chars() {
            editor.input(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    /// **D518.** The hint is paint, not text: it shows dim after the typed
    /// name, and the buffer a submit would read never contains it.
    #[test]
    fn the_hint_draws_after_the_text_and_never_enters_the_buffer() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "/team");
        editor.set_hint(Some("list | spawn <name>".to_owned()));

        let screen = drawn(&editor);
        assert!(
            screen.contains("/team list | spawn <name>"),
            "got:\n{screen}"
        );
        assert_eq!(editor.text(), "/team");
    }

    /// **D518.** A hint wider than the box clips at the border instead of
    /// wrapping onto the next row.
    #[test]
    fn a_hint_wider_than_the_box_is_clipped_not_wrapped() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "/team");
        editor.set_hint(Some("x".repeat(80)));

        let screen = drawn(&editor);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(rows[1].contains("xxx"), "got:\n{screen}");
        assert!(!rows[2].contains('x'), "got:\n{screen}");
        // The border column survives the clip.
        assert!(rows[1].ends_with('│'), "got:\n{screen}");
    }

    /// **D518.** Clearing the hint clears the paint.
    #[test]
    fn a_cleared_hint_paints_nothing() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "/team");
        editor.set_hint(Some("list".to_owned()));
        editor.set_hint(None);

        let screen = drawn(&editor);
        assert!(!screen.contains("list"), "got:\n{screen}");
    }

    #[test]
    fn an_empty_editor_has_nothing_to_submit() {
        assert_eq!(Editor::new(&Theme::default()).prompt(), None);
    }

    #[test]
    fn a_whitespace_only_editor_has_nothing_to_submit() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "   ");

        assert_eq!(editor.prompt(), None);
    }

    #[test]
    fn typed_text_becomes_the_prompt_and_clearing_takes_it_back() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "hello");

        assert_eq!(editor.prompt().as_deref(), Some("hello"));

        editor.clear();
        assert_eq!(editor.prompt(), None);
    }

    /// The gate on every key that means two things.
    #[test]
    fn an_editor_holding_only_spaces_is_not_empty_even_though_it_has_nothing_to_submit() {
        let mut editor = Editor::new(&Theme::default());
        assert!(editor.is_empty(), "a fresh editor is empty");

        typing(&mut editor, "  ");

        assert!(
            !editor.is_empty(),
            "typed spaces are text, however unsubmittable"
        );
        assert_eq!(editor.prompt(), None);
    }

    #[test]
    fn the_cursor_reports_where_typing_left_it_and_home_and_end_move_it() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "first");
        editor.insert_newline();
        typing(&mut editor, "second");

        assert_eq!(editor.cursor(), (1, 6));

        editor.line_home();
        assert_eq!(editor.cursor(), (1, 0), "home moves within the line");

        editor.line_end();
        assert_eq!(editor.cursor(), (1, 6));
    }

    #[test]
    fn the_whole_buffer_reads_back_with_its_whitespace_intact() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "/models ");

        assert_eq!(editor.text(), "/models ");
        assert_eq!(
            editor.prompt().as_deref(),
            Some("/models "),
            "prompt only discounts whitespace, it does not strip it"
        );
    }

    #[test]
    fn a_newline_keeps_both_lines_in_one_prompt() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "first");
        editor.insert_newline();
        typing(&mut editor, "second");

        assert_eq!(editor.prompt().as_deref(), Some("first\nsecond"));
    }

    /// The widget's own default underlines the cursor's whole line, which
    /// would decorate every character as it is typed — nothing upstream or
    /// Claude Code draws, so no cell here may carry it.
    #[test]
    fn the_line_being_typed_is_not_underlined() {
        use ratatui::style::Modifier;

        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 5,
        };

        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "no decoration");

        let mut buffer = Buffer::empty(AREA);
        editor.render(AREA, &mut buffer);

        for row in 0..AREA.height {
            for column in 0..AREA.width {
                assert!(
                    !buffer[(column, row)]
                        .modifier
                        .contains(Modifier::UNDERLINED),
                    "cell ({column}, {row}) is underlined"
                );
            }
        }
    }

    /// The one component whose styles are set once rather than read per frame,
    /// which is why a theme switch has to reach in and repaint it.
    #[test]
    fn restyling_repaints_the_box_without_disturbing_what_is_typed() {
        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 5,
        };

        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "a draft mid-switch");

        let mut buffer = Buffer::empty(AREA);
        editor.render(AREA, &mut buffer);
        let before = buffer[(0, 0)].fg;

        editor.restyle(
            &Themes::builtin()
                .select("gruvbox")
                .expect("gruvbox is builtin"),
        );
        editor.render(AREA, &mut buffer);

        assert_ne!(
            before,
            buffer[(0, 0)].fg,
            "the border kept the styles it was built with"
        );
        assert_eq!(
            editor.prompt().as_deref(),
            Some("a draft mid-switch"),
            "restyling must not touch the buffer"
        );
    }

    /// The mode is what the box says it is: a person about to press Enter has
    /// to be able to see whether it reaches the model or the shell.
    #[test]
    fn the_box_says_which_of_the_two_things_the_next_enter_does() {
        let mut editor = Editor::new(&Theme::default());
        assert_eq!(editor.mode(), Mode::Prompt);

        let prompting = drawn(&editor);
        assert!(prompting.contains("message"), "got:\n{prompting}");
        assert!(prompting.contains("Ask ganja"), "got:\n{prompting}");

        editor.set_mode(Mode::Shell);

        let shelling = drawn(&editor);
        assert_eq!(editor.mode(), Mode::Shell);
        assert!(shelling.contains("Shell"), "got:\n{shelling}");
        assert!(shelling.contains("Run a command"), "got:\n{shelling}");
        assert!(!shelling.contains("message"), "got:\n{shelling}");
    }

    /// A theme switch must not quietly put the prompt chrome back on a box
    /// that is running shell commands.
    #[test]
    fn restyling_keeps_the_mode_the_box_is_in() {
        let mut editor = Editor::new(&Theme::default());
        editor.set_mode(Mode::Shell);

        editor.restyle(
            &Themes::builtin()
                .select("gruvbox")
                .expect("gruvbox is builtin"),
        );

        assert!(drawn(&editor).contains("Shell"), "got:\n{}", drawn(&editor));
    }

    /// Flipping into shell mode and back leaves the text alone: upstream runs
    /// the raw buffer, so what was typed before the flip is part of it.
    #[test]
    fn changing_mode_does_not_disturb_what_is_typed() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "ls -la");

        editor.set_mode(Mode::Shell);
        assert_eq!(editor.prompt().as_deref(), Some("ls -la"));

        editor.set_mode(Mode::Prompt);
        assert_eq!(editor.prompt().as_deref(), Some("ls -la"));
    }

    /// The cursor is the terminal's, not a painted cell: the render reports
    /// the cell the widget put its cursor on — the blank one before the
    /// placeholder, the one after typed text, the character `Home` moves onto
    /// — with nothing painted there, and nothing on an area too small for a
    /// cursor.
    #[test]
    fn the_render_reports_the_cursor_cell_and_paints_nothing_on_it() {
        use ratatui::style::Modifier;
        const AREA: Rect = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 5,
        };
        let place = |editor: &Editor| {
            let mut buffer = Buffer::empty(AREA);
            let at = editor.render(AREA, &mut buffer);
            let unpainted = at.is_none_or(|(x, y)| {
                !buffer[(x, y)]
                    .modifier
                    .intersects(Modifier::REVERSED | Modifier::SLOW_BLINK)
            });
            assert!(unpainted, "nothing is painted on the cursor cell");
            at
        };
        let mut editor = Editor::new(&Theme::default());
        assert_eq!(
            place(&editor),
            Some((1, 1)),
            "empty: before the placeholder"
        );

        typing(&mut editor, "ok");
        assert_eq!(place(&editor), Some((3, 1)), "after the typed text");
        editor.line_home();
        assert_eq!(place(&editor), Some((1, 1)), "on the first character");

        let mut tiny = Buffer::empty(Rect::new(0, 0, 2, 2));
        assert_eq!(
            editor.render(Rect::new(0, 0, 2, 2), &mut tiny),
            None,
            "an area with no inside has no cursor cell"
        );
    }

    #[test]
    fn replacing_the_buffer_leaves_the_cursor_at_the_end() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "a draft");

        editor.set_text("what the editor wrote\nover two lines");

        assert_eq!(
            editor.prompt().as_deref(),
            Some("what the editor wrote\nover two lines")
        );
        assert_eq!(editor.cursor(), (1, "over two lines".chars().count()));
    }

    #[test]
    fn replacing_the_buffer_can_put_the_cursor_where_the_caller_asks() {
        let mut editor = Editor::new(&Theme::default());

        editor.set_text_at("look at @src/lib.rs and say why", 0, 19);

        assert_eq!(editor.cursor(), (0, 19));
        assert_eq!(
            editor.prompt().as_deref(),
            Some("look at @src/lib.rs and say why")
        );
    }

    #[test]
    fn replacing_the_buffer_with_nothing_empties_it() {
        let mut editor = Editor::new(&Theme::default());
        typing(&mut editor, "a draft");

        editor.set_text("");

        assert!(editor.is_empty());
        assert_eq!(editor.cursor(), (0, 0));
    }
}

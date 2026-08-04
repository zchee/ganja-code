//! The prompt editor.
//!
//! A [`TextArea`] with ganja's submit rules layered on top: the app decides
//! what Enter means before the keystroke ever reaches the widget.

use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyEvent,
    layout::Rect,
    widgets::{Block, Widget as _},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::theme::Theme;

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
}

impl Editor {
    /// Builds an empty editor styled for `theme`.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        let mut editor = Self {
            area: TextArea::default(),
            mode: Mode::default(),
            theme: theme.clone(),
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
        // Otherwise the widget's own default gray is the one color on screen a
        // theme cannot reach.
        self.area.set_placeholder_style(theme.dim);
        self.area.set_placeholder_text(self.mode.placeholder());
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

    /// Where the cursor is, as a zero-based (row, column) pair counted in
    /// characters.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        let cursor = self.area.cursor();

        (cursor.0, cursor.1)
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

    /// Hands `key` to the widget's own editing bindings.
    pub fn input(&mut self, key: KeyEvent) {
        self.area.input(key);
    }

    /// Draws the editor into `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer) {
        (&self.area).render(area, buffer);
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

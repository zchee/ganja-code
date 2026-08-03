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
use ratatui_textarea::TextArea;

use crate::theme::Theme;

/// Rows the editor occupies, borders included.
pub const HEIGHT: u16 = 5;

/// A multi-line prompt buffer.
#[derive(Debug)]
pub struct Editor {
    area: TextArea<'static>,
}

impl Editor {
    /// Builds an empty editor styled for `theme`.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        let mut editor = Self {
            area: TextArea::default(),
        };
        editor.area.set_placeholder_text("Ask ganja something...");
        editor.restyle(theme);

        editor
    }

    /// Repaints the editor for `theme`, keeping whatever is typed.
    ///
    /// The widget holds its own styles rather than being handed a theme at
    /// draw time, so nothing else would notice a switch: without this, picking
    /// a theme repaints the whole screen except the box the user is typing in.
    pub fn restyle(&mut self, theme: &Theme) {
        self.area
            .set_block(Block::bordered().title(" message ").style(theme.dim));
        self.area.set_style(theme.fg);
        // Otherwise the widget's own default gray is the one color on screen a
        // theme cannot reach.
        self.area.set_placeholder_style(theme.dim);
    }

    /// The text worth submitting, or [`None`] when the buffer holds only
    /// whitespace.
    #[must_use]
    pub fn prompt(&self) -> Option<String> {
        let area: &TextArea<'_> = &self.area;
        let text = area.lines().join("\n");

        (!text.trim().is_empty()).then_some(text)
    }

    /// Empties the buffer, which happens once a prompt has been accepted.
    pub fn clear(&mut self) {
        self.area.clear();
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

    use super::Editor;
    use crate::theme::{Theme, Themes};

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
}

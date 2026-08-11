//! The inline file menu that opens when a prompt mentions a file with `@`.
//!
//! Spec: upstream `packages/tui/src/component/prompt/autocomplete.tsx` in `@`
//! mode. The same box the command menu draws in, over a different list and
//! with one behavior deliberately not shared: **the order is the backend's**.
//! Upstream's comments say so twice — *"trust the order returned by fff"* —
//! and re-ranking a file list on the client is how a picker ends up disagreeing
//! with itself between the two places it is drawn.
//!
//! Descriptions are not matched here either, which upstream also singles out:
//! a file has no description, and the agents and resources that do are not in
//! this build's `@` roster.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};

use crate::{
    component::{
        chat::clip,
        clamped,
        dropdown::{menu_area, menu_lines},
    },
    mention::Fragment,
    theme::Theme,
};

/// What is shown when the fragment matches no file.
const EMPTY: &str = "no matching files";

/// The files a typed `@` fragment narrowed to, and which one is under the
/// cursor.
#[derive(Clone, Debug)]
pub struct Files {
    /// The mention this list was opened for. Kept so that choosing a row can
    /// replace exactly the span the user typed, wherever in the buffer it is.
    fragment: Fragment,
    /// Relative paths, in the order the walk returned them.
    paths: Vec<String>,
    /// Index into [`Files::paths`]; always in range while it is non-empty.
    selected: usize,
}

impl Files {
    /// Opens the menu over `paths`, completing `fragment`.
    #[must_use]
    pub fn new(fragment: Fragment, paths: Vec<String>) -> Self {
        Self {
            fragment,
            paths,
            selected: 0,
        }
    }

    /// The mention this list is completing.
    #[must_use]
    pub fn fragment(&self) -> &Fragment {
        &self.fragment
    }

    /// Whether this list is already the answer for `fragment`.
    ///
    /// What keeps a cursor key, or a keystroke elsewhere in the buffer, from
    /// walking the project again: the list depends on the fragment and on
    /// nothing else about the buffer.
    #[must_use]
    pub fn answers(&self, fragment: &Fragment) -> bool {
        self.fragment == *fragment
    }

    /// Whether there is nothing to choose from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The path under the cursor, or [`None`] when nothing matched.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.paths.get(self.selected).map(String::as_str)
    }

    /// Moves the cursor by `delta` rows, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.paths.len());
    }

    /// Draws the menu directly above `anchor`, which is the editor's area.
    pub fn render(&self, anchor: Rect, buffer: &mut Buffer, theme: &Theme) {
        let Some(area) = menu_area(anchor, self.paths.len()) else {
            return;
        };
        Clear.render(area, buffer);

        let inner_width = usize::from(area.width).saturating_sub(2);
        let visible = usize::from(area.height).saturating_sub(2);

        Paragraph::new(Text::from(self.lines(inner_width, visible, theme)))
            .block(Block::bordered().title(" files "))
            .style(theme.fg.patch(theme.background_panel))
            .render(area, buffer);
    }

    /// The visible slice of the menu.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.paths.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        // A path is the whole row: there is no second column to line up, so
        // every detail is empty and the name column takes the width.
        let details = vec![""; self.paths.len()];

        menu_lines(&self.paths, &details, self.selected, width, rows, theme)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::Files;
    use crate::{mention::Fragment, theme::Theme};

    fn fragment(text: &str) -> Fragment {
        Fragment {
            row: 0,
            start: 0,
            text: text.to_owned(),
        }
    }

    fn files(paths: &[&str]) -> Files {
        Files::new(
            fragment("lib"),
            paths.iter().map(|path| (*path).to_owned()).collect(),
        )
    }

    fn rendered(files: &Files, anchor: Rect, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        files.render(anchor, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Upstream says it twice in comments: the walk's order is the order.
    #[test]
    fn the_rows_keep_the_order_the_walk_returned_them_in() {
        let walked = ["zebra/lib.rs", "alpha/lib.rs", "middle/lib.rs"];
        let files = files(&walked);

        let screen = rendered(&files, Rect::new(0, 10, 40, 5), Rect::new(0, 0, 40, 16));
        let rows: Vec<&str> = screen
            .lines()
            .filter(|line| line.contains("lib.rs"))
            .collect();

        for (row, expected) in rows.iter().zip(walked.iter()) {
            assert!(row.contains(expected), "got:\n{screen}");
        }
        assert_eq!(rows.len(), walked.len(), "got:\n{screen}");
    }

    #[test]
    fn the_cursor_starts_on_the_first_row_and_clamps_at_both_ends() {
        let mut files = files(&["a/lib.rs", "b/lib.rs"]);
        assert_eq!(files.selected(), Some("a/lib.rs"));

        files.move_selection(1);
        assert_eq!(files.selected(), Some("b/lib.rs"));

        files.move_selection(9);
        assert_eq!(files.selected(), Some("b/lib.rs"));
        files.move_selection(-9);
        assert_eq!(files.selected(), Some("a/lib.rs"));
    }

    #[test]
    fn a_fragment_nothing_matches_says_so_instead_of_drawing_an_empty_box() {
        let files = files(&[]);
        assert!(files.is_empty());
        assert_eq!(files.selected(), None);

        let screen = rendered(&files, Rect::new(0, 10, 40, 5), Rect::new(0, 0, 40, 16));
        assert!(screen.contains("no matching files"), "{screen}");
    }

    #[test]
    fn the_menu_draws_above_the_editor_it_is_anchored_to() {
        let anchor = Rect::new(0, 10, 40, 5);
        let screen = rendered(&files(&["src/lib.rs"]), anchor, Rect::new(0, 0, 40, 16));

        let row = screen
            .lines()
            .position(|line| line.contains("src/lib.rs"))
            .expect("the path should be on screen");
        assert!(
            row < usize::from(anchor.y),
            "the menu should sit above row {}, found it at {row}:\n{screen}",
            anchor.y
        );
    }

    #[test]
    fn an_editor_with_no_room_above_it_gets_no_menu() {
        let screen = rendered(
            &files(&["src/lib.rs"]),
            Rect::new(0, 0, 40, 5),
            Rect::new(0, 0, 40, 8),
        );

        assert!(
            screen.trim().is_empty(),
            "nothing should have been drawn:\n{screen}"
        );
    }

    /// The list depends on the fragment alone, which is what lets the app skip
    /// a walk when nothing about the mention changed.
    #[test]
    fn a_list_answers_the_fragment_it_was_opened_for_and_no_other() {
        let files = files(&["src/lib.rs"]);

        assert!(files.answers(&fragment("lib")));
        assert!(!files.answers(&fragment("li")));
        assert!(!files.answers(&Fragment {
            row: 1,
            start: 0,
            text: "lib".to_owned(),
        }));
    }
}

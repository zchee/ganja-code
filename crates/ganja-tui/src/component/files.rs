//! The inline `@` menu: files, beside roster teammates and live sessions
//! since **D529** (**D530**'s re-derived gate widens who sees the latter
//! two).
//!
//! Spec for the file half: upstream `packages/tui/src/component/prompt/autocomplete.tsx`
//! in `@` mode. The same box the command menu draws in, over a different
//! list and with one behavior deliberately not shared: **file order is the
//! backend's**. Upstream's comments say so twice — *"trust the order
//! returned by fff"* — and re-ranking a file list on the client is how a
//! picker ends up disagreeing with itself between the two places it is
//! drawn.
//!
//! The roster and live-session rows have no upstream counterpart at all —
//! ganja's own (v2 §"What `@session` does" is the live-session half's
//! specification, read as behavior). Both carry a label (`teammate`/
//! `session`) rather than a description, because unlike a file's path a
//! bare name says nothing about which kind of thing it names.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};

use crate::component::chat::clip;
use crate::component::clamped;
use crate::component::dropdown::{menu_area, menu_lines};
use crate::mention::Fragment;
use crate::theme::Theme;

/// What is shown when the fragment matches no file, no teammate and no live
/// session.
const EMPTY: &str = "no matches";

/// One row the `@` menu offers: a file the walk found, a roster teammate
/// (**D528**: lead-assigned, so completing one is never ambiguous), or a
/// live session from the injected [`crate::lister::Lister`] (self-chosen
/// and unverified — v2's own honesty rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A path relative to the mention's own root — untouched by D529, the
    /// same string the walk always returned.
    File(String),
    /// A roster member. `lead` marks the session leading the team, the same
    /// fact `/team`'s own dialog marks (`component/team.rs`'s `Row`).
    Teammate {
        /// The name as the roster holds it.
        name: String,
        /// Whether this member leads the team.
        lead: bool,
    },
    /// A live session the registry's own listing named (**D527**).
    Session {
        /// The name as that session's own registration typed it.
        name: String,
        /// The directory it was launched in, the menu's own
        /// disambiguation column.
        cwd: PathBuf,
        /// Its socket stem — shown only while `colliding` is set, since a
        /// name with one live holder needs no stem to read.
        stem: String,
        /// The `uds:` spelling this row's own socket answers to — what a
        /// **colliding** completion splices instead of the bare name
        /// (ADJ-3), so the person's exact choice cannot be reassigned by a
        /// later resolution.
        address: String,
        /// Another row — teammate or session — shares this name under the
        /// registry's own case-insensitive fold, so completing this one
        /// must splice `address` rather than `@name` (ADJ-3).
        colliding: bool,
        /// A real file answers to this exact name too, which wins at
        /// submit time regardless (the file-wins order, D529): still
        /// shown, so the person can still reach this session's `uds:`
        /// spelling, but marked so `@name` is not mistaken for a way to
        /// reach it (**F12**).
        shadowed: bool,
    },
}

impl Row {
    /// The name column: a file's path, or a teammate's/session's bare name.
    #[must_use]
    fn label(&self) -> &str {
        match self {
            Row::File(path) => path,
            Row::Teammate { name, .. } | Row::Session { name, .. } => name,
        }
    }

    /// The detail column: nothing for a file (there is no second column to
    /// line up, the file menu's own long-standing rule), and the kind label
    /// v2's own honesty rule asks for otherwise.
    fn detail(&self) -> String {
        match self {
            Row::File(_) => String::new(),
            Row::Teammate { lead: true, .. } => "(teammate, lead)".to_owned(),
            Row::Teammate { lead: false, .. } => "(teammate)".to_owned(),
            Row::Session { cwd, stem, colliding, shadowed, .. } => {
                let mut detail = if *colliding {
                    format!("(session · {stem}) {}", cwd.display())
                } else {
                    format!("(session) {}", cwd.display())
                };
                if *shadowed {
                    detail.push_str(" — shadowed by a file");
                }
                detail
            }
        }
    }
}

/// The rows a typed `@` fragment narrowed to, and which one is under the
/// cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files {
    /// The mention this list was opened for. Kept so that choosing a row can
    /// replace exactly the span the user typed, wherever in the buffer it is.
    fragment: Fragment,
    /// Every row, in the order the caller assembled them: files first (the
    /// walk's own order, untouched), then roster, then live sessions —
    /// `App::assemble_at_rows`'s own order, not this type's business.
    rows: Vec<Row>,
    /// Index into [`Files::rows`]; always in range while it is non-empty.
    selected: usize,
    /// Set when the live-session listing this menu drew from was partial
    /// (**AC-28**): the menu marks itself incomplete and still completes —
    /// the engine's own resolution at send time is the authority, not this
    /// snapshot.
    incomplete: Option<String>,
}

impl Files {
    /// Opens the menu over `rows`, completing `fragment`. `incomplete`
    /// carries the live-session listing's own error, when it answered only
    /// partly (**AC-28**).
    #[must_use]
    pub fn new(fragment: Fragment, rows: Vec<Row>, incomplete: Option<String>) -> Self {
        Self { fragment, rows, selected: 0, incomplete }
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
        self.rows.is_empty()
    }

    /// The row under the cursor, or [`None`] when nothing matched.
    #[must_use]
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Every row this menu holds, in the order it was assembled.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Moves the cursor by `delta` rows, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.rows.len());
    }

    /// Draws the menu directly above `anchor`, which is the editor's area.
    pub fn render(&self, anchor: Rect, buffer: &mut Buffer, theme: &Theme) {
        let Some(area) = menu_area(anchor, self.rows.len()) else {
            return;
        };
        Clear.render(area, buffer);

        let inner_width = usize::from(area.width).saturating_sub(2);
        let visible = usize::from(area.height).saturating_sub(2);

        // Titled generically rather than "files": since D529 this box lists
        // roster teammates and live sessions beside files, and a name row
        // under a "files" heading would be its own small dishonesty.
        let title = if self.incomplete.is_some() { " mentions (partial) " } else { " mentions " };
        Paragraph::new(Text::from(self.lines(inner_width, visible, theme)))
            .block(Block::bordered().title(title))
            .style(theme.fg.patch(theme.background_panel))
            .render(area, buffer);
    }

    /// The visible slice of the menu.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let names: Vec<String> = self.rows.iter().map(|row| row.label().to_owned()).collect();
        let details: Vec<String> = self.rows.iter().map(Row::detail).collect();
        let details: Vec<&str> = details.iter().map(String::as_str).collect();

        menu_lines(&names, &details, self.selected, width, rows, theme)
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;

//! The inline skill menu that opens when a prompt invokes a skill with `$`.
//!
//! The OpenAI Codex CLI's selector (**D491**), drawn in the same box the `@`
//! file menu draws in: typed `$` raises it over the skills `discover` finds
//! under the engine's own roots, the fragment narrows it, and Tab or Enter
//! completes the row to `$name ` without submitting. Unlike the file menu's
//! backend-ordered rows, these are ranked here — `nucleo` on the name with
//! the description as a weaker signal, the dropdown's own scheme — because
//! there is no backend whose order could disagree with this one.

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};
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

/// What is shown when the fragment matches no skill.
const EMPTY: &str = "no matching skills";

/// The skills a typed `$` fragment narrowed to, and which one is under the
/// cursor.
#[derive(Clone, Debug)]
pub struct SkillMenu {
    /// The invocation this list was opened for. Kept so that choosing a row
    /// can replace exactly the span the user typed.
    fragment: Fragment,
    /// `(name, description)` rows, ranked; an undescribed skill shows an
    /// empty second column.
    rows: Vec<(String, String)>,
    /// Index into [`SkillMenu::rows`]; always in range while it is non-empty.
    selected: usize,
}

impl SkillMenu {
    /// Opens the menu over `skills` — each with the source tag the caller
    /// worked out, a plugin's name or `user` — narrowed and ranked by
    /// `fragment`.
    #[must_use]
    pub fn new(fragment: Fragment, skills: &[(ganja_tool::skill::Skill, String)]) -> Self {
        let rows = ranked(&fragment.text, skills);

        Self {
            fragment,
            rows,
            selected: 0,
        }
    }

    /// The invocation this list is completing.
    #[must_use]
    pub fn fragment(&self) -> &Fragment {
        &self.fragment
    }

    /// Whether this list is already the answer for `fragment` — what keeps a
    /// cursor key elsewhere in the buffer from walking the roots again.
    #[must_use]
    pub fn answers(&self, fragment: &Fragment) -> bool {
        self.fragment == *fragment
    }

    /// Whether there is nothing to choose from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The name under the cursor, or [`None`] when nothing matched.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|(name, _)| name.as_str())
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

        Paragraph::new(Text::from(self.lines(inner_width, visible, theme)))
            .block(Block::bordered().title(" skills "))
            .style(theme.fg.patch(theme.background_panel))
            .render(area, buffer);
    }

    /// The visible slice of the menu.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let names: Vec<String> = self.rows.iter().map(|(name, _)| name.clone()).collect();
        let details: Vec<&str> = self
            .rows
            .iter()
            .map(|(_, description)| description.as_str())
            .collect();

        menu_lines(&names, &details, self.selected, width, rows, theme)
    }
}

/// `skills` narrowed to `needle` and ranked, or all of them in discovery
/// order when nothing has been typed yet.
///
/// A row's second column opens with its source tag — `(plugin-name)` or
/// `(user)` — and the tag is scored with the description, so `$matt` finds a
/// plugin's skills by the plugin's own name.
fn ranked(needle: &str, skills: &[(ganja_tool::skill::Skill, String)]) -> Vec<(String, String)> {
    let row = |(skill, source): &(ganja_tool::skill::Skill, String)| {
        let description = match skill.description.as_deref() {
            Some(description) => format!("({source}) {description}"),
            None => format!("({source})"),
        };

        (skill.name.clone(), description)
    };

    if needle.is_empty() {
        return skills.iter().map(row).collect();
    }

    let atom = Atom::new(
        needle,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, (String, String))> = skills
        .iter()
        .filter_map(|entry| {
            let (name, described) = row(entry);
            let name_score = atom
                .score(Utf32Str::new(&name, &mut buffer), &mut matcher)
                .map(|score| u32::from(score) * 2);
            let description_score = atom
                .score(Utf32Str::new(&described, &mut buffer), &mut matcher)
                .map(u32::from);

            name_score
                .into_iter()
                .chain(description_score)
                .max()
                .map(|score| (score, (name, described)))
        })
        .collect();
    // Ties break on the name so a fragment always produces the same list —
    // the dropdown's own determinism rule.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));

    scored.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
#[path = "skill_menu_tests.rs"]
mod tests;

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
    /// Opens the menu over `skills`, narrowed and ranked by `fragment`.
    #[must_use]
    pub fn new(fragment: Fragment, skills: &[ganja_tool::skill::Skill]) -> Self {
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
fn ranked(needle: &str, skills: &[ganja_tool::skill::Skill]) -> Vec<(String, String)> {
    let row = |skill: &ganja_tool::skill::Skill| {
        (
            skill.name.clone(),
            skill.description.clone().unwrap_or_default(),
        )
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
        .filter_map(|skill| {
            let name = atom
                .score(Utf32Str::new(&skill.name, &mut buffer), &mut matcher)
                .map(|score| u32::from(score) * 2);
            let description = skill.description.as_deref().and_then(|description| {
                atom.score(Utf32Str::new(description, &mut buffer), &mut matcher)
                    .map(u32::from)
            });

            name.into_iter()
                .chain(description)
                .max()
                .map(|score| (score, row(skill)))
        })
        .collect();
    // Ties break on the name so a fragment always produces the same list —
    // the dropdown's own determinism rule.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));

    scored.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ganja_tool::skill::Skill;

    use super::SkillMenu;
    use crate::mention::Fragment;

    fn skill(name: &str, description: Option<&str>) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            location: PathBuf::from("SKILL.md"),
            content: String::new(),
        }
    }

    fn fragment(text: &str) -> Fragment {
        Fragment {
            row: 0,
            start: 0,
            text: text.to_owned(),
        }
    }

    #[test]
    fn an_empty_fragment_offers_every_skill_in_discovery_order() {
        let menu = SkillMenu::new(
            fragment(""),
            &[
                skill("alpha", Some("first")),
                skill("beta", None),
                skill("gamma", Some("third")),
            ],
        );

        assert!(!menu.is_empty());
        assert_eq!(menu.selected(), Some("alpha"));
    }

    #[test]
    fn a_fragment_narrows_and_ranks_with_the_name_outweighing_the_description() {
        let menu = SkillMenu::new(
            fragment("port"),
            &[
                skill("review", Some("reviews a port")),
                skill("porting", Some("how to port")),
                skill("unrelated", None),
            ],
        );

        assert_eq!(
            menu.selected(),
            Some("porting"),
            "the name match outranks the description match"
        );
    }

    #[test]
    fn a_fragment_matching_nothing_is_an_empty_menu() {
        let menu = SkillMenu::new(fragment("zzz"), &[skill("porting", None)]);

        assert!(menu.is_empty());
        assert_eq!(menu.selected(), None);
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut menu = SkillMenu::new(fragment(""), &[skill("a", None), skill("b", None)]);

        menu.move_selection(5);
        assert_eq!(menu.selected(), Some("b"));
        menu.move_selection(-9);
        assert_eq!(menu.selected(), Some("a"));
    }
}

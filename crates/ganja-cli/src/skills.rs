//! `ganja skills` — the roster a `$name` invocation answers to, printed.
//!
//! Spec: none to port. Upstream opencode has no skills CLI; the Codex CLI
//! lists its skills with `/skills` inside the TUI, and this is that listing's
//! headless twin (**D491**), sharing its discovery end to end: the same
//! `instruction::skill_roots` the session's `skill` tool and `$` expansion
//! read, walked by the same `discover`, with each row's origin attributed by
//! the same `skill::origin` the `/skills` dialog uses. One collector, two
//! surfaces — the plugin listing's own idiom.

use std::path::Path;

use anyhow::{Context as _, Result};
use ganja_core::{instruction, tool::skill};

/// Prints the discovered skills, or says honestly that there are none.
pub fn skills_command(cwd: &Path) -> Result<()> {
    let config =
        ganja_core::config::Config::load(cwd).context("failed to load the configuration")?;
    let roots = instruction::skill_roots(&config, cwd);
    let found = skill::discover(&roots);

    for line in rows(&roots, &found) {
        println!("{line}");
    }

    Ok(())
}

/// The listing, one string per printed line — separated from the printing so
/// a test reads exactly what a person would.
fn rows(roots: &skill::Roots, found: &[skill::Skill]) -> Vec<String> {
    if found.is_empty() {
        return vec![
            "no skills installed — none under ganja's homes, `skills.paths`, or an enabled \
             plugin's skills/"
                .to_owned(),
        ];
    }

    let name_width = found
        .iter()
        .map(|skill| skill.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![format!("{:<name_width$}  DESCRIPTION — ORIGIN", "NAME")];
    lines.extend(found.iter().map(|skill| {
        let origin = skill::origin(roots, skill)
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let description = skill.description.as_deref().unwrap_or("(no description)");

        format!("{:<name_width$}  {description} — {origin}", skill.name)
    }));

    lines
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ganja_core::tool::skill::{Roots, Skill};

    fn skill(name: &str, description: Option<&str>, location: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            location: PathBuf::from(location),
            content: String::new(),
        }
    }

    #[test]
    fn the_listing_aligns_names_and_attributes_each_row_to_its_root() {
        let roots = Roots::none().with_paths([PathBuf::from("/home/skills")]);
        let lines = super::rows(
            &roots,
            &[
                skill(
                    "porting",
                    Some("How to port."),
                    "/home/skills/porting/SKILL.md",
                ),
                skill("tdd", None, "/home/skills/tdd/SKILL.md"),
            ],
        );

        assert_eq!(
            lines,
            vec![
                "NAME     DESCRIPTION — ORIGIN".to_owned(),
                "porting  How to port. — /home/skills".to_owned(),
                "tdd      (no description) — /home/skills".to_owned(),
            ]
        );
    }

    #[test]
    fn an_empty_roster_is_said_rather_than_shown_as_a_bare_header() {
        let lines = super::rows(&Roots::none(), &[]);

        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("no skills installed"),
            "the empty case names where a skill could have come from: {}",
            lines[0]
        );
    }
}

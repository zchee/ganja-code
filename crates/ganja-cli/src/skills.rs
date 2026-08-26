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
///
/// Each description opens with its source tag — the plugin that installed
/// the skill, or `(user)` for a directory the user placed or configured —
/// which is also what the `$` selector's rows open with, so the two surfaces
/// read alike. The full origin path stays the `/skills` dialog's detail;
/// this listing keeps to the tag.
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
    let mut lines = vec![format!("{:<name_width$}  (SOURCE) DESCRIPTION", "NAME")];
    lines.extend(found.iter().map(|skill| {
        let source = skill::origin(roots, skill)
            .map_or_else(|| "user".to_owned(), ganja_core::plugin::skill_source);
        let description = skill.description.as_deref().unwrap_or("(no description)");

        format!("{:<name_width$}  ({source}) {description}", skill.name)
    }));

    lines
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;

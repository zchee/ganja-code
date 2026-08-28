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
fn the_listing_aligns_names_and_tags_each_row_with_its_source() {
    let roots = Roots::none().with_paths([PathBuf::from("/home/skills")]);
    let lines = super::rows(
        &roots,
        &[
            skill("porting", Some("How to port."), "/home/skills/porting/SKILL.md"),
            skill("tdd", None, "/home/skills/tdd/SKILL.md"),
        ],
    );

    assert_eq!(
        lines,
        vec![
            "NAME     (SOURCE) DESCRIPTION".to_owned(),
            "porting  (user) How to port.".to_owned(),
            "tdd      (user) (no description)".to_owned(),
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

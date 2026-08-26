use std::path::PathBuf;

use ganja_tool::skill::Skill;

use super::SkillMenu;
use crate::mention::Fragment;

fn skill(name: &str, description: Option<&str>) -> (Skill, String) {
    tagged(name, description, "user")
}

fn tagged(name: &str, description: Option<&str>, source: &str) -> (Skill, String) {
    (
        Skill {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            location: PathBuf::from("SKILL.md"),
            content: String::new(),
        },
        source.to_owned(),
    )
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

/// Every row says where it came from: the source opens the description
/// column, and a skill with no description still shows its tag.
#[test]
fn a_row_opens_its_description_with_the_source_tag() {
    let menu = SkillMenu::new(
        fragment(""),
        &[
            tagged("ask-matt", Some("A router."), "mattpocock-skills"),
            skill("mine", None),
        ],
    );

    assert_eq!(
        menu.rows,
        vec![
            (
                "ask-matt".to_owned(),
                "(mattpocock-skills) A router.".to_owned()
            ),
            ("mine".to_owned(), "(user)".to_owned()),
        ]
    );
}

/// The tag is part of what a fragment can match, so a plugin's skills are
/// findable by the plugin's own name.
#[test]
fn a_fragment_can_match_the_source_tag() {
    let menu = SkillMenu::new(
        fragment("matt"),
        &[
            tagged("ask", Some("A router."), "mattpocock-skills"),
            skill("porting", Some("how to port")),
        ],
    );

    assert_eq!(menu.selected(), Some("ask"));
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

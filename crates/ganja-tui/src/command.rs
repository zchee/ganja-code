//! The commands the palette and the `/` dropdown offer, and how a typed
//! fragment narrows them.
//!
//! Spec: upstream `packages/tui/src/component/command-palette.tsx` and
//! `packages/tui/src/component/prompt/autocomplete.tsx`. Upstream has two
//! disjoint command populations — UI actions that dispatch immediately, and
//! engine commands that insert text and run on Enter — and only the first of
//! them reaches the palette. This module is that first population; the engine
//! half arrives with the commands that carry it.
//!
//! The names are upstream's, **plurals and aliases included**: `/models` not
//! `/model`, `mo` because upstream added it to bias a half-typed `/mo` toward
//! the model list. Getting those wrong would mean muscle memory transferring
//! from opencode and landing on nothing.
//!
//! Ranking is [`nucleo_matcher`]'s, not upstream's `fuzzysort`'s. Two
//! libraries scoring the same fragment identically is not something either of
//! them promises, and pinning it would mean reimplementing one of them —
//! **D10** says parity here is not a goal. What *is* pinned is that the order
//! is total and deterministic: ties break on the command's own name, so a
//! fragment always produces the same list.

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};

use crate::keybind;

/// What choosing a command does.
///
/// A UI action, always: everything here is something the frontend performs
/// itself, which is what makes the palette able to dispatch on selection
/// rather than typing text into the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Open the stored-session picker.
    Sessions,
    /// Open the model list for the provider this session runs on.
    Models,
    /// Open the list of agents the session may switch to.
    Agents,
    /// Open the theme picker.
    Themes,
    /// Open the key and command reference.
    Help,
    /// Leave.
    Exit,
}

impl Action {
    /// The binding whose key is shown on this command's row, where one action
    /// has a key of its own.
    ///
    /// Not every command does: reaching `/models` costs a palette open, which
    /// is exactly what the palette is for.
    #[must_use]
    pub fn keybind(self) -> Option<keybind::Action> {
        match self {
            Self::Sessions => Some(keybind::Action::SessionsOpen),
            Self::Themes => Some(keybind::Action::ThemesOpen),
            Self::Exit => Some(keybind::Action::AppExit),
            Self::Models | Self::Agents | Self::Help => None,
        }
    }
}

/// The group a command is listed under in the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Things done to the conversation.
    Session,
    /// Things done to who is answering it.
    Agent,
    /// Everything else.
    System,
}

impl Category {
    /// The heading the palette prints above the group.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// One command, as both surfaces list it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it does.
    pub action: Action,
    /// What it is called, without the leading slash.
    pub name: &'static str,
    /// Other spellings that reach it, upstream's set exactly.
    pub aliases: &'static [&'static str],
    /// The short label the palette shows.
    pub title: &'static str,
    /// The longer line the dropdown shows beside the name, and which the
    /// dropdown also matches against — upstream matches descriptions in slash
    /// mode and deliberately does not in `@` mode.
    pub description: &'static str,
    /// The palette group.
    pub category: Category,
    /// Whether the palette pins it at the top while nothing has been typed.
    pub suggested: bool,
}

impl Entry {
    /// How the command is typed: the leading slash included, because that is
    /// what both surfaces print and what the dropdown matches.
    #[must_use]
    pub fn slash(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Every command both surfaces offer.
///
/// `/new`, `/compact`, `/editor` and the engine-side commands are deliberately
/// absent rather than stubbed: a palette row that does nothing is worse than a
/// palette that does not claim to have the feature.
pub const COMMANDS: &[Entry] = &[
    Entry {
        action: Action::Sessions,
        name: "sessions",
        aliases: &["resume", "continue"],
        title: "Switch session",
        description: "Reopen a conversation this project has stored",
        category: Category::Session,
        suggested: true,
    },
    Entry {
        action: Action::Models,
        name: "models",
        // Upstream's own comment: `mo` exists so that a half-typed `/mo`
        // reaches the model list rather than one of its neighbours.
        aliases: &["mo"],
        title: "Switch model",
        description: "Ask the rest of this session of a different model",
        category: Category::Agent,
        suggested: true,
    },
    Entry {
        action: Action::Agents,
        name: "agents",
        aliases: &[],
        title: "Switch agent",
        description: "Run the rest of this session as a different agent",
        category: Category::Agent,
        suggested: false,
    },
    Entry {
        action: Action::Themes,
        name: "themes",
        aliases: &[],
        title: "Switch theme",
        description: "Repaint the screen in another palette",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Help,
        name: "help",
        aliases: &[],
        title: "Help",
        description: "List the commands and the keys that reach them",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Exit,
        name: "exit",
        aliases: &["quit", "q"],
        title: "Exit the app",
        description: "Leave ganja",
        category: Category::System,
        suggested: false,
    },
];

/// The words that quit when they are the whole prompt.
///
/// Upstream checks the trimmed buffer before anything else on submit, so
/// typing `exit` and pressing Enter leaves rather than asking the model what
/// it thinks about the word (`component/prompt/index.tsx:962-966`).
const BARE_EXITS: [&str; 3] = ["exit", "quit", ":q"];

/// Whether `text`, submitted on its own, means "leave".
#[must_use]
pub fn is_bare_exit(text: &str) -> bool {
    BARE_EXITS.contains(&text.trim())
}

/// The command `name` reaches, by name or by alias.
///
/// The leading slash is optional so that the same lookup serves the dropdown's
/// `/models` and a bare `models`.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static Entry> {
    let wanted = name.strip_prefix('/').unwrap_or(name);

    COMMANDS
        .iter()
        .find(|entry| entry.name == wanted || entry.aliases.contains(&wanted))
}

/// Which surface is matching, and therefore what a fragment is compared
/// against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The palette: the name, its aliases and the title.
    Palette,
    /// The dropdown, which also reads descriptions — upstream's slash mode
    /// adds `description` to its keys where `@` mode leaves it out.
    Dropdown,
}

/// The commands `query` narrows to, best match first.
///
/// An empty query is every command in table order, which is the order the
/// table is written in rather than anything computed: the palette groups them
/// itself and the dropdown sorts by name.
#[must_use]
pub fn matches(query: &str, surface: Surface) -> Vec<&'static Entry> {
    let needle = query.trim().trim_start_matches('/');
    if needle.is_empty() {
        return COMMANDS.iter().collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    // `Atom::new` rather than `Atom::parse`: a fragment the user typed is a
    // fragment, not a query language, so a leading `^` or a trailing `$`
    // should narrow by those characters instead of changing the match mode.
    let atom = Atom::new(
        needle,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );

    let mut scored: Vec<(u32, &'static Entry)> = COMMANDS
        .iter()
        .filter_map(|entry| score(&atom, &mut matcher, entry, surface).map(|score| (score, entry)))
        .collect();
    // Descending by score, then by name so that two commands scoring the same
    // always come out in the same order — the part of the ranking that is a
    // promise, where the score itself is the matcher's business (**D10**).
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.name.cmp(right.1.name))
    });

    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// The best score `atom` gets against any of `entry`'s matchable fields.
///
/// The maximum rather than the sum: a fragment that is the whole of an alias
/// should rank as well as one that is the whole of a name, and adding the
/// fields would instead reward commands for having more of them.
fn score(atom: &Atom, matcher: &mut Matcher, entry: &Entry, surface: Surface) -> Option<u32> {
    let mut buffer = Vec::new();
    let mut best = None;

    // The name is weighted above everything else, as upstream weights its
    // title above its category: typing `mo` is a guess at a name.
    let mut consider = |text: &str, weight: u32| {
        if let Some(score) = atom.score(Utf32Str::new(text, &mut buffer), matcher) {
            let scaled = u32::from(score) * weight;
            best = Some(best.map_or(scaled, |current: u32| current.max(scaled)));
        }
    };

    consider(entry.name, 2);
    for alias in entry.aliases {
        consider(alias, 2);
    }
    consider(entry.title, 1);
    if surface == Surface::Dropdown {
        consider(entry.description, 1);
    }

    best
}

#[cfg(test)]
mod tests {
    use super::{Action, COMMANDS, Category, Surface, is_bare_exit, lookup, matches};

    /// The plurals are the whole point of porting the names rather than
    /// inventing them: `/model` is upstream's *feature*, `/models` is its
    /// command.
    #[test]
    fn the_command_names_are_upstreams_plurals_with_upstreams_aliases() {
        let cases = [
            ("sessions", &["resume", "continue"][..], Action::Sessions),
            ("models", &["mo"][..], Action::Models),
            ("agents", &[][..], Action::Agents),
            ("themes", &[][..], Action::Themes),
            ("help", &[][..], Action::Help),
            ("exit", &["quit", "q"][..], Action::Exit),
        ];

        for (name, aliases, action) in cases {
            let entry = lookup(name).unwrap_or_else(|| panic!("/{name} should exist"));
            assert_eq!(entry.action, action, "/{name} should do its own thing");
            assert_eq!(entry.aliases, aliases, "/{name} aliases");
        }
        assert_eq!(
            COMMANDS.len(),
            cases.len(),
            "the table should hold exactly the commands this wave ships"
        );
    }

    /// The ones this wave deliberately does not ship. A row that opened
    /// nothing would be worse than no row.
    #[test]
    fn the_commands_whose_engine_half_is_missing_are_absent_rather_than_inert() {
        for name in ["new", "clear", "compact", "summarize", "editor", "init"] {
            assert!(lookup(name).is_none(), "/{name} should not be listed yet");
        }
    }

    #[test]
    fn an_alias_reaches_the_command_it_abbreviates() {
        let cases = [
            ("mo", Action::Models),
            ("/mo", Action::Models),
            ("resume", Action::Sessions),
            ("continue", Action::Sessions),
            ("q", Action::Exit),
            ("quit", Action::Exit),
        ];

        for (typed, action) in cases {
            assert_eq!(
                lookup(typed).map(|entry| entry.action),
                Some(action),
                "{typed} should reach {action:?}"
            );
        }
    }

    #[test]
    fn an_empty_query_lists_every_command() {
        assert_eq!(matches("", Surface::Palette).len(), COMMANDS.len());
        assert_eq!(matches("   ", Surface::Palette).len(), COMMANDS.len());
    }

    #[test]
    fn a_fragment_narrows_to_the_commands_that_contain_it() {
        let narrowed: Vec<&str> = matches("theme", Surface::Palette)
            .iter()
            .map(|entry| entry.name)
            .collect();

        assert_eq!(narrowed, vec!["themes"]);
    }

    /// Upstream's reason for the alias, asserted rather than assumed.
    #[test]
    fn the_mo_alias_puts_the_model_list_first() {
        let ranked = matches("mo", Surface::Palette);

        assert_eq!(
            ranked.first().map(|entry| entry.name),
            Some("models"),
            "got {:?}",
            ranked.iter().map(|entry| entry.name).collect::<Vec<_>>()
        );
    }

    /// The one difference between the two surfaces.
    #[test]
    fn only_the_dropdown_matches_a_fragment_that_appears_solely_in_a_description() {
        let fragment = "repaint";

        assert!(
            matches(fragment, Surface::Palette).is_empty(),
            "the palette should not read descriptions"
        );
        assert_eq!(
            matches(fragment, Surface::Dropdown)
                .first()
                .map(|entry| entry.name),
            Some("themes")
        );
    }

    /// Ranking parity with upstream is not a goal; a stable order is.
    #[test]
    fn the_same_fragment_always_produces_the_same_order() {
        let once: Vec<&str> = matches("s", Surface::Palette)
            .iter()
            .map(|entry| entry.name)
            .collect();

        for _ in 0..8 {
            let again: Vec<&str> = matches("s", Surface::Palette)
                .iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(once, again);
        }
    }

    #[test]
    fn a_fragment_nothing_carries_narrows_to_nothing() {
        assert!(matches("zzzz", Surface::Dropdown).is_empty());
    }

    #[test]
    fn the_bare_words_that_quit_are_upstreams_three() {
        for typed in ["exit", "quit", ":q", "  exit  ", "\tquit\n"] {
            assert!(is_bare_exit(typed), "{typed:?} should quit");
        }
        for typed in ["exiting", "q!", "quit now", "", "/exit"] {
            assert!(!is_bare_exit(typed), "{typed:?} should not quit");
        }
    }

    /// The palette groups by category, so every command needs one that reads
    /// as a heading.
    #[test]
    fn every_category_has_a_heading() {
        for category in [Category::Session, Category::Agent, Category::System] {
            assert!(!category.label().is_empty());
        }
    }
}

//! The commands the palette and the `/` dropdown offer, and how a typed
//! fragment narrows them.
//!
//! Spec: upstream `packages/tui/src/component/command-palette.tsx` and
//! `packages/tui/src/component/prompt/autocomplete.tsx`. Upstream has two
//! disjoint command populations — UI actions that dispatch immediately, and
//! engine commands that insert text and run on Enter — and both live here:
//! [`Entry`] is the first, [`EngineCommand`] the second, and [`Choice`] is
//! what the `/` dropdown offers once they are merged. Only the first reaches
//! the palette, because the palette has no way to take the arguments an
//! engine command expects.
//!
//! The names follow upstream except the documented `/effort` deviation,
//! **plurals and aliases included**: `/models` not `/model`, `mo` because
//! upstream added it to bias a half-typed `/mo` toward the model list. Getting
//! those wrong would mean muscle memory transferring from opencode and landing
//! on nothing.
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
    /// Start a fresh session, and empty the screen the old one filled.
    New,
    /// Summarize the conversation so far and carry on from the summary.
    Compact,
    /// Compose the prompt in `$EDITOR`.
    Editor,
    /// Open the model list for the provider this session runs on.
    Models,
    /// Open the flat picker over the active model's catalog efforts.
    Effort,
    /// Open the list of agents the session may switch to.
    Agents,
    /// Open the theme picker.
    Themes,
    /// Open the `/mcp` dialog: every configured server's status and tool
    /// count, with Reconnect on a failed one.
    Mcp,
    /// Open the `/context` panel: what fills the model's context window,
    /// estimated per category (**D470**).
    Context,
    /// Open the `/usage` panel: what this session has spent (**D471**).
    Usage,
    /// Open the key and command reference.
    Help,
    /// Leave.
    Exit,
    /// Put the whole conversation on the clipboard.
    Copy,
    /// Put the model's last reply on the clipboard.
    CopyMessage,
    /// Take back the last prompt, and the file changes its turn made.
    Undo,
    /// Put back what an undo took, one prompt at a time.
    Redo,
    /// Open the picker that takes the session back to a checkpoint.
    Rewind,
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
            // `/undo` and `/redo` are upstream's `<leader>u` and `<leader>r`,
            // and ganja has no leader (**D4**) — so both are reached by name,
            // from the palette or the `/` menu, and by nothing else. `/rewind`
            // has no chord of its own either: its second door is the Esc Esc
            // gesture, which the binding table cannot express (**D452**, at the
            // Esc arm in `app.rs`).
            Self::New
            | Self::Compact
            | Self::Editor
            | Self::Models
            | Self::Effort
            | Self::Agents
            | Self::Mcp
            | Self::Context
            | Self::Usage
            | Self::Help
            | Self::Copy
            | Self::CopyMessage
            | Self::Undo
            | Self::Redo
            | Self::Rewind => None,
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

/// Every UI command both surfaces offer.
///
/// The engine's own commands — `/init` and whatever a config file adds — are
/// deliberately not here: they take arguments, so choosing one types its name
/// into the buffer instead of running it, and a palette has nowhere to type.
/// They reach the dropdown as [`EngineCommand`]s instead.
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
        action: Action::New,
        name: "new",
        aliases: &["clear"],
        title: "New session",
        description: "Leave this conversation and start an empty one",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Compact,
        name: "compact",
        aliases: &["summarize"],
        title: "Compact session",
        description: "Summarize the conversation so far and carry on from it",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Editor,
        name: "editor",
        aliases: &[],
        title: "Open editor",
        description: "Write the prompt in $EDITOR instead of the composer",
        category: Category::Session,
        suggested: false,
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
        action: Action::Effort,
        name: "effort",
        aliases: &[],
        // Upstream's `variant.list` command, filed under its Agent category. Deviation
        // `effort-not-variants`: upstream's slash name is "variants" and its title
        // "Switch model variant"; ganja surfaces the same catalog mechanism as
        // "effort" by owner decision. The catalog field keeps upstream's schema name.
        title: "Switch model effort",
        description: "Run the model under one of its catalog efforts",
        category: Category::Agent,
        suggested: false,
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
        action: Action::Mcp,
        name: "mcp",
        aliases: &[],
        title: "MCP servers",
        description: "See what every configured server lends, and reconnect a failed one",
        category: Category::System,
        suggested: false,
    },
    // Claude Code's `/context` and `/usage`, with no upstream opencode
    // counterpart at all (**D470**, **D471**). Filed under `System` for the
    // copy commands' reason: both panels look at the conversation and do
    // nothing to it.
    Entry {
        action: Action::Context,
        name: "context",
        aliases: &[],
        title: "Context usage",
        description: "See what fills the model's context window, estimated per category",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Usage,
        name: "usage",
        aliases: &[],
        title: "Session usage",
        description: "See what this session has spent, and its cache hit rate",
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
    // Upstream files both of these under its `Session` category; here they are
    // `System`, because ganja's `Session` is documented as *things done to the
    // conversation* and taking a copy does nothing to it (deviation:
    // copy-commands-categorised-system). Upstream's own titles and slash names
    // are kept exactly.
    Entry {
        action: Action::Copy,
        name: "copy",
        aliases: &[],
        title: "Copy session transcript",
        description: "Put the whole conversation on the clipboard, as markdown",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::CopyMessage,
        name: "copy-message",
        aliases: &[],
        title: "Copy message",
        description: "Put the model's last reply on the clipboard",
        category: Category::System,
        suggested: false,
    },
    // Upstream's titles and slash names, in upstream's own `Session` category:
    // taking a prompt back is the plainest thing there is to do *to* the
    // conversation. Both sit at the end of the table on purpose — the palette
    // lists it in order, and the rows a stock terminal can show are the ones
    // above the clip.
    Entry {
        action: Action::Undo,
        name: "undo",
        aliases: &[],
        title: "Undo previous message",
        description: "Take back the last prompt and the file changes its turn made",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Redo,
        name: "redo",
        aliases: &[],
        title: "Redo",
        description: "Put back what an undo took, one prompt at a time",
        category: Category::Session,
        suggested: false,
    },
    // Upstream reaches its message-revert dialog from the transcript rather
    // than by name, so the slash name is Claude Code's (**D451**, at
    // `RevertScope`). It sits with `/undo` and `/redo` because it is the same
    // thing done to the conversation, with the checkpoint and the scope chosen
    // rather than assumed.
    Entry {
        action: Action::Rewind,
        name: "rewind",
        aliases: &[],
        title: "Rewind to a checkpoint",
        description: "Restore the code and/or conversation to an earlier prompt",
        category: Category::Session,
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

/// The UI command a submitted buffer names on its own, or nothing when the
/// buffer is prose.
///
/// The dropdown cannot be the only door to these: its menu closes the moment
/// a space follows the name (`dropdown::triggered`), and Tab completion
/// deliberately leaves `/exit ` in the buffer (**D446**) — so the Enter that
/// follows has to read the text itself, which is how Claude Code and Codex
/// both dispatch a slash command. Only a leading `/name` (or alias) with
/// nothing but whitespace after it qualifies: these commands take no
/// arguments, so `/models gpt` stays text, under the same ruling that keeps
/// an unknown slash command out of the UI's hands.
#[must_use]
pub fn submitted(text: &str) -> Option<&'static Entry> {
    let name = text.strip_prefix('/')?.trim_end();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }

    lookup(name)
}

/// One command the **engine** offers, as the dropdown lists it.
///
/// Owned rather than borrowed because this half of the roster is resolved at
/// runtime: `/init` is compiled in, but everything beside it comes out of the
/// user's config file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineCommand {
    /// What the user types after the slash.
    pub name: String,
    /// The one line the engine had to say about it, where it said anything.
    pub description: Option<String>,
}

impl EngineCommand {
    /// Every command `registry` holds, in the order it lists them.
    #[must_use]
    pub fn roster(registry: &ganja_core::command::Registry) -> Vec<Self> {
        registry
            .commands()
            .iter()
            .map(|definition| Self {
                name: definition.name.clone(),
                description: definition.description.clone(),
            })
            .collect()
    }
}

/// One row the `/` dropdown offers, from either population.
///
/// The two halves differ in what choosing them does, which is the whole reason
/// upstream keeps them apart: a UI command runs, an engine command is typed
/// into the buffer and waits for the arguments its template expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// A UI command, which runs the moment it is chosen.
    Ui(&'static Entry),
    /// An engine command, which is inserted as `/name ` and runs on Enter.
    Engine(EngineCommand),
}

impl Choice {
    /// How the row is spelled, the leading slash included.
    #[must_use]
    pub fn slash(&self) -> String {
        match self {
            Self::Ui(entry) => entry.slash(),
            Self::Engine(command) => format!("/{}", command.name),
        }
    }

    /// The line shown beside the name.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::Ui(entry) => entry.description,
            Self::Engine(command) => command.description.as_deref().unwrap_or_default(),
        }
    }

    /// The name a row sorts under, which is the name without its slash.
    fn name(&self) -> &str {
        match self {
            Self::Ui(entry) => entry.name,
            Self::Engine(command) => &command.name,
        }
    }
}

/// The rows `query` narrows to across both populations, best match first.
///
/// An empty query is every row: the UI commands in table order, then the
/// engine's in the order the registry lists them. The dropdown re-sorts that
/// case by name, because with nothing typed there is no ranking to show.
#[must_use]
pub fn dropdown_matches(query: &str, engine: &[EngineCommand]) -> Vec<Choice> {
    let needle = query.trim().trim_start_matches('/');
    if needle.is_empty() {
        return COMMANDS
            .iter()
            .map(Choice::Ui)
            .chain(engine.iter().cloned().map(Choice::Engine))
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = fragment(needle);

    let mut scored: Vec<(u32, Choice)> = COMMANDS
        .iter()
        .filter_map(|entry| {
            score(&atom, &mut matcher, entry, Surface::Dropdown)
                .map(|score| (score, Choice::Ui(entry)))
        })
        .collect();
    scored.extend(engine.iter().filter_map(|command| {
        score_engine(&atom, &mut matcher, command)
            .map(|score| (score, Choice::Engine(command.clone())))
    }));
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.name().cmp(right.1.name()))
    });

    scored.into_iter().map(|(_, choice)| choice).collect()
}

/// The best score `atom` gets against an engine command's name or its
/// description — the same two fields, and the same weights, the dropdown
/// scores a UI command by.
fn score_engine(atom: &Atom, matcher: &mut Matcher, command: &EngineCommand) -> Option<u32> {
    let mut buffer = Vec::new();
    let mut best = None;
    let mut consider = |text: &str, weight: u32| {
        if let Some(score) = atom.score(Utf32Str::new(text, &mut buffer), matcher) {
            let scaled = u32::from(score) * weight;
            best = Some(best.map_or(scaled, |current: u32| current.max(scaled)));
        }
    };

    consider(&command.name, 2);
    if let Some(description) = &command.description {
        consider(description, 1);
    }

    best
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
    let atom = fragment(needle);

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

/// The needle a typed fragment becomes.
///
/// `Atom::new` rather than `Atom::parse`: a fragment the user typed is a
/// fragment, not a query language, so a leading `^` or a trailing `$` should
/// narrow by those characters instead of changing the match mode.
fn fragment(needle: &str) -> Atom {
    Atom::new(
        needle,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    )
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
    use super::{
        Action, COMMANDS, Category, Choice, EngineCommand, Surface, dropdown_matches, is_bare_exit,
        lookup, matches, submitted,
    };

    /// The commands the engine offers a session that loaded no config: one,
    /// and its description is upstream's own string.
    fn engine() -> Vec<EngineCommand> {
        vec![EngineCommand {
            name: "init".to_owned(),
            description: Some("guided AGENTS.md setup".to_owned()),
        }]
    }

    /// The spellings and aliases are the command surface's contract, including
    /// ganja's deliberate `/effort` deviation.
    #[test]
    fn the_command_names_and_aliases_match_their_surface_contract() {
        let cases = [
            ("sessions", &["resume", "continue"][..], Action::Sessions),
            ("new", &["clear"][..], Action::New),
            ("compact", &["summarize"][..], Action::Compact),
            ("editor", &[][..], Action::Editor),
            ("models", &["mo"][..], Action::Models),
            ("effort", &[][..], Action::Effort),
            ("agents", &[][..], Action::Agents),
            ("themes", &[][..], Action::Themes),
            ("mcp", &[][..], Action::Mcp),
            ("context", &[][..], Action::Context),
            ("usage", &[][..], Action::Usage),
            ("help", &[][..], Action::Help),
            ("exit", &["quit", "q"][..], Action::Exit),
            ("copy", &[][..], Action::Copy),
            ("copy-message", &[][..], Action::CopyMessage),
            ("undo", &[][..], Action::Undo),
            ("redo", &[][..], Action::Redo),
            ("rewind", &[][..], Action::Rewind),
        ];

        for (name, aliases, action) in cases {
            let entry = lookup(name).unwrap_or_else(|| panic!("/{name} should exist"));
            assert_eq!(entry.action, action, "/{name} should do its own thing");
            assert_eq!(entry.aliases, aliases, "/{name} aliases");
        }
        assert_eq!(
            COMMANDS.len(),
            cases.len(),
            "the table should hold exactly the UI commands this build ships"
        );
    }

    /// **R13**: two distinct copy commands, each reachable from *both*
    /// surfaces. One command set and two views of it is the architecture
    /// rule, so a row that reached only the dropdown would be a second set.
    #[test]
    fn both_copy_commands_are_offered_by_the_palette_and_by_the_dropdown() {
        let cases = [
            ("copy", "Copy session transcript"),
            ("copy-message", "Copy message"),
        ];

        for (name, title) in cases {
            let entry = lookup(name).unwrap_or_else(|| panic!("/{name} should exist"));
            assert_eq!(entry.title, title, "/{name} is titled upstream's way");

            for surface in [Surface::Palette, Surface::Dropdown] {
                assert!(
                    matches(name, surface)
                        .iter()
                        .any(|found| found.name == name),
                    "/{name} should be offered on {surface:?}"
                );
            }
            assert!(
                dropdown_matches(name, &engine())
                    .iter()
                    .any(|choice| choice.slash() == format!("/{name}")),
                "/{name} should be offered by the merged dropdown roster"
            );
        }
    }

    /// **R10**: both halves of the revert reach the palette *and* the `/`
    /// dropdown. There is no key that reaches either (**D4**), so a row that
    /// made it to only one surface would leave the other command unreachable
    /// from that surface entirely.
    #[test]
    fn undo_and_redo_are_offered_by_the_palette_and_by_the_dropdown() {
        let cases = [("undo", "Undo previous message"), ("redo", "Redo")];

        for (name, title) in cases {
            let entry = lookup(name).unwrap_or_else(|| panic!("/{name} should exist"));
            assert_eq!(entry.title, title, "/{name} is titled upstream's way");
            assert_eq!(
                entry.category,
                Category::Session,
                "/{name} does something to the conversation"
            );
            assert_eq!(
                entry.action.keybind(),
                None,
                "/{name} has no binding: `<leader>` is unported"
            );

            for surface in [Surface::Palette, Surface::Dropdown] {
                assert!(
                    matches(name, surface)
                        .iter()
                        .any(|found| found.name == name),
                    "/{name} should be offered on {surface:?}"
                );
            }
            assert!(
                dropdown_matches(name, &engine())
                    .iter()
                    .any(|choice| choice.slash() == format!("/{name}")),
                "/{name} should be offered by the merged dropdown roster"
            );
        }
    }

    /// The two are distinct commands rather than one with an argument, which
    /// is what makes each of them a single row to choose.
    #[test]
    fn copying_the_transcript_and_copying_a_message_are_different_commands() {
        assert_ne!(
            lookup("copy").map(|entry| entry.action),
            lookup("copy-message").map(|entry| entry.action)
        );
    }

    /// The engine's own commands are not UI commands, however they are
    /// spelled: choosing one has to type its name into the buffer so that the
    /// arguments its template expects can follow.
    #[test]
    fn an_engine_command_is_not_in_the_ui_table() {
        for name in ["init", "review"] {
            assert!(
                lookup(name).is_none(),
                "/{name} is the engine's, not a UI row"
            );
        }
    }

    #[test]
    fn the_dropdown_offers_both_populations() {
        let rows = dropdown_matches("", &engine());

        assert_eq!(
            rows.len(),
            COMMANDS.len() + 1,
            "every UI command plus the engine's one"
        );
        assert!(
            rows.contains(&Choice::Engine(engine().remove(0))),
            "the engine's command should be listed: {rows:?}"
        );
    }

    /// Both fields, and the weights are the UI table's: a fragment that is
    /// part of a name outranks one that is only part of a description, which
    /// is why the description case here uses a word no command is named after.
    #[test]
    fn a_fragment_reaches_an_engine_command_by_name_and_by_description() {
        for fragment in ["ini", "guided"] {
            assert_eq!(
                dropdown_matches(fragment, &engine())
                    .first()
                    .map(Choice::slash),
                Some("/init".to_owned()),
                "{fragment:?} should rank /init first"
            );
        }
    }

    /// A row has to say which population it came from, because that is what
    /// decides whether choosing it runs something or types something.
    #[test]
    fn a_ui_row_and_an_engine_row_are_told_apart_by_their_own_shape() {
        let rows = dropdown_matches("", &engine());
        let engine_rows: Vec<&Choice> = rows
            .iter()
            .filter(|row| matches!(row, Choice::Engine(_)))
            .collect();

        assert_eq!(engine_rows.len(), 1);
        assert_eq!(engine_rows[0].slash(), "/init");
        assert_eq!(engine_rows[0].description(), "guided AGENTS.md setup");
    }

    #[test]
    fn an_engine_command_with_nothing_to_say_still_lists() {
        let roster = vec![EngineCommand {
            name: "silent".to_owned(),
            description: None,
        }];

        let rows = dropdown_matches("silent", &roster);

        assert_eq!(rows.first().map(Choice::slash), Some("/silent".to_owned()));
        assert_eq!(rows[0].description(), "");
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

    /// What submit itself recognizes, with the dropdown long closed: the
    /// name stands alone, trailing whitespace tolerated because a Tab
    /// completion or a stray space leaves some behind.
    #[test]
    fn a_submitted_buffer_names_a_command_only_when_the_name_stands_alone() {
        assert_eq!(
            submitted("/models ").map(|entry| entry.action),
            Some(Action::Models)
        );
        assert_eq!(
            submitted("/mo ").map(|entry| entry.action),
            Some(Action::Models),
            "an alias reaches the same command"
        );
        assert_eq!(
            submitted("/exit").map(|entry| entry.action),
            Some(Action::Exit),
            "no whitespace at all is the plain spelling"
        );

        for text in [
            "models",
            " /models",
            "/models gpt",
            "/",
            "/ models",
            "/nonesuch ",
            "what about /models",
        ] {
            assert!(submitted(text).is_none(), "{text:?} should be prose");
        }
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

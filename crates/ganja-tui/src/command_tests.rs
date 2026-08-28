use super::{
    Action, BACKENDS, COMMANDS, Category, Choice, Completion, EngineCommand, SPAWN_GRAMMAR,
    Surface, Team, TeamSpawn, dropdown_matches, inline_hint, is_bare_exit, lookup, matches,
    submitted, team, team_completion, value_matches,
};

/// The commands the engine offers a session that loaded no config: one,
/// and its description is upstream's own string.
fn engine() -> Vec<EngineCommand> {
    vec![EngineCommand {
        name: "init".to_owned(),
        description: Some("guided AGENTS.md setup".to_owned()),
        hint: None,
    }]
}

/// **D518.** A complete builtin name with nothing after it hints; the
/// first argument character removes the hint.
#[test]
fn a_typed_team_hints_until_an_argument_arrives() {
    assert!(inline_hint("/team", &[]).is_some());
    assert!(inline_hint("/team ", &[]).is_some());
    assert_eq!(inline_hint("/team s", &[]), None);
    assert_eq!(inline_hint("/tea", &[]), None);
    assert_eq!(inline_hint("team", &[]), None);
}

/// **D518.** `/team spawn` with nothing named yet shows the one spelled
/// spawn grammar — the refusal's and the dialog's own string.
#[test]
fn a_bare_team_spawn_hints_the_spawn_grammar() {
    assert_eq!(inline_hint("/team spawn", &[]).as_deref(), Some(SPAWN_GRAMMAR));
    assert_eq!(inline_hint("/team spawn ", &[]).as_deref(), Some(SPAWN_GRAMMAR));
    assert_eq!(
        inline_hint("/team spawn w1", &[]).as_deref(),
        Some("[--backend <surface>] [--agent <kind>] [what it should do]")
    );
}

/// **D518.** Arguments consume the hint front to back — the token still
/// being typed included — so what remains stays standing (2026-08-24
/// screenshot: `foo` typed, the flags still there).
#[test]
fn typed_arguments_consume_the_hint_front_to_back() {
    let flags = "[--backend <surface>] [--agent <kind>] [what it should do]";
    assert_eq!(inline_hint("/team spawn w1", &[]).as_deref(), Some(flags));
    assert_eq!(inline_hint("/team spawn w1 ", &[]).as_deref(), Some(flags));
    assert_eq!(
        inline_hint("/team spawn w1 --backend ganja", &[]).as_deref(),
        Some("[--agent <kind>] [what it should do]")
    );
    assert_eq!(
        inline_hint("/team spawn w1 --back", &[]).as_deref(),
        Some("[--agent <kind>] [what it should do]"),
        "a flag still being typed already names its slot"
    );
    assert_eq!(
        inline_hint("/team spawn w1 fix the tests", &[]),
        None,
        "the first prompt word takes the last slot, and the words after it are prose"
    );
    assert_eq!(
        inline_hint("/team spawn w1 --bogus", &[]),
        None,
        "a flag the grammar has not got silences the hint rather than guessing"
    );
}

/// **D518.** `shutdown` hints its optional member until one is named.
#[test]
fn a_shutdown_line_hints_its_member_until_one_is_named() {
    assert_eq!(inline_hint("/team shutdown", &[]).as_deref(), Some("[member]"));
    assert_eq!(inline_hint("/team shutdown ", &[]).as_deref(), Some("[member]"));
    assert_eq!(inline_hint("/team shutdown w1", &[]), None);
}

/// **D518.** A command file's own `argument-hint` reaches the composer,
/// and a command without one hints nothing.
#[test]
fn an_engine_command_hints_what_its_file_declared() {
    let roster = vec![
        EngineCommand {
            name: "fix".to_owned(),
            description: Some("fix an issue — <issue>".to_owned()),
            hint: Some("<issue>".to_owned()),
        },
        EngineCommand { name: "plain".to_owned(), description: None, hint: None },
    ];
    assert_eq!(inline_hint("/fix", &roster).as_deref(), Some("<issue>"));
    assert_eq!(inline_hint("/fix now", &roster), None);
    assert_eq!(inline_hint("/plain", &roster), None);
    assert_eq!(inline_hint("/absent", &roster), None);
}

/// **D518.** A multi-line buffer is prose with a slash in it, never a
/// command awaiting arguments.
#[test]
fn a_multiline_buffer_hints_nothing() {
    assert_eq!(inline_hint("/team\nmore", &[]), None);
}

fn kinds() -> Vec<Completion> {
    ["general", "explore"]
        .into_iter()
        .map(|name| Completion { text: name.to_owned(), detail: String::new() })
        .collect()
}

fn texts(choices: &[Choice]) -> Vec<String> {
    choices.iter().map(Choice::slash).collect()
}

/// **D519.** The surfaces after `--backend` are the parser's own list,
/// narrowed by what has been typed, with the default saying so.
#[test]
fn the_backend_slot_offers_the_parsers_own_six() {
    let text = "/team spawn foo --backend g";
    let slot = team_completion(text, (0, text.len()), &kinds()).expect("a backend slot");
    assert_eq!(slot.title, " backends ");
    assert_eq!(slot.partial, "g");
    assert_eq!(slot.start, text.len() - 1);
    assert_eq!(
        slot.candidates.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        BACKENDS.to_vec()
    );
    assert!(
        slot.candidates.iter().any(|c| c.text == "ganja" && c.detail.contains("default")),
        "the default surface says so"
    );

    let narrowed = texts(&value_matches(&slot.partial, &slot.candidates));
    assert!(narrowed.contains(&"ganja".to_owned()) && narrowed.contains(&"grok".to_owned()));
    assert!(!narrowed.contains(&"claude".to_owned()));
}

/// **D519.** Every slot the grammar has: subcommand, flags, agent kinds —
/// and none where a name or a prompt word goes.
#[test]
fn every_team_slot_completes_and_free_words_do_not() {
    let at_end = |text: &str| team_completion(text, (0, text.len()), &kinds());

    assert_eq!(at_end("/team sp").map(|s| (s.title, s.partial)), Some((" team ", "sp".to_owned())));
    assert_eq!(at_end("/team spawn foo --").map(|s| s.title), Some(" flags "));
    assert_eq!(at_end("/team spawn foo --agent ex").map(|s| s.title), Some(" agents "));
    assert_eq!(at_end("/team spawn fo"), None, "a name is anyone's");
    assert_eq!(at_end("/team spawn foo fix it"), None, "prompt words are prose");
    assert_eq!(at_end("/team spawn foo --backend ganja "), None, "a filled slot is done");
    assert_eq!(at_end("/skills sp"), None);
    assert_eq!(at_end("team spawn --backend "), None, "no slash, no command");
}

/// **D519.** A word that already is a candidate raises no menu, so the
/// Enter after a fully typed value sends the line instead of feeding the
/// menu — the pane drills' own `/team spawn w1 --backend ganja` + Enter.
#[test]
fn a_fully_typed_value_leaves_enter_to_the_line() {
    let at_end = |text: &str| team_completion(text, (0, text.len()), &kinds());
    assert_eq!(at_end("/team spawn w1 --backend ganja"), None);
    assert_eq!(at_end("/team spawn"), None);
    assert_eq!(at_end("/team spawn w1 --agent explore"), None);
    assert!(at_end("/team spawn w1 --backend ganj").is_some());
}

/// **D519.** A flag the line already carries is not offered twice.
#[test]
fn a_flag_already_given_is_not_offered_again() {
    let text = "/team spawn foo --backend ganja --";
    let slot = team_completion(text, (0, text.len()), &kinds()).expect("a flag slot");
    assert_eq!(
        slot.candidates.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        vec!["--agent"]
    );
}

/// **D519.** Completing mid-line replaces the word under the cursor and
/// nothing after it: the span is measured to the cursor, not to the end.
#[test]
fn the_slot_spans_only_the_word_under_the_cursor() {
    let text = "/team spawn foo --backend gr fix it";
    let cursor = "/team spawn foo --backend gr".len();
    let slot = team_completion(text, (0, cursor), &kinds()).expect("a backend slot");
    assert_eq!(slot.partial, "gr");
    assert_eq!(slot.start, cursor - 2);
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
        ("skills", &[][..], Action::Skills),
        ("context", &[][..], Action::Context),
        ("usage", &[][..], Action::Usage),
        ("plugin", &[][..], Action::Plugin),
        ("team", &[][..], Action::Team),
        ("held", &[][..], Action::Held),
        ("help", &[][..], Action::Help),
        ("exit", &["quit", "q"][..], Action::Exit),
        ("copy", &[][..], Action::Copy),
        ("copy-message", &[][..], Action::CopyMessage),
        ("undo", &[][..], Action::Undo),
        ("redo", &[][..], Action::Redo),
        ("rewind", &[][..], Action::Rewind),
        ("rename", &[][..], Action::Rename),
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
    let cases = [("copy", "Copy session transcript"), ("copy-message", "Copy message")];

    for (name, title) in cases {
        let entry = lookup(name).unwrap_or_else(|| panic!("/{name} should exist"));
        assert_eq!(entry.title, title, "/{name} is titled upstream's way");

        for surface in [Surface::Palette, Surface::Dropdown] {
            assert!(
                matches(name, surface).iter().any(|found| found.name == name),
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
        assert_eq!(entry.category, Category::Session, "/{name} does something to the conversation");
        assert_eq!(entry.action.keybind(), None, "/{name} has no binding: `<leader>` is unported");

        for surface in [Surface::Palette, Surface::Dropdown] {
            assert!(
                matches(name, surface).iter().any(|found| found.name == name),
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
        assert!(lookup(name).is_none(), "/{name} is the engine's, not a UI row");
    }
}

#[test]
fn the_dropdown_offers_both_populations() {
    let rows = dropdown_matches("", &engine());

    assert_eq!(rows.len(), COMMANDS.len() + 1, "every UI command plus the engine's one");
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
            dropdown_matches(fragment, &engine()).first().map(Choice::slash),
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
    let engine_rows: Vec<&Choice> =
        rows.iter().filter(|row| matches!(row, Choice::Engine(_))).collect();

    assert_eq!(engine_rows.len(), 1);
    assert_eq!(engine_rows[0].slash(), "/init");
    assert_eq!(engine_rows[0].description(), "guided AGENTS.md setup");
}

#[test]
fn an_engine_command_with_nothing_to_say_still_lists() {
    let roster = vec![EngineCommand { name: "silent".to_owned(), description: None, hint: None }];

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
    let narrowed: Vec<&str> =
        matches("theme", Surface::Palette).iter().map(|entry| entry.name).collect();

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
        matches(fragment, Surface::Dropdown).first().map(|entry| entry.name),
        Some("themes")
    );
}

/// Ranking parity with upstream is not a goal; a stable order is.
#[test]
fn the_same_fragment_always_produces_the_same_order() {
    let once: Vec<&str> = matches("s", Surface::Palette).iter().map(|entry| entry.name).collect();

    for _ in 0..8 {
        let again: Vec<&str> =
            matches("s", Surface::Palette).iter().map(|entry| entry.name).collect();
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
    assert_eq!(submitted("/models ").map(|entry| entry.action), Some(Action::Models));
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

    for text in
        ["models", " /models", "/models gpt", "/", "/ models", "/nonesuch ", "what about /models"]
    {
        assert!(submitted(text).is_none(), "{text:?} should be prose");
    }
}

/// The one command here that takes arguments, and the subcommands it
/// takes them for. A bare `/team` means the same thing as `/team list`,
/// which is why both reach [`Team::List`] rather than two kinds of open.
#[test]
fn a_team_line_reaches_every_subcommand_the_grammar_has() {
    for text in ["/team", "/team ", "/team\n", "/team list", "/team list  "] {
        assert_eq!(team(text), Some(Team::List), "{text:?} should list");
    }
    assert_eq!(
        team("/team shutdown"),
        Some(Team::Shutdown { member: None }),
        "no name is the whole team"
    );
    assert_eq!(team("/team shutdown w1"), Some(Team::Shutdown { member: Some("w1".to_owned()) }));
}

/// Flags come before the prompt, and the first word that is not a flag
/// begins it — so AC-11's own spelling parses with no prompt at all, and a
/// prompt keeps its dashes.
#[test]
fn a_team_spawn_line_takes_its_flags_before_its_prompt() {
    let cases = [
        (
            "/team spawn w1 --backend ganja",
            TeamSpawn {
                name: "w1".to_owned(),
                backend: Some("ganja".to_owned()),
                agent_type: None,
                prompt: String::new(),
            },
        ),
        (
            "/team spawn w1",
            TeamSpawn {
                name: "w1".to_owned(),
                backend: None,
                agent_type: None,
                prompt: String::new(),
            },
        ),
        (
            "/team spawn w1 --agent explore --backend claude read the tree --carefully",
            TeamSpawn {
                name: "w1".to_owned(),
                backend: Some("claude".to_owned()),
                agent_type: Some("explore".to_owned()),
                prompt: "read the tree --carefully".to_owned(),
            },
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(team(text), Some(Team::Spawn(expected)), "{text:?}");
    }
}

/// Every way of getting a `/team` line wrong is answered with a sentence
/// naming what could not be taken, rather than being sent to the model as
/// a question about itself.
#[test]
fn a_team_line_this_grammar_has_not_got_is_refused_by_name() {
    let cases = [
        ("/team nonesuch", "nonesuch"),
        ("/team spawn", "/team spawn"),
        ("/team spawn --backend ganja", "/team spawn"),
        ("/team spawn w1 --backend", "--backend"),
        ("/team spawn w1 --agent", "--agent"),
        ("/team spawn w1 --nonesuch go", "--nonesuch"),
        // The flag P25 had and D513 retired is refused like any other
        // word this grammar has not got, not quietly swallowed.
        ("/team spawn w1 --bypass go", "--bypass"),
        ("/team list w1", "w1"),
        ("/team shutdown w1 w2", "w2"),
    ];

    for (text, named) in cases {
        let Some(Team::Refused(refusal)) = team(text) else {
            panic!("{text:?} should be refused, got {:?}", team(text));
        };
        assert!(refusal.contains(named), "{text:?} should name {named:?}: {refusal}");
    }
}

/// Nothing that is not a `/team` line has an opinion here — it is prose,
/// or it is another command's.
#[test]
fn only_a_team_line_reaches_the_team_grammar() {
    for text in ["/teammate w1", "/models", "team spawn w1", "tell /team spawn w1", ""] {
        assert_eq!(team(text), None, "{text:?} is not a /team line");
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

/// What the `/team` dialog's free-text step takes is the same grammar
/// the composer line takes after `/team spawn `, so a spawn decided in the
/// dialog is remembered in the prompt history as that line — and this is
/// the invariant the remembering rests on: put `/team spawn ` back in
/// front of the dialog's words and the composer reads the very same
/// spawn. A grammar that grew a positional flag, or a trim that moved,
/// would otherwise recall a line that spawns something other than what
/// was spawned, silently.
#[test]
fn a_dialog_spawn_re_emitted_as_a_team_spawn_line_reads_back_the_same() {
    for typed in [
        "w1",
        "w1 --backend ganja",
        "w1 --agent explore",
        "w1 --agent explore --backend claude read the tree --carefully",
        "w1 --backend codex explain  this  crate, twice-spaced and dashed-for-good",
        "  w1 --backend grok hold the fort  ",
    ] {
        let dialog = super::team_spawn(typed).expect("the dialog's grammar takes it");
        assert_eq!(
            super::team(&format!("/team spawn {typed}")),
            Some(Team::Spawn(dialog)),
            "{typed:?}: the composer line reads back the dialog's spawn"
        );
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

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{ACTIONS, Action, KeybindError, Keybinds, key, parse};

fn pressed(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn configured(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn every_default_binding_parses() {
    for (action, name, default) in ACTIONS {
        let keys = parse(default).unwrap_or_else(|bad| panic!("{name}: {bad} did not parse"));
        // `themes_open` is the one deliberate exception (**D453**): its
        // default is the empty chord, so it loads reachable by nothing.
        if *action == Action::ThemesOpen {
            assert!(keys.is_empty(), "{action:?} should bind nothing by default");
        } else {
            assert!(!keys.is_empty(), "{action:?} should bind something");
        }
    }
}

/// The untested branch **D453** relies on: `parse` treats an empty value
/// as zero alternatives rather than a parse error, so `themes_open`'s
/// empty default loads cleanly into an action nothing reaches — rather
/// than failing the run the way a genuinely unparseable chord would.
#[test]
fn an_empty_binding_parses_to_no_keys_rather_than_an_error() {
    assert_eq!(parse(""), Ok(Vec::new()));

    let binds = Keybinds::defaults();
    assert_eq!(
        binds.hint(Action::ThemesOpen),
        None,
        "no keys means no hint to show"
    );
    for key in [
        pressed(KeyCode::Char('t'), KeyModifiers::CONTROL),
        pressed(KeyCode::Char('t'), KeyModifiers::NONE),
    ] {
        assert!(
            !binds.binds(Action::ThemesOpen, key),
            "an action with no keys should reach nothing"
        );
    }
}

#[test]
fn the_defaults_are_the_keys_this_frontend_has_always_used() {
    let binds = Keybinds::defaults();
    let cases = [
        (Action::AppExit, KeyCode::Char('c'), KeyModifiers::CONTROL),
        (Action::AppExit, KeyCode::Char('q'), KeyModifiers::CONTROL),
        (Action::AppExit, KeyCode::Char('d'), KeyModifiers::CONTROL),
        (
            Action::PaletteOpen,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ),
        (
            Action::SessionsOpen,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ),
        // Not `Action::ThemesOpen`: `ctrl+t` moved to the inspector
        // overlay (**D453**), and the picker ships with no chord at all —
        // see `an_empty_binding_parses_to_no_keys_rather_than_an_error`.
        (
            Action::TranscriptOpen,
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        ),
        (Action::AgentCycle, KeyCode::Tab, KeyModifiers::NONE),
        (
            Action::InputNewline,
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        ),
        (Action::InputNewline, KeyCode::Enter, KeyModifiers::SHIFT),
        (Action::Redraw, KeyCode::Char('l'), KeyModifiers::CONTROL),
        (
            Action::HistorySearch,
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        ),
    ];

    for (action, code, modifiers) in cases {
        assert!(
            binds.binds(action, pressed(code, modifiers)),
            "{code:?}+{modifiers:?} should reach {action:?}"
        );
    }
}

/// `ctrl+j` is ASCII LF, which every terminal delivers; the three
/// `*+enter` chords need the kitty protocol, so the row carries all four
/// and the universal one is what a plain terminal falls back on.
#[test]
fn the_newline_row_carries_ctrl_j_beside_the_kitty_only_chords() {
    let binds = Keybinds::defaults();
    for (code, modifiers) in [
        (KeyCode::Char('j'), KeyModifiers::CONTROL),
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Enter, KeyModifiers::CONTROL),
        (KeyCode::Enter, KeyModifiers::ALT),
    ] {
        assert_eq!(
            binds.action(pressed(code, modifiers)),
            Some(Action::InputNewline),
            "{code:?}+{modifiers:?} should break the line"
        );
    }
}

#[test]
fn shift_enter_is_a_distinct_chord_from_a_bare_enter() {
    let binds = Keybinds::defaults();

    assert_eq!(
        binds.action(pressed(KeyCode::Enter, KeyModifiers::NONE)),
        None,
        "a bare Enter must stay a submit, not a bound action"
    );
    assert_eq!(
        binds.action(pressed(KeyCode::Enter, KeyModifiers::SHIFT)),
        Some(Action::InputNewline),
        "shift+enter is the line break"
    );
}

/// The chord is data, so a config file can move it like any other.
#[test]
fn the_newline_chord_is_rebindable_from_config() {
    let binds = Keybinds::from_config(&configured(&[("input_newline", "ctrl+n")]))
        .expect("a legible binding loads");

    assert_eq!(
        binds.action(pressed(KeyCode::Char('n'), KeyModifiers::CONTROL)),
        Some(Action::InputNewline),
        "the rebind reaches the action"
    );
    assert!(
        !binds.binds(
            Action::InputNewline,
            pressed(KeyCode::Char('j'), KeyModifiers::CONTROL)
        ),
        "and the default is replaced, not kept alongside"
    );
}

/// Ctrl+L has no upstream counterpart (**D445**); it is still an
/// ordinary row in the table, rebindable exactly like every other.
#[test]
fn the_redraw_chord_is_rebindable_from_config() {
    let binds =
        Keybinds::from_config(&configured(&[("redraw", "f6")])).expect("a legible binding loads");

    assert_eq!(
        binds.action(pressed(KeyCode::F(6), KeyModifiers::NONE)),
        Some(Action::Redraw),
        "the rebind reaches the action"
    );
    assert!(
        !binds.binds(
            Action::Redraw,
            pressed(KeyCode::Char('l'), KeyModifiers::CONTROL)
        ),
        "and the default is replaced, not kept alongside"
    );
}

/// Ctrl+R has no upstream counterpart either (**D447**); upstream's own
/// ctrl+r is `session_rename`, which ganja does not have.
#[test]
fn the_history_search_chord_is_rebindable_from_config() {
    let binds = Keybinds::from_config(&configured(&[("history_search", "f7")]))
        .expect("a legible binding loads");

    assert_eq!(
        binds.action(pressed(KeyCode::F(7), KeyModifiers::NONE)),
        Some(Action::HistorySearch),
        "the rebind reaches the action"
    );
    assert!(
        !binds.binds(
            Action::HistorySearch,
            pressed(KeyCode::Char('r'), KeyModifiers::CONTROL)
        ),
        "and the default is replaced, not kept alongside"
    );
}

/// A rebind is accepted and takes for its own action even when the chord
/// collides with another action's default — `ctrl+t` is `transcript`'s
/// own binding (**D453**), and [`Keybinds::binds`] still answers `true` for
/// `history_search` too. Which of the two [`Keybinds::action`] resolves a
/// bare press to is the separate, first-match-wins rule the reference
/// order decides; a config that creates a collision is the person's own
/// to avoid, not something `from_config` refuses.
#[test]
fn a_rebind_still_binds_even_when_it_collides_with_anothers_default() {
    let binds = Keybinds::from_config(&configured(&[("history_search", "ctrl+t")]))
        .expect("a legible binding loads");

    assert!(binds.binds(
        Action::HistorySearch,
        pressed(KeyCode::Char('t'), KeyModifiers::CONTROL)
    ));
}

#[test]
fn a_key_string_parses_the_shapes_a_config_file_can_write() {
    let cases = [
        ("ctrl+x", KeyCode::Char('x'), KeyModifiers::CONTROL),
        ("CTRL+X", KeyCode::Char('x'), KeyModifiers::CONTROL),
        ("f5", KeyCode::F(5), KeyModifiers::NONE),
        ("f12", KeyCode::F(12), KeyModifiers::NONE),
        ("home", KeyCode::Home, KeyModifiers::NONE),
        ("pgup", KeyCode::PageUp, KeyModifiers::NONE),
        ("esc", KeyCode::Esc, KeyModifiers::NONE),
        ("space", KeyCode::Char(' '), KeyModifiers::NONE),
        (
            "ctrl+alt+delete",
            KeyCode::Delete,
            KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        ),
        ("shift+tab", KeyCode::Tab, KeyModifiers::SHIFT),
    ];

    for (text, code, modifiers) in cases {
        assert_eq!(
            key(text),
            Some(pressed(code, modifiers)),
            "{text} should parse"
        );
    }
}

#[test]
fn a_key_string_this_build_cannot_read_parses_to_nothing() {
    for text in ["", "ctrl+", "hyperspace+x", "f99", "notakey", "ctrl+ab"] {
        assert_eq!(key(text), None, "{text:?} should not parse");
    }
}

#[test]
fn a_config_binding_replaces_the_default_rather_than_joining_it() {
    let binds = Keybinds::from_config(&configured(&[("palette_open", "f5")]))
        .expect("a legible binding loads");

    assert!(binds.binds(
        Action::PaletteOpen,
        pressed(KeyCode::F(5), KeyModifiers::NONE)
    ));
    assert!(
        !binds.binds(
            Action::PaletteOpen,
            pressed(KeyCode::Char('p'), KeyModifiers::CONTROL)
        ),
        "the default should be gone, not kept alongside"
    );
}

#[test]
fn comma_separated_alternatives_all_reach_the_action() {
    let binds = Keybinds::from_config(&configured(&[("themes_open", "f2, ctrl+y")]))
        .expect("a legible binding loads");

    assert!(binds.binds(
        Action::ThemesOpen,
        pressed(KeyCode::F(2), KeyModifiers::NONE)
    ));
    assert!(binds.binds(
        Action::ThemesOpen,
        pressed(KeyCode::Char('y'), KeyModifiers::CONTROL)
    ));
}

#[test]
fn an_action_this_build_does_not_have_is_named_rather_than_ignored() {
    let refusal = Keybinds::from_config(&configured(&[("session_share", "ctrl+z")]))
        .expect_err("an unknown action must not load");

    assert!(
        matches!(&refusal, KeybindError::UnknownAction { name } if name == "session_share"),
        "got {refusal:?}"
    );
    assert!(
        refusal.to_string().contains("session_share"),
        "the message should name it: {refusal}"
    );
}

#[test]
fn a_key_this_build_cannot_parse_is_named_rather_than_ignored() {
    let refusal = Keybinds::from_config(&configured(&[("app_exit", "ctrl+c, hypermeta+z")]))
        .expect_err("an unparseable key must not load");

    assert!(
        matches!(&refusal, KeybindError::UnparseableKey { action, key }
                if action == "app_exit" && key == "hypermeta+z"),
        "got {refusal:?}"
    );
    assert!(
        refusal.to_string().contains("hypermeta+z"),
        "the message should name it: {refusal}"
    );
}

/// A `shift+…` binding has to survive the round trip through the terminal,
/// which reports the letter shift already produced rather than the letter
/// the config file wrote — with the modifier still set on some terminals
/// and folded away on others. Both have to reach the action, and the
/// unshifted key must still not.
#[test]
fn a_shifted_binding_answers_to_the_key_the_terminal_actually_reports() {
    let binds = Keybinds::from_config(&configured(&[("agent_cycle", "shift+a")]))
        .expect("a legible binding loads");

    let cases = [
        (KeyCode::Char('A'), KeyModifiers::SHIFT, true),
        (KeyCode::Char('A'), KeyModifiers::NONE, true),
        (KeyCode::Char('a'), KeyModifiers::SHIFT, true),
        (KeyCode::Char('a'), KeyModifiers::NONE, false),
    ];

    for (code, modifiers, reaches) in cases {
        assert_eq!(
            binds.binds(Action::AgentCycle, pressed(code, modifiers)),
            reaches,
            "{code:?}+{modifiers:?}"
        );
        assert_eq!(
            binds.action(pressed(code, modifiers)) == Some(Action::AgentCycle),
            reaches,
            "{code:?}+{modifiers:?} through the lookup that has no action in hand"
        );
    }
}

/// Shift-tab is the one key with two names, and every terminal reports it
/// under the second. Both spellings must reach it, and neither may reach
/// plain tab.
#[test]
fn shift_tab_and_backtab_are_one_key_however_they_were_written() {
    for spelling in ["shift+tab", "backtab"] {
        let binds = Keybinds::from_config(&configured(&[("themes_open", spelling)]))
            .expect("a legible binding loads");

        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
            assert!(
                binds.binds(Action::ThemesOpen, pressed(KeyCode::BackTab, modifiers)),
                "{spelling} should answer to backtab+{modifiers:?}"
            );
        }
        assert!(
            !binds.binds(
                Action::ThemesOpen,
                pressed(KeyCode::Tab, KeyModifiers::NONE)
            ),
            "{spelling} is not plain tab"
        );
        assert_eq!(
            Keybinds::defaults().action(pressed(KeyCode::BackTab, KeyModifiers::SHIFT)),
            None,
            "and cycling agents on tab is not reached by shift-tab"
        );
    }
}

#[test]
fn a_hint_spells_every_key_that_reaches_an_action() {
    let binds = Keybinds::defaults();

    assert_eq!(
        binds.hint(Action::PaletteOpen).as_deref(),
        Some("ctrl+p"),
        "one key"
    );
    assert_eq!(
        binds.hint(Action::AppExit).as_deref(),
        Some("ctrl+c, ctrl+q, ctrl+d"),
        "every alternative"
    );
}

#[test]
fn an_unbound_key_reaches_nothing() {
    assert_eq!(
        Keybinds::defaults().action(pressed(KeyCode::Char('z'), KeyModifiers::NONE)),
        None
    );
}

#[test]
fn every_action_has_a_config_name() {
    for action in Action::all() {
        assert!(!action.key().is_empty(), "{action:?} should be nameable");
    }
}

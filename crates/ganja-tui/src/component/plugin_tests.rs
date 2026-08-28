use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Effect, Plugin, Row, summarize};
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 76, height: 20 };

fn row(name: &str, enabled: bool) -> Row {
    Row {
        name: name.to_owned(),
        enabled,
        marketplace: "company-tools".to_owned(),
        summary: "1 hook \u{b7} skills".to_owned(),
    }
}

fn dialog() -> Plugin {
    Plugin::new(vec![row("formatter", true), row("deployer", false)])
}

fn rendered(dialog: &Plugin, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_plugin_lists_with_its_state_marketplace_and_summary() {
    let screen = rendered(&dialog(), AREA);

    assert!(screen.contains("formatter"), "got:\n{screen}");
    assert!(screen.contains("Enabled"), "got:\n{screen}");
    assert!(screen.contains("deployer"), "got:\n{screen}");
    assert!(screen.contains("Disabled"), "got:\n{screen}");
    assert!(screen.contains("company-tools"), "got:\n{screen}");
    assert!(screen.contains("1 hook \u{b7} skills"), "got:\n{screen}");
}

#[test]
fn the_top_level_actions_are_always_offered_even_over_an_empty_store() {
    for dialog in [dialog(), Plugin::new(Vec::new())] {
        let screen = rendered(&dialog, AREA);

        assert!(screen.contains("Add marketplace"), "got:\n{screen}");
        assert!(screen.contains("Install plugin"), "got:\n{screen}");
        assert!(screen.contains("Reload"), "got:\n{screen}");
    }
    assert!(
        rendered(&Plugin::new(Vec::new()), AREA).contains("no plugins installed"),
        "an empty store says so"
    );
}

/// Enter on an enabled plugin offers Disable; on a disabled one, Enable —
/// the toggle that applies, never both.
#[test]
fn enter_on_a_plugin_row_offers_the_applicable_toggle_and_remove() {
    let mut dialog = dialog();

    assert_eq!(dialog.submit(), None, "Enter opens the action step");
    assert!(dialog.is_choosing_action());
    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("Disable"), "got:\n{screen}");
    assert!(screen.contains("Remove"), "got:\n{screen}");
    assert!(!screen.contains("> Enable\n"), "got:\n{screen}");

    assert_eq!(
        dialog.submit(),
        Some(Effect::Disable("formatter".to_owned())),
        "Enter on the toggle answers with it"
    );
    assert!(!dialog.is_choosing_action(), "running an action returns to the list");
}

#[test]
fn enter_on_a_disabled_plugin_offers_enable() {
    let mut dialog = dialog();
    dialog.move_selection(1);

    assert_eq!(dialog.submit(), None);
    assert_eq!(dialog.submit(), Some(Effect::Enable("deployer".to_owned())));
}

#[test]
fn remove_is_the_action_after_the_toggle() {
    let mut dialog = dialog();
    dialog.submit();
    dialog.move_selection(1);

    assert_eq!(dialog.submit(), Some(Effect::Remove("formatter".to_owned())));
}

/// The free-text step is TUI-local: Enter with text answers with an
/// [`Effect`] for the app to run, and nothing here has an engine to ask.
#[test]
fn the_add_input_takes_text_and_submits_it_on_enter() {
    let mut dialog = dialog();
    dialog.move_selection(2);
    assert_eq!(dialog.submit(), None, "Add marketplace opens the input");
    assert!(dialog.is_typing());

    for character in "/tmp/market".chars() {
        dialog.push(character);
    }
    dialog.backspace();
    assert_eq!(dialog.input(), Some("/tmp/marke"));

    assert_eq!(dialog.submit(), Some(Effect::AddMarketplace("/tmp/marke".to_owned())));
    assert!(!dialog.is_typing(), "a submit leaves the input step");
}

#[test]
fn the_install_input_spells_the_claude_spec_spelling() {
    let mut dialog = Plugin::new(Vec::new());
    dialog.move_selection(1);
    assert_eq!(dialog.submit(), None);
    let screen = rendered(&dialog, AREA);
    assert!(
        screen.contains("<plugin>@<marketplace>"),
        "the input says what spelling it wants:\n{screen}"
    );

    for character in "formatter@company-tools".chars() {
        dialog.push(character);
    }
    assert_eq!(dialog.submit(), Some(Effect::Install("formatter@company-tools".to_owned())));
}

/// Esc on the input step cancels the edit and keeps the dialog open;
/// on the other steps it is not consumed, so the app closes the dialog —
/// the `/mcp` dialog's own Esc.
#[test]
fn esc_cancels_the_input_step_and_closes_from_anywhere_else() {
    let mut dialog = dialog();
    dialog.move_selection(2);
    dialog.submit();
    dialog.push('x');

    assert!(dialog.cancel(), "the input step consumes the Esc");
    assert!(!dialog.is_typing());
    assert!(!dialog.cancel(), "the list step leaves Esc to the app");

    dialog.move_selection(-2);
    dialog.submit();
    assert!(dialog.is_choosing_action());
    assert!(!dialog.cancel(), "the action step leaves Esc to the app too");
}

#[test]
fn an_empty_input_submits_nothing_and_stays_where_it_is() {
    let mut dialog = Plugin::new(Vec::new());
    dialog.submit();
    assert!(dialog.is_typing());

    assert_eq!(dialog.submit(), None);
    assert!(dialog.is_typing(), "nothing typed is not a decision");
}

#[test]
fn enter_on_reload_answers_with_the_reload_effect() {
    let mut dialog = dialog();
    dialog.move_selection(4);

    assert_eq!(dialog.submit(), Some(Effect::Reload));
}

#[test]
fn the_notice_line_surfaces_an_actions_outcome() {
    let mut dialog = dialog();
    dialog.set_notice("git clone failed: repository not found");

    assert!(
        rendered(&dialog, AREA).contains("git clone failed: repository not found"),
        "got:\n{}",
        rendered(&dialog, AREA)
    );
}

/// `zus`: while the app has a store action running off the loop, the two
/// actions that would write the store are refused where they are chosen —
/// the input step never opens — and the notice line says why. Reload is
/// untouched: it writes nothing there is to race.
#[test]
fn a_running_store_action_refuses_the_two_that_would_race_it() {
    let mut dialog = dialog();
    dialog.set_busy(true);
    assert!(dialog.is_busy());

    for offset in 0..2 {
        let mut dialog = dialog.clone();
        dialog.move_selection(2 + offset);

        assert_eq!(dialog.submit(), None, "the store action is refused");
        assert!(!dialog.is_typing(), "and the input step it would have opened stays shut");
        let screen = rendered(&dialog, AREA);
        assert!(screen.contains("already running"), "got:\n{screen}");
    }

    let mut reload = dialog.clone();
    reload.move_selection(4);
    assert_eq!(reload.submit(), Some(Effect::Reload), "the reload races nothing and stays live");
}

/// The refusal lasts exactly as long as the action does: the app clears
/// the flag when it reaps the task, and the input opens again.
#[test]
fn clearing_the_running_flag_opens_the_add_again() {
    let mut dialog = dialog();
    dialog.set_busy(true);
    dialog.move_selection(2);
    assert_eq!(dialog.submit(), None);

    dialog.set_busy(false);
    assert_eq!(dialog.submit(), None, "Add marketplace opens the input");
    assert!(dialog.is_typing());
    dialog.push('x');
    assert_eq!(dialog.submit(), Some(Effect::AddMarketplace("x".to_owned())));
}

#[test]
fn a_refresh_reclamps_the_cursor_after_a_remove_shrinks_the_list() {
    let mut dialog = dialog();
    dialog.move_selection(4);

    dialog.refresh(Vec::new());
    // Three top-level rows are all that is left; the cursor holds inside
    // them rather than pointing past the end.
    assert_eq!(dialog.submit(), Some(Effect::Reload));
}

#[test]
fn the_summary_counts_components_by_surface() {
    let components = [
        "hook PreToolUse".to_owned(),
        "hook Stop".to_owned(),
        "mcp db".to_owned(),
        "skills".to_owned(),
        "agent reviewer".to_owned(),
        "lsp go".to_owned(),
    ];

    assert_eq!(
        summarize(&components),
        "2 hooks \u{b7} 1 mcp \u{b7} skills \u{b7} 1 agent \u{b7} 1 lsp"
    );
    assert_eq!(summarize(&[]), "no components");
}

#[test]
fn a_tiny_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (3, 2), (8, 4)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        dialog().render(area, &mut buffer, &Theme::default());
    }
}

#[test]
fn a_zero_area_draws_nothing_and_does_not_panic() {
    let screen = rendered(&dialog(), Rect::new(0, 0, 0, 0));

    assert!(screen.is_empty(), "a zero area has no cell: {screen}");
}

use ratatui::{buffer::Buffer, layout::Rect};

use super::{Action, Mcp, Row};
use crate::theme::Theme;

const AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 76,
    height: 20,
};

fn connected(name: &str, tools: usize) -> Row {
    Row {
        name: name.to_owned(),
        status: "Connected".to_owned(),
        tools: Some(tools),
        detail: None,
        actions: Vec::new(),
    }
}

fn failed(name: &str, error: &str) -> Row {
    Row {
        name: name.to_owned(),
        status: "Failed".to_owned(),
        tools: None,
        detail: Some(error.to_owned()),
        actions: vec![Action::Reconnect],
    }
}

/// A remote server configured with `oauth` — Login belongs on it whatever
/// its status, unlike Reconnect's `Failed`-only gate.
fn oauth_configured(name: &str, status: &str, tools: Option<usize>) -> Row {
    Row {
        name: name.to_owned(),
        status: status.to_owned(),
        tools,
        detail: None,
        actions: vec![Action::Login],
    }
}

fn dialog() -> Mcp {
    Mcp::new(vec![
        connected("github", 3),
        failed("flaky", "timed out after 30000ms"),
        Row {
            name: "off".to_owned(),
            status: "Disabled".to_owned(),
            tools: None,
            detail: None,
            actions: Vec::new(),
        },
    ])
}

fn rendered(dialog: &Mcp, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_server_lists_with_its_status_and_tool_count() {
    let screen = rendered(&dialog(), AREA);

    assert!(screen.contains("github"), "got:\n{screen}");
    assert!(screen.contains("Connected"), "got:\n{screen}");
    assert!(screen.contains("3 tools"), "got:\n{screen}");
    assert!(screen.contains("flaky"), "got:\n{screen}");
    assert!(screen.contains("Failed"), "got:\n{screen}");
    assert!(screen.contains("timed out after 30000ms"), "got:\n{screen}");
    assert!(screen.contains("off"), "got:\n{screen}");
    assert!(screen.contains("Disabled"), "got:\n{screen}");
}

#[test]
fn the_cursor_starts_on_the_first_row_and_clamps_at_both_ends() {
    let mut dialog = dialog();
    assert_eq!(
        dialog.selected().map(|row| row.name.as_str()),
        Some("github")
    );

    dialog.move_selection(-9);
    assert_eq!(
        dialog.selected().map(|row| row.name.as_str()),
        Some("github")
    );

    dialog.move_selection(9);
    assert_eq!(dialog.selected().map(|row| row.name.as_str()), Some("off"));
}

/// Enter on a connected server has nothing to offer: the dialog stays
/// exactly as it was rather than closing.
#[test]
fn enter_on_a_row_with_no_actions_does_nothing() {
    let mut dialog = dialog();

    assert!(!dialog.advance(), "a connected server has no actions");
    assert!(!dialog.is_choosing_action());
    assert_eq!(dialog.chosen(), None);
}

/// Enter on a failed server opens Reconnect, and answers with it.
#[test]
fn enter_on_a_failed_row_opens_reconnect_and_answers_with_it() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    assert_eq!(
        dialog.selected().map(|row| row.name.as_str()),
        Some("flaky")
    );

    assert!(dialog.advance());
    assert!(dialog.is_choosing_action());
    assert_eq!(dialog.chosen(), Some(("flaky", Action::Reconnect)));

    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("flaky"), "got:\n{screen}");
    assert!(screen.contains("Reconnect"), "got:\n{screen}");
}

/// Login belongs on an `oauth`-configured server whatever its status —
/// unlike Reconnect, gated on `Failed` alone — so even a row this dialog
/// shows as `Connected` still offers it.
#[test]
fn enter_on_a_connected_oauth_row_still_opens_login() {
    let mut dialog = Mcp::new(vec![oauth_configured("hub", "Connected", Some(3))]);

    assert!(dialog.advance());
    assert!(dialog.is_choosing_action());
    assert_eq!(dialog.chosen(), Some(("hub", Action::Login)));

    let screen = rendered(&dialog, AREA);
    assert!(screen.contains("Login"), "got:\n{screen}");
}

/// Running an action returns to the server list without closing the
/// dialog, and the cursor is still on the same row.
#[test]
fn back_to_servers_leaves_the_action_step_and_keeps_the_selection() {
    let mut dialog = dialog();
    dialog.move_selection(1);
    dialog.advance();

    dialog.back_to_servers();

    assert!(!dialog.is_choosing_action());
    assert_eq!(
        dialog.selected().map(|row| row.name.as_str()),
        Some("flaky")
    );
}

/// A poll refresh keeps the cursor on the same position rather than
/// resetting it — the whole reason `refresh` exists instead of a fresh
/// [`Mcp::new`] every tick.
#[test]
fn refreshing_keeps_the_cursor_where_it_was() {
    let mut dialog = dialog();
    dialog.move_selection(1);

    // Reconnected: the failed row is now connected and lends a tool.
    dialog.refresh(vec![
        connected("github", 3),
        connected("flaky", 1),
        Row {
            name: "off".to_owned(),
            status: "Disabled".to_owned(),
            tools: None,
            detail: None,
            actions: Vec::new(),
        },
    ]);

    assert_eq!(
        dialog.selected().map(|row| row.name.as_str()),
        Some("flaky")
    );
    assert_eq!(
        dialog.selected().map(|row| row.status.as_str()),
        Some("Connected")
    );
}

#[test]
fn an_empty_configuration_says_so_instead_of_drawing_an_empty_box() {
    let dialog = Mcp::new(Vec::new());

    assert!(dialog.selected().is_none());
    assert!(
        rendered(&dialog, AREA).contains("no MCP servers configured"),
        "{}",
        rendered(&dialog, AREA)
    );
}

#[test]
fn a_row_too_wide_for_the_column_is_cut_rather_than_wrapped() {
    let dialog = Mcp::new(vec![failed("x", &"very long error ".repeat(20))]);

    for line in rendered(&dialog, Rect::new(0, 0, 60, 20)).lines() {
        assert!(
            line.chars().count() <= 60,
            "a row must not overflow the dialog: {line:?}"
        );
    }
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

    assert!(
        screen.is_empty(),
        "a zero area has no cell to hold: {screen}"
    );
}

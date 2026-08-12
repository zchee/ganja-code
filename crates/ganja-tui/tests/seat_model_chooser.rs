//! `/models` on a ChatGPT seat session offers the pinned five (**D476**).
//!
//! The chooser's wire lane used to be reachable only where the catalog had
//! nothing to show — cursor's tier. A seat's provider has plenty of catalog
//! rows and is still not offered them, so the lane is now chosen by the seam's
//! own `wire_lists_models` rather than by an empty table, and this is what
//! pins that: the five appear, and `gpt-5.4` — an openai row this build's
//! catalog carries, servable on the seat and deliberately unoffered — does not.
//!
//! It lives out here rather than beside the module because the decision reads
//! the credential store, which means `XDG_DATA_HOME` and `OPENAI_API_KEY`:
//! process-wide state, so this file is one test in a process of its own, the
//! `plugin_dialog` discipline.

use std::{fs, sync::Arc};

use ganja_core::{
    Engine,
    provider::{FakeProvider, fake},
};
use ganja_tui::{app::App, event::AppEvent, theme::Themes};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers},
};
use tempfile::TempDir;

/// One keypress, as the event loop would deliver it.
fn key(code: KeyCode) -> AppEvent {
    AppEvent::Term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

/// The whole screen as text, for asserting on what a person would see.
fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = buffer.area;

    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_model_chooser_on_a_chatgpt_seat_offers_the_pinned_roster_rather_than_the_catalog() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    let data = home.path().join("xdg-data");
    // SAFETY: nothing else runs yet — this is the only test in this binary,
    // and the runtime it starts on is current-thread.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &data);
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg-config"));
        // An exported key outranks a stored login, so a developer's own would
        // make this session the platform's rather than a seat's.
        std::env::remove_var("OPENAI_API_KEY");
    }

    // A stored ChatGPT credential in the shape `ganja auth login` writes one.
    // Inert tokens: the roster is compile-time, so nothing here is presented
    // to anybody.
    let store = data.join("ganja");
    fs::create_dir_all(&store).expect("the store directory is creatable");
    let path = store.join("auth.json");
    fs::write(
        &path,
        r#"{"openai": {"type": "oauth", "refresh": "rt-seat-fixture",
             "access": "at-seat-fixture", "expires": 4102444800000}}"#,
    )
    .expect("the fixture writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");
    }

    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    );
    let mut app = App::new(engine, None, Themes::builtin()).with_provider("openai");

    for character in "/models".chars() {
        app.handle(key(KeyCode::Char(character)))
            .await
            .expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter))
        .await
        .expect("the submit is handled");

    // The listing runs off the render loop, so the tick that reaps it is what
    // opens the dialog — instant answer or not, the seat rides the one lane.
    let mut terminal =
        Terminal::new(TestBackend::new(100, 30)).expect("a test terminal is creatable");
    let mut offered = String::new();
    for _ in 0..400 {
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        app.draw(&mut terminal).expect("a frame draws");
        offered = screen(&terminal);
        if offered.contains("gpt-5.6-sol") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    for model in [
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.3-codex-spark",
    ] {
        assert!(
            offered.contains(model),
            "the seat is offered `{model}`:\n{offered}"
        );
    }
    assert!(
        !offered.contains("gpt-5.4"),
        "and is not offered the vendor's catalog rows, servable though they \
         are:\n{offered}"
    );
}

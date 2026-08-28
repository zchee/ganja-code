//! The `/plugin` dialog against the real discovery path: the store under
//! `GANJA_CONFIG_HOME`, found the way `ganja plugin` finds it, and the
//! Reload action's honest split (**D474**) over a config load that reads the
//! same redirected homes.
//!
//! It lives out here rather than beside the module because it has to set
//! `GANJA_CONFIG_HOME` and both XDG homes, which are process-wide: an
//! integration test gets a process of its own, so nothing else can see this
//! one move the homes out from under it. Everything in this file is
//! therefore one test, the `theme_paths` discipline.

use std::fs;
use std::sync::Arc;

use ganja_core::Engine;
use ganja_core::plugin::Store;
use ganja_core::provider::{FakeProvider, fake};
use ganja_testkit::plant;
use ganja_tui::app::App;
use ganja_tui::event::AppEvent;
use ganja_tui::theme::Themes;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
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
        .map(|row| (0..area.width).map(|column| buffer[(column, row)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_dialog_discovers_the_store_under_the_config_home_and_reload_reports_the_split() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    let config_home = home.path().join("ganja-home");
    let project = home.path().join("project");
    fs::create_dir_all(&project).expect("the project directory is creatable");

    // SAFETY: nothing else runs yet — this is the only test in this binary,
    // and the runtime it starts on is current-thread.
    unsafe {
        std::env::set_var("GANJA_CONFIG_HOME", &config_home);
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg-config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("xdg-data"));
    }

    // A marketplace with one plugin carrying a hook and a skill, installed
    // through the store the config home resolves — the same store
    // `ganja plugin` and the config load discover.
    let market = home.path().join("market");
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        r#"{
          "name": "company-tools",
          "owner": { "name": "DevTools" },
          "plugins": [{ "name": "formatter", "source": "./plugins/formatter" }]
        }"#,
    );
    plant(
        &market,
        "plugins/formatter/hooks/hooks.json",
        r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    plant(&market, "plugins/formatter/skills/fmt/SKILL.md", "# fmt\n");
    let store = Store::discover().expect("the redirected config home resolves");
    store
        .add_marketplace(market.to_str().expect("the fixture path is unicode"))
        .expect("the fixture marketplace adds");
    store.install("formatter", "company-tools").expect("the fixture plugin installs");

    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    );
    let mut app = App::new(engine, None, Themes::builtin()).with_cwd(&project).with_root(&project);

    // `/plugin` typed at the composer, dispatched on Enter — no builder store
    // was handed in, so the dialog's rows prove the discovery path.
    for character in "/plugin".chars() {
        app.handle(key(KeyCode::Char(character))).await.expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter)).await.expect("the submit is handled");

    let mut terminal =
        Terminal::new(TestBackend::new(100, 30)).expect("a test terminal is creatable");
    app.draw(&mut terminal).expect("a frame draws");
    let listed = screen(&terminal);
    assert!(listed.contains("formatter"), "got:\n{listed}");
    assert!(listed.contains("Enabled"), "got:\n{listed}");
    assert!(listed.contains("company-tools"), "got:\n{listed}");
    assert!(
        listed.contains("1 hook \u{b7} skills"),
        "the collector's components reach the row:\n{listed}"
    );

    // One plugin row, then Add, Install, Reload: three Downs land on Reload.
    for _ in 0..3 {
        app.handle(key(KeyCode::Down)).await.expect("the key is handled");
    }
    app.handle(key(KeyCode::Enter)).await.expect("the reload is handled");

    app.draw(&mut terminal).expect("a frame draws");
    let reloaded = screen(&terminal);
    assert!(
        reloaded.contains("reloaded now: hooks, skills"),
        "the notice names what really rebuilt:\n{reloaded}"
    );
    assert!(
        reloaded.contains("restart required: agents, mcp, lsp"),
        "and what honestly did not:\n{reloaded}"
    );
}

//! Where a user's themes and their theme pick are looked for on disk.
//!
//! The unit tests point the registry at paths directly; this one goes the whole
//! way round — resolve both XDG homes, discover a theme file under the config
//! one, adopt the pick stored under the data one, and write a new pick back.
//!
//! It lives out here rather than beside the module because it has to set
//! `XDG_CONFIG_HOME` and `XDG_DATA_HOME`, which are process-wide: an
//! integration test gets a process of its own, so nothing else can see this one
//! move the homes out from under it. Everything in this file is therefore one
//! test.

use std::fs;

use ganja_tui::theme::{Rgba, Themes};
use tempfile::TempDir;

/// A theme file naming one color, which is all it takes to be a theme.
const MIDNIGHT: &str = "{\"theme\": {\"text\": \"#101020\"}}";

#[test]
fn a_theme_and_the_pick_naming_it_are_found_under_the_two_xdg_homes() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    let config = home.path().join("config");
    let data = home.path().join("data");

    let themes_dir = config.join("ganja").join("themes");
    fs::create_dir_all(&themes_dir).expect("the theme directory is creatable");
    fs::write(themes_dir.join("midnight.json"), MIDNIGHT).expect("the theme file writes");

    let store = data.join("ganja").join("tui.json");
    fs::create_dir_all(store.parent().expect("the store has a parent"))
        .expect("the data directory is creatable");
    fs::write(&store, "{\"version\":1,\"theme\":\"midnight\"}\n").expect("the pick writes");

    // SAFETY: nothing else runs yet — this is the only test in this binary, and
    // it has not started a thread.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("XDG_DATA_HOME", &data);
    }

    let mut themes = Themes::load();

    assert!(
        themes.names().contains(&"midnight".to_owned()),
        "a theme under the config home should have been discovered, got: {:?}",
        themes.names()
    );
    assert_eq!(
        themes.active(),
        "midnight",
        "the pick under the data home should have been adopted"
    );
    assert_eq!(
        themes.theme().color("text"),
        Some(Rgba::rgb(0x10, 0x10, 0x20)),
        "the discovered file is what the active theme resolves from"
    );

    // And the other direction: a new pick lands in the same file.
    themes.select("gruvbox").expect("gruvbox is builtin");
    themes.persist().expect("the pick stores");

    assert_eq!(
        fs::read_to_string(&store).expect("the store reads"),
        "{\"version\":1,\"theme\":\"gruvbox\"}\n"
    );
}

use std::fs;

use tempfile::TempDir;

use super::{BUILTIN_FILES, DEFAULT_THEME, Entry, TERMINAL_THEME, Themes};
use crate::theme::Mode;

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// Writes `name`.json holding `body` into `directory`.
fn custom(directory: &TempDir, name: &str, body: &str) {
    fs::write(directory.path().join(format!("{name}.json")), body)
        .expect("the fixture theme writes");
}

/// A theme file with one key, enough to be a theme.
fn minimal(color: &str) -> String {
    format!("{{\"theme\": {{\"text\": \"{color}\"}}}}")
}

/// The acceptance criterion in one test: every ported theme parses and
/// resolves, in both modes, with no key left unresolvable.
#[test]
fn every_builtin_theme_resolves_in_both_modes() {
    for (name, text) in BUILTIN_FILES {
        let entry = Entry::parse(text)
            .unwrap_or_else(|refusal| panic!("{name} should resolve, got: {refusal}"));

        let Entry::Json { dark, light } = entry else {
            panic!("{name} should be a file-backed theme");
        };

        // 52 keys upstream names, less the two optional ones aura and the
        // rest omit, plus the two the post-pass fills back in.
        assert!(dark.len() >= 50, "{name} resolved only {} keys in dark", dark.len());
        assert_eq!(dark.len(), light.len(), "{name} should resolve the same keys in both modes");
    }
}

/// `aura` is the one ported theme with no variants at all, so it is the
/// only one that exercises the flat-string arm end to end.
#[test]
fn a_flat_theme_resolves_to_the_same_colors_in_both_modes() {
    let mut themes = Themes::builtin();

    let dark = themes.select("aura").expect("aura is builtin");
    themes.set_mode(Mode::Light);
    let light = themes.select("aura").expect("aura is builtin");

    assert_eq!(
        dark.palette(),
        light.palette(),
        "a theme with no variants cannot change with the mode"
    );
}

/// The variant arm, from the other side: a theme built out of variants must
/// not resolve identically in the two modes.
#[test]
fn a_variant_theme_resolves_differently_per_mode() {
    let mut themes = Themes::builtin();

    let dark = themes.select(DEFAULT_THEME).expect("opencode is builtin");
    themes.set_mode(Mode::Light);
    let light = themes.select(DEFAULT_THEME).expect("opencode is builtin");

    assert_ne!(dark.palette(), light.palette());
    assert_ne!(
        dark.color("background"),
        light.color("background"),
        "the backgrounds are what the two modes are for"
    );
}

#[test]
fn the_builtins_are_the_four_ported_themes_plus_the_terminal_one() {
    assert_eq!(
        Themes::builtin().names(),
        vec!["aura", "gruvbox", "opencode", "terminal", "tokyonight"],
        "listed case-insensitively, as the dialog shows them"
    );
}

#[test]
fn a_fresh_registry_starts_on_upstreams_default() {
    let mut themes = Themes::builtin();

    assert_eq!(themes.active(), DEFAULT_THEME);
    assert_eq!(themes.theme().name(), DEFAULT_THEME);
    assert_eq!(themes.mode(), Mode::Dark, "deviation D3: dark until told");
}

#[test]
fn every_resolve_carries_a_revision_no_earlier_one_used() {
    let mut themes = Themes::builtin();

    let first = themes.theme().revision();
    let second = themes.select("gruvbox").expect("gruvbox is builtin");
    let third = themes.theme().revision();

    assert!(first > 0, "revision zero belongs to the standalone default");
    assert!(second.revision() > first);
    assert!(third > second.revision());
}

#[test]
fn selecting_a_theme_this_run_does_not_have_changes_nothing() {
    let mut themes = Themes::builtin();

    assert!(themes.select("nosuchtheme").is_none());
    assert_eq!(themes.active(), DEFAULT_THEME);
}

#[test]
fn a_custom_theme_is_listed_beside_the_builtins() {
    let directory = temporary();
    custom(&directory, "midnight", &minimal("#101020"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    assert!(themes.names().contains(&"midnight".to_owned()));
    assert_eq!(
        themes.select("midnight").expect("the custom theme registers").color("text"),
        Some(crate::theme::Rgba::rgb(0x10, 0x10, 0x20))
    );
}

/// R11 sorts the dialog case-insensitively, and every builtin is
/// lowercase — so only a custom theme written with a capital can tell that
/// apart from a plain byte sort, under which every capital would be herded
/// to the top away from the name it belongs beside.
#[test]
fn the_listing_sorts_by_name_and_not_by_the_case_it_was_written_in() {
    let directory = temporary();
    custom(&directory, "Zenburn", &minimal("#3f3f3f"));
    custom(&directory, "Ayu", &minimal("#0a0e14"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    assert_eq!(
        themes.names(),
        vec!["aura", "Ayu", "gruvbox", "opencode", "terminal", "tokyonight", "Zenburn"],
        "a byte sort would have listed Ayu and Zenburn before aura"
    );
}

/// D21 from the registry's side: resolving in one mode is not enough to be
/// listed. A theme that only answers in dark would leave the screen with
/// no colors the moment the mode changed under it, so it never registers
/// at all — and the ones beside it in the same directory still do.
#[test]
fn a_theme_that_only_answers_in_one_mode_never_reaches_the_listing() {
    let directory = temporary();
    custom(&directory, "darkonly", "{\"theme\": {\"text\": {\"dark\": \"#101020\"}}}");
    custom(&directory, "bothmodes", &minimal("#123456"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    assert!(!themes.names().contains(&"darkonly".to_owned()), "got: {:?}", themes.names());
    assert!(themes.select("darkonly").is_none(), "and there is nothing to select either");
    assert!(themes.names().contains(&"bothmodes".to_owned()), "the theme beside it still loaded");
}

/// R11: a name collision is the user's file winning. That is the whole
/// point of being able to write one.
#[test]
fn a_custom_theme_shadows_the_builtin_of_the_same_name() {
    let directory = temporary();
    custom(&directory, DEFAULT_THEME, &minimal("#abcdef"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    assert_eq!(
        themes.select(DEFAULT_THEME).expect("the name still resolves").color("text"),
        Some(crate::theme::Rgba::rgb(0xab, 0xcd, 0xef))
    );
    assert_eq!(
        themes.names().iter().filter(|name| *name == DEFAULT_THEME).count(),
        1,
        "shadowing replaces the entry rather than listing it twice"
    );
}

/// Deviation D16, and the reason for it: upstream would have dropped
/// `readable` too.
#[test]
fn a_malformed_custom_theme_is_skipped_and_the_rest_still_load() {
    let directory = temporary();
    custom(&directory, "broken", "{ not json at all");
    custom(&directory, "notatheme", "{\"defs\": {}}");
    custom(&directory, "cyclic", "{\"theme\": {\"text\": \"text\"}}");
    custom(&directory, "readable", &minimal("#123456"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    let names = themes.names();
    assert!(names.contains(&"readable".to_owned()), "got: {names:?}");
    for skipped in ["broken", "notatheme", "cyclic"] {
        assert!(
            !names.contains(&skipped.to_owned()),
            "{skipped} should not have registered: {names:?}"
        );
    }
}

#[test]
fn files_that_are_not_themes_are_not_looked_at() {
    let directory = temporary();
    fs::write(directory.path().join("README.md"), "not a theme").expect("the fixture writes");
    fs::create_dir(directory.path().join("nested.json")).expect("the fixture directory");
    custom(&directory, "kept", &minimal("#ffffff"));

    let mut themes = Themes::builtin();
    themes.add_custom_dir(directory.path());

    assert!(themes.names().contains(&"kept".to_owned()));
    assert!(!themes.names().contains(&"README".to_owned()));
    assert!(!themes.names().contains(&"nested".to_owned()));
}

#[test]
fn a_missing_custom_directory_is_not_an_error() {
    let directory = temporary();
    let mut themes = Themes::builtin();

    themes.add_custom_dir(&directory.path().join("nothing-here"));

    assert_eq!(themes.names(), Themes::builtin().names());
}

#[test]
fn a_pick_survives_into_the_next_run() {
    let directory = temporary();
    let store = directory.path().join("tui.json");

    let mut themes = Themes::builtin();
    themes.adopt_store(store.clone());
    themes.select("gruvbox").expect("gruvbox is builtin");
    themes.persist().expect("the pick stores");

    let mut reopened = Themes::builtin();
    reopened.adopt_store(store);

    assert_eq!(reopened.active(), "gruvbox");
    assert_eq!(reopened.theme().name(), "gruvbox");
}

#[test]
fn a_stored_pick_this_build_does_not_have_falls_back_to_the_default() {
    let directory = temporary();
    let store = directory.path().join("tui.json");
    fs::write(&store, "{\"version\":1,\"theme\":\"a-theme-that-was-deleted\"}")
        .expect("the fixture writes");

    let mut themes = Themes::builtin();
    themes.adopt_store(store);

    assert_eq!(themes.active(), DEFAULT_THEME);
}

#[test]
fn a_pick_with_nowhere_to_go_is_refused_rather_than_lost_silently() {
    let mut themes = Themes::builtin();
    themes.select(TERMINAL_THEME).expect("terminal is builtin");

    let refusal = themes.persist().expect_err("there is no store");

    assert!(refusal.to_string().contains("could not be located"), "got: {refusal}");
}

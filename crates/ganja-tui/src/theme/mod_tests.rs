use ratatui::style::{Color, Modifier, Style};

use super::{Mode, Rgba, TERMINAL_THEME, Theme, ThemeJson};

/// A theme file naming `body`, resolved for dark.
fn theme(body: &str) -> Theme {
    let file = ThemeJson::parse(&format!("{{\"theme\": {{{body}}}}}")).expect("the fixture parses");

    Theme::from_palette(
        "fixture".to_owned(),
        7,
        file.resolve(Mode::Dark).expect("the fixture resolves"),
    )
}

/// The six P1 roles must keep landing where they did, or every component
/// silently changes meaning.
#[test]
fn the_six_original_roles_map_to_the_keys_they_were_named_for() {
    let theme = theme(
        "\"text\": \"#111111\", \"textMuted\": \"#222222\", \"accent\": \"#333333\", \
             \"diffAdded\": \"#444444\", \"diffRemoved\": \"#555555\", \"error\": \"#666666\"",
    );

    let cases = [
        (theme.fg, Color::Rgb(0x11, 0x11, 0x11)),
        (theme.dim, Color::Rgb(0x22, 0x22, 0x22)),
        (theme.accent, Color::Rgb(0x33, 0x33, 0x33)),
        (theme.add, Color::Rgb(0x44, 0x44, 0x44)),
        (theme.remove, Color::Rgb(0x55, 0x55, 0x55)),
        (theme.error, Color::Rgb(0x66, 0x66, 0x66)),
    ];

    for (style, expected) in cases {
        assert_eq!(style, Style::new().fg(expected));
    }
}

#[test]
fn the_background_slots_carry_a_background_not_a_foreground() {
    let theme = theme(
        "\"background\": \"#0a0a0a\", \"backgroundPanel\": \"#141414\", \
             \"backgroundElement\": \"#1e1e1e\"",
    );

    assert_eq!(
        theme.background,
        Style::new().bg(Color::Rgb(0x0a, 0x0a, 0x0a))
    );
    assert_eq!(
        theme.background_panel,
        Style::new().bg(Color::Rgb(0x14, 0x14, 0x14))
    );
    assert_eq!(
        theme.background_element,
        Style::new().bg(Color::Rgb(0x1e, 0x1e, 0x1e))
    );
}

/// The ruling R11 turns on: alpha zero is an unset color, so the terminal
/// keeps showing through instead of the cell being painted black.
#[test]
fn a_transparent_key_leaves_its_slot_unset() {
    let theme = theme("\"background\": \"none\", \"text\": \"transparent\"");

    assert_eq!(theme.background, Style::new(), "no background is painted");
    assert_eq!(theme.fg, Style::new(), "no foreground is painted");
}

#[test]
fn a_key_the_theme_never_names_leaves_its_slot_unset() {
    let theme = theme("\"text\": \"#ffffff\"");

    assert_eq!(theme.warning, Style::new());
    assert_eq!(theme.border_active, Style::new());
}

/// Keys nothing renders yet still have to be reachable, or a theme would
/// have to be reloaded once markdown rendering arrives.
#[test]
fn unconsumed_keys_are_carried_on_the_theme() {
    let theme = theme("\"text\": \"#ffffff\", \"syntaxKeyword\": \"#ff00ff\"");

    assert_eq!(
        theme.color("syntaxKeyword"),
        Some(Rgba::rgb(0xff, 0x00, 0xff))
    );
    assert_eq!(theme.color("syntaxString"), None);
}

#[test]
fn the_name_and_revision_travel_with_the_theme() {
    let theme = theme("\"text\": \"#ffffff\"");

    assert_eq!(theme.name(), "fixture");
    assert_eq!(theme.revision(), 7);
}

/// Upstream's first branch: a theme that said what selected text should be
/// gets that, whatever the background is doing.
#[test]
fn an_explicit_selected_text_wins_over_the_contrast_rule() {
    let theme = theme(
        "\"background\": \"none\", \"primary\": \"#ffffff\", \
             \"selectedListItemText\": \"#ff8800\"",
    );

    assert_eq!(theme.selected_fg(None), Color::Rgb(0xff, 0x88, 0x00));
}

/// The contrast rule itself: over a transparent background the fill behind
/// the text is what decides, and the threshold is upstream's 0.5.
#[test]
fn a_transparent_background_picks_black_or_white_by_brightness() {
    let theme = theme("\"background\": \"none\", \"primary\": \"#fab283\"");

    // Bright fills take black text.
    assert_eq!(
        theme.selected_fg(Some(Rgba::rgb(0xff, 0xff, 0xff))),
        Color::Black
    );
    // Dark fills take white.
    assert_eq!(
        theme.selected_fg(Some(Rgba::rgb(0x1e, 0x1e, 0x1e))),
        Color::White
    );
    // With no fill named, the theme's own primary is what is measured;
    // #fab283 is bright, so black.
    assert_eq!(theme.selected_fg(None), Color::Black);
}

#[test]
fn an_opaque_background_is_what_selected_text_falls_back_to() {
    let theme = theme("\"background\": \"#0a0a0a\", \"primary\": \"#fab283\"");

    assert_eq!(
        theme.selected_fg(Some(Rgba::rgb(0xff, 0xff, 0xff))),
        Color::Rgb(0x0a, 0x0a, 0x0a),
        "an opaque theme punches its own background through the fill"
    );
}

#[test]
fn luminance_uses_upstreams_weights() {
    // Green weighs most, blue least: the ordering the weights encode.
    assert!(Rgba::rgb(0, 255, 0).luminance() > Rgba::rgb(255, 0, 0).luminance());
    assert!(Rgba::rgb(255, 0, 0).luminance() > Rgba::rgb(0, 0, 255).luminance());
    assert!((Rgba::rgb(255, 255, 255).luminance() - 1.0).abs() < f32::EPSILON);
    assert!(Rgba::rgb(0, 0, 0).luminance().abs() < f32::EPSILON);
}

/// The default has to stay exactly what P1 shipped: it is what every
/// component test renders against, and what the terminal theme is.
#[test]
fn the_default_theme_is_the_terminal_one_and_still_has_p1s_colors() {
    let theme = Theme::default();

    assert_eq!(theme.name(), TERMINAL_THEME);
    assert_eq!(theme.revision(), 0);
    assert_eq!(theme.fg, Style::new().fg(Color::Reset));
    assert_eq!(theme.dim, Style::new().fg(Color::DarkGray));
    assert_eq!(
        theme.accent,
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    );
    assert_eq!(theme.add, Style::new().fg(Color::Green));
    assert_eq!(theme.remove, Style::new().fg(Color::Red));
    assert_eq!(theme.error, Style::new().fg(Color::Red));
}

/// A terminal with a translucent background keeps it: nothing is painted
/// over the surfaces.
#[test]
fn the_terminal_theme_paints_no_surfaces() {
    let theme = Theme::default();

    assert_eq!(theme.background, Style::new());
    assert_eq!(theme.background_panel, Style::new());
    assert_eq!(theme.background_element, Style::new());
}

/// Even the theme that names ANSI slots has to answer the contrast
/// question, because the dialogs ask it.
#[test]
fn the_terminal_theme_still_answers_the_contrast_rule() {
    let theme = Theme::default();

    assert_eq!(
        theme.selected_fg(Some(Rgba::rgb(0xff, 0xff, 0xff))),
        Color::Black
    );
    assert_eq!(theme.selected_fg(None), Color::White, "ANSI cyan is dark");
}

#[test]
fn a_mode_names_the_arm_it_reads() {
    assert_eq!(Mode::Dark.key(), "dark");
    assert_eq!(Mode::Light.key(), "light");
    assert_eq!(Mode::default(), Mode::Dark);
    assert_eq!(Mode::Light.to_string(), "light");
}

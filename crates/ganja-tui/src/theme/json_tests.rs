use super::{ThemeError, ThemeJson, ansi_to_rgba, from_hex};
use crate::theme::{Mode, Rgba};

/// A theme file whose `theme` block is `body`, with `defs` prepended when
/// there are any.
fn theme(defs: &str, body: &str) -> ThemeJson {
    let text = if defs.is_empty() {
        format!("{{\"theme\": {{{body}}}}}")
    } else {
        format!("{{\"defs\": {{{defs}}}, \"theme\": {{{body}}}}}")
    };

    ThemeJson::parse(&text).expect("the fixture is a theme")
}

#[test]
fn a_file_without_a_theme_block_is_not_a_theme() {
    let refusal = ThemeJson::parse("{\"defs\": {}}").expect_err("a theme needs a theme block");

    assert!(matches!(refusal, ThemeError::Decode(_)), "got: {refusal:?}");
}

#[test]
fn a_schema_key_is_accepted_and_ignored() {
    let parsed = ThemeJson::parse(
        "{\"$schema\": \"https://opencode.ai/theme.json\", \"theme\": {\"text\": \"#ffffff\"}}",
    )
    .expect("the schema key is not an error");

    assert_eq!(
        parsed.resolve(Mode::Dark).expect("it resolves").get("text"),
        Some(Rgba::rgb(0xff, 0xff, 0xff))
    );
}

#[test]
fn hex_values_resolve_to_their_channels() {
    let palette =
        theme("", "\"text\": \"#1e2a3b\"").resolve(Mode::Dark).expect("a hex value resolves");

    assert_eq!(palette.get("text"), Some(Rgba::rgb(0x1e, 0x2a, 0x3b)));
}

#[test]
fn none_and_transparent_both_mean_the_terminal_shows_through() {
    for keyword in ["none", "transparent"] {
        let palette = theme("", &format!("\"background\": \"{keyword}\""))
            .resolve(Mode::Dark)
            .expect("a keyword resolves");

        assert_eq!(
            palette.get("background"),
            Some(Rgba::TRANSPARENT),
            "{keyword} should be alpha zero"
        );
        assert_eq!(
            palette.get("background").expect("it resolved").color(),
            None,
            "{keyword} must leave the ratatui color unset rather than paint black"
        );
    }
}

/// A flat theme — `aura`'s shape — where every value is a bare reference
/// into `defs`.
#[test]
fn a_bare_reference_resolves_through_defs() {
    let palette = theme("\"purple\": \"#a277ff\"", "\"primary\": \"purple\"")
        .resolve(Mode::Dark)
        .expect("a reference resolves");

    assert_eq!(palette.get("primary"), Some(Rgba::rgb(0xa2, 0x77, 0xff)));
}

/// Upstream reads `defs[c] ?? theme.theme[c]`, so a name in both places is
/// the def. Getting this backwards would silently change every theme that
/// happens to name a def after a color key.
#[test]
fn defs_win_over_theme_keys_of_the_same_name() {
    let palette =
        theme("\"accent\": \"#111111\"", "\"accent\": \"#222222\", \"primary\": \"accent\"")
            .resolve(Mode::Dark)
            .expect("it resolves");

    assert_eq!(palette.get("primary"), Some(Rgba::rgb(0x11, 0x11, 0x11)), "the def should win");
    assert_eq!(
        palette.get("accent"),
        Some(Rgba::rgb(0x22, 0x22, 0x22)),
        "the key itself still resolves to its own value"
    );
}

/// A reference into the theme block rather than into `defs`, which is the
/// half of the lookup the test above cannot reach.
#[test]
fn a_reference_falls_back_to_the_theme_block() {
    let palette = theme("", "\"text\": \"#abcdef\", \"markdownText\": \"text\"")
        .resolve(Mode::Dark)
        .expect("it resolves");

    assert_eq!(palette.get("markdownText"), Some(Rgba::rgb(0xab, 0xcd, 0xef)));
}

#[test]
fn a_reference_to_nothing_is_refused_by_name() {
    let refusal = theme("", "\"primary\": \"nosuchcolor\"")
        .resolve(Mode::Dark)
        .expect_err("an unknown reference cannot resolve");

    let message = refusal.to_string();
    assert!(message.contains("nosuchcolor"), "got: {message}");
    assert!(
        message.contains("primary"),
        "the key being resolved should be named too, got: {message}"
    );
}

/// The load-time error R11 asks for: upstream raises this at render time,
/// which paints half a screen before it gives up.
#[test]
fn a_reference_cycle_is_refused_with_the_chain_that_closed_it() {
    let refusal = theme("\"a\": \"b\", \"b\": \"c\", \"c\": \"a\"", "\"primary\": \"a\"")
        .resolve(Mode::Dark)
        .expect_err("a cycle cannot resolve");

    let message = refusal.to_string();
    assert!(
        message.contains("a -> b -> c -> a"),
        "the chain should say how the cycle closed, got: {message}"
    );
}

/// A value that points at itself is the shortest cycle there is, and the
/// one a hand-edited file produces most often.
#[test]
fn a_self_reference_is_a_cycle_too() {
    let refusal = theme("", "\"primary\": \"primary\"")
        .resolve(Mode::Dark)
        .expect_err("a self-reference cannot resolve");

    assert!(
        matches!(refusal, ThemeError::Key { ref source, .. } if matches!(**source, ThemeError::Cycle { .. })),
        "got: {refusal:?}"
    );
}

/// The same name reached twice down different branches is not a cycle; only
/// a name already on the path is. A detector that remembered every name it
/// had ever seen would reject most real themes.
#[test]
fn a_name_reused_by_two_keys_is_not_a_cycle() {
    let palette = theme("\"gray\": \"#808080\"", "\"text\": \"gray\", \"textMuted\": \"gray\"")
        .resolve(Mode::Dark)
        .expect("sharing a def is not a cycle");

    assert_eq!(palette.get("text"), palette.get("textMuted"));
}

#[test]
fn a_variant_resolves_the_arm_the_mode_names() {
    let file = theme(
        "\"darkFg\": \"#eeeeee\", \"lightFg\": \"#111111\"",
        "\"text\": {\"dark\": \"darkFg\", \"light\": \"lightFg\"}",
    );

    assert_eq!(
        file.resolve(Mode::Dark).expect("dark resolves").get("text"),
        Some(Rgba::rgb(0xee, 0xee, 0xee))
    );
    assert_eq!(
        file.resolve(Mode::Light).expect("light resolves").get("text"),
        Some(Rgba::rgb(0x11, 0x11, 0x11))
    );
}

/// The arm goes back through the whole dispatch, so a variant may hold
/// anything a top-level value may hold.
#[test]
fn a_variant_arm_may_be_a_keyword_or_an_ansi_code() {
    let file = theme("", "\"background\": {\"dark\": \"none\", \"light\": 15}");

    assert_eq!(
        file.resolve(Mode::Dark).expect("dark resolves").get("background"),
        Some(Rgba::TRANSPARENT)
    );
    assert_eq!(
        file.resolve(Mode::Light).expect("light resolves").get("background"),
        Some(Rgba::rgb(0xff, 0xff, 0xff))
    );
}

#[test]
fn a_variant_missing_the_arm_this_mode_needs_is_refused() {
    let refusal = theme("", "\"text\": {\"dark\": \"#ffffff\"}")
        .resolve(Mode::Light)
        .expect_err("there is no light arm");

    assert!(refusal.to_string().contains("light"), "got: {refusal}");
}

#[test]
fn a_value_that_is_not_a_color_is_refused_by_kind() {
    let cases = [
        ("\"text\": null", "null"),
        ("\"text\": true", "a boolean"),
        ("\"text\": [\"#ffffff\"]", "a list"),
    ];

    for (body, expected) in cases {
        let refusal = theme("", body).resolve(Mode::Dark).expect_err("{body} is not a color");

        assert!(refusal.to_string().contains(expected), "{body}: got {refusal}");
    }
}

#[test]
fn a_malformed_hex_value_is_refused_rather_than_guessed_at() {
    for value in ["#fff", "#ffffffff", "#nothex", "#"] {
        let refusal = theme("", &format!("\"text\": \"{value}\""))
            .resolve(Mode::Dark)
            .expect_err("a short or long hex value is not a color");

        assert!(refusal.to_string().contains(value), "got: {refusal}");
    }
}

#[test]
fn six_hex_digits_parse_case_insensitively() {
    assert_eq!(from_hex("AbCdEf"), Some(Rgba::rgb(0xab, 0xcd, 0xef)));
    assert_eq!(from_hex("abcde"), None);
    assert_eq!(from_hex("abcdeg"), None);
}

/// The absent optional keys upstream fills in after the loop.
#[test]
fn the_optional_keys_fall_back_the_way_upstream_fills_them_in() {
    let palette = theme("", "\"background\": \"#0a0a0a\", \"backgroundElement\": \"#1e1e1e\"")
        .resolve(Mode::Dark)
        .expect("it resolves");

    assert_eq!(
        palette.get("selectedListItemText"),
        Some(Rgba::rgb(0x0a, 0x0a, 0x0a)),
        "an absent selectedListItemText takes the background"
    );
    assert_eq!(
        palette.get("backgroundMenu"),
        Some(Rgba::rgb(0x1e, 0x1e, 0x1e)),
        "an absent backgroundMenu takes backgroundElement"
    );
    assert!(!palette.has_explicit_selected_text());
}

#[test]
fn a_theme_that_sets_the_optional_keys_keeps_its_own_values() {
    let palette = theme(
        "",
        "\"background\": \"#0a0a0a\", \"backgroundElement\": \"#1e1e1e\", \
             \"selectedListItemText\": \"#ff0000\", \"backgroundMenu\": \"#00ff00\"",
    )
    .resolve(Mode::Dark)
    .expect("it resolves");

    assert_eq!(palette.get("selectedListItemText"), Some(Rgba::rgb(0xff, 0x00, 0x00)));
    assert_eq!(palette.get("backgroundMenu"), Some(Rgba::rgb(0x00, 0xff, 0x00)));
    assert!(palette.has_explicit_selected_text(), "the contrast helper branches on this");
}

/// Keys the UI does not read still have to survive resolution, because the
/// markdown and syntax renderers are the ones that will read them.
#[test]
fn keys_the_ui_does_not_consume_are_resolved_and_kept() {
    let palette = theme(
        "",
        "\"text\": \"#ffffff\", \"syntaxKeyword\": \"#ff00ff\", \
             \"markdownHeading\": \"#00ffff\", \"thinkingOpacity\": 0.6",
    )
    .resolve(Mode::Dark)
    .expect("it resolves");

    assert_eq!(palette.get("syntaxKeyword"), Some(Rgba::rgb(0xff, 0x00, 0xff)));
    assert_eq!(palette.get("markdownHeading"), Some(Rgba::rgb(0x00, 0xff, 0xff)));
    assert_eq!(
        palette.get("thinkingOpacity"),
        None,
        "the one scalar in the block is not a color and must not resolve as ANSI 0"
    );
}

#[test]
fn the_ansi_table_matches_upstreams() {
    // Slot, then the hex upstream writes for it. Two from each arm of the
    // function plus both edges of each range.
    let cases: [(i64, Rgba); 12] = [
        (0, Rgba::rgb(0x00, 0x00, 0x00)),
        (7, Rgba::rgb(0xc0, 0xc0, 0xc0)),
        (15, Rgba::rgb(0xff, 0xff, 0xff)),
        // The cube starts at 16 with all channels zero.
        (16, Rgba::rgb(0, 0, 0)),
        // index 1 -> b = 1 -> 1 * 40 + 55.
        (17, Rgba::rgb(0, 0, 95)),
        // index 6 -> g = 1.
        (22, Rgba::rgb(0, 95, 0)),
        // index 36 -> r = 1.
        (52, Rgba::rgb(95, 0, 0)),
        // The far corner: every channel at level 5.
        (231, Rgba::rgb(255, 255, 255)),
        // The gray ramp's ends.
        (232, Rgba::rgb(8, 8, 8)),
        (255, Rgba::rgb(238, 238, 238)),
        // Out of range at both ends answers black.
        (256, Rgba::rgb(0, 0, 0)),
        (-1, Rgba::rgb(0, 0, 0)),
    ];

    for (code, expected) in cases {
        assert_eq!(ansi_to_rgba(code), expected, "ANSI code {code}");
    }
}

/// The whole cube, checked against the formula rather than against a table
/// copied out of the same source the implementation was: every level must
/// be one of the six upstream can produce.
#[test]
fn every_cube_code_lands_on_one_of_the_six_levels() {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    for code in 16..232 {
        let color = ansi_to_rgba(code);

        for channel in [color.r, color.g, color.b] {
            assert!(
                LEVELS.contains(&channel),
                "ANSI {code} produced channel {channel}, which is not a cube level"
            );
        }
        assert_eq!(color.a, 255, "ANSI colors are opaque");
    }
}

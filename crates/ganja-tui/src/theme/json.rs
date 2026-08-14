//! Upstream's theme file, parsed and resolved into colors.
//!
//! Spec: upstream `packages/tui/src/theme/index.ts` — `ThemeJson`,
//! `resolveTheme` and `ansiToRgba`. The dispatch in [`ThemeJson::resolve_value`] is
//! upstream's, arm for arm and in the same order, because a theme written
//! against opencode's schema has to mean the same thing here; the port's own
//! judgement is confined to what a *failure* does, which upstream defers to
//! render time and this one settles at load time.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use super::{Mode, Rgba};

/// The optional key that falls back to [`BACKGROUND`], and the flag the
/// contrast helper branches on.
pub(crate) const SELECTED_LIST_ITEM_TEXT: &str = "selectedListItemText";
/// The optional key that falls back to [`BACKGROUND_ELEMENT`].
pub(crate) const BACKGROUND_MENU: &str = "backgroundMenu";
/// See [`SELECTED_LIST_ITEM_TEXT`].
pub(crate) const BACKGROUND: &str = "background";
/// See [`BACKGROUND_MENU`].
pub(crate) const BACKGROUND_ELEMENT: &str = "backgroundElement";

/// The one entry in the theme block that is a number meaning a number rather
/// than a number meaning an ANSI color.
///
/// Upstream filters it out of the resolve loop for exactly that reason
/// (`index.ts:268`); left in, it would resolve as ANSI code `0` and paint
/// something black. Nothing renders thinking text yet, so it is dropped rather
/// than carried.
const THINKING_OPACITY: &str = "thinkingOpacity";

/// What a theme file can fail at.
///
/// Upstream throws these from `resolveTheme`, which runs at render time; here
/// they surface while a theme is being loaded, so a broken file is refused
/// before it can be selected rather than while it is being drawn.
#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("the file is not a theme: {0}")]
    Decode(#[from] serde_json::Error),
    /// Which key was being resolved when one of the errors below was raised.
    /// Upstream's messages carry no such context; a user editing a 50-key file
    /// needs it.
    #[error("{key}: {source}")]
    Key {
        key: String,
        #[source]
        source: Box<ThemeError>,
    },
    #[error("color reference \"{name}\" is not in defs or theme")]
    UnknownReference { name: String },
    #[error("circular color reference: {}", chain.join(" -> "))]
    Cycle { chain: Vec<String> },
    #[error("\"{value}\" is not a #rrggbb color")]
    Hex { value: String },
    #[error("a variant has no {mode} arm")]
    MissingArm { mode: Mode },
    #[error("{found} is not a color")]
    NotAColor { found: &'static str },
}

/// A theme file, as upstream writes them.
///
/// Unknown top-level keys — `$schema` among them — are accepted and ignored,
/// which is upstream's behavior: the published JSON Schema is editor tooling
/// and is never enforced at runtime (`packages/web/public/theme.json`).
#[derive(Clone, Debug, Deserialize)]
pub struct ThemeJson {
    /// Named colors the theme block refers to by name.
    ///
    /// Upstream types these as hex-or-reference, but resolution recurses
    /// through the same dispatch a theme value takes, so a def may be anything
    /// a theme value may be. Typing it as a raw value is what the runtime
    /// actually accepts.
    #[serde(default)]
    defs: BTreeMap<String, Value>,
    /// The color keys themselves. Required: a file without one is not a theme,
    /// which is the whole of upstream's `isTheme` check (`index.ts:194-198`).
    theme: BTreeMap<String, Value>,
}

/// Every color key a theme names, resolved for one mode.
///
/// Keys are kept as strings rather than as an enum of the 52 upstream names:
/// the port consumes about a dozen of them today and the rest have to survive
/// a round trip untouched so that markdown and syntax rendering can pick them
/// up without every theme file needing to be reloaded first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Palette {
    colors: BTreeMap<String, Rgba>,
    explicit_selected_text: bool,
}

impl Palette {
    /// The color `key` resolved to, or [`None`] for a key this theme does not
    /// name.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Rgba> {
        self.colors.get(key).copied()
    }

    /// Whether the theme set `SELECTED_LIST_ITEM_TEXT` itself rather than
    /// inheriting the background, which is what [`super::Theme::selected_fg`]
    /// branches on first.
    #[must_use]
    pub fn has_explicit_selected_text(&self) -> bool {
        self.explicit_selected_text
    }

    /// How many keys resolved, which is how a test tells that a theme carrying
    /// markdown and syntax keys kept them rather than dropping them at the
    /// dozen the UI reads.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.colors.len()
    }

    /// Whether nothing resolved; beside [`Palette::len`] because clippy wants
    /// the pair whole.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.colors.is_empty()
    }

    /// Builds a palette from colors named in code rather than in a file, which
    /// is what the built-in terminal theme is.
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (&'static str, Rgba)>) -> Self {
        Self {
            colors: pairs
                .into_iter()
                .map(|(key, color)| (key.to_owned(), color))
                .collect(),
            explicit_selected_text: false,
        }
    }
}

impl ThemeJson {
    /// Decodes one theme file.
    ///
    /// # Errors
    ///
    /// Returns an error if `text` is not JSON, or is JSON without a `theme`
    /// object.
    pub fn parse(text: &str) -> Result<Self, ThemeError> {
        Ok(serde_json::from_str(text)?)
    }

    /// Resolves every color key for `mode`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the key that carried a reference to nothing, a
    /// reference cycle, a malformed hex value, a variant missing the arm this
    /// mode needs, or a value that is not a color at all.
    pub fn resolve(&self, mode: Mode) -> Result<Palette, ThemeError> {
        let mut colors = BTreeMap::new();

        for (key, value) in &self.theme {
            if key == THINKING_OPACITY {
                continue;
            }
            // The two optional keys resolve here like any other when they are
            // present; their absence is what the post-pass below fills in.
            let color = self
                .resolve_value(value, mode, &mut Vec::new())
                .map_err(|source| ThemeError::Key {
                    key: key.clone(),
                    source: Box::new(source),
                })?;
            colors.insert(key.clone(), color);
        }

        // Upstream's post-pass (`index.ts:275-289`). A theme that names neither
        // key still has to answer for both, because the UI reads them.
        let explicit_selected_text = colors.contains_key(SELECTED_LIST_ITEM_TEXT);
        if !explicit_selected_text && let Some(background) = colors.get(BACKGROUND).copied() {
            colors.insert(SELECTED_LIST_ITEM_TEXT.to_owned(), background);
        }
        if !colors.contains_key(BACKGROUND_MENU)
            && let Some(element) = colors.get(BACKGROUND_ELEMENT).copied()
        {
            colors.insert(BACKGROUND_MENU.to_owned(), element);
        }

        Ok(Palette {
            colors,
            explicit_selected_text,
        })
    }

    /// One value, resolved the way upstream's `resolveColor` resolves it.
    ///
    /// `chain` is the reference path walked so far, which is both the cycle
    /// detector and what a cycle error reports.
    fn resolve_value(
        &self,
        value: &Value,
        mode: Mode,
        chain: &mut Vec<String>,
    ) -> Result<Rgba, ThemeError> {
        match value {
            Value::String(text) => self.resolve_string(text, mode, chain),
            // Runtime-legal though the TypeScript type omits it, and the
            // published schema allows `integer 0..255`: a theme may name an
            // ANSI slot instead of an RGB value.
            Value::Number(number) => Ok(ansi_to_rgba(number.as_i64().unwrap_or(-1))),
            Value::Object(arms) => {
                let arm = arms
                    .get(mode.key())
                    .ok_or(ThemeError::MissingArm { mode })?;
                // The arm goes back through the same dispatch, so it may itself
                // be a reference, a hex value, "none", or an ANSI code.
                self.resolve_value(arm, mode, chain)
            }
            Value::Null => Err(ThemeError::NotAColor { found: "null" }),
            Value::Bool(_) => Err(ThemeError::NotAColor { found: "a boolean" }),
            Value::Array(_) => Err(ThemeError::NotAColor { found: "a list" }),
        }
    }

    /// The string arm: a keyword, a hex value, or a reference to resolve.
    fn resolve_string(
        &self,
        text: &str,
        mode: Mode,
        chain: &mut Vec<String>,
    ) -> Result<Rgba, ThemeError> {
        // Alpha zero, not black: transparency is what lets the terminal show
        // through, and ratatui says that by leaving the color unset.
        if text == "transparent" || text == "none" {
            return Ok(Rgba::TRANSPARENT);
        }
        if let Some(digits) = text.strip_prefix('#') {
            return from_hex(digits).ok_or_else(|| ThemeError::Hex {
                value: text.to_owned(),
            });
        }

        if chain.iter().any(|link| link == text) {
            let mut chain = chain.clone();
            chain.push(text.to_owned());
            return Err(ThemeError::Cycle { chain });
        }

        // `defs` wins over the theme block, so a def and a color key may share
        // a name without the def becoming unreachable.
        let next = self
            .defs
            .get(text)
            .or_else(|| self.theme.get(text))
            .ok_or_else(|| ThemeError::UnknownReference {
                name: text.to_owned(),
            })?;

        chain.push(text.to_owned());
        let resolved = self.resolve_value(next, mode, chain);
        chain.pop();

        resolved
    }
}

/// `#rrggbb` with the `#` already stripped.
///
/// Six digits exactly: upstream's published schema pins `^#[0-9a-fA-F]{6}$`,
/// and every one of the 33 shipped themes obeys it. Three- and eight-digit
/// forms are refused rather than guessed at, so a typo is reported instead of
/// rendering a color nobody chose (deviation D18 — upstream's `RGBA.fromHex`
/// returns garbage for these rather than raising).
fn from_hex(digits: &str) -> Option<Rgba> {
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();

    Some(Rgba::rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// The 16 standard ANSI colors, in slot order.
///
/// Upstream spells them as hex strings (`index.ts:304-321`); the hex is kept in
/// the comments so the two tables can be compared by eye.
const ANSI_STANDARD: [Rgba; 16] = [
    Rgba::rgb(0x00, 0x00, 0x00), // #000000 black
    Rgba::rgb(0x80, 0x00, 0x00), // #800000 red
    Rgba::rgb(0x00, 0x80, 0x00), // #008000 green
    Rgba::rgb(0x80, 0x80, 0x00), // #808000 yellow
    Rgba::rgb(0x00, 0x00, 0x80), // #000080 blue
    Rgba::rgb(0x80, 0x00, 0x80), // #800080 magenta
    Rgba::rgb(0x00, 0x80, 0x80), // #008080 cyan
    Rgba::rgb(0xc0, 0xc0, 0xc0), // #c0c0c0 white
    Rgba::rgb(0x80, 0x80, 0x80), // #808080 bright black
    Rgba::rgb(0xff, 0x00, 0x00), // #ff0000 bright red
    Rgba::rgb(0x00, 0xff, 0x00), // #00ff00 bright green
    Rgba::rgb(0xff, 0xff, 0x00), // #ffff00 bright yellow
    Rgba::rgb(0x00, 0x00, 0xff), // #0000ff bright blue
    Rgba::rgb(0xff, 0x00, 0xff), // #ff00ff bright magenta
    Rgba::rgb(0x00, 0xff, 0xff), // #00ffff bright cyan
    Rgba::rgb(0xff, 0xff, 0xff), // #ffffff bright white
];

/// Upstream's `ansiToRgba` (`index.ts:301-344`), ported arm for arm.
///
/// Codes outside 0..=255 answer black, which is what upstream's array lookup
/// and its trailing fallback both produce.
pub(crate) fn ansi_to_rgba(code: i64) -> Rgba {
    const BLACK: Rgba = Rgba::rgb(0, 0, 0);

    let Ok(code) = u16::try_from(code) else {
        return BLACK;
    };

    // Standard slots.
    if let Some(color) = ANSI_STANDARD.get(usize::from(code)) {
        return *color;
    }

    // The 6x6x6 cube: each channel is one of six levels, and the levels are
    // not evenly spaced — the first is 0 and the rest start at 95.
    if code < 232 {
        let index = code - 16;
        let level = |x: u16| -> u8 {
            let value = if x == 0 { 0 } else { x * 40 + 55 };

            u8::try_from(value).unwrap_or(u8::MAX)
        };

        return Rgba::rgb(level(index / 36), level((index / 6) % 6), level(index % 6));
    }

    // The 24-step gray ramp.
    if code < 256 {
        let gray = u8::try_from((code - 232) * 10 + 8).unwrap_or(u8::MAX);

        return Rgba::rgb(gray, gray, gray);
    }

    BLACK
}

#[cfg(test)]
mod tests {
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
        let palette = theme("", "\"text\": \"#1e2a3b\"")
            .resolve(Mode::Dark)
            .expect("a hex value resolves");

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
        let palette = theme(
            "\"accent\": \"#111111\"",
            "\"accent\": \"#222222\", \"primary\": \"accent\"",
        )
        .resolve(Mode::Dark)
        .expect("it resolves");

        assert_eq!(
            palette.get("primary"),
            Some(Rgba::rgb(0x11, 0x11, 0x11)),
            "the def should win"
        );
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

        assert_eq!(
            palette.get("markdownText"),
            Some(Rgba::rgb(0xab, 0xcd, 0xef))
        );
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
        let refusal = theme(
            "\"a\": \"b\", \"b\": \"c\", \"c\": \"a\"",
            "\"primary\": \"a\"",
        )
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
        let palette = theme(
            "\"gray\": \"#808080\"",
            "\"text\": \"gray\", \"textMuted\": \"gray\"",
        )
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
            file.resolve(Mode::Light)
                .expect("light resolves")
                .get("text"),
            Some(Rgba::rgb(0x11, 0x11, 0x11))
        );
    }

    /// The arm goes back through the whole dispatch, so a variant may hold
    /// anything a top-level value may hold.
    #[test]
    fn a_variant_arm_may_be_a_keyword_or_an_ansi_code() {
        let file = theme("", "\"background\": {\"dark\": \"none\", \"light\": 15}");

        assert_eq!(
            file.resolve(Mode::Dark)
                .expect("dark resolves")
                .get("background"),
            Some(Rgba::TRANSPARENT)
        );
        assert_eq!(
            file.resolve(Mode::Light)
                .expect("light resolves")
                .get("background"),
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
            let refusal = theme("", body)
                .resolve(Mode::Dark)
                .expect_err("{body} is not a color");

            assert!(
                refusal.to_string().contains(expected),
                "{body}: got {refusal}"
            );
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
        let palette = theme(
            "",
            "\"background\": \"#0a0a0a\", \"backgroundElement\": \"#1e1e1e\"",
        )
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

        assert_eq!(
            palette.get("selectedListItemText"),
            Some(Rgba::rgb(0xff, 0x00, 0x00))
        );
        assert_eq!(
            palette.get("backgroundMenu"),
            Some(Rgba::rgb(0x00, 0xff, 0x00))
        );
        assert!(
            palette.has_explicit_selected_text(),
            "the contrast helper branches on this"
        );
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

        assert_eq!(
            palette.get("syntaxKeyword"),
            Some(Rgba::rgb(0xff, 0x00, 0xff))
        );
        assert_eq!(
            palette.get("markdownHeading"),
            Some(Rgba::rgb(0x00, 0xff, 0xff))
        );
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
}

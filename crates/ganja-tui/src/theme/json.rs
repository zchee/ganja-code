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
#[path = "json_tests.rs"]
mod tests;

//! Themes as loadable data.
//!
//! Spec: upstream `packages/tui/src/theme/index.ts`. The file format is ported
//! whole — `defs` indirection, dark/light variants, integer ANSI codes — so a
//! theme written for opencode loads here unchanged. What is *done* with the
//! resolved colors is this port's own, because upstream draws onto a
//! compositing surface where an alpha-zero color really does let the terminal
//! through, while ratatui draws cells: here that same alpha-zero means the
//! color is left unset, which is the cell-level way of saying the same thing.
//!
//! [`Theme`] keeps the six roles P1 shipped so that every component compiles
//! unchanged, and grows the slots the rest of the UI needs. Nothing outside
//! this module names a literal color.

mod json;
mod registry;
mod selection;

use ratatui::style::{Color, Modifier, Style};

pub use self::{
    json::{Palette, ThemeError, ThemeJson},
    registry::{DEFAULT_THEME, TERMINAL_THEME, Themes},
    selection::SelectionError,
};

/// Theme keys the UI reads. The rest resolve and are carried on [`Theme`]
/// untouched, for the markdown and syntax renderers that will read them.
mod key {
    pub(super) const TEXT: &str = "text";
    pub(super) const TEXT_MUTED: &str = "textMuted";
    pub(super) const ACCENT: &str = "accent";
    pub(super) const DIFF_ADDED: &str = "diffAdded";
    pub(super) const DIFF_REMOVED: &str = "diffRemoved";
    pub(super) const ERROR: &str = "error";
    pub(super) const PRIMARY: &str = "primary";
    pub(super) const SECONDARY: &str = "secondary";
    pub(super) const WARNING: &str = "warning";
    pub(super) const SUCCESS: &str = "success";
    pub(super) const INFO: &str = "info";
    pub(super) const BACKGROUND_PANEL: &str = "backgroundPanel";
    pub(super) const BORDER: &str = "border";
    pub(super) const BORDER_ACTIVE: &str = "borderActive";
}

/// Which arm of a `{dark, light}` variant a resolve picks.
///
/// Upstream detects this from the terminal and lets the user pin it; ganja
/// takes it from configuration and defaults to dark (deviation D3 — terminal
/// auto-detection is deferred).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

impl Mode {
    /// The name the arm carries in a theme file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.key())
    }
}

/// A color a theme resolved to.
///
/// The alpha channel is either fully opaque or fully transparent — the only
/// two values upstream's resolver produces — and transparent means the
/// terminal shows through, which [`Rgba::color`] expresses as no color at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// What `"none"` and `"transparent"` resolve to.
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// An opaque color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// The ratatui color to paint, or [`None`] when the terminal should show
    /// through.
    ///
    /// A transparent color is *unset*, not black: a cell whose foreground is
    /// left alone keeps whatever the terminal's own palette puts there, which
    /// is what upstream's alpha-zero achieves on its compositing surface.
    #[must_use]
    pub fn color(self) -> Option<Color> {
        (self.a != 0).then_some(Color::Rgb(self.r, self.g, self.b))
    }

    /// Perceived brightness on a 0..1 scale, upstream's weights
    /// (`index.ts:105`).
    #[must_use]
    pub fn luminance(self) -> f32 {
        let channel = |value: u8| f32::from(value) / 255.0;

        0.299 * channel(self.r) + 0.587 * channel(self.g) + 0.114 * channel(self.b)
    }
}

/// The styles the components share.
///
/// The six roles P1 shipped keep their names and meanings; the rest are the
/// slots upstream's UI reads that ganja is growing into. Foreground roles carry
/// a foreground; the three `background*` slots carry a background, so a surface
/// is composed as `theme.fg.patch(theme.background_panel)` rather than by
/// picking colors apart.
///
/// A slot whose key the theme does not name, or names transparent, is left
/// unset rather than given an invented color — the terminal's own default is a
/// better answer than a guess.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Body text.
    pub fg: Style,
    /// Chrome that should recede: headers, hints, separators.
    pub dim: Style,
    /// Whatever the eye should land on first.
    pub accent: Style,
    /// A diff line that added text.
    pub add: Style,
    /// A diff line that removed text.
    pub remove: Style,
    /// A tool call that failed, or was refused.
    pub error: Style,
    /// The selected row's fill, and the cursor.
    pub primary: Style,
    /// Attachments and agent chips.
    pub secondary: Style,
    /// Something the user should notice but that is not a failure.
    pub warning: Style,
    /// Something that finished cleanly.
    pub success: Style,
    /// Neutral status.
    pub info: Style,
    /// The surface everything is drawn onto.
    pub background: Style,
    /// Panels and message cards, a step off the background.
    pub background_panel: Style,
    /// Interactive fills: the prompt box, a hovered row.
    pub background_element: Style,
    /// A frame at rest.
    pub border: Style,
    /// A frame with the focus.
    pub border_active: Style,
    /// The row under a cursor: `primary` as the fill, with whichever of black
    /// or white stays readable over it (`ui/dialog-select.tsx:539-543`).
    ///
    /// Derived rather than mapped from a key, because upstream derives it too
    /// — and deriving it here is what keeps [`Theme::selected_fg`]'s rule in
    /// one place instead of in every dialog.
    pub selection: Style,
    name: String,
    revision: u64,
    palette: Palette,
}

impl Theme {
    /// The name it was selected by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A counter that changes whenever the active theme does.
    ///
    /// The transcript caches the lines it wrapped, styles included, so a cache
    /// keyed on width alone would keep painting the old palette after a switch.
    /// Comparing this alongside the width is what makes a switch repaint.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The color a theme key resolved to, including the keys no component
    /// reads yet.
    #[must_use]
    pub fn color(&self, key: &str) -> Option<Rgba> {
        self.palette.get(key)
    }

    /// Every color this theme resolved.
    #[must_use]
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// What to write over `bg` so the text stays readable.
    ///
    /// Upstream's `selectedForeground` (`index.ts:95-111`): a theme that names
    /// `selectedListItemText` gets what it asked for; one drawn over a
    /// transparent background has to pick black or white by the brightness of
    /// whatever is actually behind the text; anything else reads as a hole
    /// punched in the background color.
    #[must_use]
    pub fn selected_fg(&self, bg: Option<Rgba>) -> Color {
        if self.palette.has_explicit_selected_text()
            && let Some(chosen) = self.palette.get(json::SELECTED_LIST_ITEM_TEXT)
        {
            return chosen.color().unwrap_or(Color::Reset);
        }

        // An absent background is treated as a transparent one: both mean this
        // theme has no fill to punch through.
        match self.palette.get(json::BACKGROUND) {
            Some(background) if background.a != 0 => background.color().unwrap_or(Color::Reset),
            _ => match bg.or_else(|| self.palette.get(key::PRIMARY)) {
                Some(target) if target.luminance() > 0.5 => Color::Black,
                Some(_) => Color::White,
                None => Color::Reset,
            },
        }
    }

    /// Maps a resolved palette onto the slots the UI reads.
    ///
    /// `accent` deliberately carries no weight here even though the terminal
    /// theme's does: upstream's `accent` is a color and nothing more, and a
    /// theme that chose its accent has already said how it wants to stand out.
    pub(crate) fn from_palette(name: String, revision: u64, palette: Palette) -> Self {
        let fg = |key: &str| {
            palette
                .get(key)
                .and_then(Rgba::color)
                .map_or_else(Style::new, |color| Style::new().fg(color))
        };
        let bg = |key: &str| {
            palette
                .get(key)
                .and_then(Rgba::color)
                .map_or_else(Style::new, |color| Style::new().bg(color))
        };

        let mut theme = Self {
            fg: fg(key::TEXT),
            dim: fg(key::TEXT_MUTED),
            accent: fg(key::ACCENT),
            add: fg(key::DIFF_ADDED),
            remove: fg(key::DIFF_REMOVED),
            error: fg(key::ERROR),
            primary: fg(key::PRIMARY),
            secondary: fg(key::SECONDARY),
            warning: fg(key::WARNING),
            success: fg(key::SUCCESS),
            info: fg(key::INFO),
            background: bg(json::BACKGROUND),
            background_panel: bg(key::BACKGROUND_PANEL),
            background_element: bg(json::BACKGROUND_ELEMENT),
            border: fg(key::BORDER),
            border_active: fg(key::BORDER_ACTIVE),
            // Filled in below: it is the one slot that reads the palette
            // through a rule rather than through a key.
            selection: Style::new(),
            name,
            revision,
            palette,
        };
        theme.selection = theme.selection_style();

        theme
    }

    /// The selected-row style, built the way upstream builds it.
    fn selection_style(&self) -> Style {
        let fill = self.palette.get(key::PRIMARY);
        let style = Style::new().fg(self.selected_fg(fill));

        match fill.and_then(Rgba::color) {
            Some(color) => style.bg(color),
            None => style,
        }
    }

    /// The built-in theme that defers to the terminal's own palette.
    ///
    /// Stands in for upstream's generated `system` theme (deviation D15):
    /// upstream builds that one by querying the terminal for its 16 colors and
    /// deriving a 12-step ramp from them, which needs a capability ganja does
    /// not have yet. Naming the ANSI slots instead gets the same effect for the
    /// roles that matter — the user's configured colors, whatever they are —
    /// without pretending to know their values.
    ///
    /// The palette carries the *standard* values of the slots it names, so that
    /// [`Theme::selected_fg`] has something to measure. Those are the styles'
    /// nominal colors, not necessarily what the terminal will paint.
    pub(crate) fn terminal(revision: u64) -> Self {
        // Upstream's `generateSystem` assigns the same roles to the same slots:
        // primary/accent/info cyan, secondary magenta, error red, warning
        // yellow, success green (`index.ts:377-388, 401-409`).
        let palette = Palette::from_pairs([
            (key::TEXT_MUTED, json::ansi_to_rgba(8)),
            (key::ACCENT, json::ansi_to_rgba(6)),
            (key::DIFF_ADDED, json::ansi_to_rgba(2)),
            (key::DIFF_REMOVED, json::ansi_to_rgba(1)),
            (key::ERROR, json::ansi_to_rgba(1)),
            (key::PRIMARY, json::ansi_to_rgba(6)),
            (key::SECONDARY, json::ansi_to_rgba(5)),
            (key::WARNING, json::ansi_to_rgba(3)),
            (key::SUCCESS, json::ansi_to_rgba(2)),
            (key::INFO, json::ansi_to_rgba(6)),
            (key::BORDER, json::ansi_to_rgba(8)),
            (key::BORDER_ACTIVE, json::ansi_to_rgba(6)),
            // Transparent on purpose, exactly as upstream's system theme is:
            // it is what keeps a translucent terminal translucent.
            (json::BACKGROUND, Rgba::TRANSPARENT),
        ]);

        Self {
            fg: Style::new().fg(Color::Reset),
            dim: Style::new().fg(Color::DarkGray),
            accent: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            add: Style::new().fg(Color::Green),
            remove: Style::new().fg(Color::Red),
            error: Style::new().fg(Color::Red),
            primary: Style::new().fg(Color::Cyan),
            secondary: Style::new().fg(Color::Magenta),
            warning: Style::new().fg(Color::Yellow),
            success: Style::new().fg(Color::Green),
            info: Style::new().fg(Color::Cyan),
            // The three surfaces stay unset so the terminal's own background —
            // image, transparency and all — is what shows.
            background: Style::new(),
            background_panel: Style::new(),
            background_element: Style::new(),
            border: Style::new().fg(Color::DarkGray),
            border_active: Style::new().fg(Color::Cyan),
            // Named rather than derived, so the fill is the terminal's own
            // cyan. White is what the contrast rule answers for it: standard
            // ANSI cyan is `#008080`, well below the 0.5 threshold.
            selection: Style::new().fg(Color::White).bg(Color::Cyan),
            name: TERMINAL_THEME.to_owned(),
            revision,
            palette,
        }
    }
}

impl Default for Theme {
    /// The terminal theme, at revision zero.
    ///
    /// Revision zero is reserved for it: [`Themes`] numbers what it hands out
    /// from one, so a cache filled from this default can never be mistaken for
    /// a cache filled from a selected theme.
    fn default() -> Self {
        Self::terminal(0)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};

    use super::{Mode, Rgba, TERMINAL_THEME, Theme, ThemeJson};

    /// A theme file naming `body`, resolved for dark.
    fn theme(body: &str) -> Theme {
        let file =
            ThemeJson::parse(&format!("{{\"theme\": {{{body}}}}}")).expect("the fixture parses");

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
}

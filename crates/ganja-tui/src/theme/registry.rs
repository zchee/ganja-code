//! The themes a run can choose between, and which one is active.
//!
//! Spec: upstream `packages/tui/src/theme/index.ts` (`DEFAULT_THEMES`,
//! `listThemes`) and `packages/tui/src/context/theme.tsx` (discovery,
//! selection, persistence).
//!
//! Two deliberate divergences from upstream's discovery, both recorded in the
//! phase contract:
//!
//! * a custom theme that will not load is skipped on its own, naming the file
//!   (D16). Upstream lets one unparseable file reject the whole scan, which
//!   silently discards *every* custom theme and resets the selection — a
//!   failure mode a user editing one file cannot diagnose;
//! * themes are looked for under the config directory only, not additionally in
//!   every `.opencode` directory from the cwd up to the filesystem root.
//!   Upstream's walk lets a directory *above* the project override the one
//!   inside it, which its own documentation contradicts.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use super::{
    Mode, Palette, Theme, ThemeJson,
    selection::{self, SelectionError},
};

/// The theme a run starts on, and what an unknown name falls back to.
///
/// Upstream's default (`context/theme.tsx:121-122`), and ganja's for the same
/// reason: it is the one theme every screenshot and every bug report shares.
pub const DEFAULT_THEME: &str = "opencode";

/// The built-in that defers to the terminal's own palette. See
/// `Theme::terminal`.
pub const TERMINAL_THEME: &str = "terminal";

/// Directory under ganja's config home holding a user's own themes.
const CUSTOM_DIRECTORY: &str = "themes";

/// The extension a theme file has to carry to be picked up.
const EXTENSION: &str = "json";

/// The themes compiled into the binary, verbatim from upstream's
/// `packages/tui/src/theme/assets/`. Each is enumerated in
/// `THIRD_PARTY_NOTICES.md`.
const BUILTIN_FILES: [(&str, &str); 4] = [
    (
        DEFAULT_THEME,
        include_str!("../../assets/themes/opencode.json"),
    ),
    (
        "tokyonight",
        include_str!("../../assets/themes/tokyonight.json"),
    ),
    ("gruvbox", include_str!("../../assets/themes/gruvbox.json")),
    ("aura", include_str!("../../assets/themes/aura.json")),
];

/// One theme, ready to be handed out in either mode.
///
/// Both modes are resolved when the theme is registered rather than when it is
/// picked, which is what makes a reference cycle a load-time error (R11): a
/// theme that cannot resolve never enters the registry, so selecting one can
/// no longer fail.
#[derive(Clone, Debug)]
enum Entry {
    Json {
        dark: Palette,
        light: Palette,
    },
    /// The stand-in for upstream's generated `system` theme, which is built in
    /// code rather than resolved from a file.
    Terminal,
}

impl Entry {
    /// Resolves `text` for both modes, or answers why it cannot be a theme.
    fn parse(text: &str) -> Result<Self, super::ThemeError> {
        let file = ThemeJson::parse(text)?;

        Ok(Self::Json {
            dark: file.resolve(Mode::Dark)?,
            light: file.resolve(Mode::Light)?,
        })
    }

    fn theme(&self, name: &str, mode: Mode, revision: u64) -> Theme {
        match self {
            Self::Json { dark, light } => {
                let palette = match mode {
                    Mode::Dark => dark,
                    Mode::Light => light,
                };

                Theme::from_palette(name.to_owned(), revision, palette.clone())
            }
            Self::Terminal => Theme::terminal(revision),
        }
    }
}

/// Every theme this run can switch between.
#[derive(Clone, Debug)]
pub struct Themes {
    entries: BTreeMap<String, Entry>,
    active: String,
    mode: Mode,
    /// Where a runtime pick is written, or [`None`] when there is nowhere to
    /// write one — in which case a pick lasts for this run only.
    store: Option<PathBuf>,
    revision: u64,
}

impl Themes {
    /// The compiled-in themes alone.
    ///
    /// Touches no disk, which is what makes it the right constructor for a test
    /// and for any path that must not depend on the user's home directory.
    #[must_use]
    pub fn builtin() -> Self {
        let mut entries = BTreeMap::new();

        for (name, text) in BUILTIN_FILES {
            match Entry::parse(text) {
                Ok(entry) => {
                    entries.insert(name.to_owned(), entry);
                }
                // Unreachable short of a build that shipped a broken asset;
                // `every_builtin_theme_resolves_in_both_modes` is what keeps it
                // that way. Logged rather than panicked because a frontend that
                // refuses to start over one theme is worse than one that starts
                // without it.
                Err(refusal) => {
                    tracing::error!(theme = name, %refusal, "a builtin theme did not load");
                }
            }
        }
        entries.insert(TERMINAL_THEME.to_owned(), Entry::Terminal);

        Self {
            entries,
            active: DEFAULT_THEME.to_owned(),
            mode: Mode::default(),
            store: None,
            revision: 0,
        }
    }

    /// The builtins, the user's own themes, and the pick they last made.
    ///
    /// Directories that do not exist are not an error: most users have no
    /// custom themes, and the ones who do create the directory themselves.
    #[must_use]
    pub fn load() -> Self {
        let mut themes = Self::builtin();

        if let Some(directory) = custom_directory() {
            themes.add_custom_dir(&directory);
        }
        // P5 tui-2: a `theme` in the config file outranks the stored pick
        // permanently, so the config wiring lane selects after this and the
        // dialog's Enter stops being the last word.
        if let Some(path) = selection::path() {
            themes.adopt_store(path);
        }

        themes
    }

    /// Registers every theme file in `directory`, shadowing builtins by name.
    ///
    /// A file that will not load is skipped with a warning naming it, so one
    /// bad theme costs its author that theme and nothing else.
    pub fn add_custom_dir(&mut self, directory: &Path) {
        let listing = match fs::read_dir(directory) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(
                    directory = %directory.display(),
                    %error,
                    "the custom theme directory could not be read"
                );
                return;
            }
        };

        // Sorted so that what a run registers does not depend on the order the
        // filesystem happens to answer in.
        let mut files: Vec<PathBuf> = listing
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == EXTENSION)
            })
            .collect();
        files.sort();

        for path in files {
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }

            match fs::read_to_string(&path).map_err(|error| error.to_string()) {
                Ok(text) => match Entry::parse(&text) {
                    Ok(entry) => {
                        self.entries.insert(name.to_owned(), entry);
                    }
                    Err(refusal) => tracing::warn!(
                        theme = %path.display(),
                        refusal = %refusal,
                        "a custom theme was skipped"
                    ),
                },
                Err(error) => tracing::warn!(
                    theme = %path.display(),
                    %error,
                    "a custom theme could not be read"
                ),
            }
        }
    }

    /// Points runtime picks at `path`, and adopts the pick already stored there.
    ///
    /// A stored name this build does not have — a custom theme that was
    /// deleted, or one from a newer version — leaves the default active rather
    /// than leaving the app with no theme at all.
    pub fn adopt_store(&mut self, path: PathBuf) {
        if let Some(stored) = selection::read(&path)
            && self.entries.contains_key(&stored)
        {
            self.active = stored;
        }

        self.store = Some(path);
    }

    /// Every theme's name, ordered the way the dialog lists them.
    ///
    /// Case-insensitively, as upstream sorts (`dialog-theme-list.tsx:8-9`), with
    /// the raw name breaking ties so the order is total.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.keys().cloned().collect();
        names.sort_by(|left, right| {
            left.to_lowercase()
                .cmp(&right.to_lowercase())
                .then_with(|| left.cmp(right))
        });

        names
    }

    /// The name of the theme in use.
    #[must_use]
    pub fn active(&self) -> &str {
        &self.active
    }

    /// Which arm of a variant resolves.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switches every theme to `mode`'s arm.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Whether `name` is a theme this run has.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// The active theme, at a revision no earlier call handed out.
    pub fn theme(&mut self) -> Theme {
        self.revision += 1;

        // The active name is checked on the way in, so this only falls back if
        // the registry lost every theme, which the terminal entry prevents.
        self.entries.get(&self.active).map_or_else(
            || Theme::terminal(self.revision),
            |entry| entry.theme(&self.active, self.mode, self.revision),
        )
    }

    /// Makes `name` active and resolves it, or answers [`None`] for a name this
    /// run does not have.
    pub fn select(&mut self, name: &str) -> Option<Theme> {
        if !self.entries.contains_key(name) {
            return None;
        }
        self.active = name.to_owned();

        Some(self.theme())
    }

    /// Writes the active theme's name, so the next run opens on it.
    ///
    /// # Errors
    ///
    /// Returns an error if there is nowhere to store a pick, or if the file
    /// cannot be written.
    pub fn persist(&self) -> Result<(), SelectionError> {
        let path = self.store.as_ref().ok_or(SelectionError::Unlocatable)?;

        selection::write(path, &self.active)
    }
}

impl Default for Themes {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Where a user's own themes live: `<config home>/themes`.
///
/// Resolved through `ganja_core::config::config_home` rather than a private
/// XDG lookup, so a build pointed somewhere by `GANJA_CONFIG_HOME` — or served
/// by `~/.ganja` — reads its themes from the same directory its config,
/// instructions and skills come from.
fn custom_directory() -> Option<PathBuf> {
    Some(ganja_core::config::config_home()?.join(CUSTOM_DIRECTORY))
}

#[cfg(test)]
mod tests {
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
            assert!(
                dark.len() >= 50,
                "{name} resolved only {} keys in dark",
                dark.len()
            );
            assert_eq!(
                dark.len(),
                light.len(),
                "{name} should resolve the same keys in both modes"
            );
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
            themes
                .select("midnight")
                .expect("the custom theme registers")
                .color("text"),
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
            vec![
                "aura",
                "Ayu",
                "gruvbox",
                "opencode",
                "terminal",
                "tokyonight",
                "Zenburn"
            ],
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
        custom(
            &directory,
            "darkonly",
            "{\"theme\": {\"text\": {\"dark\": \"#101020\"}}}",
        );
        custom(&directory, "bothmodes", &minimal("#123456"));

        let mut themes = Themes::builtin();
        themes.add_custom_dir(directory.path());

        assert!(
            !themes.names().contains(&"darkonly".to_owned()),
            "got: {:?}",
            themes.names()
        );
        assert!(
            themes.select("darkonly").is_none(),
            "and there is nothing to select either"
        );
        assert!(
            themes.names().contains(&"bothmodes".to_owned()),
            "the theme beside it still loaded"
        );
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
            themes
                .select(DEFAULT_THEME)
                .expect("the name still resolves")
                .color("text"),
            Some(crate::theme::Rgba::rgb(0xab, 0xcd, 0xef))
        );
        assert_eq!(
            themes
                .names()
                .iter()
                .filter(|name| *name == DEFAULT_THEME)
                .count(),
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
        fs::write(
            &store,
            "{\"version\":1,\"theme\":\"a-theme-that-was-deleted\"}",
        )
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

        assert!(
            refusal.to_string().contains("could not be located"),
            "got: {refusal}"
        );
    }
}

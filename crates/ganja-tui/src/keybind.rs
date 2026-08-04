//! Which keys reach which actions, and how a config file rebinds them.
//!
//! Spec: upstream `packages/tui/src/config/keybind.ts`. Upstream binds
//! roughly a hundred actions and layers a `<leader>` prefix over them;
//! ganja binds the five a frontend this size actually has (**D4**), and has
//! no leader — a chord that exists to disambiguate a hundred bindings is
//! nothing but a delay when there are five.
//!
//! The whole table is rebindable from `keybinds` in the config file, and both
//! ways of getting it wrong fail at startup naming what was wrong: an action
//! this build does not have, and a key string this build cannot parse. Neither
//! is survivable by guessing — a typo'd action silently doing nothing is the
//! failure mode the hard error exists to prevent.

use std::{collections::BTreeMap, fmt};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything a key can be bound to.
///
/// Deliberately short. Every entry here is something a user reaches often
/// enough to want a key for; everything else lives behind the palette, which
/// is one key away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Leave ganja.
    AppExit,
    /// Open the command palette.
    PaletteOpen,
    /// Open the stored-session picker.
    SessionsOpen,
    /// Open the theme picker.
    ThemesOpen,
    /// Move to the next agent.
    AgentCycle,
}

/// The action a config key names, its default binding, in the order a
/// reference lists them.
const ACTIONS: &[(Action, &str, &str)] = &[
    // Upstream's default is `ctrl+c,ctrl+d,<leader>q`. Ganja keeps `ctrl+q`
    // beside them because it has always had it, and drops the leader chord it
    // has never had (deviation: keybind-app-exit-keeps-ctrl-q).
    (Action::AppExit, "app_exit", "ctrl+c,ctrl+q,ctrl+d"),
    (Action::PaletteOpen, "palette_open", "ctrl+p"),
    (Action::SessionsOpen, "sessions_open", "ctrl+s"),
    (Action::ThemesOpen, "themes_open", "ctrl+t"),
    (Action::AgentCycle, "agent_cycle", "tab"),
];

impl Action {
    /// The name a config file spells this action with.
    #[must_use]
    pub fn key(self) -> &'static str {
        ACTIONS
            .iter()
            .find(|(action, _, _)| *action == self)
            .map_or("", |(_, name, _)| *name)
    }

    /// Every action, in reference order.
    pub fn all() -> impl Iterator<Item = Self> {
        ACTIONS.iter().map(|(action, _, _)| *action)
    }
}

/// A `keybinds` map this build cannot use.
#[derive(Debug, thiserror::Error)]
pub enum KeybindError {
    /// A key of the map is not an action this build has. Named rather than
    /// ignored: a binding that silently does nothing looks like a broken
    /// keyboard.
    #[error("unknown keybind action {name:?}; this build binds {}", known())]
    UnknownAction {
        /// What the config file said.
        name: String,
    },
    /// A value of the map is not a key this build can recognize.
    #[error("{action}: cannot parse the key {key:?}")]
    UnparseableKey {
        /// The action whose binding it was.
        action: String,
        /// The offending alternative, not the whole comma-separated value, so
        /// the message points at the part that is wrong.
        key: String,
    },
}

/// The action names this build has, for the error above.
fn known() -> String {
    ACTIONS
        .iter()
        .map(|(_, name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One binding: the keys that reach an action.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    action: Action,
    keys: Vec<KeyEvent>,
}

/// Which keys reach which actions this run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keybinds {
    bindings: Vec<Binding>,
}

impl Keybinds {
    /// The compiled-in bindings.
    ///
    /// # Panics
    ///
    /// Never in a build whose defaults parse, which
    /// `every_default_binding_parses` is what keeps true.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            bindings: ACTIONS
                .iter()
                .map(|(action, _, default)| Binding {
                    action: *action,
                    keys: parse(default).expect("a compiled-in default binding parses"),
                })
                .collect(),
        }
    }

    /// The defaults, with whatever `configured` rebinds.
    ///
    /// A map that mentions an action replaces that action's keys outright
    /// rather than adding to them: a user writing `"app_exit": "f5"` means f5
    /// and not "f5 as well as ctrl+c".
    ///
    /// # Errors
    ///
    /// Returns [`KeybindError`] for an action this build does not have, and
    /// for a key string it cannot parse. Both fail the run rather than being
    /// dropped, because a binding that quietly did not take is indistinguishable
    /// from one that did nothing.
    pub fn from_config(configured: &BTreeMap<String, String>) -> Result<Self, KeybindError> {
        let mut binds = Self::defaults();

        for (name, value) in configured {
            let action = ACTIONS
                .iter()
                .find(|(_, key, _)| key == name)
                .map(|(action, _, _)| *action)
                .ok_or_else(|| KeybindError::UnknownAction { name: name.clone() })?;

            let keys = parse(value).map_err(|key| KeybindError::UnparseableKey {
                action: name.clone(),
                key,
            })?;

            if let Some(binding) = binds
                .bindings
                .iter_mut()
                .find(|binding| binding.action == action)
            {
                binding.keys = keys;
            }
        }

        Ok(binds)
    }

    /// Whether `key` reaches `action`.
    #[must_use]
    pub fn binds(&self, action: Action, key: KeyEvent) -> bool {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .is_some_and(|binding| binding.keys.iter().any(|bound| same(*bound, key)))
    }

    /// The action `key` reaches, or [`None`].
    ///
    /// Answers in reference order, so a key bound twice reaches whichever
    /// action is listed first rather than whichever happened to be checked.
    #[must_use]
    pub fn action(&self, key: KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .find(|binding| binding.keys.iter().any(|bound| same(*bound, key)))
            .map(|binding| binding.action)
    }

    /// How `action`'s keys are shown to a person, or [`None`] when it has
    /// none left.
    #[must_use]
    pub fn hint(&self, action: Action) -> Option<String> {
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.action == action)?;
        let keys: Vec<String> = binding.keys.iter().map(|key| render(*key)).collect();

        (!keys.is_empty()).then(|| keys.join(", "))
    }
}

impl Default for Keybinds {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Whether two key events are the same binding.
///
/// Code and modifiers only: the kind (press, repeat) is the caller's gate, and
/// the state carries things like caps-lock that no binding is about.
///
/// Both sides are folded into one spelling first, because shift has more than
/// one. A config file writes `shift+a`, and the terminal reports the capital
/// `A` that shift already produced — sometimes with the modifier still set and
/// sometimes without. Comparing those literally is how a `shift+…` binding
/// parses, loads, renders in a hint, and then never fires.
fn same(bound: KeyEvent, pressed: KeyEvent) -> bool {
    canonical(bound) == canonical(pressed)
}

/// One key event in the single spelling a comparison can be made in.
///
/// Shift is not a modifier of a key the way ctrl is: the terminal applies it
/// and hands over the result, so it is folded into the key itself and dropped
/// from the modifiers. Every other modifier is left exactly as it came, and an
/// unshifted `a` therefore still does not answer to `A`.
fn canonical(key: KeyEvent) -> (KeyCode, KeyModifiers) {
    let shifted = key.modifiers.contains(KeyModifiers::SHIFT);
    let code = match key.code {
        KeyCode::Char(character) if shifted => {
            // `to_uppercase` can yield more than one character for a few
            // letters; a key is one, and the first is the one a keyboard sends.
            KeyCode::Char(character.to_uppercase().next().unwrap_or(character))
        }
        // Shift-tab has its own key code, so `shift+tab` and `backtab` are two
        // ways of writing the key every terminal reports as the second.
        KeyCode::Tab if shifted => KeyCode::BackTab,
        other => other,
    };

    (code, key.modifiers - KeyModifiers::SHIFT)
}

/// Every alternative in a comma-separated binding.
///
/// [`Err`] carries the alternative that did not parse rather than the whole
/// value, so the message can point at the part that is wrong.
fn parse(value: &str) -> Result<Vec<KeyEvent>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|alternative| !alternative.is_empty())
        .map(|alternative| key(alternative).ok_or_else(|| alternative.to_owned()))
        .collect()
}

/// One `ctrl+shift+x` style binding.
///
/// Case-insensitive throughout, which is why case cannot be what carries
/// shift: a shifted letter is written `shift+a`, never `A`. [`same`] is where
/// that meets what the terminal reports.
fn key(text: &str) -> Option<KeyEvent> {
    let lowered = text.to_ascii_lowercase();
    let mut parts = lowered.split('+').peekable();
    let mut modifiers = KeyModifiers::NONE;

    let mut name = parts.next()?;
    while parts.peek().is_some() {
        modifiers |= modifier(name)?;
        name = parts.next()?;
    }
    if name.is_empty() {
        return None;
    }

    Some(KeyEvent::new(code(name)?, modifiers))
}

/// One modifier word.
fn modifier(name: &str) -> Option<KeyModifiers> {
    match name {
        "ctrl" | "control" => Some(KeyModifiers::CONTROL),
        "alt" | "option" | "meta" => Some(KeyModifiers::ALT),
        "shift" => Some(KeyModifiers::SHIFT),
        "super" | "cmd" | "command" => Some(KeyModifiers::SUPER),
        "hyper" => Some(KeyModifiers::HYPER),
        _ => None,
    }
}

/// One key name.
fn code(name: &str) -> Option<KeyCode> {
    if let Some(number) = name.strip_prefix('f')
        && let Ok(number) = number.parse::<u8>()
        && (1..=24).contains(&number)
    {
        return Some(KeyCode::F(number));
    }

    let named = match name {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        _ => {
            let mut characters = name.chars();
            let single = characters.next()?;

            return characters.next().is_none().then_some(KeyCode::Char(single));
        }
    };

    Some(named)
}

/// How a binding is written back out, for a hint line.
fn render(key: KeyEvent) -> String {
    let mut rendered = String::new();
    for (modifier, name) in [
        (KeyModifiers::CONTROL, "ctrl"),
        (KeyModifiers::ALT, "alt"),
        (KeyModifiers::SHIFT, "shift"),
        (KeyModifiers::SUPER, "super"),
        (KeyModifiers::HYPER, "hyper"),
    ] {
        if key.modifiers.contains(modifier) {
            rendered.push_str(name);
            rendered.push('+');
        }
    }
    rendered.push_str(&name(key.code));

    rendered
}

/// How one key code is spelled in a hint.
fn name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

impl fmt::Display for Action {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.key())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{ACTIONS, Action, KeybindError, Keybinds, key, parse};

    fn pressed(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn configured(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn every_default_binding_parses() {
        for (action, name, default) in ACTIONS {
            let keys = parse(default).unwrap_or_else(|bad| panic!("{name}: {bad} did not parse"));
            assert!(!keys.is_empty(), "{action:?} should bind something");
        }
    }

    #[test]
    fn the_defaults_are_the_keys_this_frontend_has_always_used() {
        let binds = Keybinds::defaults();
        let cases = [
            (Action::AppExit, KeyCode::Char('c'), KeyModifiers::CONTROL),
            (Action::AppExit, KeyCode::Char('q'), KeyModifiers::CONTROL),
            (Action::AppExit, KeyCode::Char('d'), KeyModifiers::CONTROL),
            (
                Action::PaletteOpen,
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
            ),
            (
                Action::SessionsOpen,
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            ),
            (
                Action::ThemesOpen,
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            ),
            (Action::AgentCycle, KeyCode::Tab, KeyModifiers::NONE),
        ];

        for (action, code, modifiers) in cases {
            assert!(
                binds.binds(action, pressed(code, modifiers)),
                "{code:?}+{modifiers:?} should reach {action:?}"
            );
        }
    }

    #[test]
    fn a_key_string_parses_the_shapes_a_config_file_can_write() {
        let cases = [
            ("ctrl+x", KeyCode::Char('x'), KeyModifiers::CONTROL),
            ("CTRL+X", KeyCode::Char('x'), KeyModifiers::CONTROL),
            ("f5", KeyCode::F(5), KeyModifiers::NONE),
            ("f12", KeyCode::F(12), KeyModifiers::NONE),
            ("home", KeyCode::Home, KeyModifiers::NONE),
            ("pgup", KeyCode::PageUp, KeyModifiers::NONE),
            ("esc", KeyCode::Esc, KeyModifiers::NONE),
            ("space", KeyCode::Char(' '), KeyModifiers::NONE),
            (
                "ctrl+alt+delete",
                KeyCode::Delete,
                KeyModifiers::CONTROL.union(KeyModifiers::ALT),
            ),
            ("shift+tab", KeyCode::Tab, KeyModifiers::SHIFT),
        ];

        for (text, code, modifiers) in cases {
            assert_eq!(
                key(text),
                Some(pressed(code, modifiers)),
                "{text} should parse"
            );
        }
    }

    #[test]
    fn a_key_string_this_build_cannot_read_parses_to_nothing() {
        for text in ["", "ctrl+", "hyperspace+x", "f99", "notakey", "ctrl+ab"] {
            assert_eq!(key(text), None, "{text:?} should not parse");
        }
    }

    #[test]
    fn a_config_binding_replaces_the_default_rather_than_joining_it() {
        let binds = Keybinds::from_config(&configured(&[("palette_open", "f5")]))
            .expect("a legible binding loads");

        assert!(binds.binds(
            Action::PaletteOpen,
            pressed(KeyCode::F(5), KeyModifiers::NONE)
        ));
        assert!(
            !binds.binds(
                Action::PaletteOpen,
                pressed(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            "the default should be gone, not kept alongside"
        );
    }

    #[test]
    fn comma_separated_alternatives_all_reach_the_action() {
        let binds = Keybinds::from_config(&configured(&[("themes_open", "f2, ctrl+y")]))
            .expect("a legible binding loads");

        assert!(binds.binds(
            Action::ThemesOpen,
            pressed(KeyCode::F(2), KeyModifiers::NONE)
        ));
        assert!(binds.binds(
            Action::ThemesOpen,
            pressed(KeyCode::Char('y'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn an_action_this_build_does_not_have_is_named_rather_than_ignored() {
        let refusal = Keybinds::from_config(&configured(&[("session_share", "ctrl+z")]))
            .expect_err("an unknown action must not load");

        assert!(
            matches!(&refusal, KeybindError::UnknownAction { name } if name == "session_share"),
            "got {refusal:?}"
        );
        assert!(
            refusal.to_string().contains("session_share"),
            "the message should name it: {refusal}"
        );
    }

    #[test]
    fn a_key_this_build_cannot_parse_is_named_rather_than_ignored() {
        let refusal = Keybinds::from_config(&configured(&[("app_exit", "ctrl+c, hypermeta+z")]))
            .expect_err("an unparseable key must not load");

        assert!(
            matches!(&refusal, KeybindError::UnparseableKey { action, key }
                if action == "app_exit" && key == "hypermeta+z"),
            "got {refusal:?}"
        );
        assert!(
            refusal.to_string().contains("hypermeta+z"),
            "the message should name it: {refusal}"
        );
    }

    /// A `shift+…` binding has to survive the round trip through the terminal,
    /// which reports the letter shift already produced rather than the letter
    /// the config file wrote — with the modifier still set on some terminals
    /// and folded away on others. Both have to reach the action, and the
    /// unshifted key must still not.
    #[test]
    fn a_shifted_binding_answers_to_the_key_the_terminal_actually_reports() {
        let binds = Keybinds::from_config(&configured(&[("agent_cycle", "shift+a")]))
            .expect("a legible binding loads");

        let cases = [
            (KeyCode::Char('A'), KeyModifiers::SHIFT, true),
            (KeyCode::Char('A'), KeyModifiers::NONE, true),
            (KeyCode::Char('a'), KeyModifiers::SHIFT, true),
            (KeyCode::Char('a'), KeyModifiers::NONE, false),
        ];

        for (code, modifiers, reaches) in cases {
            assert_eq!(
                binds.binds(Action::AgentCycle, pressed(code, modifiers)),
                reaches,
                "{code:?}+{modifiers:?}"
            );
            assert_eq!(
                binds.action(pressed(code, modifiers)) == Some(Action::AgentCycle),
                reaches,
                "{code:?}+{modifiers:?} through the lookup that has no action in hand"
            );
        }
    }

    /// Shift-tab is the one key with two names, and every terminal reports it
    /// under the second. Both spellings must reach it, and neither may reach
    /// plain tab.
    #[test]
    fn shift_tab_and_backtab_are_one_key_however_they_were_written() {
        for spelling in ["shift+tab", "backtab"] {
            let binds = Keybinds::from_config(&configured(&[("themes_open", spelling)]))
                .expect("a legible binding loads");

            for modifiers in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
                assert!(
                    binds.binds(Action::ThemesOpen, pressed(KeyCode::BackTab, modifiers)),
                    "{spelling} should answer to backtab+{modifiers:?}"
                );
            }
            assert!(
                !binds.binds(
                    Action::ThemesOpen,
                    pressed(KeyCode::Tab, KeyModifiers::NONE)
                ),
                "{spelling} is not plain tab"
            );
            assert_eq!(
                Keybinds::defaults().action(pressed(KeyCode::BackTab, KeyModifiers::SHIFT)),
                None,
                "and cycling agents on tab is not reached by shift-tab"
            );
        }
    }

    #[test]
    fn a_hint_spells_every_key_that_reaches_an_action() {
        let binds = Keybinds::defaults();

        assert_eq!(
            binds.hint(Action::PaletteOpen).as_deref(),
            Some("ctrl+p"),
            "one key"
        );
        assert_eq!(
            binds.hint(Action::AppExit).as_deref(),
            Some("ctrl+c, ctrl+q, ctrl+d"),
            "every alternative"
        );
    }

    #[test]
    fn an_unbound_key_reaches_nothing() {
        assert_eq!(
            Keybinds::defaults().action(pressed(KeyCode::Char('z'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn every_action_has_a_config_name() {
        for action in Action::all() {
            assert!(!action.key().is_empty(), "{action:?} should be nameable");
        }
    }
}

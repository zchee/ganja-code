//! Typed builders for tmux's own commands, and the register of which ones
//! this crate has named so far.
//!
//! Synthesized, not ported — the Go specification under
//! [`crate::control_mode`] speaks only the persistent protocol, so by the
//! convention that module's doc states, nothing here carries a `Spec:` line.
//!
//! # A builder is words, not a call
//!
//! Every builder answers one question: which argv words does this command
//! want, after the `tmux -S <socket>` a [`Server`][crate::Server] pins? It
//! runs nothing, owns no server, and holds no socket — so the same words can
//! be sent to any server, and nothing here has to grow a second copy of
//! [`Server`][crate::Server]'s addressing.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::{Server, commands::ListPanes};
//!
//! let server = Server::current()?;
//! let panes = server.run(ListPanes::new().all().format("#{pane_id}").args()).await?;
//! for pane in panes.text_lossy().lines() {
//!     println!("{pane}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # The escape hatch came first
//!
//! [`Server::run`][crate::Server::run] already carries **every** tmux
//! command, including the ones this module has not named and the ones a tmux
//! newer than this crate will grow. A builder therefore buys documentation
//! and a compiler-checked flag spelling, never reach — which is why partial
//! coverage is a shippable state, and why the gap is measured rather than
//! claimed: [`REGISTRY`] lists what is typed, [`EXCLUDED`] lists what is not
//! and says why, and `tests/inventory.rs` holds the running tmux's own
//! `list-commands` against their union.
//!
//! # What a flag's argument is typed as
//!
//! - A flag that takes no argument is a no-argument method, and setting it
//!   twice sets it once — argv is a set of flags, not a tally.
//! - A flag that takes one is a method taking `impl Into<OsString>`, and
//!   setting it twice keeps the last value. `OsString` because these come
//!   from the caller's world — a path, a target, a title, a window name —
//!   and a path is not obliged to be UTF-8.
//! - A flag whose argument is *tmux's own language* — a format, a filter, a
//!   sort order, a style, an enumerated position — takes `&str` instead,
//!   because those words are spelled by tmux and could not be anything else.
//! - A flag tmux accepts more than once is a method that may be called more
//!   than once, keeping the order it was called in.
//! - A target (`-t`, `-s`) takes `impl Into<OsString>` so both a
//!   [`PaneId`][crate::PaneId] read out of a previous answer and a raw
//!   `mysession:1.2` spelling pass without either being restrung.
//!
//! **A size is a word, not a number.** tmux spells `-x 10`, `-x 10%`, `-S -`
//! and a negative adjustment in the same argument position, so narrowing a
//! size or a count to an integer would refuse values tmux accepts. Every one
//! of them is therefore an ordinary word, and this crate declines to be
//! stricter than the program it is addressing.
//!
//! # Everything after the flags is separated by `--`
//!
//! A positional argument and a trailing command are emitted after a `--`, so
//! a value that begins with `-` — a search pattern, a program name — cannot
//! be read back as a flag. This is the shape a real consumer already proved
//! against a live server (`split-window -d -P -F … -c … -e … -- argv`), and
//! it is the reason this layer can promise a byte-faithful pass-through at
//! all: without the separator, a caller's bytes could still change meaning.
//!
//! # A required argument is a method like any other
//!
//! `rename-window` needs a new name and `find-window` needs something to
//! find, but omitting one builds fine and fails at tmux, which answers in
//! its own words through
//! [`Error::ClientRefused`][crate::Error::ClientRefused]. That is deliberate:
//! this crate ships the material and leaves the judgment — including "you
//! forgot something" — to the caller and to tmux, exactly as
//! [`crate::server`]'s doc says of an absent `$TMUX`.
//!
//! # Baseline
//!
//! Flags are those of the tmux the port was written against (next-3.8), read
//! from that binary's own usage strings and documented from its manual. An
//! older tmux refuses a flag it does not know, in its own words; a newer one
//! grows commands this module has not named, and the inventory test reports
//! them by name rather than going quiet.

use std::ffi::OsString;

pub mod panes;
pub mod sessions;

// The families keep their own modules — each one's doc is where its roster is
// argued for — but their builders are re-exported here, so a command is named
// `tmux::commands::SplitWindow` for the same reason a control-mode type is
// named `tmux::control_mode::Client` rather than through the file it lives in.
pub use panes::*;
pub use sessions::*;

/// One command this crate has named: what tmux calls it, and what tmux's own
/// abbreviation for it is.
///
/// The abbreviation is carried because `list-commands` prints it and a
/// consumer's config may use it, so the inventory test can hold both halves
/// of tmux's vocabulary against this crate's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// The command's full name, such as `split-window`.
    pub name: &'static str,
    /// tmux's own abbreviation for it, such as `splitw`, when it has one.
    pub alias: Option<&'static str>,
}

/// One command this crate has **not** named yet, and why not.
///
/// A reason rather than a bare name, because an exclusion nobody can justify
/// is an omission wearing a table's clothes. The table shrinks as families
/// land, which is what makes its size a progress measure rather than a
/// permanent apology.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Excluded {
    /// The command's full name.
    pub name: &'static str,
    /// tmux's own abbreviation for it, when it has one.
    pub alias: Option<&'static str>,
    /// Why it is not in [`REGISTRY`] yet.
    pub reason: &'static str,
}

/// What every typed command has in common: the name tmux knows it by, and
/// the words it wants after the socket pin.
///
/// A trait rather than a bare convention so a caller can be generic over
/// "some command" — and so the name in [`REGISTRY`] and the name a builder
/// renders come from the same literal, which is what keeps the register
/// honest as families land.
///
/// It is deliberately **not** called `Command`: in this crate that word
/// already means a control-mode command line
/// ([`control_mode::Command`][crate::control_mode::Command]), which is a
/// rendered *line* rather than a list of argv words, and the two must not
/// read as the same thing.
pub trait Invocation {
    /// The command's full name, such as `split-window`.
    const NAME: &'static str;

    /// tmux's own abbreviation for it, when it has one.
    const ALIAS: Option<&'static str>;

    /// The argv words this invocation wants, everything after the
    /// `tmux -S <socket>` a [`Server`][crate::Server] pins — starting with
    /// the command's own name.
    fn args(&self) -> Vec<OsString>;
}

/// The words a builder has collected, in the order they were asked for.
///
/// One accumulator shared by every builder rather than a struct field per
/// flag: the storage is identical for all of them, so generating fields
/// would generate nothing but noise, and the three set-semantics tmux
/// distinguishes ([`Words::switch`], [`Words::value`], [`Words::repeat`])
/// live here once instead of in each expansion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Words {
    flags: Vec<(&'static str, Option<OsString>)>,
    positional: Vec<OsString>,
    trailing: Vec<OsString>,
}

impl Words {
    /// Sets a flag that takes no argument, once however often it is asked
    /// for: `-d -d` says nothing `-d` does not.
    pub(crate) fn switch(&mut self, flag: &'static str) {
        if !self.flags.iter().any(|(name, _)| *name == flag) {
            self.flags.push((flag, None));
        }
    }

    /// Sets a flag that takes one argument, replacing an earlier value in
    /// the position it was first given — a caller who sets `-c` twice is
    /// correcting themselves, not asking for two working directories.
    pub(crate) fn value(&mut self, flag: &'static str, value: OsString) {
        match self.flags.iter_mut().find(|(name, _)| *name == flag) {
            Some(entry) => entry.1 = Some(value),
            None => self.flags.push((flag, Some(value))),
        }
    }

    /// Adds one more occurrence of a flag tmux accepts repeatedly, keeping
    /// call order.
    pub(crate) fn repeat(&mut self, flag: &'static str, value: OsString) {
        self.flags.push((flag, Some(value)));
    }

    /// Adds a positional argument, after any already given.
    pub(crate) fn positional(&mut self, value: OsString) {
        self.positional.push(value);
    }

    /// Sets the trailing command and its arguments, replacing any already
    /// set — one call is the whole command, so a second is a correction.
    pub(crate) fn trailing<I, S>(&mut self, words: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.trailing = words.into_iter().map(Into::into).collect();
    }

    /// The command's argv: its name, its flags in the order they were asked
    /// for, then — behind a `--` — whatever follows them.
    pub(crate) fn render(&self, name: &'static str) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(1 + self.flags.len() * 2 + self.positional.len() + 1);
        argv.push(OsString::from(name));
        for (flag, value) in &self.flags {
            argv.push(OsString::from(*flag));
            if let Some(value) = value {
                argv.push(value.clone());
            }
        }
        if !self.positional.is_empty() || !self.trailing.is_empty() {
            argv.push(OsString::from("--"));
            argv.extend(self.positional.iter().cloned());
            argv.extend(self.trailing.iter().cloned());
        }

        argv
    }
}

/// One builder method, by the kind of argument its flag takes.
///
/// Split out of [`invocations`] because a `macro_rules!` arm cannot branch on
/// a kind it has already captured; passing the kind through as a token tree
/// keeps it matchable here, which is what lets one table entry spell six
/// different method shapes.
macro_rules! method {
    (switch, $method:ident, $flag:literal, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method(mut self) -> Self {
            self.words.switch($flag);
            self
        }
    };
    (value, $method:ident, $flag:literal, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method(mut self, value: impl Into<::std::ffi::OsString>) -> Self {
            self.words.value($flag, value.into());
            self
        }
    };
    (text, $method:ident, $flag:literal, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method(mut self, value: &str) -> Self {
            self.words.value($flag, ::std::ffi::OsString::from(value));
            self
        }
    };
    (repeat, $method:ident, $flag:literal, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method(mut self, value: impl Into<::std::ffi::OsString>) -> Self {
            self.words.repeat($flag, value.into());
            self
        }
    };
    (positional, $method:ident, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method(mut self, value: impl Into<::std::ffi::OsString>) -> Self {
            self.words.positional(value.into());
            self
        }
    };
    (trailing, $method:ident, $(#[$doc:meta])*) => {
        $(#[$doc])*
        #[must_use]
        pub fn $method<I, S>(mut self, words: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<::std::ffi::OsString>,
        {
            self.words.trailing(words);
            self
        }
    };
}

/// Declares one family of tmux commands: a builder type each, and the
/// module's own slice of [`Entry`] for [`REGISTRY`] to gather.
///
/// One table entry carries everything a command has — its Rust name, tmux's
/// name and abbreviation, its doc, and one line per flag naming the method,
/// the kind of argument it takes and the letter tmux spells it with. The
/// docs come from the table, which is what keeps the crate's
/// `warn(missing_docs)` satisfiable across a hundred generated methods
/// without a hundred hand-written impl blocks.
///
/// Kinds are `switch` (no argument), `value` (`impl Into<OsString>`), `text`
/// (`&str`, for tmux's own languages), `repeat` (a value flag tmux accepts
/// more than once), `positional` and `trailing`; the last two take no flag
/// letter. See this module's doc for why each is typed the way it is.
macro_rules! invocations {
    ($(
        $(#[$outer:meta])*
        $type:ident = $name:literal, $alias:expr => {
            $(
                $(#[$inner:meta])*
                $method:ident: $kind:tt $($flag:literal)?;
            )*
        }
    )*) => {
        $(
            $(#[$outer])*
            #[derive(Clone, Debug, Default, PartialEq, Eq)]
            pub struct $type {
                words: $crate::commands::Words,
            }

            impl $type {
                #[doc = concat!("A `", $name, "` with nothing set.")]
                #[must_use]
                pub fn new() -> Self {
                    Self::default()
                }

                $(
                    $crate::commands::method!(
                        $kind, $method, $($flag,)? $(#[$inner])*
                    );
                )*

                #[doc = concat!(
                    "The argv words this `", $name, "` wants, after the socket pin."
                )]
                #[must_use]
                pub fn args(&self) -> Vec<::std::ffi::OsString> {
                    self.words.render($name)
                }
            }

            impl $crate::commands::Invocation for $type {
                const NAME: &'static str = $name;
                const ALIAS: Option<&'static str> = $alias;

                fn args(&self) -> Vec<::std::ffi::OsString> {
                    $type::args(self)
                }
            }
        )*

        /// Every command this module declares, for [`REGISTRY`] to gather.
        ///
        /// Crate-internal: [`REGISTRY`] is the surface, and a per-family
        /// slice re-exported beside it would give two families one name.
        ///
        /// [`REGISTRY`]: crate::commands::REGISTRY
        pub(crate) const ENTRIES: &[$crate::commands::Entry] = &[
            $($crate::commands::Entry { name: $name, alias: $alias },)*
        ];
    };
}

pub(crate) use invocations;
pub(crate) use method;

/// The families, in the order [`REGISTRY`] lists them. Each later wave adds
/// one name here and nothing else.
const FAMILIES: &[&[Entry]] = &[panes::ENTRIES, sessions::ENTRIES];

/// How many commands the families hold between them.
const fn total(families: &[&[Entry]]) -> usize {
    let mut total = 0;
    let mut family = 0;
    while family < families.len() {
        total += families[family].len();
        family += 1;
    }

    total
}

/// The families laid end to end.
///
/// A `const fn` rather than a `Vec` built at startup, so [`REGISTRY`] is one
/// slice a caller can match against without the module owning any runtime
/// state — and so adding a family cannot cost more than a line.
const fn flattened<const N: usize>(families: &[&[Entry]]) -> [Entry; N] {
    let mut all = [Entry {
        name: "",
        alias: None,
    }; N];
    let mut at = 0;
    let mut family = 0;
    while family < families.len() {
        let entries = families[family];
        let mut index = 0;
        while index < entries.len() {
            all[at] = entries[index];
            at += 1;
            index += 1;
        }
        family += 1;
    }

    all
}

const TYPED: [Entry; total(FAMILIES)] = flattened(FAMILIES);

/// Every tmux command this crate has given a builder, with tmux's own
/// abbreviation for each.
///
/// The anchor the inventory test diffs the running tmux's `list-commands`
/// against; a command in neither this nor [`EXCLUDED`] fails that test by
/// name.
pub const REGISTRY: &[Entry] = &TYPED;

/// Why the buffer, key, prompt and display commands are not here yet.
const BUFFERS_KEYS: &str = "typed in W5: buffers, keys, the prompt and the display";

/// Why the option, hook, environment and mode commands are not here yet.
const OPTIONS_MISC: &str = "typed in W6: options, hooks, the environment, the modes and the rest";

/// Every tmux command this crate knows of and has **not** given a builder,
/// each with the reason.
///
/// Seeded with the three families this crate has not reached, so the
/// inventory test can be green today and shrink to nothing as they land —
/// a table that starts empty would have made the test red for three waves,
/// and a red test nobody can fix is a test nobody reads.
pub const EXCLUDED: &[Excluded] = &[
    // Buffers, keys, the prompt and the display.
    Excluded {
        name: "bind-key",
        alias: Some("bind"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "choose-buffer",
        alias: None,
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "clear-history",
        alias: Some("clearhist"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "clear-prompt-history",
        alias: Some("clearphist"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "command-prompt",
        alias: None,
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "confirm-before",
        alias: Some("confirm"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "delete-buffer",
        alias: Some("deleteb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "display-menu",
        alias: Some("menu"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "display-message",
        alias: Some("display"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "display-popup",
        alias: Some("popup"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "list-buffers",
        alias: Some("lsb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "list-keys",
        alias: Some("lsk"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "load-buffer",
        alias: Some("loadb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "paste-buffer",
        alias: Some("pasteb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "save-buffer",
        alias: Some("saveb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "send-keys",
        alias: Some("send"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "send-prefix",
        alias: None,
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "set-buffer",
        alias: Some("setb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "show-buffer",
        alias: Some("showb"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "show-prompt-history",
        alias: Some("showphist"),
        reason: BUFFERS_KEYS,
    },
    Excluded {
        name: "unbind-key",
        alias: Some("unbind"),
        reason: BUFFERS_KEYS,
    },
    // Options, hooks, the environment, the interactive modes, and the rest.
    Excluded {
        name: "choose-client",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "choose-tree",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "clock-mode",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "copy-mode",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "customize-mode",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "if-shell",
        alias: Some("if"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "run-shell",
        alias: Some("run"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "set-environment",
        alias: Some("setenv"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "set-hook",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "set-option",
        alias: Some("set"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "set-window-option",
        alias: Some("setw"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "show-environment",
        alias: Some("showenv"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "show-hooks",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "show-options",
        alias: Some("show"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "show-window-options",
        alias: Some("showw"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "source-file",
        alias: Some("source"),
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "switch-mode",
        alias: None,
        reason: OPTIONS_MISC,
    },
    Excluded {
        name: "wait-for",
        alias: Some("wait"),
        reason: OPTIONS_MISC,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::panes::{ListPanes, SplitWindow};

    fn words(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_command_with_nothing_set_is_its_name_alone() {
        assert_eq!(words(&ListPanes::new().args()), ["list-panes"]);
    }

    #[test]
    fn a_switch_asked_for_twice_is_still_one_flag() {
        assert_eq!(
            words(&ListPanes::new().all().all().args()),
            ["list-panes", "-a"],
            "argv is a set of flags, not a tally of how often each was asked for"
        );
    }

    #[test]
    fn a_value_set_twice_keeps_the_last_in_the_first_position() {
        assert_eq!(
            words(
                &SplitWindow::new()
                    .start_directory("/one")
                    .detached()
                    .start_directory("/two")
                    .args()
            ),
            ["split-window", "-c", "/two", "-d"],
            "a caller who sets -c twice is correcting themselves, not asking for two directories"
        );
    }

    #[test]
    fn a_repeatable_flag_keeps_every_value_in_call_order() {
        assert_eq!(
            words(
                &SplitWindow::new()
                    .environment("A=1")
                    .environment("B=2")
                    .args()
            ),
            ["split-window", "-e", "A=1", "-e", "B=2"],
            "tmux reads -e once per variable, so the builder must not fold them together"
        );
    }

    #[test]
    fn flags_keep_the_order_they_were_asked_for() {
        assert_eq!(
            words(&ListPanes::new().format("#{pane_id}").all().args()),
            ["list-panes", "-F", "#{pane_id}", "-a"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_value_outside_utf8_survives_into_argv_byte_for_byte() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let directory = OsString::from_vec(b"/tmp/a\x80b".to_vec());
        let argv = SplitWindow::new().start_directory(directory).args();
        assert_eq!(
            argv.last().map(|word| word.as_bytes()),
            Some(&b"/tmp/a\x80b"[..]),
            "a working directory is a path, and a path is not obliged to be UTF-8"
        );
    }

    #[test]
    fn a_trailing_command_is_fenced_off_from_the_flags() {
        assert_eq!(
            words(
                &SplitWindow::new()
                    .detached()
                    .command(["sh", "-c", "sleep 1"])
                    .args()
            ),
            ["split-window", "-d", "--", "sh", "-c", "sleep 1"],
            "without the fence, a program named like a flag would be read as one"
        );
    }

    #[test]
    fn a_trailing_command_set_twice_replaces_rather_than_appends() {
        assert_eq!(
            words(
                &SplitWindow::new()
                    .command(["first"])
                    .command(["second"])
                    .args()
            ),
            ["split-window", "--", "second"]
        );
    }

    #[test]
    fn the_trait_and_the_register_agree_about_a_commands_names() {
        assert_eq!(SplitWindow::NAME, "split-window");
        assert_eq!(SplitWindow::ALIAS, Some("splitw"));
        assert!(
            REGISTRY.contains(&Entry {
                name: SplitWindow::NAME,
                alias: SplitWindow::ALIAS,
            }),
            "a builder tmux knows about but the register does not would be invisible to the \
             inventory test"
        );
    }

    #[test]
    fn the_register_gathers_every_family() {
        assert_eq!(
            REGISTRY.len(),
            FAMILIES.iter().map(|family| family.len()).sum::<usize>(),
            "a family added to FAMILIES but lost by the flattening would go unmeasured"
        );
    }

    #[test]
    fn no_command_is_both_typed_and_excluded() {
        for entry in REGISTRY {
            assert!(
                !EXCLUDED.iter().any(|excluded| excluded.name == entry.name),
                "{} is typed, so its exclusion is a leftover that would outlive its reason",
                entry.name
            );
        }
    }

    #[test]
    fn no_name_or_alias_is_claimed_twice() {
        let mut seen: Vec<&str> = Vec::new();
        for (name, alias) in REGISTRY
            .iter()
            .map(|entry| (entry.name, entry.alias))
            .chain(EXCLUDED.iter().map(|entry| (entry.name, entry.alias)))
        {
            for word in std::iter::once(name).chain(alias) {
                assert!(
                    !seen.contains(&word),
                    "{word:?} is claimed twice; tmux has one command per name and one per alias"
                );
                seen.push(word);
            }
        }
    }

    #[test]
    fn every_exclusion_says_why() {
        for entry in EXCLUDED {
            assert!(
                !entry.reason.trim().is_empty(),
                "{} is excluded without a reason, which is an omission wearing a table's clothes",
                entry.name
            );
        }
    }
}

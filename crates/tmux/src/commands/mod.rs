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
//! - **A positional argument is emitted in the order its method was
//!   called.** A flag is named, so the order it was asked for carries
//!   nothing; a positional is *placed*, so its order is its meaning. The
//!   five commands taking more than one — `set-option`,
//!   `set-window-option`, `set-hook`, `set-environment` and `if-shell` —
//!   must therefore be built in the order tmux's own synopsis gives, and
//!   building one the other way round is this crate's one quiet failure
//!   mode: tmux reads a well-formed command line and runs the wrong thing
//!   rather than refusing it. Where a trailing command follows a
//!   positional instead (`bind-key`, `run-shell`), nothing is left to the
//!   caller: the renderer emits every positional before it.
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
//! The floor is tmux **3.7c** — the release homebrew bottles and CI pours
//! on both platforms — read from that binary's own usage strings and
//! documented from its manual. The tables were first written against the
//! next tmux (next-3.8), and what only it takes was not deleted but
//! *shelved*: an `ahead_` prefix on the row keeps the letter, arity, name
//! and doc in place while generating no method and no served flag, so a
//! builder cannot spell an argv the floor answers with `unknown flag`, and
//! serving the row again when the floor moves is deleting the prefix
//! ([`Entry::ahead`] carries the shelf as data, and the inventory test
//! measures it). An older tmux refuses a flag it does not know, in its own
//! words; a newer one grows commands this module has not named, and the
//! inventory test reports them by name rather than going quiet —
//! `switch-mode` is the one whole command typed ahead of the floor, for the
//! reason its own doc gives.

use std::ffi::OsString;

pub mod buffers_keys;
pub mod options_misc;
pub mod panes;
pub mod sessions;

// The families keep their own modules — each one's doc is where its roster is
// argued for — but their builders are re-exported here, so a command is named
// `tmux::commands::SplitWindow` for the same reason a control-mode type is
// named `tmux::control_mode::Client` rather than through the file it lives in.
pub use buffers_keys::*;
pub use options_misc::*;
pub use panes::*;
pub use sessions::*;

/// One command this crate has named: what tmux calls it, what tmux's own
/// abbreviation for it is, and which flags its builder claims tmux takes.
///
/// The abbreviation is carried because `list-commands` prints it and a
/// consumer's config may use it, so the inventory test can hold both halves
/// of tmux's vocabulary against this crate's. The flags are carried for the
/// same reason and one step further: a table can only *claim* that `-d` is a
/// flag and takes nothing, and a claim nobody measures is where this crate
/// would quietly stop agreeing with tmux. Carrying them makes the claim
/// something `tests/inventory.rs` can put to the running tmux's own parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// The command's full name, such as `split-window`.
    pub name: &'static str,
    /// tmux's own abbreviation for it, such as `splitw`, when it has one.
    pub alias: Option<&'static str>,
    /// The flags its builder declares, in the order the family table spells
    /// them. A command whose builder is all positionals declares none.
    pub flags: &'static [Flag],
    /// The flags held ahead of the floor: letters a newer tmux takes and
    /// the targeted 3.7c refuses, kept in the family table as `ahead_*`
    /// rows — doc and all, one prefix away from being served — with no
    /// method generated, so no builder can spell an argv the floor answers
    /// with `unknown flag`. Carried as data for the same reason
    /// [`Entry::flags`] is: a shelf nobody measures is where this crate
    /// would quietly stop agreeing with tmux, so `tests/inventory.rs` holds
    /// each against the running parser — arity-checked where it is served,
    /// reported where it has not arrived, failed if the floor turns out to
    /// take it.
    pub ahead: &'static [Flag],
}

/// One flag a command declares: the letter tmux spells it with, and whether
/// tmux reads the word after it as its argument.
///
/// Two facts rather than the six method kinds, because these two are what a
/// parser can be asked about: `value`, `text` and `repeat` differ in what
/// Rust type the method takes and in how often it may be called, and tmux
/// cannot see any of that — it sees a letter, and whether a word follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Flag {
    /// The letter as tmux spells it, `-d` rather than `d`. A doubled letter
    /// (`-EE`, which `display-popup` reads as `-E` twice) is one word here
    /// for the same reason it is one word in argv.
    pub letter: &'static str,
    /// Whether tmux reads the following word as this flag's argument: false
    /// for a `switch`, true for a `value`, a `text` and a `repeat`.
    pub argument: bool,
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
    // The shelf: an `ahead_*` row keeps its method name, kind and doc in
    // the table and produces no method at all — the letter lives on in
    // [`Entry::ahead`] instead, so the knowledge is measured while the
    // argv it would build stays unbuildable against the floor.
    (ahead_switch, $method:ident, $flag:literal, $(#[$doc:meta])*) => {};
    (ahead_value, $method:ident, $flag:literal, $(#[$doc:meta])*) => {};
    (ahead_text, $method:ident, $flag:literal, $(#[$doc:meta])*) => {};
    (ahead_repeat, $method:ident, $flag:literal, $(#[$doc:meta])*) => {};
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
///
/// Any of the four flag kinds may be shelved by an `ahead_` prefix: the row
/// stays in the table — doc, method name, letter, arity — and produces no
/// method and no [`Entry::flags`] slot, landing in [`Entry::ahead`] instead.
/// That is the spelling for a flag the next tmux takes and the targeted
/// 3.7c refuses; serving it later is deleting the prefix.
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

                // The flags above, as data: the same table that spells the
                // methods answers `tests/inventory.rs`, so the two cannot
                // drift the way a hand-kept list beside them would.
                const DECLARED: [$crate::commands::Flag;
                    { 0 $(+ $crate::commands::flag_count!($kind $(, $flag)?))* }] =
                    $crate::commands::declared(&[
                        $($crate::commands::flag_slot!($kind $(, $flag)?),)*
                    ]);

                // The shelf beside them: the `ahead_*` rows, as data, so
                // the same test can measure what has not arrived yet.
                const AHEAD: [$crate::commands::Flag;
                    { 0 $(+ $crate::commands::ahead_count!($kind $(, $flag)?))* }] =
                    $crate::commands::declared(&[
                        $($crate::commands::ahead_slot!($kind $(, $flag)?),)*
                    ]);

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
            $($crate::commands::Entry {
                name: $name,
                alias: $alias,
                flags: &$type::DECLARED,
                ahead: &$type::AHEAD,
            },)*
        ];
    };
}

/// One method's contribution to its command's [`Entry::flags`]: the flag it
/// spells, or nothing at all when it spells none.
///
/// A sibling of [`method`] for the sibling reason — the kind has to stay a
/// token tree to be matched on, and this is the second thing worth matching
/// it for. `positional` and `trailing` produce `None`, which
/// [`declared`] then compacts away.
macro_rules! flag_slot {
    (switch, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: false,
        })
    };
    (value, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (text, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (repeat, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (positional) => {
        ::core::option::Option::None
    };
    (trailing) => {
        ::core::option::Option::None
    };
    (ahead_switch, $flag:literal) => {
        ::core::option::Option::None
    };
    (ahead_value, $flag:literal) => {
        ::core::option::Option::None
    };
    (ahead_text, $flag:literal) => {
        ::core::option::Option::None
    };
    (ahead_repeat, $flag:literal) => {
        ::core::option::Option::None
    };
}

/// How many flags one method contributes: one, or none.
///
/// Separate from [`flag_slot`] because an array's length is needed *before*
/// its elements, and a `macro_rules!` repetition can be summed in the length
/// position but not counted after the fact. The `ahead_*` arms must sit
/// before the catch-all, which would otherwise count a shelved letter as a
/// declared one.
macro_rules! flag_count {
    (positional) => {
        0
    };
    (trailing) => {
        0
    };
    (ahead_switch, $flag:literal) => {
        0
    };
    (ahead_value, $flag:literal) => {
        0
    };
    (ahead_text, $flag:literal) => {
        0
    };
    (ahead_repeat, $flag:literal) => {
        0
    };
    ($kind:tt, $flag:literal) => {
        1
    };
}

/// [`flag_slot`]'s mirror for [`Entry::ahead`]: the shelved rows produce
/// the flag, and everything served today produces `None`.
///
/// The arity survives the shelving — `ahead_switch` is a letter the newer
/// tmux takes bare, the other three one it wants a word after — because
/// that is the half of the row `tests/inventory.rs` can still hold against
/// a parser that serves it.
macro_rules! ahead_slot {
    (ahead_switch, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: false,
        })
    };
    (ahead_value, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (ahead_text, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (ahead_repeat, $flag:literal) => {
        ::core::option::Option::Some($crate::commands::Flag {
            letter: $flag,
            argument: true,
        })
    };
    (positional) => {
        ::core::option::Option::None
    };
    (trailing) => {
        ::core::option::Option::None
    };
    ($kind:tt, $flag:literal) => {
        ::core::option::Option::None
    };
}

/// [`flag_count`]'s mirror for [`Entry::ahead`]: one per shelved row.
macro_rules! ahead_count {
    (positional) => {
        0
    };
    (trailing) => {
        0
    };
    (ahead_switch, $flag:literal) => {
        1
    };
    (ahead_value, $flag:literal) => {
        1
    };
    (ahead_text, $flag:literal) => {
        1
    };
    (ahead_repeat, $flag:literal) => {
        1
    };
    ($kind:tt, $flag:literal) => {
        0
    };
}

/// The flags out of one command's method list, with the methods that spell
/// no flag dropped.
///
/// A `const fn` over `Option`s rather than a table that only holds flags,
/// because a `macro_rules!` repetition cannot skip an element: every method
/// line must produce one slot, so the ones that are not flags produce `None`
/// and this compacts them away before the binary is written.
const fn declared<const N: usize>(slots: &[Option<Flag>]) -> [Flag; N] {
    let mut flags = [Flag {
        letter: "",
        argument: false,
    }; N];
    let mut at = 0;
    let mut slot = 0;
    while slot < slots.len() {
        if let Some(flag) = slots[slot] {
            flags[at] = flag;
            at += 1;
        }
        slot += 1;
    }

    flags
}

pub(crate) use ahead_count;
pub(crate) use ahead_slot;
pub(crate) use flag_count;
pub(crate) use flag_slot;
pub(crate) use invocations;
pub(crate) use method;

/// The families, in the order [`REGISTRY`] lists them. Each later wave adds
/// one name here and nothing else.
const FAMILIES: &[&[Entry]] = &[
    panes::ENTRIES,
    sessions::ENTRIES,
    buffers_keys::ENTRIES,
    options_misc::ENTRIES,
];

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
        flags: &[],
        ahead: &[],
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

/// Every tmux command this crate knows of and has **not** given a builder,
/// each with the reason.
///
/// Empty since the last family landed — 92 of 92 installed commands are
/// typed. The table stays because it is the inventory test's second answer:
/// when a future tmux grows a command this crate has not met, the red test's
/// fix is either a builder or a row here carrying its reason, and the row is
/// the honest holding state while the builder is written.
pub const EXCLUDED: &[Excluded] = &[];

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
            REGISTRY
                .iter()
                .any(|entry| entry.name == SplitWindow::NAME && entry.alias == SplitWindow::ALIAS),
            "a builder tmux knows about but the register does not would be invisible to the \
             inventory test"
        );
    }

    /// Every command every family declares reaches [`REGISTRY`] whole.
    ///
    /// Deliberately not a length comparison: `REGISTRY`'s length *is*
    /// `total(FAMILIES)` by its own type, so holding one against the other
    /// asks the type system a question it has already answered. What is
    /// worth asking is whether the entries themselves survived the
    /// flattening — a family dropped, an entry overwritten, or a filler row
    /// left where a name should be.
    #[test]
    fn the_register_carries_every_family_entry_and_no_filler() {
        for family in FAMILIES {
            assert!(
                !family.is_empty(),
                "a family declaring nothing is a table that stopped expanding, and a register \
                 measured by length alone would never say so"
            );
            for entry in *family {
                assert!(
                    !entry.name.is_empty(),
                    "an unnamed entry is the flattening's own filler showing through, which means \
                     it copied fewer entries than it made room for"
                );
                assert!(
                    REGISTRY.iter().any(|listed| listed == entry),
                    "{} is declared by a family and missing from the register, so nothing would \
                     ever hold it against tmux",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn a_commands_flags_reach_the_register_with_their_arity() {
        let split = REGISTRY
            .iter()
            .find(|entry| entry.name == "split-window")
            .expect("split-window is typed");
        assert!(
            split.flags.contains(&Flag {
                letter: "-d",
                argument: false,
            }),
            "-d takes nothing, and a register claiming otherwise would send the inventory test \
             looking for an argument tmux does not want"
        );
        assert!(
            split.flags.contains(&Flag {
                letter: "-c",
                argument: true,
            }),
            "-c takes a working directory"
        );
        assert!(
            split
                .flags
                .iter()
                .all(|flag| flag.letter.starts_with('-') && flag.letter.len() >= 2),
            "a flag is carried the way tmux reads it in argv, leading dash and all"
        );
        assert_eq!(
            REGISTRY
                .iter()
                .find(|entry| entry.name == "kill-server")
                .map(|entry| entry.flags.len()),
            Some(0),
            "kill-server takes no flags, so its entry must declare none rather than inherit a \
             neighbour's"
        );
    }

    /// A letter double-claimed would pass the inventory test twice over —
    /// the served probe verifies it while the shelf probe reports it
    /// arrived — so the one place the mistake is visible is here, before
    /// any tmux is asked.
    #[test]
    fn a_letter_is_served_or_shelved_but_never_both() {
        for entry in REGISTRY {
            let mut seen = std::collections::BTreeSet::new();
            for flag in entry.flags.iter().chain(entry.ahead) {
                assert!(
                    seen.insert(flag.letter),
                    "{} claims {} more than once across its served rows and its shelf, and the \
                     probes would quietly confirm both claims",
                    entry.name,
                    flag.letter
                );
            }
            assert!(
                entry
                    .ahead
                    .iter()
                    .all(|flag| flag.letter.starts_with('-') && flag.letter.len() >= 2),
                "a shelved flag is carried the way tmux reads it in argv, leading dash and all, \
                 or the day it is unshelved the probes ask about a different word"
            );
        }
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

    /// The one-shot surface imports nothing from `control_mode` — the
    /// quoting quarantine, held by a machine instead of a reviewer.
    ///
    /// Doc comments may *name* the boundary (the accepted exception every
    /// wave carried), so comment lines are stripped before the assertion;
    /// what remains is code, and code mentioning `control_mode` in any of
    /// these six files is a crossing. `error.rs` is deliberately outside
    /// the list: shared vocabulary wraps *two* control-mode types by the
    /// architecture's own assignment — `CommandError` wraps a
    /// `control_mode::protocol::Response`, and `InvalidCommand` wraps a
    /// `control_mode::commandline::RenderError`.
    #[test]
    fn the_one_shot_surface_crosses_the_transport_boundary_only_in_prose() {
        // Built at run time so this test's own source — which the list below
        // includes — does not carry the token it hunts.
        let needle = ["control", "_mode"].concat();
        let surface: &[(&str, &str)] = &[
            ("server.rs", include_str!("../server.rs")),
            ("commands/mod.rs", include_str!("mod.rs")),
            ("commands/panes.rs", include_str!("panes.rs")),
            ("commands/sessions.rs", include_str!("sessions.rs")),
            ("commands/buffers_keys.rs", include_str!("buffers_keys.rs")),
            ("commands/options_misc.rs", include_str!("options_misc.rs")),
        ];
        for (name, source) in surface {
            for (index, line) in source.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(line);
                assert!(
                    !code.contains(&needle),
                    "{name}:{} reaches across the transport boundary: {line:?}",
                    index + 1
                );
            }
        }
    }
}

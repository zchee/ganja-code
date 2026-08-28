//! The teammate backends that can only run on the machine the lead is running
//! on: tmux panes, the `ganja` and `claude` panes split into them, and the
//! three foreign CLIs driven in their own native TUIs.
//!
//! # Why this is a crate (**D539**)
//!
//! [`ganja_core::teammate::TeammateBackend`] is a seam with more than one
//! adapter, which is what makes it a cut worth making. Everything here is one
//! of those adapters plus what only they need, and every one of them needs a
//! tmux server, a shell to split into, or another vendor's binary on `PATH` —
//! machine-bound facts an engine has no business holding. So the crate sits
//! **above** [`ganja_core`] the way a frontend does: it names the engine, and
//! the engine names nothing here. CI asserts both directions — the crate's
//! internal dependency list is a closed allowlist, and `ganja-core`'s is
//! unchanged by this crate's existence.
//!
//! The gate that is the point of the split is the inverted one: **`ganja-serve`
//! never links this crate.** The closure `ganja serve` ships is the closure a
//! cloud worker will ship, and a worker must provably be unable to spawn a pane
//! on somebody else's machine.
//!
//! # The `tmux` module is not the `tmux` crate
//!
//! [`tmux`] here is *this workspace's own* control-mode driver for the two pane
//! backends — a handful of `tmux` subprocess calls with the pane-identity rules
//! P25b needs. It is **not** the sealed-leaf `tmux` workspace member, which is
//! a full control-mode client that this crate does not consume and that
//! consumes nothing of ours (the P26 user directive, asserted in CI in both
//! directions).
//!
//! # What is here
//!
//! The five external adapters [`backends`] assembles, the tmux calls two of
//! them are built on, the shim core the three foreign CLIs share, the per-CLI
//! drivers, the records a shim child leaves behind, the reaper that sweeps
//! panes a dead lead left standing, and the transcript readers that carry a
//! pane teammate's answers home.

/// The `agy` backend: a name that parses and a spawn that refuses, because
/// W4's ship test measured `--sandbox` as terminal-only (**D508(a)**).
pub mod agy;
/// A teammate that is a real `claude` pane (P25b).
pub mod claude;
/// A teammate that is a headless `codex exec` child (**D508**, **D509**).
pub mod codex;
/// A teammate that is a headless `grok` child (**D508**, **D509**, **D510**).
pub mod grok;
/// A teammate in a `ganja` pane of its own (P25b).
pub mod pane;
/// Carrying a shim pane teammate's answers back to its lead (**D515**): the
/// per-CLI transcript readers, and the one clause that says what each carries.
pub mod readback;
/// Killing panes the lead left behind when it died (P25b).
pub mod reaper;
/// A teammate that is another vendor's CLI, driven through its own
/// non-interactive door (**D508**, **D509**).
pub mod shim;
/// The same three CLIs rendered in their own native TUI, in a pane of their
/// own, spoken to through bracketed paste (P28, **D512**).
pub mod shim_tui;
/// The tmux calls the two pane backends are built on (P25b).
pub mod tmux;

use std::sync::Arc;

use ganja_core::Backends;

use crate::agy::Agy;
use crate::claude::ClaudePane;
use crate::codex::Codex;
use crate::grok::Grok;
use crate::pane::{GanjaPane, PaneShare, PaneShell};
use crate::shim_tui::ShimTui;

/// The surfaces this build can spawn a teammate onto, except the engine's own
/// (**D538**).
///
/// Assembled outside the engine because a pane needs a tmux server and the
/// shell a spawn splits into, and a foreign CLI's TUI needs those plus a binary
/// on `PATH` — none of which an engine holds. `Engine::with_teammates` adds the
/// in-process implementation it *can* build, out of that session's own
/// provider, tool set and store.
///
/// **D512 (P28)**: all three shim slots open the CLI's own native TUI in a
/// pane, spoken to through bracketed paste, and **no spawn door in this build
/// reaches the headless [`shim::ShimBackend`]** any more — that machinery stays
/// in the tree, unit-tested, reachable only by the tests that drive it against
/// a fake CLI. Which is also why `teammates.shim_turn_timeout` is not read
/// here: a pane-mode shim has no per-turn deadline (the module doc owns why),
/// and the key governs only the headless machinery it was written for
/// (**D509**).
///
/// These slots search the real `PATH`; a test that reached one would spawn the
/// developer's own CLI. Tests assemble their backends through `ganja_testkit`,
/// never through this.
pub fn backends(shell: PaneShell, share: PaneShare) -> Backends {
    Backends::new()
        .with(Arc::new(GanjaPane::new(shell.clone(), share)))
        .with(Arc::new(ClaudePane::new(shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Codex::new()), shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Agy::new()), shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Grok::new()), shell, share)))
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

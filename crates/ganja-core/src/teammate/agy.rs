//! The teammate that **is not built**, and the measurement that decided it
//! (**D508(a)**, W4's ship test).
//!
//! Spec: none. Upstream opencode has no teammates and Claude Code does not run
//! another vendor's agent as one, so every sentence here is ganja's own, and
//! the vendor surface it is written against is the binary itself — `agy 1.1.15`
//! (the Antigravity CLI), probed on this machine rather than read out of a
//! checkout that may not match what a person has installed.
//!
//! This module is deliberately not a [`crate::teammate::shim::Driver`]. The
//! other two CLIs contribute words to a shared runner; agy contributes a
//! refusal, because its floor was measured and there is nothing under it.
//!
//! # The ship test, and why it was one probe rather than a wording exercise
//!
//! D508(a) pins `--sandbox` **and** `--mode plan` as agy's floor. But
//! `--sandbox`'s own help claims *"Run in a sandbox with terminal restrictions
//! enabled"*, which is a claim about a terminal and not about a filesystem —
//! and a mode is the vendor agent's own self-restraint, not a bound under it.
//! So the plan made the measurement decide whether agy ships at all rather
//! than only what its column says: if `--sandbox` is not a filesystem bound,
//! an agy teammate would be a foreign agent holding its own tools with no
//! enforced filesystem bound, one consent at spawn and no permission channel —
//! which contradicts the ADR's *"scoped in v1 to read-only foreign agents"* in
//! terms.
//!
//! # What was measured
//!
//! The full recording is `tests/fixtures/agy-posture-probe.txt`, which is what
//! [`REFUSED_NO_FILESYSTEM_BOUND`](crate::teammate::agy::REFUSED_NO_FILESYSTEM_BOUND) is
//! asserted against rather than against a
//! second copy of itself. In short, over one byte-identical prompt run under
//! four flag sets:
//!
//! - **The terminal is bounded, and genuinely.** Under `--sandbox` a shell
//!   write and a shell read one directory outside the cwd both failed with
//!   `Operation not permitted`, where the unsandboxed control performed both,
//!   and the shell's network came back intercepted by agy's own proxy. That is
//!   the responsiveness control doing its job: the instrument is not refusing
//!   everything.
//! - **The filesystem is not.** In the same runs, agy's own `write_to_file`
//!   tool created a file at an absolute path in the very directory the shell
//!   had just been refused — in **2 of 2** runs of that flag set. It is not
//!   the shell under another name, because the shell was denied that exact
//!   directory in the same turn.
//!
//! A probe that had exercised only the shell would have measured a real
//! denial and shipped a false posture. That is the trap this file exists to
//! record.
//!
//! Two rows refused the write instead, and they are not a bound either: the
//! refusal came from agy's own argument validator ("is not a valid artifact
//! path"), before execution, and the two runs that wrote are what that
//! validator is worth. Self-restraint that does not always hold is not a
//! floor — and this one cannot be credited to the composed flags in any case,
//! since the control row, which composes neither of them, met the same
//! validator.
//!
//! # The verdict
//!
//! `--sandbox` is terminal-only, so under D508(a)'s ship test and Principle 6
//! clause 3 **agy does not ship in v1**. [`MemberBackend::Agy`] stays in the
//! enum and stays parseable — D501's grammar is "named and refused", and a
//! name that refuses with a reason tells a person more than a missing name
//! does — but [`Agy`](crate::teammate::agy::Agy)'s `spawn` refuses every one, no
//! child is ever created,
//! and [`crate::teammate::posture_line`] answers [`None`] for it, because a
//! posture row is a description of a running teammate and there is none.
//!
//! `--mode plan` cannot rescue it, and that was settled in advance rather than
//! after the fact: the ship test is about the sandbox, and a mode is the
//! vendor agent's self-restraint. It was measured anyway (the third row of the
//! ladder) and the answer changed nothing.
//!
//! # What a later wave must not compose
//!
//! Recorded as prose rather than as a constant, because nothing here composes
//! a command line and a list no code reads is a list that rots. From agy's own
//! `--help`: `--dangerously-skip-permissions`, `--mode accept-edits`,
//! `--add-dir`, and `--continue` **in both spellings** — the short alias is
//! `-c`, which a literal `--continue` grep would miss — since it resumes the
//! machine's most recent conversation, which may be another teammate's or the
//! person's own. `--conversation <id>` is the resume door that names what it
//! resumes.
//!
//! Two findings that would have cost a later wave a debugging session:
//!
//! - **`-p`/`--print` takes the prompt as its flag value** (agy parses with
//!   Go's `flag` package), so the plan's `agy -p --input-format stream-json …`
//!   would parse `--input-format` *as the prompt*. Any launch line must put
//!   `-p` last. This was found by a control run that answered a question about
//!   `--print-timeout` instead of the prompt it was given.
//! - **agy runs shell commands with cwd = its own scratch directory**, not the
//!   directory it was launched from, so `SpawnSpec.cwd` would not have been the
//!   child's shell cwd.
//!
//! # No auth pre-check
//!
//! There is none to write, and no `ready` beyond the trait's default: agy
//! offers no `login status` equivalent, so a first-turn failure would have been
//! the auth surface. Moot while `spawn` refuses, and recorded because the
//! wave that builds this backend will look here for it.

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;

use crate::teammate::{Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported, backend_name};

/// Why a spawn on this CLI is refused, measured (**AC-28**'s no-ship arm).
///
/// Asserted equal to the `refusal:` line of
/// `tests/fixtures/agy-posture-probe.txt`, so the sentence a person reads is
/// checked against the measurement rather than against a second literal.
///
/// It ends where [`crate::teammate::shim::REFUSED_BYPASS`] ends, and on
/// purpose: both are the same answer to the same question — this build grants
/// read and not write, and the door that would carry more is the permission
/// channel. Naming the follow-up rather than the wave, because which version
/// carries it is the plan's to say.
pub const REFUSED_NO_FILESYSTEM_BOUND: &str = "--sandbox bounds the terminal only: under it a shell read or write outside agy's own \
     scratch directory is denied, while agy's own write_to_file tool wrote to an absolute path \
     outside the working directory in 2 of 2 runs of that flag set — so an agy teammate has no \
     enforced filesystem bound; v1 grants read and not write, and what agy would need is recorded in D508(b) and \
     lands with the permission channel that can carry it";

/// The `agy` backend: a name that parses and a spawn that refuses.
///
/// A unit struct for [`crate::teammate::codex::Codex`]'s reason — a backend
/// holds no per-member state, so one value serves every member — and here the
/// point is sharper still, since it holds nothing at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct Agy;

impl Agy {
    /// The backend.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TeammateBackend for Agy {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Agy
    }

    async fn spawn(&self, _spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        // Unconditional, and before any look at the spec: a `bypass` spawn is
        // refused by this same sentence rather than by the shim's bypass one,
        // which is the honest order. Whoever asked cannot have the surface at
        // all, so telling them their *flag* was the problem would send them to
        // fix the wrong thing.
        Err(Unsupported {
            backend: MemberBackend::Agy,
            reason: REFUSED_NO_FILESYSTEM_BOUND.to_owned(),
        })
    }

    async fn kill(&self, handle: &Handle) {
        // Nothing this backend made can be here to end: its `spawn` has never
        // returned a handle. Named rather than ignored, because a handle
        // arriving here would mean a registry had crossed two backends.
        tracing::warn!(
            ?handle,
            backend = backend_name(MemberBackend::Agy),
            "a backend that never spawns was asked to end something it did not start"
        );
    }

    fn delivery(&self) -> Delivery {
        // The answer the shim would have given, kept so the lead's queue strip
        // behaves identically either side of a wave that changes nothing about
        // delivery — and so **AC-2**'s table has one rule for all three CLIs
        // rather than an exception whose reason is "this one refuses".
        Delivery::Acknowledged
    }
}

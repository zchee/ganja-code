//! The name a tool is advertised to a vendor under, when its own name is one
//! the vendor's pattern refuses.
//!
//! Every OpenAI-family endpoint documents a tool name as
//! `^[a-zA-Z0-9_-]{1,64}$`, and Anthropic Messages the same alphabet at
//! `{1,128}`. Ganja's registry names are not bound by either: an MCP tool is
//! `mcp__<server>__<tool>`, and a server contributed by a plugin arrives
//! namespaced `plugin:<name>:<server>` (**D473**), which puts colons in the
//! name and pushes it past 64 characters. That combination is what killed a
//! live turn: `meta/muse-spark-1.2` over openrouter refused
//! `mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result` with
//! ``\`name\` must be at most 64 characters, got 69``.
//!
//! Renaming the tool is not the fix. The registry name is what permissions,
//! hooks, transcripts and the TUI all key on, so a vendor's pattern is answered
//! where the vendor is spoken to: each wire advertises an [`alias`] and
//! translates an incoming call's name back through the [`Aliases`] map it built
//! for that same request. Aliasing is a pure function of the original name, so
//! a history item re-encoded on a later request carries the same alias the
//! model originally saw without anything having to be remembered between turns.
//!
//! # Upstream
//!
//! opencode v1.18.22 has **no length transform anywhere** — nothing in its tree
//! caps a tool name at 64 or 128. It does carry this module's alphabet twice,
//! and both are cited because the rule here is theirs:
//!
//! - `packages/opencode/src/mcp/catalog.ts:117-119` —
//!   `sanitize = (value) => value.replace(/[^a-zA-Z0-9_-]/g, "_")`, applied at
//!   *registration* (`toolName(client, name) = sanitize(client) + "_" +
//!   sanitize(name)`), so upstream's registry name is already conforming and
//!   its plugin-shaped colon can never exist — it has no plugin system.
//! - `packages/opencode/src/provider/transform.ts:224` — the same regex as a
//!   wire-boundary `scrub`, but over a tool *call id* rather than a tool name,
//!   and only for claude models.
//!
//! So the alphabet is ported; the cap, the reverse map and the
//! hash-disambiguated truncation are ganja's own correctness patch for a name
//! shape upstream cannot produce.
//!
//! # The one collision this cannot resolve
//!
//! Two originals longer than the cap can never alias equal: the truncation
//! carries `HASH` hex digits of the original's SHA-256. What is *not* ruled
//! out is a roster holding both `a_b` and `a:b`, where the second scrubs onto
//! the first's own name. Upstream's `sanitize` has exactly that property, and
//! resolving it would mean renaming one of two tools a person deliberately
//! installed. It is left as the documented bound rather than papered over.

use std::{borrow::Cow, collections::HashMap};

use sha2::{Digest as _, Sha256};

use crate::tool::ToolDefinition;

/// What the OpenAI family accepts: chat completions and the Responses API both.
pub const OPENAI_CAP: usize = 64;

/// What Anthropic Messages accepts.
pub const ANTHROPIC_CAP: usize = 128;

/// Hex digits of the original's digest a truncated alias ends with.
///
/// Eight is what makes a truncation a *disambiguation*: two originals sharing a
/// prefix long enough to survive the cut still differ here, so the alias a wire
/// advertises stays one-to-one with the tool the engine will be asked to run.
const HASH: usize = 8;

/// Whether `byte` is one the vendors' shared pattern allows.
const fn conforming(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// The name `name` is advertised under to an endpoint capping names at `cap`.
///
/// Deterministic, and byte-identical to `name` whenever `name` already
/// conforms — which is every tool this build ships and every MCP tool whose
/// server was configured rather than contributed. The common case allocates
/// nothing, which is why this returns a [`Cow`] rather than a [`String`].
///
/// A name that does not conform has every character outside `[A-Za-z0-9_-]`
/// replaced by `_` — one underscore per character, so the result is ASCII and
/// can be cut on any byte — and, if that is still longer than `cap`, is cut to
/// leave room for `_` and `HASH` hex digits of the **original** name's
/// SHA-256. The result is then exactly `cap` bytes.
///
/// # Panics
///
/// Debug-only: `cap` must leave room for the suffix a truncation appends.
#[must_use]
pub fn alias(name: &str, cap: usize) -> Cow<'_, str> {
    debug_assert!(
        cap > HASH + 1,
        "a cap with no room for the disambiguating suffix"
    );

    if name.len() <= cap && name.bytes().all(conforming) {
        return Cow::Borrowed(name);
    }

    let mut out: String = name
        .chars()
        .map(|c| {
            if u8::try_from(c).is_ok_and(conforming) {
                c
            } else {
                '_'
            }
        })
        .collect();

    if out.len() > cap {
        out.truncate(cap - HASH - 1);
        out.push('_');
        // Lowercase hex of the leading bytes, which is what `{:02x}` over the
        // digest's own byte order gives — stable across builds and platforms.
        for byte in &Sha256::digest(name.as_bytes())[..HASH / 2] {
            out.push_str(&format!("{byte:02x}"));
        }
    }

    Cow::Owned(out)
}

/// What one request's aliases map back to.
///
/// Holds only the tools whose names actually changed, so a roster that already
/// conforms — the ordinary one — builds an empty map and every lookup misses.
/// A miss passes the name through unchanged, which is also the right answer for
/// a model that invented a tool: the engine already reports an unknown tool as
/// error text the model reads next, and it must report the name the model
/// actually said.
#[derive(Clone, Debug, Default)]
pub struct Aliases(HashMap<String, String>);

impl Aliases {
    /// The map for a request advertising `tools` to an endpoint capping names
    /// at `cap`.
    #[must_use]
    pub fn of(tools: &[ToolDefinition], cap: usize) -> Self {
        Self(
            tools
                .iter()
                .filter_map(|tool| match alias(&tool.name, cap) {
                    // Borrowed is the pass-through: nothing to map back.
                    Cow::Borrowed(_) => None,
                    Cow::Owned(aliased) => Some((aliased, tool.name.clone())),
                })
                .collect(),
        )
    }

    /// The registry name behind `name`, or `name` itself when it is not one of
    /// this request's aliases.
    #[must_use]
    pub fn original(&self, name: String) -> String {
        self.0.get(&name).map_or(name, Clone::clone)
    }
}

#[cfg(test)]
#[path = "toolname_tests.rs"]
mod tests;

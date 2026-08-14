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
//! opencode v1.18.13 has **no length transform anywhere** — nothing in its tree
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
mod tests {
    use super::*;

    /// The exact name that killed a live turn, colons and all.
    const FIELD: &str = "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: "for the roster".to_owned(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn a_name_the_vendors_already_accept_is_advertised_byte_identically() {
        for name in ["read", "todowrite", "mcp__docs__search", "plan-exit_1"] {
            assert!(
                matches!(alias(name, OPENAI_CAP), Cow::Borrowed(kept) if kept == name),
                "{name} should not have been rewritten"
            );
        }
    }

    #[test]
    fn the_field_name_that_was_refused_becomes_one_the_pattern_accepts() {
        assert_eq!(FIELD.chars().count(), 69, "the reported length");

        let aliased = alias(FIELD, OPENAI_CAP);

        assert_eq!(aliased.len(), OPENAI_CAP);
        assert!(
            aliased.bytes().all(conforming),
            "{aliased} is outside [A-Za-z0-9_-]"
        );
        // Pinned rather than merely described: determinism is what lets an
        // encoder and a decoder recompute the same string, and what lets a
        // transcript replayed on a later turn name what that turn advertises.
        // A change to this literal is a change to both, never one of them.
        assert_eq!(
            aliased,
            "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_r_6bb398bf"
        );
    }

    /// Anthropic's own cap is wide enough that the same name only loses its
    /// colons — a truncation there would be a rewrite nobody's API asked for.
    #[test]
    fn the_same_name_only_loses_its_colons_under_the_wider_cap() {
        assert_eq!(
            alias(FIELD, ANTHROPIC_CAP),
            "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_research_result"
        );
    }

    #[test]
    fn aliasing_the_same_name_twice_gives_the_same_string() {
        assert_eq!(alias(FIELD, OPENAI_CAP), alias(FIELD, OPENAI_CAP));
    }

    /// The whole reason the truncation carries a digest: a shared prefix long
    /// enough to survive the cut must not become a shared alias.
    #[test]
    fn two_long_names_sharing_a_prefix_never_alias_equal() {
        let stem = "mcp__plugin:some-very-long-marketplace-name:some-server__";
        let (first, second) = (
            format!("{stem}first_tool_here"),
            format!("{stem}second_tool"),
        );
        let one = alias(&first, OPENAI_CAP);
        let two = alias(&second, OPENAI_CAP);

        assert_eq!(one.len(), OPENAI_CAP);
        assert_eq!(two.len(), OPENAI_CAP);
        assert_ne!(one, two);
        assert_eq!(one[..OPENAI_CAP - HASH], two[..OPENAI_CAP - HASH]);
    }

    /// A name outside ASCII scrubs one underscore per *character*, so the
    /// result stays cuttable on any byte.
    #[test]
    fn a_name_outside_ascii_scrubs_to_one_underscore_per_character() {
        assert_eq!(alias("読み込み_tool", OPENAI_CAP), "_____tool");
    }

    #[test]
    fn a_conforming_roster_builds_no_map_at_all() {
        let aliases = Aliases::of(&[definition("read"), definition("write")], OPENAI_CAP);

        assert_eq!(aliases.original("read".to_owned()), "read");
        assert!(aliases.0.is_empty());
    }

    #[test]
    fn an_aliased_call_comes_back_out_under_the_name_the_engine_registered() {
        let aliases = Aliases::of(&[definition(FIELD), definition("read")], OPENAI_CAP);
        let aliased = alias(FIELD, OPENAI_CAP).into_owned();

        assert_eq!(aliases.original(aliased), FIELD);
    }

    /// A model that invented a tool must be reported under the name it said,
    /// because that is what the engine's unknown-tool error text has to name.
    #[test]
    fn a_name_the_map_never_saw_passes_through_unchanged() {
        let aliases = Aliases::of(&[definition(FIELD)], OPENAI_CAP);

        assert_eq!(
            aliases.original("invented_tool".to_owned()),
            "invented_tool"
        );
    }
}

//! What the two Responses ids accept under `[provider.<id>.options]`, and the
//! value a resolved turn hands this wire (**D563**).
//!
//! Spec: `.omc/research/2026-09-16-chatgpt-seat-param-probe.md` — 120 live
//! calls against the ChatGPT seat on `gpt-5.5`, `gpt-5.6-sol` and
//! `gpt-6-astra`, whose vocabulary every list here is written in: **honored**
//! (the echo moved), **recognized** (200, the echo shows the default or
//! nothing), **rejected** (400, with the backend's own sentence). The platform
//! id's list is the vendor's SDK surface and is **unprobed**; the live test
//! that measures it is this plan's W6, and a refusal there moves a name from
//! one list to the other by one edit.
//!
//! # Why the lists live in this crate and the decode struct lives in core
//!
//! `ganja-core`'s loader is the only place that can refuse a key *before* a
//! request is spent, and the seat answers an unknown key with a 400 — so the
//! refusal has to happen at load. But which keys a backend takes is a fact
//! about the **wire**, measured against the wire, and a list of them in the
//! config module would be a fact about a vendor kept where nothing else about
//! that vendor lives. So the vocabulary — every string list below, plus
//! [`FAST_TIERS`] and [`RequestOptions`] — is this module's, and the decode
//! struct, the loader gate that reads these lists and the per-turn resolver are
//! core's. The two agree by one set-equality test per list.
//!
//! Nothing here is ported from upstream opencode: it ships no per-provider
//! option surface at all, and since 2026-09-08 its files are not this
//! repository's specification either. The probe report is.

use serde_json::{Map, Value};

use super::{CHATGPT_ID, ID};

/// Keys the **ChatGPT seat** takes, in the dotted spelling
/// `ResponsesOptions::set_keys` reports.
///
/// Every one was sent live and came back 200. What is missing is missing
/// because the seat answered 400, and the three whose refusal was not a bare
/// `Unsupported parameter` — `reasoning.mode`, `prompt_cache_key`,
/// `reasoning.summary` — get refusal sentences of their own in the loader, so
/// that somebody reading one learns what the backend actually did rather than
/// a generic "not here".
pub const SEAT_ACCEPTED: &[&str] = &[
    "service_tier",
    "reasoning.context",
    "text.verbosity",
    "parallel_tool_calls",
    "stream_options.include_obfuscation",
    "tool_choice",
    "custom_tools",
    "server_tools",
    "include",
    "context_management",
    "client_metadata",
    "access_programs",
];

/// Keys the **platform** takes: [`SEAT_ACCEPTED`] plus everything the seat
/// rejected and `api.openai.com` documents.
///
/// Spelled out in full rather than concatenated, because a `const` cannot
/// concatenate slices — and because a reader of either list should be able to
/// see the whole answer without holding the other one in their head. That the
/// seat's list is a subset is a test, not a comment.
///
/// **Unprobed.** No call in the 2026-09-16 probe went to `api.openai.com`; the
/// names below are the vendor's `response_create_params` surface, which is why
/// nothing here is sent by default and why the platform gets no spending
/// default at all.
pub const PLATFORM_ACCEPTED: &[&str] = &[
    "service_tier",
    "reasoning.context",
    "reasoning.summary",
    "reasoning.mode",
    "text.verbosity",
    "parallel_tool_calls",
    "stream_options.include_obfuscation",
    "tool_choice",
    "custom_tools",
    "server_tools",
    "include",
    "context_management",
    "client_metadata",
    "access_programs",
    "max_output_tokens",
    "max_tool_calls",
    "prompt_cache_key",
    "prompt_cache_retention",
    "prompt_cache_options",
    "temperature",
    "top_p",
    "top_logprobs",
    "truncation",
    "safety_identifier",
    "user",
    "metadata",
    "moderation",
];

/// `service_tier` values the seat took (probe rows 1–7).
///
/// `fast`, `flex`, `auto` and `scale` all came back
/// `Unsupported service_tier: <v>`; `fast` is nonetheless a *config* spelling
/// of `priority`, rewritten at load and never sent, because it is the word
/// somebody reaches for and a config that means something is better than a
/// refusal that is technically right.
///
/// All three were **recognized** rather than honored: the completed frame
/// echoed `default` for every one of them, and the single `ultrafast` sample
/// was slower than its baseline. That is the measurement, and it is why the
/// status bar draws what was *asked* and `/usage` draws what was served.
pub const SEAT_TIERS: &[&str] = &["default", "priority", "ultrafast"];

/// `service_tier` values the platform documents. Unprobed, like every other
/// platform list here.
pub const PLATFORM_TIERS: &[&str] = &["auto", "default", "flex", "scale", "priority", "ultrafast"];

/// Hosted tool types the seat registered (probe rows 32 and 35).
pub const SEAT_SERVER_TOOLS: &[&str] = &["web_search", "image_generation"];

/// Hosted tool types the platform documents.
pub const PLATFORM_SERVER_TOOLS: &[&str] =
    &["web_search", "image_generation", "file_search", "code_interpreter", "mcp"];

/// Hosted tool types refused on **both** ids, for a reason that is neither
/// id's: each of them runs on the client, and a hosted advertisement of one
/// promises the vendor an executor this build does not offer through that
/// door. A session gets these as ordinary ganja tools instead.
pub const CLIENT_SIDE_SERVER_TOOLS: &[&str] = &["shell", "apply_patch", "computer"];

/// `include` values the seat took (probe row 37: recognized, no echo).
///
/// `reasoning.encrypted_content` is deliberately **absent**: it is the wire's
/// own entry, added whenever the model seals its reasoning, and a config that
/// spells it is deduped rather than refused.
pub const SEAT_INCLUDE: &[&str] = &["web_search_call.action.sources"];

/// `include` values the platform documents — the seat's, plus what a
/// configured `top_logprobs` comes back through.
pub const PLATFORM_INCLUDE: &[&str] =
    &["web_search_call.action.sources", "message.output_text.logprobs"];

/// Builtin tools that can be advertised as a Responses **custom tool**: those
/// whose argument schema has exactly one `required` property and it is a
/// string.
///
/// That predicate is the whole rule, and it is what makes the list derivable
/// rather than chosen — a custom advertisement carries one free-text `input`
/// and nothing else, so a tool needing two required arguments has no way to
/// receive the second, and a tool whose one required argument is an array has
/// no way to receive it as a sentence. `options_tests.rs` derives this list
/// from `Registry::with_builtins()` and fails if the two disagree, in both
/// directions.
///
/// **Optional arguments are unreachable through the custom advertisement** —
/// `read`'s `offset`/`limit`, for instance. That is why the function
/// advertisement stays beside the custom one rather than being replaced by it.
pub const CUSTOM_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "bash",
    "webfetch",
    "websearch",
    "skill",
    "bash_output",
    "kill_shell",
];

/// The `service_tier` a model gets when a session asks for *fast* and names no
/// value — for the models where that is not [`DEFAULT_FAST_TIER`].
///
/// A slice of pairs rather than a `match`, so that adding a row changes no
/// type and no signature. Read by the **engine**, which owns the whole
/// `service_tier` ladder, and never by the wire: what
/// `Engine::service_tier()` reports has to be what was sent, and that is only
/// true while one side resolves it.
pub const FAST_TIERS: &[(&str, &str)] = &[("gpt-5.6-sol", "ultrafast")];

/// What [`fast_tier`] answers for a model [`FAST_TIERS`] does not name.
pub const DEFAULT_FAST_TIER: &str = "priority";

/// The fast tier for `model`.
#[must_use]
pub fn fast_tier(model: &str) -> &'static str {
    FAST_TIERS
        .iter()
        .find(|(named, _)| *named == model)
        .map_or(DEFAULT_FAST_TIER, |(_, tier)| *tier)
}

/// Whether `id` is one of the two ids this module describes.
#[must_use]
pub fn speaks_options(id: &str) -> bool {
    id == CHATGPT_ID || id == ID
}

/// The keys `id` accepts, or [`None`] for an id that reads no options at all.
///
/// [`None`] rather than an empty slice because the two answers mean different
/// things to a loader: "this id takes none of these" is a refusal naming the
/// id, and "this id takes none of *those*" is a refusal naming the key.
#[must_use]
pub fn accepted(id: &str) -> Option<&'static [&'static str]> {
    per_id(id, SEAT_ACCEPTED, PLATFORM_ACCEPTED)
}

/// The `service_tier` values `id` accepts.
#[must_use]
pub fn tiers(id: &str) -> Option<&'static [&'static str]> {
    per_id(id, SEAT_TIERS, PLATFORM_TIERS)
}

/// The hosted tool types `id` accepts.
#[must_use]
pub fn server_tools(id: &str) -> Option<&'static [&'static str]> {
    per_id(id, SEAT_SERVER_TOOLS, PLATFORM_SERVER_TOOLS)
}

/// The `include` values `id` accepts.
#[must_use]
pub fn include(id: &str) -> Option<&'static [&'static str]> {
    per_id(id, SEAT_INCLUDE, PLATFORM_INCLUDE)
}

/// The one place the seat/platform branch is spelled, so that a list added
/// later cannot pick the wrong side of it by a typo.
fn per_id(
    id: &str,
    seat: &'static [&'static str],
    platform: &'static [&'static str],
) -> Option<&'static [&'static str]> {
    if id == CHATGPT_ID {
        Some(seat)
    } else if id == ID {
        Some(platform)
    } else {
        None
    }
}

/// Everything a resolved turn tells this wire, beyond the request itself.
///
/// The type is **provider-owned on purpose**. The engine resolves the whole
/// ladder — config provider-wide, config per-model, the `/fast` intent,
/// `run --json-schema` — and hands the answer across as this, so that no
/// `ganja-core` config type ever appears in this crate and no loose key ever
/// crosses either. The split between the named fields and
/// [`body`](Self::body) is what enforces the second half: a *directive* is
/// something this wire acts on (it advertises a tool, it writes a `text`
/// object, it adds an `include` entry), and only `body` is spliced into the
/// request, so a directive can never arrive at the backend as an unknown
/// top-level key.
///
/// Every wire but this one ignores it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestOptions {
    /// The resolved `service_tier` literal, or [`None`] to send no key.
    ///
    /// Already resolved: the wire never injects a tier and never consults
    /// [`FAST_TIERS`], because the one key a status bar and `/usage` report
    /// has to be reported by whoever decided it.
    pub service_tier: Option<String>,
    /// The `text.format` document `ganja run --json-schema` rides, written
    /// into the request's `text` object above whatever a configured
    /// `verbosity` put there.
    pub text_format: Option<Value>,
    /// Registry names to advertise as custom tools beside their function
    /// entries — see [`CUSTOM_TOOLS`] for which names can be.
    pub custom_tools: Vec<String>,
    /// Hosted tool entries, sent verbatim after the function roster. Already
    /// serialized, `type` first, because what the config carried was TOML and
    /// this crate does not read TOML.
    pub server_tools: Vec<Map<String, Value>>,
    /// Configured `include` entries, unioned with the wire's own and the
    /// effort's rather than replacing them.
    pub include: Vec<String>,
    /// `reasoning.summary`, applied on the platform alone and after the
    /// layers, because it is the one key where the catalog effort carries a
    /// *default* rather than a selection and a configured value has to be
    /// able to outrank it.
    pub reasoning_summary: Option<String>,
    /// The configured keys in the wire's own spelling, spliced as one layer.
    pub body: Map<String, Value>,
}

#[cfg(test)]
#[path = "options_tests.rs"]
mod tests;

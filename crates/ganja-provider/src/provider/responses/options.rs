//! What the two Responses ids accept under `[provider.<id>.options]`, and the
//! value a resolved turn hands this wire (**D563**).
//!
//! Spec: `.omc/research/2026-09-16-chatgpt-seat-param-probe.md` — 120 live
//! calls against the ChatGPT seat on `gpt-5.5`, `gpt-5.6-sol` and
//! `gpt-6-astra`, whose vocabulary every list here is written in: **honored**
//! (the echo moved), **recognized** (200, the echo shows the default or
//! nothing), **rejected** (400, with the backend's own sentence). The platform
//! id's lists started as the vendor's SDK surface and were measured by W6:
//! `.omc/research/2026-09-16-openai-platform-param-probe.md`, run 2026-09-17 on
//! `gpt-5.5` and `gpt-5.6-sol`, which moved three names — `scale` and
//! `ultrafast` out of [`PLATFORM_TIERS`], `access_programs` out of
//! [`PLATFORM_ACCEPTED`] — and one more on the seat: `context_management` out
//! of [`SEAT_ACCEPTED`], measured to do nothing there.
//!
//! # Rejections that moved nothing, on purpose
//!
//! Five platform keys were refused by that run and are **kept** in
//! [`PLATFORM_ACCEPTED`], because each refusal names the model rather than the
//! key, and reaching a model the seat does not serve is what the `openai` id is
//! for. The loader stays silent about them by choice; the sentences, verbatim
//! from `gpt-5.5`:
//!
//! - `reasoning.mode = "pro"`: `` `reasoning.mode` is not supported with this
//!   model. ``
//! - `prompt_cache_options`: `prompt_cache_options is not supported on this
//!   model` (the SDK documents it for `gpt-5.6` and later).
//! - `temperature`: `Unsupported parameter: 'temperature' is not supported with
//!   this model.`
//! - `top_p`: `Unsupported parameter: 'top_p' is not supported with this model.`
//! - `top_logprobs`, and `include = ["message.output_text.logprobs"]` it comes
//!   back through: `logprobs are not supported with reasoning models.`
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
/// a generic "not here". `context_management` is missing for the opposite
/// reason and gets a sentence too: the seat takes it and, on a 56k-token
/// transcript under a 20k threshold, neither compacted nor echoed anything
/// (probe 2026-09-17), so sending it would spend bytes to say nothing.
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
    "client_metadata",
    "access_programs",
];

/// Keys the **platform** takes: what `api.openai.com` documents and the
/// 2026-09-17 probe did not refuse by name.
///
/// Spelled out in full rather than concatenated, because a `const` cannot
/// concatenate slices — and because a reader of either list should be able to
/// see the whole answer without holding the other one in their head. The
/// overlap with [`SEAT_ACCEPTED`] is a test, not a comment, and it is no longer
/// a plain subset in either direction: `access_programs` is the seat's alone,
/// refused here with `The access_programs parameter is not enabled for this
/// organization.`, and `context_management` is the platform's alone.
///
/// **Measured, not defaulted.** Every name below was sent to the platform on
/// 2026-09-17; the module doc lists the five kept despite a model-dependent
/// refusal. Nothing here is sent unless configured, and the platform still gets
/// no spending default at all.
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

/// `service_tier` values the platform took (probe 2026-09-17).
///
/// `auto` was recognized (it echoed the project's own tier) and the other three
/// honored. `scale` and `ultrafast` are **absent** because they were refused:
/// `scale` with `Invalid value: 'scale'. Supported values are: 'auto',
/// 'default', 'fast', 'flex', and 'priority'.`, and `ultrafast` with a 500,
/// `Invalid service_tier argument`, on `gpt-5.5` and twice on `gpt-5.6-sol`.
/// That second refusal is why `/fast on` over `openai` resolves to `priority`
/// on every model.
pub const PLATFORM_TIERS: &[&str] = &["auto", "default", "flex", "priority"];

/// Hosted tool types the seat registered (probe rows 32 and 35).
pub const SEAT_SERVER_TOOLS: &[&str] = &["web_search", "image_generation"];

/// Hosted tool types the platform took (probe 2026-09-17; `file_search`'s one
/// refusal was about a missing store, not the type).
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

impl RequestOptions {
    /// What a session's **compaction** request carries of a resolved turn:
    /// the tier and the configured body, and none of the directives.
    ///
    /// A summary is the same model answering in the same session, so it is
    /// billed and scheduled the way the steps are and reads the same `text`
    /// and `reasoning` settings. What it does not share is anything about the
    /// conversation's own shape: it offers no tools (so neither a custom
    /// advertisement nor a hosted tool has anywhere to go), a `run
    /// --json-schema` document describes the answer to the person's prompt
    /// rather than a summary of it, and an `include` asks for fields of a
    /// tool-bearing response. The roster keys left in `body` are this wire's
    /// own to drop, because only the wire sees that the request offers nothing.
    #[must_use]
    pub fn summary_view(&self) -> Self {
        Self { service_tier: self.service_tier.clone(), body: self.body.clone(), ..Self::default() }
    }

    /// What a **subagent's** requests carry, from a value resolved for the
    /// child's own model: the same two halves [`summary_view`](Self::summary_view)
    /// keeps.
    ///
    /// The same shape for a different reason. A child is offered its own
    /// roster by its own agent, so a custom or hosted advertisement configured
    /// against the session's tools is not a statement about the child's; a
    /// JSON schema belongs to the headless run's own answer; and a child's
    /// transcript is not the one an `include` was configured to enrich.
    #[must_use]
    pub fn child_view(&self) -> Self {
        self.summary_view()
    }
}

#[cfg(test)]
#[path = "options_tests.rs"]
mod tests;

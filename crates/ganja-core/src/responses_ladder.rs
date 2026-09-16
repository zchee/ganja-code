//! The per-turn Responses options ladder (**D563**): what a session's
//! configuration, its per-model overrides and its `/fast` choice resolve to
//! for one request to one model.
//!
//! Spec: `.omc/plans/2026-09-16-responses-provider-options.md` §3.3 and the
//! probe it cites, `.omc/research/2026-09-16-chatgpt-seat-param-probe.md`.
//!
//! # Why the engine resolves the tier, and the wire never does
//!
//! `service_tier` is the one key a status bar and `/usage` report, so whoever
//! reports it has to be whoever decided it. The wire therefore receives an
//! already-resolved literal on [`RequestOptions::service_tier`] and injects
//! nothing of its own; the ladder — configuration provider-wide, then
//! per-model, then the session's `/fast` choice, and on `chatgpt` alone a
//! fast-tier default below all three — lives here, and [`Engine`]'s accessor
//! reads the same function a request is built from.
//!
//! # Why this is one free function over a seed
//!
//! Two callers resolve, and they hold different things. The engine holds the
//! installed table and the session's choice. A subagent's turn holds neither —
//! only the per-turn `Host` its parent built — and has to resolve for **its
//! own** model, which may not be the parent's: a per-model entry and the fast
//! tier are both keyed on the model. So the inputs travel as one value the
//! parent snapshots at its turn's start, and both sides call `resolve` on
//! it. The snapshot is also what keeps D474's rule for this seam: a table
//! replaced mid-turn reaches the next turn's children, never this one's.
//!
//! [`Engine`]: crate::Engine

use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::ResponsesOptions;
use crate::protocol::FastChoice;
use crate::provider::responses::options::RequestOptions;
use crate::provider::responses::{self, options};

/// Everything a resolution reads, snapshotted at a turn's start.
#[derive(Clone, Debug, Default)]
pub(crate) struct Seed {
    /// This provider's configured table, or [`None`] where the config gave
    /// the id none.
    pub(crate) table: Option<ResponsesOptions>,
    /// The session's `/fast` choice.
    pub(crate) fast: Option<FastChoice>,
    /// Which provider the requests go to, because the two Responses ids
    /// resolve the choice differently and every other id resolves nothing.
    pub(crate) provider: String,
}

/// Which rung of the ladder decided a request's `service_tier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The session's `/fast` choice.
    Fast,
    /// A `[provider.<id>.options.model."<model>"]` entry.
    PerModel,
    /// The provider-wide `[provider.<id>.options]` table.
    ProviderWide,
    /// Nothing was configured or chosen, and the provider is `chatgpt`, whose
    /// default is the model's fast tier.
    ChatgptDefault,
}

impl Source {
    /// How `/usage` and the request log spell this rung — one map, so the two
    /// can never name the same rung two ways.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "/fast",
            Self::PerModel => "per-model config",
            Self::ProviderWide => "config",
            Self::ChatgptDefault => "chatgpt default",
        }
    }
}

/// What the engine reports about the tier: what the next request asks for,
/// which rung decided it, and what the backend last said it served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierView {
    /// The literal the next request carries.
    pub requested: String,
    /// The rung that decided it.
    pub source: Source,
    /// What the backend's last terminal frame echoed for this session, or
    /// [`None`] before one has. Measured on the ChatGPT seat as `default`
    /// whatever was asked (probe 2026-09-16), which is why this is reported
    /// beside the request rather than instead of it.
    pub served: Option<String>,
}

/// The tier `/fast on` resolves to on `provider` for `model`, or [`None`] on a
/// provider that has no tier to move.
///
/// On `openai` this is `priority` for every model: the platform has never been
/// sent `ultrafast`, and until a live measurement says it takes it, one keystroke
/// sends the one value somebody has seen work.
#[must_use]
pub fn fast_tier(provider: &str, model: &str) -> Option<&'static str> {
    if provider == responses::CHATGPT_ID {
        Some(options::fast_tier(model))
    } else if provider == responses::ID {
        Some(options::DEFAULT_FAST_TIER)
    } else {
        None
    }
}

/// Resolves `seed` for one request to `model`: the provider-owned value the
/// wire reads, and the rung that decided its tier.
///
/// Every provider but the two Responses ids resolves to the default value and
/// no rung, which is a request byte-identical to one built before any of this
/// existed. `text_format` is never set here — it is the headless run's, held
/// by the engine alone and never handed to a child.
pub(crate) fn resolve(seed: &Seed, model: &str) -> (RequestOptions, Option<Source>) {
    if !options::speaks_options(&seed.provider) {
        return (RequestOptions::default(), None);
    }

    let overlaid = seed.table.as_ref().map(|table| table.for_model(model)).unwrap_or_default();
    let tier = tier(seed, &overlaid, model);
    let resolved = RequestOptions {
        service_tier: tier.map(|(literal, _)| literal.to_owned()),
        text_format: None,
        custom_tools: overlaid.custom_tools.clone(),
        server_tools: overlaid.server_tools.iter().map(server_tool).collect(),
        include: overlaid.include.clone(),
        // Configurable on the platform alone; the loader refuses it under
        // `chatgpt`, and this is the belt to that.
        reasoning_summary: (seed.provider == responses::ID)
            .then(|| overlaid.reasoning.as_ref()?.summary)
            .flatten()
            .and_then(|summary| string(&summary)),
        body: body(&overlaid),
    };

    (resolved, tier.map(|(_, source)| source))
}

/// The tier ladder, highest rung first.
///
/// `overlaid` is the table [`ResponsesOptions::for_model`] already merged for
/// `model`, so the per-model rule lives in one place; the rung is then named
/// by whether the model's own entry is what set the value.
fn tier(seed: &Seed, overlaid: &ResponsesOptions, model: &str) -> Option<(&'static str, Source)> {
    match seed.fast {
        Some(FastChoice::Off) => return Some(("default", Source::Fast)),
        Some(FastChoice::On) => return fast_tier(&seed.provider, model).map(|t| (t, Source::Fast)),
        None => {}
    }

    if let Some(tier) = overlaid.service_tier {
        let per_model = seed
            .table
            .as_ref()
            .and_then(|table| table.model.get(model))
            .is_some_and(|entry| entry.service_tier.is_some());
        let source = if per_model { Source::PerModel } else { Source::ProviderWide };

        return Some((tier.as_str(), source));
    }

    // The platform is billed per request at whatever tier it is asked for, so
    // it gets no spending default (ruling 1); the seat does.
    (seed.provider == responses::CHATGPT_ID)
        .then(|| (options::fast_tier(model), Source::ChatgptDefault))
}

/// One hosted tool entry as the wire sends it: `type`, then everything else
/// the entry said.
fn server_tool(entry: &crate::config::ServerToolEntry) -> Map<String, Value> {
    let mut sent = Map::new();
    sent.insert("type".to_owned(), Value::String(entry.kind.clone()));
    if let Value::Object(rest) = json(&entry.rest) {
        sent.extend(rest);
    }

    sent
}

/// The configured keys that are not directives, in the wire's own spelling.
///
/// Built key by key rather than by serializing the whole table and deleting
/// what does not belong, so that what reaches the body is a list somebody can
/// read — and so that a directive (`service_tier`, `custom_tools`,
/// `server_tools`, `include`, `reasoning.summary`) or the overlay's own `model`
/// table can never arrive as a top-level key the seat would 400 on. The keys
/// the typed request body writes itself are not reachable from here at all.
fn body(options: &ResponsesOptions) -> Map<String, Value> {
    let mut body = Map::new();
    let mut put = |key: &str, value: Value| {
        let empty = match &value {
            Value::Null => true,
            Value::Object(object) => object.is_empty(),
            Value::Array(array) => array.is_empty(),
            _ => false,
        };
        if !empty {
            body.insert(key.to_owned(), value);
        }
    };

    if let Some(reasoning) = &options.reasoning {
        // `summary` is a directive, applied by the wire after the layers.
        put(
            "reasoning",
            json(&serde_json::json!({ "context": reasoning.context, "mode": reasoning.mode })),
        );
    }
    put("text", json(&options.text));
    put("parallel_tool_calls", json(&options.parallel_tool_calls));
    put("stream_options", json(&options.stream_options));
    put("tool_choice", json(&options.tool_choice));
    put("context_management", json(&options.context_management));
    put("client_metadata", json(&options.client_metadata));
    put("access_programs", json(&options.access_programs));
    put("max_output_tokens", json(&options.max_output_tokens));
    put("max_tool_calls", json(&options.max_tool_calls));
    put("prompt_cache_key", json(&options.prompt_cache_key));
    put("prompt_cache_retention", json(&options.prompt_cache_retention));
    put("prompt_cache_options", json(&options.prompt_cache_options));
    put("temperature", json(&options.temperature));
    put("top_p", json(&options.top_p));
    put("top_logprobs", json(&options.top_logprobs));
    put("truncation", json(&options.truncation));
    put("safety_identifier", json(&options.safety_identifier));
    put("user", json(&options.user));
    put("metadata", json(&options.metadata));
    put("moderation", json(&options.moderation));

    body
}

/// `value` as JSON with every `null` member removed, at any depth.
///
/// The decode structs spell an unset nested field as [`None`], which
/// serializes as `null`; a `null` the backend reads is a value somebody sent,
/// and nobody configured one.
fn json(value: &impl Serialize) -> Value {
    fn strip(value: Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .filter(|(_, member)| !member.is_null())
                    .map(|(key, member)| (key, strip(member)))
                    .collect(),
            ),
            Value::Array(array) => Value::Array(array.into_iter().map(strip).collect()),
            other => other,
        }
    }

    // Every type handed here is a plain serde-derived config value or a TOML
    // table, neither of which has a way to fail serializing.
    strip(serde_json::to_value(value).expect("a decoded config value serializes"))
}

/// A unit enum's wire spelling.
fn string(value: &impl Serialize) -> Option<String> {
    json(value).as_str().map(str::to_owned)
}

#[cfg(test)]
#[path = "responses_ladder_tests.rs"]
mod tests;

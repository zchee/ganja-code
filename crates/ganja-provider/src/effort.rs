//! What efforts a cataloged model may run under, synthesized from its row.
//!
//! Spec: upstream `packages/opencode/src/provider/transform.ts` —
//! `reasoningVariants` reads models.dev's `reasoning_options`, `variants` is
//! the hardcoded capability table behind it — assembled the way
//! `provider.ts:1257` and `provider.ts:1509` assemble them: capability data
//! first, the table where the data says nothing, and whatever the catalog
//! *declares* merged deep on top with declarations winning.
//!
//! **The exclusion rule, stated once for the whole module.** Upstream keys its
//! table on the AI-SDK transport (`model.api.npm`) and covers every provider
//! models.dev lists. Ganja has no npm ids; a catalog row's `provider_id` names
//! the wire ganja would serve it through, so the table here is keyed on that —
//! and **only the branches reachable through ganja's own providers are
//! ported**: anthropic (the Messages wire), openai (Responses), grok and
//! github-copilot (chat completions). Everything else in upstream's table — the
//! gateways, bedrock, google/vertex, azure, sap, groq, the alibaba/cohere
//! toggle shapes, and the minimax/glm/kimi/deepseek/qwen early-outs — belongs
//! to transports ganja cannot select, and is deliberately not here. A compat
//! endpoint reuses a builtin id's rows and therefore its wire's encoding;
//! cursor is uncataloged and has no rows to synthesize for; a row under any
//! other provider id keeps only what the catalog declares.
//!
//! **openrouter is the one selectable provider deliberately left in that last
//! group**, and it moved there rather than out of the list: it is cataloged and
//! it is a wire ganja serves, so [`wire`] could name one. Two things say not
//! yet. Upstream's branch for it is keyed to `@openrouter/ai-sdk-provider`, a
//! *chat-completions* transport, while ganja reaches this vendor over Responses
//! (`crate::provider::openrouter`); and the fields a Responses map splices —
//! [`INCLUDE_ENCRYPTED_REASONING`] above all — are exactly the ones that module
//! refuses to send this vendor unasked, so synthesizing here would put them
//! back through the effort door. An openrouter row therefore keeps only what
//! the catalog declares, until a live probe settles what that surface accepts.
//!
//! Two translations ride every map. Upstream's option maps are AI-SDK
//! provider options (`budgetTokens`, `reasoningEffort`) that the SDK re-spells
//! onto the wire; ganja's `splice_effort` merges a map into the HTTP body
//! verbatim, so the maps here are written in wire spelling from the start —
//! `budget_tokens` on a Messages body, `reasoning` / `include` on a Responses
//! one, `reasoning_effort` on chat completions. And upstream returns efforts
//! in publication order where [`ModelInfo::variants`] is a `BTreeMap`: the
//! roster sorts by name, which is the catalog schema's standing order and not
//! this module's to change.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::catalog::{ModelInfo, ReasoningOption};

/// The efforts a model carries, by name — the value [`ModelInfo::variants`]
/// holds once a row is parsed.
type Roster = BTreeMap<String, Map<String, Value>>;

/// The wire a cataloged provider's rows are served through, which fixes the
/// field names a synthesized option map may use. This enum *is* the
/// npm-to-wire seam the module doc describes: upstream's transport ids
/// collapse onto the four lanes ganja actually serves cataloged rows on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    /// Anthropic's Messages API: thinking budgets and adaptive efforts.
    Messages,
    /// OpenAI's Responses API: `reasoning.effort`, summaries, sealed state.
    Responses,
    /// Chat completions as xAI serves it: `reasoning_effort`, nothing else.
    Grok,
    /// Chat completions as GitHub Copilot serves it: `reasoning_effort`, with
    /// upstream's per-family gates (`transform.ts:891`).
    Copilot,
}

/// Which wire serves `provider_id`'s rows, or [`None`] for every provider the
/// exclusion rule leaves out. The ids are the same vocabulary
/// [`crate::catalog`]'s `DEFAULTS` and `auth::storage_key` speak.
fn wire(provider_id: &str) -> Option<Wire> {
    match provider_id {
        "anthropic" => Some(Wire::Messages),
        "openai" => Some(Wire::Responses),
        "grok" => Some(Wire::Grok),
        "github-copilot" => Some(Wire::Copilot),
        _ => None,
    }
}

/// Upstream's `WIDELY_SUPPORTED_EFFORTS`, weakest to strongest.
const WIDELY_SUPPORTED_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// The day OpenAI rolled out the `none` reasoning tier; older models 400 on
/// it, so it is only synthesized for models new enough to accept it.
const OPENAI_NONE_EFFORT_RELEASE_DATE: &str = "2025-11-13";

/// The same for `xhigh`.
const OPENAI_XHIGH_EFFORT_RELEASE_DATE: &str = "2025-12-04";

/// What a Responses request asks back beside the reply, so a `store: false`
/// turn can replay the model's reasoning next request.
const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// Upstream's `OUTPUT_TOKEN_MAX`, the ceiling every thinking budget clamps to.
const OUTPUT_TOKEN_MAX: f64 = 32_000.0;

/// The roster `model` runs under: synthesized from its capability data, with
/// the catalog's declared variants merged deep on top, declarations winning.
///
/// Called on a row whose `variants` field still holds only what the catalog
/// declared; the caller stores the result back over it.
pub(crate) fn roster(model: &ModelInfo) -> Roster {
    let synthesized = reasoning_efforts(model).unwrap_or_else(|| table(model));

    merged(synthesized, model.variants.clone())
}

/// `declared` over `base`, upstream's `mergeDeep(variants, model.variants)`:
/// a declared name overrides the synthesized entry key by key — nested
/// objects merge recursively, anything else is replaced — and a name the
/// synthesis never produced joins the roster whole.
fn merged(mut base: Roster, declared: Roster) -> Roster {
    for (name, over) in declared {
        match base.get_mut(&name) {
            Some(entry) => merge_deep(entry, over),
            None => {
                base.insert(name, over);
            }
        }
    }

    base
}

/// Deep JSON-object merge, `over` winning wherever the two are not both
/// objects.
fn merge_deep(base: &mut Map<String, Value>, over: Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Object(nested)), Value::Object(incoming)) => {
                merge_deep(nested, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// The capability-data source, upstream's `reasoningVariants`
/// (`transform.ts:1648`). [`None`] means the data said nothing and the caller
/// falls through to [`table`]; an empty roster means it said *no efforts*,
/// which the table may not override.
fn reasoning_efforts(model: &ModelInfo) -> Option<Roster> {
    let options = model.reasoning_options.as_ref()?;
    if options.is_empty() {
        return Some(Roster::new());
    }

    if let Some(ReasoningOption::Effort { values }) = options
        .iter()
        .find(|option| matches!(option, ReasoningOption::Effort { .. }))
    {
        return Some(effort_values(model, values));
    }

    // A toggle contributes shapes only on transports outside this port
    // (alibaba, cohere — `transform.ts:1699`), so here it contributes nothing
    // and a toggle-only row falls through to the table exactly as upstream's
    // `nonEmptyVariants({})` does.
    let (min, max) = options.iter().find_map(|option| match option {
        ReasoningOption::BudgetTokens { min, max } => Some((*min, *max)),
        _ => None,
    })?;

    let efforts = budget_efforts(model, min, max);
    (!efforts.is_empty()).then_some(efforts)
}

/// One roster entry per published effort value, upstream's `effortVariants`:
/// `null` spells `none`, and a value the wire has no encoding for is skipped
/// rather than sent as a field the endpoint would refuse.
fn effort_values(model: &ModelInfo, values: &[Option<String>]) -> Roster {
    let mut roster = Roster::new();
    for value in values {
        let id = value.as_deref().unwrap_or("none");
        if let Some(settings) = effort_settings(model, id) {
            roster.insert(id.to_owned(), settings);
        }
    }

    roster
}

/// The option map one named effort splices, upstream's `reasoningEffort`
/// collapsed onto ganja's wires.
fn effort_settings(model: &ModelInfo, effort: &str) -> Option<Map<String, Value>> {
    match wire(&model.provider_id)? {
        Wire::Messages => {
            // Upstream's `?? { effort }`: a Claude generation with neither
            // adaptive thinking nor the opus-4.5 shape still takes the bare
            // field.
            Some(anthropic_effort(model, effort).unwrap_or_else(|| bare_effort(effort)))
        }
        Wire::Responses => Some(responses_effort(effort)),
        Wire::Grok => Some(chat_effort(effort)),
        // Copilot serves Gemini's thinking on no dial at all
        // (`transform.ts:1748`), so a published value has nowhere to go.
        Wire::Copilot => (!model.id.contains("gemini")).then(|| chat_effort(effort)),
    }
}

/// `high` and `max` from a published budget range, upstream's
/// `budgetVariants`: `high` is half the ceiling floored, held between the
/// published minimum and the ceiling itself; the ceiling is the published
/// maximum clamped under both the model's own output cap and
/// [`OUTPUT_TOKEN_MAX`]. Only the Messages wire encodes a budget, so on every
/// other wire this is empty and the caller falls through to the table.
fn budget_efforts(model: &ModelInfo, min: Option<f64>, max: Option<f64>) -> Roster {
    let maximum = max
        .unwrap_or(OUTPUT_TOKEN_MAX - 1.0)
        .min(model.max_output as f64 - 1.0)
        .min(OUTPUT_TOKEN_MAX - 1.0);
    if maximum <= 0.0 {
        return Roster::new();
    }
    let high = ((maximum + 1.0) / 2.0)
        .floor()
        .max(min.unwrap_or(0.0))
        .min(maximum);

    let mut roster = Roster::new();
    for (id, budget) in [("high", high), ("max", maximum)] {
        if let Some(settings) = budget_settings(model, budget) {
            roster.insert(id.to_owned(), settings);
        }
    }

    roster
}

/// The option map one thinking budget splices, upstream's `reasoningBudget`:
/// only the Messages wire has a field for it.
fn budget_settings(model: &ModelInfo, budget: f64) -> Option<Map<String, Value>> {
    match wire(&model.provider_id)? {
        Wire::Messages => Some(thinking_budget(budget)),
        Wire::Responses | Wire::Grok | Wire::Copilot => None,
    }
}

/// The hardcoded capability table, upstream's `variants`
/// (`transform.ts:721`), pruned to the branches the module doc names.
fn table(model: &ModelInfo) -> Roster {
    if !model.reasoning {
        return Roster::new();
    }
    let Some(wire) = wire(&model.provider_id) else {
        return Roster::new();
    };

    match wire {
        Wire::Messages => anthropic_table(model),
        Wire::Responses => {
            let release = model.release_date.as_deref().unwrap_or_default();
            openai_reasoning_efforts(&model.id, release)
                .into_iter()
                .map(|effort| (effort.to_owned(), responses_effort(effort)))
                .collect()
        }
        Wire::Grok => {
            // The xAI doc branch (`transform.ts:787`): grok-3-mini takes
            // exactly two tiers; everything else that reasons takes the
            // widely supported three.
            let efforts: &[&str] = if model.id.to_lowercase().contains("grok-3-mini") {
                &["low", "high"]
            } else {
                &WIDELY_SUPPORTED_EFFORTS
            };
            efforts
                .iter()
                .map(|effort| ((*effort).to_owned(), chat_effort(effort)))
                .collect()
        }
        Wire::Copilot => copilot_table(model),
    }
}

/// The Messages-wire rows of the table: adaptive efforts where the generation
/// speaks them, the opus-4.5 budget-plus-effort shape, and plain thinking
/// budgets for everything older.
fn anthropic_table(model: &ModelInfo) -> Roster {
    if let Some(efforts) = anthropic_adaptive_efforts(&model.id) {
        // Upstream filters this list further for `providerID ===
        // "github-copilot"`; that gate lives on copilot's Anthropic
        // transport, which ganja does not serve — copilot rows ride
        // [`Wire::Copilot`].
        return efforts
            .iter()
            .map(|effort| ((*effort).to_owned(), adaptive_effort(model, effort)))
            .collect();
    }

    if anthropic_opus_45(&model.id) {
        return WIDELY_SUPPORTED_EFFORTS
            .iter()
            .map(|effort| ((*effort).to_owned(), opus_45_effort(model, effort)))
            .collect();
    }

    let output = model.max_output as f64;
    let mut roster = Roster::new();
    roster.insert(
        "high".to_owned(),
        thinking_budget(16_000.0_f64.min((output / 2.0 - 1.0).floor())),
    );
    roster.insert(
        "max".to_owned(),
        thinking_budget(31_999.0_f64.min(output - 1.0)),
    );

    roster
}

/// The Copilot rows of the table (`transform.ts:891`): Gemini takes nothing,
/// Claude takes the widely supported three, and the GPT families take
/// upstream's `xhigh` gates — all spliced as `reasoning_effort` alone.
/// Upstream also sends `reasoningSummary` and encrypted-reasoning `include`,
/// which are Responses-API fields the chat wire has no place for; dropping
/// them here is the wire-capability translation, not a divergence in which
/// efforts exist.
fn copilot_table(model: &ModelInfo) -> Roster {
    if model.id.contains("gemini") {
        return Roster::new();
    }
    if model.id.contains("claude") {
        return WIDELY_SUPPORTED_EFFORTS
            .iter()
            .map(|effort| ((*effort).to_owned(), chat_effort(effort)))
            .collect();
    }

    let id = model.id.to_lowercase();
    let release = model.release_date.as_deref().unwrap_or_default();
    let mut efforts: Vec<&str> = WIDELY_SUPPORTED_EFFORTS.to_vec();
    // Two gates, one tier: 5.2 and later (and the 5.1 codex-max) carry
    // `xhigh` by id; the unversioned gpt-5 line carries it by release date.
    if id.contains("5.1-codex-max")
        || id.contains("5.2")
        || id.contains("5.3")
        || (id.contains("gpt-5") && release >= OPENAI_XHIGH_EFFORT_RELEASE_DATE)
    {
        efforts.push("xhigh");
    }

    efforts
        .into_iter()
        .map(|effort| (effort.to_owned(), chat_effort(effort)))
        .collect()
}

/// `{"reasoning_effort": E}` — the whole vocabulary of the chat wire.
fn chat_effort(effort: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("reasoning_effort".to_owned(), effort.into());

    map
}

/// `{"effort": E}` — the bare field a Claude row falls back to.
fn bare_effort(effort: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("effort".to_owned(), effort.into());

    map
}

/// The Responses-wire shape of one effort: `reasoning.effort` with an `auto`
/// summary, plus the sealed-reasoning ask. The wire already sends `include`
/// for the ids its own literal knows and the splice keeps the wire's copy
/// where both speak; carrying it here too covers the rows that literal does
/// not (the o-family, the pro models), exactly as upstream sends it on every
/// variant.
fn responses_effort(effort: &str) -> Map<String, Value> {
    let mut reasoning = Map::new();
    reasoning.insert("effort".to_owned(), effort.into());
    reasoning.insert("summary".to_owned(), "auto".into());

    let mut map = Map::new();
    map.insert("reasoning".to_owned(), Value::Object(reasoning));
    map.insert(
        "include".to_owned(),
        Value::Array(vec![INCLUDE_ENCRYPTED_REASONING.into()]),
    );

    map
}

/// `{"thinking": {"type": "enabled", "budget_tokens": N}}`, the Messages
/// API's own spelling of upstream's `budgetTokens` option.
fn thinking_budget(budget: f64) -> Map<String, Value> {
    let mut thinking = Map::new();
    thinking.insert("type".to_owned(), "enabled".into());
    thinking.insert("budget_tokens".to_owned(), tokens(budget));

    let mut map = Map::new();
    map.insert("thinking".to_owned(), Value::Object(thinking));

    map
}

/// A token count as JSON: whole values as integers — a float rendering of
/// `16000` would put `16000.0` on the wire — anything else as itself.
fn tokens(value: f64) -> Value {
    if value.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&value) {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// Upstream's `anthropicEffort`: the opus-4.5 shape, else adaptive where the
/// generation speaks it, else [`None`] for the caller's bare fallback. The
/// Kimi arm is excluded with the rest of its family — no kimi id appears
/// under the `anthropic` provider.
fn anthropic_effort(model: &ModelInfo, effort: &str) -> Option<Map<String, Value>> {
    if anthropic_opus_45(&model.id) {
        return Some(opus_45_effort(model, effort));
    }
    anthropic_adaptive_efforts(&model.id)?;

    Some(adaptive_effort(model, effort))
}

/// `{"thinking": {"type": "adaptive"}, "effort": E}`, with `display` forced
/// to `summarized` on the generations that would otherwise return empty
/// thinking blocks (`transform.ts:830`'s comment, ported with its behavior).
fn adaptive_effort(model: &ModelInfo, effort: &str) -> Map<String, Value> {
    let mut thinking = Map::new();
    thinking.insert("type".to_owned(), "adaptive".into());
    if anthropic_omits_thinking(&model.id) {
        thinking.insert("display".to_owned(), "summarized".into());
    }

    let mut map = Map::new();
    map.insert("thinking".to_owned(), Value::Object(thinking));
    map.insert("effort".to_owned(), effort.into());

    map
}

/// Upstream's `anthropicOpus45Effort`: a fixed thinking budget under the
/// model's output cap, with the effort riding beside it.
fn opus_45_effort(model: &ModelInfo, effort: &str) -> Map<String, Value> {
    let mut map = thinking_budget(16_000.0_f64.min((model.max_output as f64 / 2.0 - 1.0).floor()));
    map.insert("effort".to_owned(), effort.into());

    map
}

/// Whether this Claude generation defaults adaptive `display` to omitted —
/// upstream's `anthropicOmitsThinking`, which is its modern-adaptive check.
fn anthropic_omits_thinking(api_id: &str) -> bool {
    anthropic_modern_adaptive(api_id)
}

/// Claude 4.7 and newer speak adaptive thinking natively; an id that names no
/// version at all is read as newest, upstream's own default.
fn anthropic_modern_adaptive(api_id: &str) -> bool {
    let id = api_id.to_lowercase();
    if !id.contains("claude-") {
        return false;
    }
    match claude_version(&id) {
        None => true,
        Some((major, minor)) => major > 4 || (major == 4 && minor >= 7),
    }
}

/// The adaptive effort tiers a Claude id carries, upstream's
/// `anthropicAdaptiveEfforts`: the full five for modern generations, four for
/// the 4.6 pair, none for anything older.
fn anthropic_adaptive_efforts(api_id: &str) -> Option<&'static [&'static str]> {
    if anthropic_modern_adaptive(api_id) {
        return Some(&["low", "medium", "high", "xhigh", "max"]);
    }
    const FOUR_SIX: [&str; 8] = [
        "opus-4-6",
        "opus-4.6",
        "4-6-opus",
        "4.6-opus",
        "sonnet-4-6",
        "sonnet-4.6",
        "4-6-sonnet",
        "4.6-sonnet",
    ];
    FOUR_SIX
        .iter()
        .any(|name| api_id.contains(name))
        .then_some(&["low", "medium", "high", "max"])
}

/// Upstream's `anthropicOpus45`.
fn anthropic_opus_45(api_id: &str) -> bool {
    ["opus-4-5", "opus-4.5"]
        .iter()
        .any(|name| api_id.contains(name))
}

/// `claude-(?:[a-z]+-)?(\d+)(?:[.-](\d{1,2}))?(?:[.@-]|$)` on the lowered id,
/// first match winning — hand-rolled because this crate carries no regex
/// engine and the pattern is four fixed pieces. Minors are limited to two
/// digits so a release date in an id such as `claude-opus-4-20250514` is not
/// read as a version.
fn claude_version(id: &str) -> Option<(u64, u64)> {
    for (at, _) in id.match_indices("claude-") {
        let rest = &id[at + "claude-".len()..];
        // The optional family word, greedily first, the way the regex
        // engine would: a run of letters can never contain the `-` that ends
        // it, so at most one of the two attempts can reach the digits.
        let attempts = [family_stripped(rest), Some(rest)];
        for tail in attempts.into_iter().flatten() {
            if let Some(version) = version_tail(tail) {
                return Some(version);
            }
        }
    }

    None
}

/// `(?:[a-z]+-)` — one family word and its dash, or nothing to strip.
fn family_stripped(rest: &str) -> Option<&str> {
    let letters = rest
        .find(|c: char| !c.is_ascii_lowercase())
        .unwrap_or(rest.len());
    (letters > 0 && rest[letters..].starts_with('-')).then(|| &rest[letters + 1..])
}

/// `(\d+)(?:[.-](\d{1,2}))?(?:[.@-]|$)` — the version itself. The minor is
/// tried greedily (two digits, then one) and the no-minor reading still
/// stands when neither fits, exactly the regex's backtracking.
fn version_tail(tail: &str) -> Option<(u64, u64)> {
    let digits = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    if digits == 0 {
        return None;
    }
    let major: u64 = tail[..digits].parse().ok()?;
    let rest = &tail[digits..];

    if rest.starts_with(['.', '-']) {
        let after = &rest[1..];
        for take in [2usize, 1] {
            if after.len() >= take
                && after.as_bytes()[..take].iter().all(u8::is_ascii_digit)
                && after[take..]
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '.' | '@' | '-'))
            {
                let minor = after[..take].parse().ok()?;
                return Some((major, minor));
            }
        }
    }

    rest.chars()
        .next()
        .is_none_or(|c| matches!(c, '.' | '@' | '-'))
        .then_some((major, 0))
}

/// Upstream's `openaiReasoningEfforts`: the tiers a Responses-wire model
/// exposes, weakest to strongest, decided by family, version and release
/// date. Dates compare as strings, which for `YYYY-MM-DD` is date order.
fn openai_reasoning_efforts(api_id: &str, release_date: &str) -> Vec<&'static str> {
    let id = api_id.to_lowercase();
    if id.contains("deep-research") {
        return vec!["medium"];
    }
    if let Some(efforts) = gpt5_chat_efforts(&id) {
        return efforts;
    }
    if is_gpt5_pro(&id) {
        return vec!["high"];
    }
    if let Some(efforts) = gpt5_codex_efforts(&id) {
        return efforts;
    }
    if let Some(efforts) = versioned_gpt5_efforts(&id) {
        return efforts;
    }

    let mut efforts = WIDELY_SUPPORTED_EFFORTS.to_vec();
    if is_gpt5_family(&id) {
        efforts.insert(0, "minimal");
    }
    if release_date >= OPENAI_NONE_EFFORT_RELEASE_DATE {
        efforts.insert(0, "none");
    }
    if release_date >= OPENAI_XHIGH_EFFORT_RELEASE_DATE {
        efforts.push("xhigh");
    }

    efforts
}

/// The `-chat` family: one middling tier, or none at all for the versionless
/// original — upstream returns the empty list there, and an empty list is an
/// answer, not a fall-through.
fn gpt5_chat_efforts(id: &str) -> Option<Vec<&'static str>> {
    if !is_gpt5_family(id) || !id.contains("-chat") {
        return None;
    }
    Some(match gpt5_version(id) {
        None => vec![],
        Some(_) => vec!["medium"],
    })
}

/// The codex family's tiers by version, upstream's
/// `gpt5CodexReasoningEfforts`.
fn gpt5_codex_efforts(id: &str) -> Option<Vec<&'static str>> {
    if !is_gpt5_family(id) || !id.contains("codex") {
        return None;
    }
    let version = gpt5_version(id);
    if version.is_some_and(|v| v >= 3) {
        return Some(vec!["none", "low", "medium", "high", "xhigh"]);
    }
    if id.contains("codex-max") || version.is_some_and(|v| v >= 2) {
        return Some(vec!["low", "medium", "high", "xhigh"]);
    }

    Some(WIDELY_SUPPORTED_EFFORTS.to_vec())
}

/// The versioned families' tiers, upstream's `versionedGpt5ReasoningEfforts`:
/// GPT-5.1 swapped `minimal` for `none`, 5.2 and later added `xhigh`, and a
/// versioned pro model takes the pro triple.
fn versioned_gpt5_efforts(id: &str) -> Option<Vec<&'static str>> {
    if is_versioned_gpt5_pro(id) {
        return Some(vec!["medium", "high", "xhigh"]);
    }
    match gpt5_version(id)? {
        1 => Some(vec!["none", "low", "medium", "high"]),
        _ => Some(vec!["none", "low", "medium", "high", "xhigh"]),
    }
}

/// Every start of `gpt-5` anchored to the beginning or a `/`, upstream's
/// `(?:^|\/)gpt-5` anchor — so `gpt-50` and `agpt-5` never match.
fn gpt5_tails(id: &str) -> impl Iterator<Item = &str> {
    id.match_indices("gpt-5")
        .filter(|(at, _)| *at == 0 || id.as_bytes()[at - 1] == b'/')
        .map(|(at, _)| &id[at + "gpt-5".len()..])
}

/// `(?:^|\/)gpt-5(?:[.-]|$)`.
fn is_gpt5_family(id: &str) -> bool {
    gpt5_tails(id).any(|tail| tail.is_empty() || tail.starts_with(['.', '-']))
}

/// `(?:^|\/)gpt-5[.-](\d+)(?:[.-]|$)`, with upstream's `Number(...) ||
/// undefined` reading a matched `0` as no version at all.
fn gpt5_version(id: &str) -> Option<u64> {
    for tail in gpt5_tails(id) {
        if !tail.starts_with(['.', '-']) {
            continue;
        }
        let after = &tail[1..];
        let digits = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits == 0
            || !after[digits..]
                .chars()
                .next()
                .is_none_or(|c| matches!(c, '.' | '-'))
        {
            continue;
        }
        if let Ok(version) = after[..digits].parse::<u64>()
            && version != 0
        {
            return Some(version);
        }
    }

    None
}

/// `(?:^|\/)gpt-5[.-]?pro(?:[.-]|$)`.
fn is_gpt5_pro(id: &str) -> bool {
    gpt5_tails(id).any(|tail| {
        let tail = tail.strip_prefix(['.', '-']).unwrap_or(tail);
        tail.strip_prefix("pro")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-']))
    })
}

/// `(?:^|\/)gpt-5[.-]\d+[.-]pro(?:[.-]|$)`.
fn is_versioned_gpt5_pro(id: &str) -> bool {
    gpt5_tails(id).any(|tail| {
        let Some(after) = tail.strip_prefix(['.', '-']) else {
            return false;
        };
        let digits = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits == 0 {
            return false;
        }
        let Some(rest) = after[digits..].strip_prefix(['.', '-']) else {
            return false;
        };
        rest.strip_prefix("pro")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-']))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        budget_efforts, claude_version, gpt5_version, is_gpt5_family, is_gpt5_pro,
        openai_reasoning_efforts, roster,
    };
    use crate::catalog::{ModelInfo, ModelStatus, Pricing, ReasoningOption};

    /// A reasoning row under `provider_id`, sized like a real one, with no
    /// declared variants and no capability options until a test adds them.
    fn model(provider_id: &str, id: &str, max_output: u64) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            provider_id: provider_id.to_owned(),
            name: id.to_owned(),
            context_window: 200_000,
            max_output,
            input_limit: None,
            pricing: Pricing {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: None,
            },
            family: None,
            release_date: None,
            tool_call: true,
            status: ModelStatus::Active,
            reasoning: true,
            reasoning_options: None,
            variants: BTreeMap::new(),
        }
    }

    /// The published-option shapes the real api.json carries.
    fn efforts(values: &[Option<&str>]) -> ReasoningOption {
        ReasoningOption::Effort {
            values: values
                .iter()
                .map(|value| value.map(str::to_owned))
                .collect(),
        }
    }

    #[test]
    fn an_anthropic_budget_model_gets_thinking_budgets_in_the_messages_shape() {
        // claude-haiku-4-5 as published: a budget floor and no ceiling.
        let mut haiku = model("anthropic", "claude-haiku-4-5", 64_000);
        haiku.reasoning_options = Some(vec![ReasoningOption::BudgetTokens {
            min: Some(1024.0),
            max: None,
        }]);

        let roster = roster(&haiku);
        assert_eq!(
            serde_json::to_value(&roster).expect("a roster serializes"),
            json!({
                "high": {"thinking": {"type": "enabled", "budget_tokens": 16_000}},
                "max": {"thinking": {"type": "enabled", "budget_tokens": 31_999}},
            }),
            "half of the clamped ceiling floored, and the ceiling itself"
        );
    }

    #[test]
    fn an_anthropic_adaptive_model_synthesizes_maps_from_its_published_values() {
        let mut opus = model("anthropic", "claude-opus-4-8", 128_000);
        opus.reasoning_options = Some(vec![efforts(&[
            Some("low"),
            Some("medium"),
            Some("high"),
            Some("xhigh"),
            Some("max"),
        ])]);

        let synthesized = roster(&opus);
        assert_eq!(synthesized.len(), 5);
        assert_eq!(
            serde_json::to_value(&synthesized["xhigh"]).expect("an entry serializes"),
            json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "xhigh"}),
            "4.8 is modern adaptive, which forces the summarized display"
        );

        // The 4.6 pair speaks adaptive too, but defaults its display.
        let mut sonnet = model("anthropic", "claude-sonnet-4-6", 64_000);
        sonnet.reasoning_options = Some(vec![efforts(&[Some("low"), Some("max")])]);
        assert_eq!(
            serde_json::to_value(&roster(&sonnet)["max"]).expect("an entry serializes"),
            json!({"thinking": {"type": "adaptive"}, "effort": "max"}),
        );
    }

    #[test]
    fn the_anthropic_table_answers_when_the_row_publishes_no_options() {
        // No `reasoning_options` at all: the hardcoded table speaks. 4.5-era
        // haiku is neither modern adaptive nor in the 4.6 pair nor opus-4.5,
        // so it takes the plain budget branch.
        let haiku = model("anthropic", "claude-haiku-4-5", 64_000);
        assert_eq!(
            serde_json::to_value(roster(&haiku)).expect("a roster serializes"),
            json!({
                "high": {"thinking": {"type": "enabled", "budget_tokens": 16_000}},
                "max": {"thinking": {"type": "enabled", "budget_tokens": 31_999}},
            }),
        );

        // A versionless claude id reads as newest: the full adaptive five.
        let unversioned = model("anthropic", "claude-vnext", 64_000);
        let synthesized = roster(&unversioned);
        let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
        assert_eq!(names, ["high", "low", "max", "medium", "xhigh"]);
    }

    #[test]
    fn an_openai_model_new_enough_for_xhigh_carries_it_and_an_old_one_does_not() {
        // gpt-5.2 is versioned past 1: none through xhigh, no date needed.
        let gated = model("openai", "gpt-5.2", 128_000);
        let synthesized = roster(&gated);
        let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
        assert_eq!(names, ["high", "low", "medium", "none", "xhigh"]);

        // gpt-5.1 swapped minimal for none and predates xhigh.
        let earlier = model("openai", "gpt-5.1", 128_000);
        assert!(!roster(&earlier).contains_key("xhigh"));
        assert!(roster(&earlier).contains_key("none"));

        // o3 is neither gpt-5 family nor released past either gate.
        let mut o3 = model("openai", "o3", 100_000);
        o3.release_date = Some("2025-04-16".to_owned());
        let synthesized = roster(&o3);
        let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
        assert_eq!(names, ["high", "low", "medium"]);

        // Every map is the Responses body's own spelling.
        assert_eq!(
            serde_json::to_value(&synthesized["high"]).expect("an entry serializes"),
            json!({
                "reasoning": {"effort": "high", "summary": "auto"},
                "include": ["reasoning.encrypted_content"],
            }),
        );
    }

    #[test]
    fn grok_3_mini_gets_exactly_low_and_high() {
        let mini = model("grok", "grok-3-mini", 30_000);
        assert_eq!(
            serde_json::to_value(roster(&mini)).expect("a roster serializes"),
            json!({
                "low": {"reasoning_effort": "low"},
                "high": {"reasoning_effort": "high"},
            }),
        );

        // Every other reasoning grok takes the widely supported three.
        let grown = model("grok", "grok-4.5", 500_000);
        let synthesized = roster(&grown);
        let names: Vec<&str> = synthesized.keys().map(String::as_str).collect();
        assert_eq!(names, ["high", "low", "medium"]);
    }

    #[test]
    fn copilot_claude_rides_reasoning_effort_and_gemini_stays_empty() {
        // A budget-only row encodes nothing on the chat wire, which is the
        // fall-through to the table — and the table gives Claude the three.
        let mut claude = model("github-copilot", "claude-opus-4.5", 32_000);
        claude.reasoning_options = Some(vec![ReasoningOption::BudgetTokens {
            min: Some(1024.0),
            max: Some(32_000.0),
        }]);
        assert_eq!(
            serde_json::to_value(roster(&claude)).expect("a roster serializes"),
            json!({
                "low": {"reasoning_effort": "low"},
                "medium": {"reasoning_effort": "medium"},
                "high": {"reasoning_effort": "high"},
            }),
        );

        // Copilot's Gemini publishes values, and every one is skipped: the
        // endpoint has no dial for them.
        let mut gemini = model("github-copilot", "gemini-3.5-flash", 64_000);
        gemini.reasoning_options = Some(vec![efforts(&[Some("low"), Some("high")])]);
        assert!(roster(&gemini).is_empty());

        // The GPT table rows take the xhigh gates: 5.2 by id, gpt-5 by date.
        let unversioned_old = model("github-copilot", "gpt-5", 128_000);
        assert!(!roster(&unversioned_old).contains_key("xhigh"));
        let mut unversioned_new = model("github-copilot", "gpt-5", 128_000);
        unversioned_new.release_date = Some("2025-12-04".to_owned());
        assert!(roster(&unversioned_new).contains_key("xhigh"));
        let versioned = model("github-copilot", "gpt-5.2", 128_000);
        assert!(roster(&versioned).contains_key("xhigh"));
    }

    #[test]
    fn a_model_that_does_not_reason_synthesizes_nothing() {
        let mut plain = model("anthropic", "claude-opus-4-8", 128_000);
        plain.reasoning = false;

        assert!(roster(&plain).is_empty());
    }

    #[test]
    fn empty_reasoning_options_mean_no_efforts_even_where_the_table_would_speak() {
        // The published empty list is "explicitly none", which the table may
        // not override — grok-4.20-0309-reasoning ships exactly this shape.
        let mut refused = model("anthropic", "claude-opus-4-8", 128_000);
        refused.reasoning_options = Some(vec![]);

        assert!(roster(&refused).is_empty());
    }

    #[test]
    fn a_toggle_alone_falls_through_to_the_table() {
        // A toggle encodes only on transports outside this port, so a
        // toggle-only row is answered by the table, upstream's own
        // fall-through.
        let mut toggled = model("anthropic", "claude-haiku-4-5", 64_000);
        toggled.reasoning_options = Some(vec![ReasoningOption::Toggle]);

        assert_eq!(
            roster(&toggled),
            roster(&model("anthropic", "claude-haiku-4-5", 64_000))
        );
    }

    #[test]
    fn budget_boundaries_clamp_to_the_output_limit_and_vanish_at_zero() {
        // The model's own output cap outranks a larger published maximum.
        let capped = model("anthropic", "claude-haiku-4-5", 8_000);
        let efforts = budget_efforts(&capped, None, Some(32_000.0));
        assert_eq!(
            efforts["high"]["thinking"]["budget_tokens"],
            serde_json::Value::from(4_000),
            "half of the capped ceiling: floor((7999 + 1) / 2)"
        );
        assert_eq!(
            efforts["max"]["thinking"]["budget_tokens"],
            serde_json::Value::from(7_999)
        );

        // A published floor above the halfway point raises `high`…
        let raised = budget_efforts(&capped, Some(6_000.0), Some(32_000.0));
        assert_eq!(
            raised["high"]["thinking"]["budget_tokens"],
            serde_json::Value::from(6_000)
        );
        // …but never past the ceiling.
        let pinned = budget_efforts(&capped, Some(9_000.0), Some(32_000.0));
        assert_eq!(
            pinned["high"]["thinking"]["budget_tokens"],
            serde_json::Value::from(7_999)
        );

        // An output limit of one token leaves no room at all.
        let empty = budget_efforts(&model("anthropic", "claude-haiku-4-5", 1), None, None);
        assert!(empty.is_empty(), "maximum <= 0 yields no budget efforts");
    }

    #[test]
    fn a_declared_variant_overrides_the_synthesized_entry_and_a_new_name_joins() {
        let mut opus = model("anthropic", "claude-opus-4-8", 128_000);
        opus.reasoning_options = Some(vec![efforts(&[Some("low"), Some("high")])]);
        opus.variants = serde_json::from_value(json!({
            "high": {"effort": "overridden"},
            "custom": {"thinking": {"type": "disabled"}},
        }))
        .expect("declared variants parse");

        let roster = roster(&opus);
        assert_eq!(
            serde_json::to_value(&roster["high"]).expect("an entry serializes"),
            json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "effort": "overridden",
            }),
            "the declared key wins while the synthesized siblings survive: mergeDeep"
        );
        assert_eq!(
            serde_json::to_value(&roster["custom"]).expect("an entry serializes"),
            json!({"thinking": {"type": "disabled"}}),
            "a name the synthesis never produced joins whole"
        );
        assert!(roster.contains_key("low"), "untouched entries stand");
    }

    #[test]
    fn a_provider_outside_the_exclusion_rule_keeps_only_what_it_declares() {
        let mut foreign = model("mistral", "mistral-large", 64_000);
        foreign.reasoning_options = Some(vec![efforts(&[Some("high")])]);
        foreign.variants =
            serde_json::from_value(json!({"declared": {"reasoning_effort": "high"}}))
                .expect("declared variants parse");

        let roster = roster(&foreign);
        assert_eq!(
            roster.keys().collect::<Vec<_>>(),
            ["declared"],
            "no wire, no synthesis — the declaration alone"
        );
    }

    #[test]
    fn the_id_matchers_read_versions_the_way_upstreams_regexes_do() {
        // The claude version pattern, including the date guard.
        assert_eq!(claude_version("claude-opus-4-8"), Some((4, 8)));
        assert_eq!(claude_version("claude-sonnet-4-6"), Some((4, 6)));
        assert_eq!(claude_version("claude-3-5-sonnet"), Some((3, 5)));
        assert_eq!(
            claude_version("claude-opus-4-20250514"),
            Some((4, 0)),
            "an eight-digit date is not a minor version"
        );
        assert_eq!(claude_version("claude-vnext"), None);

        // The gpt-5 anchors: start or slash, never mid-word.
        assert!(is_gpt5_family("gpt-5.4-mini"));
        assert!(is_gpt5_family("openai/gpt-5.4-codex"));
        assert!(!is_gpt5_family("gpt-50"));
        assert!(!is_gpt5_family("agpt-5"));

        // `Number(...) || undefined`: a matched zero is no version.
        assert_eq!(gpt5_version("gpt-5.4-mini"), Some(4));
        assert_eq!(gpt5_version("gpt-5.0"), None);
        assert_eq!(gpt5_version("gpt-5-nano"), None);

        assert!(is_gpt5_pro("gpt-5-pro"));
        assert!(is_gpt5_pro("gpt-5pro"));
        assert!(!is_gpt5_pro("gpt-5-provider"));

        // The pro and codex tiers, straight off the model pages.
        assert_eq!(openai_reasoning_efforts("gpt-5-pro", ""), ["high"]);
        assert_eq!(
            openai_reasoning_efforts("gpt-5.2-pro", ""),
            ["medium", "high", "xhigh"]
        );
        assert_eq!(
            openai_reasoning_efforts("gpt-5.3-codex", ""),
            ["none", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            openai_reasoning_efforts("gpt-5.1-codex-max", ""),
            ["low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            openai_reasoning_efforts("gpt-5-chat", ""),
            [] as [&str; 0],
            "the versionless chat model answers empty rather than falling through"
        );
        assert_eq!(openai_reasoning_efforts("o3-deep-research", ""), ["medium"]);
        assert_eq!(
            openai_reasoning_efforts("gpt-5", "2025-08-07"),
            ["minimal", "low", "medium", "high"]
        );
    }
}

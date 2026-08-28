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
//! **Two of the three gateways are the selectable providers deliberately left
//! in that last group** — `opencode` and `opencode-go`. They are there rather
//! than out of the list: each is cataloged and each is a wire ganja serves, so
//! [`wire`] could name one. What says not yet is that **one provider id serves
//! three dialects**, so one [`Wire`] per provider cannot describe them at all.
//! What would describe them is
//! [`ModelInfo::npm`](crate::catalog::ModelInfo::npm) — the per-row transport
//! `crate::provider::opencode` dispatches on, and which this module could key
//! on instead of the provider id. A real road, and a deliberate follow-up
//! rather than something to do while adding the providers. Until then a row
//! under either keeps only what the catalog declares, which is what every
//! uncatalogued-wire row already does.
//!
//! **`openrouter` left that group in P20** and has a lane of its own,
//! [`Wire::OpenRouter`]. What kept it out was never the provider: upstream's
//! branch for it is keyed to `@openrouter/ai-sdk-provider`, a
//! *chat-completions* transport, while ganja reaches that vendor over Responses
//! (`crate::provider::openrouter`) — so upstream's map is about a wire this
//! build does not use, and [`responses_effort`]'s map splices exactly the
//! fields — [`INCLUDE_ENCRYPTED_REASONING`] above all — that module refuses to
//! send unasked. A *third* map settles it: that vendor's own reference
//! publishes `reasoning: {effort: minimal|low|medium|high}` and nothing else
//! about reasoning, so [`openrouter_effort`] carries that one field, the ledger
//! keeps its drops, and neither map is the other's.
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
use std::sync::LazyLock;

use regex::Regex;
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
    /// OpenRouter's Responses surface: `reasoning.effort` and nothing beside
    /// it. A lane of its own rather than [`Self::Responses`] because the two
    /// vendors' bodies differ exactly where this module writes — see
    /// [`openrouter_effort`].
    OpenRouter,
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
        "openrouter" => Some(Wire::OpenRouter),
        "grok" => Some(Wire::Grok),
        "github-copilot" => Some(Wire::Copilot),
        _ => None,
    }
}

/// Upstream's `WIDELY_SUPPORTED_EFFORTS`, weakest to strongest.
const WIDELY_SUPPORTED_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// The efforts OpenRouter's own reference publishes, weakest to strongest
/// (`openrouter.ai/docs/api_reference/responses/reasoning`, the "Reasoning
/// Effort Levels" table, read 2026-08-14).
///
/// **Authored here rather than served by the catalog**, which is the one thing
/// to know about it: `models.dev` publishes no `reasoning_options` for this
/// gateway's rows today, so without this table `/effort` would offer nothing on
/// a vendor whose reference documents the vocabulary in a table of four. The
/// catalog still outranks it wherever it speaks — [`reasoning_efforts`] is
/// consulted first and this is only the fall-through — so the day those rows
/// carry efforts of their own, this stops being consulted for them without a
/// line changing.
///
/// The vendor scopes reasoning to reasoning-capable models and publishes no
/// per-model flag ganja can read; [`table`]'s own `model.reasoning` gate is
/// what keeps this off a row that does not reason at all.
const OPENROUTER_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

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

    if let Some(ReasoningOption::Effort { values }) =
        options.iter().find(|option| matches!(option, ReasoningOption::Effort { .. }))
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
        Wire::OpenRouter => Some(openrouter_effort(effort)),
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
    let high = ((maximum + 1.0) / 2.0).floor().max(min.unwrap_or(0.0)).min(maximum);

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
        Wire::Responses | Wire::OpenRouter | Wire::Grok | Wire::Copilot => None,
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
        // No per-model gates, because the reference publishes none: the four
        // levels are documented for the surface rather than for a family, and
        // which of this gateway's 349 rows honours which is the vendor's own
        // per-model variance — a model that refuses the field answers the
        // ordinary status error naming the request.
        Wire::OpenRouter => OPENROUTER_EFFORTS
            .iter()
            .map(|effort| ((*effort).to_owned(), openrouter_effort(effort)))
            .collect(),
        Wire::Grok => {
            // The xAI doc branch (`transform.ts:787`): grok-3-mini takes
            // exactly two tiers; everything else that reasons takes the
            // widely supported three.
            let efforts: &[&str] = if model.id.to_lowercase().contains("grok-3-mini") {
                &["low", "high"]
            } else {
                &WIDELY_SUPPORTED_EFFORTS
            };
            efforts.iter().map(|effort| ((*effort).to_owned(), chat_effort(effort))).collect()
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
    roster
        .insert("high".to_owned(), thinking_budget(16_000.0_f64.min((output / 2.0 - 1.0).floor())));
    roster.insert("max".to_owned(), thinking_budget(31_999.0_f64.min(output - 1.0)));

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

    efforts.into_iter().map(|effort| (effort.to_owned(), chat_effort(effort))).collect()
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
    map.insert("include".to_owned(), Value::Array(vec![INCLUDE_ENCRYPTED_REASONING.into()]));

    map
}

/// The OpenRouter shape of one effort: `reasoning.effort`, alone.
///
/// **Deliberately not [`responses_effort`]**, although the two vendors serve
/// the same dialect. The other two fields in that map are the other vendor's:
/// `summary: "auto"` is what *its* CLI sends, and
/// [`INCLUDE_ENCRYPTED_REASONING`] is half of a sealed-state pairing this
/// gateway documents no way to complete — `crate::provider::openrouter`'s
/// ledger drops both rather than guess, and an effort map is not the door to
/// put them back through. What is left is the one field that vendor's own
/// reference documents.
fn openrouter_effort(effort: &str) -> Map<String, Value> {
    let mut reasoning = Map::new();
    reasoning.insert("effort".to_owned(), effort.into());

    let mut map = Map::new();
    map.insert("reasoning".to_owned(), Value::Object(reasoning));

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
    if anthropic_modern_adaptive(&model.id) {
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

/// Claude 4.7 and newer speak adaptive thinking natively; an id that names no
/// version at all is read as newest, upstream's own default. Upstream also
/// asks this under a second name — `anthropicOmitsThinking`, the generations
/// that default adaptive `display` to omitted — and the two checks are one
/// function there as here.
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
    FOUR_SIX.iter().any(|name| api_id.contains(name)).then_some(&["low", "medium", "high", "max"])
}

/// Upstream's `anthropicOpus45`.
fn anthropic_opus_45(api_id: &str) -> bool {
    ["opus-4-5", "opus-4.5"].iter().any(|name| api_id.contains(name))
}

/// The major and minor a Claude id carries, by upstream's own pattern.
///
/// Minors stop at two digits so a release date in an id such as
/// `claude-opus-4-20250514` is not read as one; the trailing class is what
/// makes that stop stick, since a longer run has to end on a separator the
/// pattern names or the whole minor is given back.
///
/// `packages/opencode/src/provider/transform.ts:653` calls one non-global
/// match. JavaScript's matcher and [`Regex::captures`] both search
/// leftmost-first for this pattern, so neither needs a separate retry at a
/// later `claude-`.
fn claude_version(id: &str) -> Option<(u64, u64)> {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"claude-(?:[a-z]+-)?([0-9]+)(?:[.-]([0-9]{1,2}))?(?:[.@-]|$)")
            .expect("the version pattern is a literal")
    });

    let found = PATTERN.captures(id)?;
    let major = found.get(1)?.as_str().parse().ok()?;
    let minor = found.get(2).map_or(Ok(0), |minor| minor.as_str().parse()).ok()?;

    Some((major, minor))
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
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        if digits == 0 || !after[digits..].chars().next().is_none_or(|c| matches!(c, '.' | '-')) {
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
        tail.strip_prefix("pro").is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-']))
    })
}

/// `(?:^|\/)gpt-5[.-]\d+[.-]pro(?:[.-]|$)`.
fn is_versioned_gpt5_pro(id: &str) -> bool {
    gpt5_tails(id).any(|tail| {
        let Some(after) = tail.strip_prefix(['.', '-']) else {
            return false;
        };
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        if digits == 0 {
            return false;
        }
        let Some(rest) = after[digits..].strip_prefix(['.', '-']) else {
            return false;
        };
        rest.strip_prefix("pro").is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-']))
    })
}

#[cfg(test)]
#[path = "effort_tests.rs"]
mod tests;

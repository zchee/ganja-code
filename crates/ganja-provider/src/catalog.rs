//! What each model costs and how much it can hold.
//!
//! Spec: upstream `packages/core/src/models-dev.ts`.
//!
//! Two tiers answer the same question. A catalog fetched from
//! `https://models.opencode.ai/api.json` is written to the XDG cache directory
//! and adopted at startup; where there is no cache to adopt, the compiled-in
//! snapshot answers. That snapshot is pruned from <https://models.dev/api.json>
//! as taken on **2026-08-03**, covering the current generation of the two
//! providers this build ships, and it is unconditional — sizing and pricing
//! work with no network, no cache and no home directory, so a session started
//! offline is never left without a context window. Upstream's third tier, an
//! empty table when fetching is disabled and nothing else answered, is
//! therefore unreachable here.
//!
//! None of the timers is a day, which an earlier note here claimed. A refresh
//! is debounced against the cache file's modification time for **5 minutes**,
//! the background loop repeats every **60 minutes**, and a read of the cache
//! is stale-tolerant without bound: serve whatever is on disk, revalidate
//! behind it. Nothing here expires a catalog, because a price from last week
//! is worth immeasurably more than no price at all.
//!
//! Display names are the upstream `name` field with a trailing "(latest)"
//! dropped, because a table column is not the place to explain aliasing.
//!
//! Prices are US dollars per million tokens, the unit models.dev publishes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;
use std::{fs, io};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::atomic;
use crate::protocol::Usage;

/// Tokens a price is quoted per.
const PER: f64 = 1_000_000.0;

/// Directory this build keeps its own files under, in the cache home as in the
/// data home.
const DIRECTORY: &str = "ganja";

/// Where a catalog is fetched from when [`MODELS_URL_ENV`] names nowhere else.
///
/// Upstream serves its own mirror of models.dev here, and this is the URL its
/// cache-file naming treats as the unnamed default.
const DEFAULT_SOURCE: &str = "https://models.opencode.ai";

/// Names the base URL `api.json` hangs off, for pointing a build at a mirror
/// or at a test server.
pub const MODELS_URL_ENV: &str = "GANJA_MODELS_URL";

/// Names a file to read the catalog from instead of the cache.
///
/// A read-only override: a fetch still writes the cache it would have written,
/// and a file named here is never deleted, however unreadable it turns out to
/// be. It is somebody else's file.
pub const MODELS_PATH_ENV: &str = "GANJA_MODELS_PATH";

/// Turns every fetch off when truthy, leaving the cache and the snapshot.
pub const DISABLE_FETCH_ENV: &str = "GANJA_DISABLE_MODELS_FETCH";

/// What the catalog endpoint is told the client is.
///
/// Upstream sends `opencode/<channel>/<version>/<client>`; there is one
/// channel and one client here, so the product and the version alone say as
/// much. **Ganja's own name, not a borrowed one** — no client registration is
/// involved in reading a public catalog, so this host never had a reason to be
/// told anything else, and it is
/// [`GANJA_USER_AGENT`](crate::auth::device::GANJA_USER_AGENT) itself rather
/// than a second spelling of it: a server logging ganja's traffic sees one
/// product name across every wire this build speaks in its own voice, and
/// naming the constant is what keeps that true rather than what claims it.
const USER_AGENT: &str = crate::auth::device::GANJA_USER_AGENT;

/// One deadline over connect, headers and body together.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts after the first, upstream's two.
const RETRIES: u32 = 2;

/// The first wait between attempts; each later one doubles it.
const RETRY_BASE: Duration = Duration::from_millis(200);

/// How recently the cache must have been written for a refresh to skip.
///
/// This is a debounce on writing, not an expiry on reading: a cache older than
/// this is still served, it is merely also revalidated.
const DEBOUNCE: Duration = Duration::from_secs(5 * 60);

/// How long [`spawn_refresh_loop`] waits between rounds.
const REPEAT: Duration = Duration::from_secs(60 * 60);

/// What a provider charges for a million tokens of each kind.
#[derive(Clone, Debug, PartialEq)]
pub struct Pricing {
    /// Input tokens the provider read fresh.
    pub input: f64,
    /// Tokens the model generated, thinking included.
    pub output: f64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read: f64,
    /// Input tokens written into the prompt cache, where that is billed apart.
    ///
    /// [`None`] means the provider bills a cache write as ordinary input,
    /// which is what OpenAI-style automatic caching does; [`cost`] prices those
    /// tokens at [`Pricing::input`]. Upstream reads an absent `cache_write` as
    /// zero, which prices those tokens as free; charging them as input is the
    /// deliberate divergence, and it is the rule this table was written under.
    pub cache_write: Option<f64>,
}

/// How far along its life a model is, as the catalog publishes it.
///
/// Carried rather than acted on: a provider that still serves a deprecated
/// model will still answer a request for it, and dropping the row would only
/// mean pricing that turn at nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelStatus {
    /// The catalog says nothing, which upstream reads as generally available.
    #[default]
    Active,
    /// Published for trial.
    Alpha,
    /// Published, not yet settled.
    Beta,
    /// On its way out.
    Deprecated,
}

/// One model this build knows how to price and size.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// Identifier the provider expects on the wire.
    pub id: String,
    /// Provider that serves it, spelled as [`Provider::id`](crate::provider::Provider::id).
    pub provider_id: String,
    /// Name to show a person.
    pub name: String,
    /// Tokens the model can be given, prompt and reply together.
    pub context_window: u64,
    /// Tokens it will generate in one reply before stopping.
    pub max_output: u64,
    /// Tokens it will accept in the prompt alone, where that is capped apart
    /// from the window.
    pub input_limit: Option<u64>,
    /// What it charges.
    pub pricing: Pricing,
    /// Generation the provider groups it under, where the catalog names one.
    ///
    /// Carried rather than acted on, like [`ModelStatus`]: decoded and
    /// re-served, while prompt families are picked off the model id.
    pub family: Option<String>,
    /// The day it was published, as the catalog spells it.
    pub release_date: Option<String>,
    /// Whether it can be given tools at all.
    pub tool_call: bool,
    /// How far along its life the catalog says it is.
    pub status: ModelStatus,
    /// Whether the model reasons at all — the gate the capability table in
    /// `effort.rs` synthesizes under. Absent from a published row means it
    /// does not, which is also what every row of the compiled-in snapshot
    /// says.
    pub reasoning: bool,
    /// How its provider lets a request ask for that reasoning, as models.dev
    /// publishes it. [`None`] — the catalog said nothing — is what lets the
    /// hardcoded table answer instead; the empty list is a published answer
    /// ("no efforts") that the table may not override.
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    /// Which SDK transport the catalog says serves this row — the schema's own
    /// `npm`, kept under the schema's name for [`Self::variants`]'s reason.
    ///
    /// **The effective value, not the literal one.** A row may carry
    /// `provider: {npm}` of its own; where it does not, the provider's
    /// top-level `npm` answers, which is upstream's own `??` (`plugin/provider/
    /// opencode.ts:127-136`, `:140`). Resolved once here so that every reader
    /// gets one answer rather than re-deriving the fallback.
    ///
    /// Carried rather than acted on **by this module** — it is not sizing and
    /// it is not price. One provider reads it, and it is the reason the field
    /// exists: `provider::opencode`'s gateway serves three different dialects
    /// off one base URL, and this hint is the only thing that says which. For
    /// every other provider the dialect is fixed at the provider, so nothing
    /// consults it and [`None`] costs nothing.
    pub npm: Option<String>,
    /// The named variants a session may run this model under, each carrying
    /// the provider options its wire splices into the request body — upstream's
    /// `variants: Record<string, Record<string, any>>` (`provider.ts:1049`).
    /// Ganja's own surface calls these "effort"; this field keeps the catalog
    /// schema's name (`effort-not-variants`).
    ///
    /// Assembled at parse the way upstream assembles it (`provider.ts:1257`):
    /// synthesized from the row's capability data — [`Self::reasoning_options`]
    /// first, the table in `effort.rs` behind it — with whatever the catalog
    /// *declares* merged deep on top, declarations winning. Empty for every
    /// row of the compiled-in snapshot — its rows carry no capability data to
    /// synthesize from, and a fresh fetch fills the roster naturally — and
    /// for every uncataloged provider, which is what keeps "no catalog, no
    /// efforts" one rule rather than two.
    pub variants: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// One way a model's provider lets a request ask for reasoning, as models.dev
/// publishes it (upstream `packages/core/src/models-dev.ts`,
/// `ReasoningOption`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    /// Named effort tiers, weakest to strongest.
    Effort {
        /// The tier names; `null` is upstream's spelling of `none`.
        values: Vec<Option<String>>,
    },
    /// Thinking switches on and off, with no dial between.
    Toggle,
    /// A thinking budget in tokens, bounded where the provider says so.
    ///
    /// Floats because the catalog publishes JSON numbers (`Schema.Finite`),
    /// the same reason the limit fields are read as [`f64`].
    BudgetTokens {
        /// The smallest budget the provider accepts.
        #[serde(default)]
        min: Option<f64>,
        /// The largest, absent when only the model's own output cap bounds it.
        #[serde(default)]
        max: Option<f64>,
    },
    /// A kind this build has not heard of — carried as nothing rather than
    /// failing the row, so a catalog that grows a word keeps its prices.
    #[serde(other)]
    Unknown,
}

/// What one turn cost, in US dollars.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Everything charged on the way in: fresh input, cache reads, cache
    /// writes.
    pub input_usd: f64,
    /// Everything charged on the way out.
    pub output_usd: f64,
    /// The two above, which is what a status bar shows.
    pub total_usd: f64,
}

/// Why a catalog could not be refreshed.
///
/// Every one of these is survivable: the table in memory is left exactly as it
/// was, which is at worst the compiled-in snapshot.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The request never produced a body.
    #[error("the catalog request did not complete: {0}")]
    Request(String),
    /// The endpoint answered, with something other than success.
    #[error("the catalog endpoint answered {status}")]
    Status {
        /// The status it answered with.
        status: u16,
    },
    /// The body is not a catalog this build can read.
    #[error("the catalog payload could not be read: {0}")]
    Parse(String),
    /// The cache could not be written.
    #[error("the catalog cache at {path} could not be written: {source}")]
    Cache {
        /// The file the write was aimed at.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// There is no home directory to resolve the cache against.
    #[error("the cache directory could not be located: {0}")]
    Home(String),
}

/// The model each provider is asked for when nothing says otherwise.
///
/// Deliberately not read from the fetched catalog: `api.json` publishes no
/// per-provider default, so this pin is ganja's own and a refresh has nothing
/// to say about it.
const DEFAULTS: &[(&str, &str)] = &[
    ("anthropic", "claude-opus-4-8"),
    // **This row is the key wire's default — the seat never reads it.** A
    // default has to be a model that can take a ganja turn, and every ganja
    // turn offers tools. That is why this row spent a round on `gpt-5.4`:
    // chat completions had answered `400 "Function tools with reasoning_effort
    // are not supported for gpt-5.6 in /v1/chat/completions. To use function
    // tools, use /v1/responses or set reasoning_effort to 'none'."` — and the
    // repair was to change wires, not defaults. An API key now rides the
    // Responses API, and the turn that first refusal named was then taken
    // live (2026-08-06): `gpt-5.6` ran a tool and completed on a key at the
    // platform. The ChatGPT seat still cannot run it (`provider::responses`,
    // from `codex.ts:289`), and still does not care: a subscription session
    // takes its default from `provider::responses::SUBSCRIPTION_DEFAULT`,
    // never from here.
    ("openai", "gpt-5.6"),
    ("grok", "grok-4.5"),
    ("github-copilot", "claude-opus-4.8"),
    // The one uncataloged pin. `default` is not a row this table holds — it
    // is the id cursor's own listing serves first, the server-side Auto
    // routing the reference plugin spells `auto` and the Run API spells
    // `default` (live roster, 2026-08-10). A default has to be an id the
    // backend accepts when nobody chose, and for cursor the backend itself
    // publishes that id; sizing, pricing and auto-compaction stay off
    // exactly as for any model this table cannot see.
    ("cursor", "default"),
];

/// One row of the compiled-in snapshot.
///
/// A [`ModelInfo`] owns its strings, so the snapshot cannot be one directly;
/// this is the same row with the strings still static, expanded once into the
/// runtime table.
struct Row {
    /// See [`ModelInfo::id`].
    id: &'static str,
    /// See [`ModelInfo::provider_id`].
    provider_id: &'static str,
    /// See [`ModelInfo::name`].
    name: &'static str,
    /// See [`ModelInfo::context_window`].
    context_window: u64,
    /// See [`ModelInfo::max_output`].
    max_output: u64,
    /// See [`ModelInfo::pricing`].
    pricing: Pricing,
}

/// The snapshot itself.
const SNAPSHOT: &[Row] = &[
    Row {
        id: "claude-sonnet-5",
        provider_id: "anthropic",
        name: "Claude Sonnet 5",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing { input: 2.0, output: 10.0, cache_read: 0.2, cache_write: Some(2.5) },
    },
    Row {
        id: "claude-opus-5",
        provider_id: "anthropic",
        name: "Claude Opus 5",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: Some(6.25) },
    },
    Row {
        id: "claude-opus-4-8",
        provider_id: "anthropic",
        name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: Some(6.25) },
    },
    Row {
        id: "claude-sonnet-4-6",
        provider_id: "anthropic",
        name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: Some(3.75) },
    },
    Row {
        id: "claude-haiku-4-5",
        provider_id: "anthropic",
        name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output: 64_000,
        pricing: Pricing { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: Some(1.25) },
    },
    Row {
        id: "gpt-5.6",
        provider_id: "openai",
        name: "GPT-5.6",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing { input: 5.0, output: 30.0, cache_read: 0.5, cache_write: Some(6.25) },
    },
    Row {
        id: "gpt-5.4",
        provider_id: "openai",
        name: "GPT-5.4",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing { input: 2.5, output: 15.0, cache_read: 0.25, cache_write: None },
    },
    Row {
        id: "gpt-5.4-mini",
        provider_id: "openai",
        name: "GPT-5.4 mini",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing { input: 0.75, output: 4.5, cache_read: 0.075, cache_write: None },
    },
    Row {
        id: "gpt-5.4-nano",
        provider_id: "openai",
        name: "GPT-5.4 nano",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing { input: 0.2, output: 1.25, cache_read: 0.02, cache_write: None },
    },
    Row {
        id: "gpt-5.3-codex",
        provider_id: "openai",
        name: "GPT-5.3 Codex",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing { input: 1.75, output: 14.0, cache_read: 0.175, cache_write: None },
    },
    // `provider_id` is `grok` and not `xai`: the file a credential is stored in
    // uses upstream's name for this provider and everything else uses ganja's,
    // and this table is read by `provider::serves` and by the session layer's
    // pricing lookup, both of which hold a `Provider::id`.
    //
    // **The price under-reports a very long context.** xAI charges in tiers —
    // above 200k tokens the rate roughly doubles (input 2.5, output 5,
    // cache_read 0.4) — and `Pricing` has no tier concept. The row carries the
    // base rate, which is the same approximation every other tiered provider in
    // this table already gets; modelling tiers is a schema change, and a schema
    // change is not something a new row should smuggle in.
    Row {
        id: "grok-4.3",
        provider_id: "grok",
        name: "Grok 4.3",
        context_window: 1_000_000,
        max_output: 30_000,
        pricing: Pricing {
            input: 1.25,
            output: 2.5,
            cache_read: 0.2,
            // xAI publishes no cache-write price.
            cache_write: None,
        },
    },
    // The default's row (2026-08-10, published catalog). xAI tiers this model
    // — 4/12/0.6 past 200k of context — and this row carries the base rate,
    // the same approximation the note above already owns for every tiered
    // provider in the table.
    Row {
        id: "grok-4.5",
        provider_id: "grok",
        name: "Grok 4.5",
        context_window: 500_000,
        max_output: 500_000,
        pricing: Pricing {
            input: 2.0,
            output: 6.0,
            cache_read: 0.3,
            // xAI publishes no cache-write price.
            cache_write: None,
        },
    },
    // **Two rows, deliberately — and no more.** `api.githubcopilot.com/models`
    // answers with dozens across the Claude, GPT and Gemini families, and a
    // row is a promise that a turn asking for that model works; the rest are
    // unkept until somebody checks. That is D274's precedent — grok ships the
    // same way — and asking `api.githubcopilot.com/models` at runtime, which
    // would settle the question wholesale, is deliberately out of this build.
    // The second row exists because the seat's default moved onto it
    // (2026-08-10, owner's pin): a default has to be a row this table can
    // size, so the pin and the row travel together.
    // So this tier answers `provider::serves` for Copilot on its own; the
    // published catalog, when it has been fetched, replaces it with a much
    // longer list.
    //
    // **The limits are GitHub's, not the model's**, and the difference is a
    // factor of five: the published catalog sizes this row at a 200k window
    // with 32k of output, where the same model served by Anthropic directly
    // takes a million. A proxy is free to cap what it resells and this one
    // does. Sizing the row at the model's own limits would have been the
    // plausible guess and the wrong one — a session sized to a window the
    // endpoint will not accept stops compacting and starts being refused —
    // which is why these are copied from the catalog rather than inferred from
    // the model's name.
    //
    // **The prices are zero, and that is the honest figure rather than a hole
    // somebody forgot to fill.** A Copilot seat is billed by the month: there
    // is no per-token rate to report, and inventing the underlying vendor's
    // would tell a person they had spent money they had not. `cost` therefore
    // returns zero for every Copilot turn priced from *this* tier and the
    // status bar shows none, while the token counters — which are real, and
    // are what a seat's quota is actually spent against — still show.
    //
    // The published catalog disagrees, and a build that has fetched it will
    // price these turns at the underlying vendor's rates: this tier is the
    // floor, not the last word. Reconciling the two is a decision about what a
    // subscription turn should report, not a number to change here.
    Row {
        id: "claude-sonnet-4.6",
        provider_id: "github-copilot",
        name: "Claude Sonnet 4.6 (Copilot)",
        context_window: 200_000,
        max_output: 32_000,
        pricing: Pricing { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: None },
    },
    // The default's row: sized from the published catalog's GitHub limits
    // (window 200k, output 64k) and priced at the seat's honest zero, both
    // for the reasons the tier note above already gives.
    Row {
        id: "claude-opus-4.8",
        provider_id: "github-copilot",
        name: "Claude Opus 4.8 (Copilot)",
        context_window: 200_000,
        max_output: 64_000,
        pricing: Pricing { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: None },
    },
];

/// Every model one tier of the catalog knows, in the order it lists them.
struct Catalog {
    /// The rows. Behind an [`Arc`] each, so handing one out is a refcount
    /// rather than a copy of its strings, and so a row a caller is holding
    /// stays valid across a refresh that replaced the table under it.
    models: Vec<Arc<ModelInfo>>,
}

/// The table every lookup reads, swapped wholesale by [`refresh`].
///
/// A [`OnceLock`] rather than a `LazyLock` only because the initializer is a
/// function call either way; what matters is that the first read never fails
/// and never touches the disk — the snapshot needs neither a home directory
/// nor a filesystem, so no lookup anywhere can be made to block or to fail by
/// the state of the machine.
static TABLE: OnceLock<RwLock<Arc<Catalog>>> = OnceLock::new();

/// The table, initialized to the snapshot on first use.
fn table() -> &'static RwLock<Arc<Catalog>> {
    TABLE.get_or_init(|| RwLock::new(Arc::new(snapshot())))
}

/// The catalog as it stands, held past the lock so no reader blocks a swap.
fn current() -> Arc<Catalog> {
    Arc::clone(&table().read().unwrap_or_else(PoisonError::into_inner))
}

/// Replaces the table wholesale.
///
/// A poisoned lock is taken anyway: what it guards is one `Arc`, replaced as a
/// unit, so a panic elsewhere cannot have left it half-written.
fn install(catalog: Catalog) {
    let mut table = table().write().unwrap_or_else(PoisonError::into_inner);
    *table = Arc::new(catalog);
}

/// The compiled-in tier, expanded into owned rows.
fn snapshot() -> Catalog {
    Catalog {
        models: SNAPSHOT
            .iter()
            .map(|row| {
                Arc::new(ModelInfo {
                    id: row.id.to_owned(),
                    provider_id: row.provider_id.to_owned(),
                    name: row.name.to_owned(),
                    context_window: row.context_window,
                    max_output: row.max_output,
                    input_limit: None,
                    pricing: row.pricing.clone(),
                    family: None,
                    release_date: None,
                    tool_call: true,
                    status: ModelStatus::Active,
                    // The snapshot is sizes and prices, not capabilities: a
                    // row here says nothing about reasoning, so nothing is
                    // synthesized from it and the roster stays empty until a
                    // fetched catalog replaces the table.
                    reasoning: false,
                    reasoning_options: None,
                    // The compiled-in tier is every provider whose dialect is
                    // fixed at the provider, so there is nothing for a
                    // per-row hint to say.
                    npm: None,
                    variants: BTreeMap::new(),
                })
            })
            .collect(),
    }
}

/// Looks up a model by the identifier providers use for it.
///
/// The first row carrying the id wins. A fetched catalog covers dozens of
/// providers and the same model id appears under several of them; answering
/// with the first is what keeps this a lookup rather than a question about
/// which provider was meant, and every caller here already knows its provider
/// from somewhere truer than the model id.
#[must_use]
pub fn model(id: &str) -> Option<Arc<ModelInfo>> {
    current().models.iter().find(|model| model.id == id).cloned()
}

/// Looks up the row `provider_id` serves under `id`.
///
/// [`model`]'s first-row answer stopped being enough the day rosters were
/// synthesized per wire: openai and github-copilot both publish `gpt-5.4`,
/// and the copilot row's efforts splice as a chat-completions body where the
/// openai row's splice as Responses. Every effort consumer knows its
/// provider, so the roster is read through this — sizing and pricing, where
/// any provider's numbers are close enough, keep the id-only lookup.
#[must_use]
pub fn model_for(provider_id: &str, id: &str) -> Option<Arc<ModelInfo>> {
    scoped(&current(), provider_id, id)
}

/// [`model_for`] against a named table — the seam that lets the lookup be
/// tested on a parsed catalog without installing it over the process-global
/// one every other test reads.
fn scoped(catalog: &Catalog, provider_id: &str, id: &str) -> Option<Arc<ModelInfo>> {
    catalog.models.iter().find(|model| model.provider_id == provider_id && model.id == id).cloned()
}

/// Every model in the table.
///
/// The snapshot is listed in the order it is written — provider by provider,
/// each provider's default first. A fetched catalog is listed by provider id
/// and then by model id, because that is the order its JSON objects decode in
/// and an order nobody chose is better than one that changes between runs.
pub fn models() -> impl Iterator<Item = Arc<ModelInfo>> {
    current().models.clone().into_iter()
}

/// Whether this table has anything to say about `provider_id`.
///
/// The second of the two tiers [`crate::provider::PROVIDERS`] describes:
/// **selectable** is what a session may run as, and **cataloged** is the
/// narrower set that has rows here. A provider outside it is not broken — it
/// takes whole turns — but nothing can size its context window, price its
/// tokens or pick its model, so its session runs the degradation path
/// documented at [`crate::provider::serves`] and at `session.rs`'s two sites:
/// the title falls back to the session's own model and auto-compaction is off
/// with a warning.
///
/// Every configured endpoint is uncataloged, because no published catalog
/// knows a private one. So is a builtin whose wire ships before its rows do,
/// which is why this is a predicate rather than a list.
#[must_use]
pub fn carries(provider_id: &str) -> bool {
    current().models.iter().any(|model| model.provider_id == provider_id)
}

/// The model `provider_id` is asked for when the user names none.
///
/// [`None`] for a provider this build does not pin a default for: answering
/// with some other provider's model would be a silent misconfiguration rather
/// than a default. The pins are compiled in and a refresh does not move them,
/// because the published catalog does not carry the concept.
#[must_use]
pub fn default_model(provider_id: &str) -> Option<&'static str> {
    DEFAULTS.iter().find(|(provider, _)| *provider == provider_id).map(|(_, model)| *model)
}

/// Renders a token count for somewhere there is no room to spell it out.
///
/// Counts below a thousand are exact, because the difference between 12 and 90
/// tokens is worth seeing; above that a tenth of the unit is as much precision
/// as a status bar or a table column can justify.
#[must_use]
pub fn compact_tokens(tokens: u64) -> String {
    const THOUSAND: f64 = 1_000.0;
    const MILLION: f64 = 1_000_000.0;

    let count = tokens as f64;
    if count < THOUSAND {
        return tokens.to_string();
    }

    // Rounding can push a count into the next unit: 999,950 tokens reads as
    // 1.0M rather than as the 1000.0k a naive division would print.
    if count < MILLION - 50.0 {
        return format!("{:.1}k", count / THOUSAND);
    }

    format!("{:.1}M", count / MILLION)
}

/// Prices one turn's [`Usage`] against `model`.
///
/// The three input counters are treated as disjoint — plain input, cache reads,
/// and cache writes each billed at their own rate — which is the shape
/// [`Usage`] documents and which providers normalize to.
/// [`Usage::reasoning_tokens`] is deliberately not priced: it counts a subset of
/// [`Usage::output_tokens`] that both providers already bill as output.
///
/// Tiered pricing is not read: a catalog row may quote a different rate above
/// some context size, and a turn that crosses that line is priced here at the
/// flat rate and so under-reported.
#[must_use]
pub fn cost(usage: &Usage, model: &ModelInfo) -> Cost {
    let pricing = &model.pricing;
    let priced = |per_mtok: f64, tokens: u64| per_mtok * tokens as f64 / PER;

    let input_usd = priced(pricing.input, usage.input_tokens)
        + priced(pricing.cache_read, usage.cache_read_tokens)
        + priced(pricing.cache_write.unwrap_or(pricing.input), usage.cache_write_tokens);
    let output_usd = priced(pricing.output, usage.output_tokens);

    Cost { input_usd, output_usd, total_usd: input_usd + output_usd }
}

/// Adopts whatever catalog is cached on disk, at any age.
///
/// Returns whether it found one. This is the startup tier: a frontend calls it
/// — [`spawn_refresh_loop`] does — before anything reads a price, and until it
/// does, every lookup is answered by the compiled-in snapshot. Deliberately
/// not folded into the first lookup: a table that reads the filesystem the
/// first time anybody asks it a question makes every test that asks one depend
/// on the machine it runs on.
///
/// A cache that does not parse is deleted and read as absent, so a body that
/// arrived truncated or from a mirror serving something else heals itself on
/// the next run rather than poisoning every run after it. A file named by
/// [`MODELS_PATH_ENV`] is never deleted.
pub fn load_cached() -> bool {
    let source = match source() {
        Ok(source) => source,
        Err(error) => {
            tracing::debug!(%error, "no cached catalog could be located");
            return false;
        }
    };

    match read_cached(&source) {
        Some(catalog) => {
            install(catalog);
            true
        }
        None => false,
    }
}

/// Fetches the catalog and swaps the table for it.
///
/// Returns whether the table was replaced. `false` is the ordinary answer to
/// "there was nothing to do": fetching is off, or the cache was written less
/// than five minutes ago and `force` was not set.
///
/// The body is parsed before it is written, so an endpoint answering with
/// something that is not a catalog cannot leave one on disk — upstream writes
/// first and parses after, and poisons its own cache doing it.
///
/// # Errors
///
/// [`CatalogError`] when the request, the payload or the cache write fails.
/// The table is left as it was in every one of those cases.
pub async fn refresh(force: bool) -> Result<bool, CatalogError> {
    if fetching_disabled() {
        return Ok(false);
    }

    let source = source()?;
    if !force && fresh(&source.cache) {
        return Ok(false);
    }

    let body = fetch(&source.url).await?;
    let catalog = parse(&body)?;
    write_cache(&source.cache, &body)?;
    install(catalog);

    Ok(true)
}

/// Adopts the cached catalog, then keeps it current until `cancel` fires.
///
/// The disk tier is adopted on the calling thread before this returns, so a
/// frontend that calls it during startup has the cached prices in hand for its
/// first frame. The loop behind it refreshes immediately and then hourly,
/// logging and swallowing every failure: a catalog that could not be refreshed
/// is not a reason to interrupt anybody, and the tier below it still answers.
///
/// # Panics
///
/// Through [`tokio::spawn`], when called outside a runtime.
pub fn spawn_refresh_loop(cancel: CancellationToken) {
    load_cached();

    // Nothing to schedule: the loop's only job is fetching.
    if fetching_disabled() {
        return;
    }

    tokio::spawn(async move {
        loop {
            match refresh(false).await {
                Ok(true) => tracing::info!("the model catalog was refreshed"),
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "the model catalog was not refreshed"),
            }

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(REPEAT) => {}
            }
        }
    });
}

/// Where a catalog is read from and written to, resolved from the environment.
struct Source {
    /// Base URL `api.json` hangs off.
    url: String,
    /// The file a fetch writes and a refresh dates itself against.
    cache: PathBuf,
    /// The file a read comes from — [`Source::cache`] unless overridden.
    read: PathBuf,
    /// Whether [`Source::read`] came from [`MODELS_PATH_ENV`].
    overridden: bool,
}

/// Resolves [`Source`] from the environment.
fn source() -> Result<Source, CatalogError> {
    let url = setting(MODELS_URL_ENV).unwrap_or_else(|| DEFAULT_SOURCE.to_owned());
    let base = Xdg::new().map_err(|error| CatalogError::Home(error.to_string()))?;
    let cache = base.cache_dir().join(DIRECTORY).join(cache_name(&url));
    let (read, overridden) = match setting(MODELS_PATH_ENV) {
        Some(path) => (PathBuf::from(path), true),
        None => (cache.clone(), false),
    };

    Ok(Source { url, cache, read, overridden })
}

/// What the cache file for `url` is called.
///
/// The default source keeps the plain name; anything else is fingerprinted, so
/// pointing a build at a mirror cannot have it read a catalog fetched from
/// somewhere else. Upstream hashes with SHA-1; this is FNV-1a, because the
/// name only has to differ per URL — nothing here is defending against
/// somebody choosing a URL that collides with another URL they also control —
/// and a hash function this build does not otherwise need is not worth a
/// dependency.
fn cache_name(url: &str) -> String {
    if url == DEFAULT_SOURCE {
        return "models.json".to_owned();
    }

    format!("models-{}.json", fingerprint(url))
}

/// FNV-1a over `value`, as sixteen hex digits.
fn fingerprint(value: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:016x}")
}

/// Reads `variable`, treating an empty value as unset.
fn setting(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Whether [`DISABLE_FETCH_ENV`] is set to something upstream reads as true.
fn fetching_disabled() -> bool {
    setting(DISABLE_FETCH_ENV).is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value == "1" || value == "true"
    })
}

/// Reads the cached catalog, deleting one that cannot be read.
fn read_cached(source: &Source) -> Option<Catalog> {
    let body = fs::read_to_string(&source.read).ok()?;

    match parse(&body) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::warn!(
                path = %source.read.display(),
                %error,
                "the cached catalog could not be read; discarding it"
            );
            // A file this build wrote is this build's to remove. One the
            // environment pointed at belongs to whoever pointed at it, and
            // deleting it would be answering a bad payload by destroying
            // somebody else's data.
            if !source.overridden {
                let _ = fs::remove_file(&source.cache);
            }
            None
        }
    }
}

/// Whether `path` was written recently enough for a refresh to skip.
///
/// A file that cannot be stat'd, or one dated in the future, is not fresh: the
/// answer to not knowing how old a cache is, is to fetch.
fn fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age < DEBOUNCE))
}

/// Turns a catalog payload into a table.
///
/// Tolerant by construction, because the payload is a schema nobody validates
/// at the other end: the document is walked as JSON and each model decoded on
/// its own, so an unknown field is ignored, a missing optional takes its
/// default, and a single model whose shape drifted is skipped instead of
/// rejecting the catalog it came in.
///
/// What is not tolerated is a row that does not say what it holds or what it
/// will generate. Both numbers are load-bearing — one decides when a session
/// compacts, the other caps the reply a request may ask for — and a zero in
/// either is worse than an absent row, which every caller already handles.
///
/// The payload's provider keys are upstream's vocabulary, the same vocabulary
/// `auth.json` is written in, so each one is read through
/// [`auth::provider_id_for_storage_key`] on the way in. That is the identity
/// for every provider the two projects name the same way and turns `xai` into
/// `grok`, which matters because every consumer of this table holds a
/// [`Provider::id`](crate::provider::Provider::id): a fetched catalog filing
/// grok's models under `xai` would leave a refreshed install unable to price a
/// grok turn or to confirm the model it is about to ask for.
fn parse(body: &str) -> Result<Catalog, CatalogError> {
    let root: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(body).map_err(|error| CatalogError::Parse(error.to_string()))?;

    let mut models = Vec::new();
    for (published_under, provider) in root {
        let Some(published) = provider.get("models").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let provider_id = crate::auth::provider_id_for_storage_key(&published_under);
        // The provider's own transport, which every row it did not override
        // inherits — read once here rather than per row.
        let provider_npm = provider.get("npm").and_then(serde_json::Value::as_str);

        for (model_id, published) in published {
            let Ok(wire) = serde_json::from_value::<Wire>(published.clone()) else {
                tracing::debug!(provider_id, model_id, "a catalog row was not readable");
                continue;
            };
            if let Some(info) = wire.into_info(provider_id, model_id, provider_npm) {
                models.push(Arc::new(info));
            }
        }
    }

    if models.is_empty() {
        return Err(CatalogError::Parse(
            "no row in the payload carries the limits a session needs".to_owned(),
        ));
    }

    Ok(Catalog { models })
}

/// One model as the catalog publishes it.
///
/// Every field is optional and defaulted; the id and the provider come from the
/// keys the row was found under, which upstream also treats as authoritative.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Wire {
    /// Display name.
    name: Option<String>,
    /// Generation the provider groups it under.
    family: Option<String>,
    /// Publication date, as text.
    release_date: Option<String>,
    /// Whether it takes tools. Absent means it does, upstream's default.
    tool_call: Option<bool>,
    /// Where in its life it is.
    status: Option<String>,
    /// Prices, in dollars per million tokens.
    cost: Option<WireCost>,
    /// Sizes, in tokens.
    limit: Option<WireLimit>,
    /// Whether the model reasons, the capability table's gate.
    reasoning: Option<bool>,
    /// How its provider lets a request ask for that reasoning.
    reasoning_options: Option<Vec<ReasoningOption>>,
    /// Named variants, each a map of provider options. The struct-level
    /// default is what lets a cache written before variants existed — and
    /// every row that publishes none — parse unchanged.
    variants: Option<BTreeMap<String, serde_json::Map<String, serde_json::Value>>>,
    /// The row's own transport override, `{"npm": …, "api": …}`. Absent — the
    /// common case, and `null` on the wire — means the provider's own `npm`
    /// answers for it.
    provider: Option<WireTransport>,
}

/// The `provider` object a row may carry to override its provider's transport.
///
/// Only `npm` is read. The sibling `api` would name a per-model base URL, which
/// no row this build has met uses and which nothing here would know what to do
/// with — a row that needs a *different host* from its provider is a provider,
/// so it is left undecoded rather than half-honoured.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireTransport {
    /// The SDK package that serves this row.
    npm: Option<String>,
}

/// The `cost` object.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireCost {
    /// Fresh input.
    input: Option<f64>,
    /// Output.
    output: Option<f64>,
    /// Input served from the prompt cache.
    cache_read: Option<f64>,
    /// Input written into the prompt cache.
    cache_write: Option<f64>,
}

/// The `limit` object — singular, upstream's spelling.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireLimit {
    /// Prompt and reply together.
    context: Option<f64>,
    /// Prompt alone, where it is capped apart.
    input: Option<f64>,
    /// One reply.
    output: Option<f64>,
}

impl Wire {
    /// Turns a published row into a table row, or [`None`] when it carries no
    /// usable sizes.
    fn into_info(
        self,
        provider_id: &str,
        model_id: &str,
        provider_npm: Option<&str>,
    ) -> Option<ModelInfo> {
        let limit = self.limit.unwrap_or_default();
        let context_window = tokens(limit.context)?;
        let max_output = tokens(limit.output)?;
        let cost = self.cost.unwrap_or_default();

        let mut info = ModelInfo {
            id: model_id.to_owned(),
            provider_id: provider_id.to_owned(),
            name: self.name.unwrap_or_else(|| model_id.to_owned()),
            context_window,
            max_output,
            input_limit: tokens(limit.input),
            pricing: Pricing {
                input: cost.input.unwrap_or_default(),
                output: cost.output.unwrap_or_default(),
                cache_read: cost.cache_read.unwrap_or_default(),
                cache_write: cost.cache_write,
            },
            family: self.family,
            release_date: self.release_date,
            tool_call: self.tool_call.unwrap_or(true),
            status: status(self.status.as_deref()),
            reasoning: self.reasoning.unwrap_or_default(),
            reasoning_options: self.reasoning_options,
            // Upstream's `??`, resolved here: the row's own transport, else the
            // one its provider declared for every row it did not override.
            npm: self
                .provider
                .and_then(|transport| transport.npm)
                .or_else(|| provider_npm.map(str::to_owned)),
            // Held as what the catalog declared just long enough for the
            // synthesis to read the whole row, then replaced with the merged
            // roster — capability data first, the declaration winning on top.
            variants: self.variants.unwrap_or_default(),
        };
        info.variants = crate::effort::roster(&info);

        Some(info)
    }
}

/// A published token count, as a count.
///
/// The catalog publishes these as JSON numbers, which are not integers; a
/// value that is not a whole positive count of tokens is no count at all.
fn tokens(value: Option<f64>) -> Option<u64> {
    let value = value?;

    (value.is_finite() && value >= 1.0).then_some(value as u64)
}

/// Reads a published status, defaulting anything unrecognized to active.
///
/// A status this build has not heard of is a catalog that grew a word, not a
/// model to hide: upstream branches on the two it acts on and ignores the
/// rest.
fn status(published: Option<&str>) -> ModelStatus {
    match published {
        Some("alpha") => ModelStatus::Alpha,
        Some("beta") => ModelStatus::Beta,
        Some("deprecated") => ModelStatus::Deprecated,
        _ => ModelStatus::Active,
    }
}

/// Gets `${base}/api.json`, retrying a transport failure or a refusal.
///
/// Redirects are followed. The provider client refuses them because every
/// provider request carries an API key in a header; this request carries no
/// credential at all, so a 3xx costs nothing to follow — the same reasoning
/// [`webfetch`](crate::tool) is built on.
async fn fetch(base: &str) -> Result<String, CatalogError> {
    let client = reqwest::Client::builder()
        // One deadline over connect, headers and body, so an endpoint that
        // answers instantly and then dribbles the body forever is bounded.
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| CatalogError::Request(error.to_string()))?;
    let url = format!("{}/api.json", base.trim_end_matches('/'));

    let mut attempt = 0;
    loop {
        let outcome =
            match client.get(&url).header(reqwest::header::USER_AGENT, USER_AGENT).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        response
                            .text()
                            .await
                            .map_err(|error| CatalogError::Request(error.to_string()))
                    } else {
                        Err(CatalogError::Status { status: status.as_u16() })
                    }
                }
                Err(error) => Err(CatalogError::Request(error.to_string())),
            };

        match outcome {
            Ok(body) => return Ok(body),
            Err(error) if attempt == RETRIES => return Err(error),
            Err(error) => {
                tracing::debug!(%error, attempt, "the catalog request failed; retrying");
                tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        }
    }
}

/// How long to wait before attempt `attempt + 1`.
///
/// Exponential from [`RETRY_BASE`], scattered across half to one and a half of
/// it. All the scatter has to do is keep two processes that started together
/// from staying in step — which is exactly why it may not be read off the
/// clock, the one thing two such processes agree about.
///
/// `backon`'s ladder was weighed here and declined: its jitter is additive
/// over `(0, delay)`, an effective one to two times the wait, where this one
/// multiplies around one. Taking it would move pacing this module's own test
/// pins, and that is a change to make deliberately or not at all.
fn backoff(attempt: u32) -> Duration {
    scattered(attempt, crate::jitter::draw())
}

/// [`backoff`] over a draw the caller holds, so the window can be walked to
/// its edges instead of sampled.
fn scattered(attempt: u32, entropy: u64) -> Duration {
    let factor = 0.5 + (entropy % 1_000_000) as f64 / 1_000_000.0;

    (RETRY_BASE * 2_u32.pow(attempt)).mul_f64(factor)
}

/// Writes `body` at `path`, verbatim, through a sibling that is renamed over
/// it.
///
/// Verbatim because what is cached is the bytes that arrived, not this build's
/// reading of them: a later version that understands more of the payload finds
/// the whole payload waiting. Through a sibling because a reader arriving mid
/// write must find either the old file or the new one, never half of either.
///
/// A write that fails at either step takes the sibling with it, so a failure
/// cannot leave a file nobody will ever read.
fn write_cache(path: &Path, body: &str) -> Result<(), CatalogError> {
    let cache =
        |path: &Path, source: io::Error| CatalogError::Cache { path: path.to_path_buf(), source };
    let parent = path.parent().ok_or_else(|| {
        cache(
            path,
            io::Error::new(
                io::ErrorKind::NotFound,
                "the cache file has no directory to be created in",
            ),
        )
    })?;
    fs::create_dir_all(parent).map_err(|source| cache(parent, source))?;

    let temporary = atomic::temporary_beside(path);
    atomic::write_new(&temporary, body.as_bytes()).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        cache(&temporary, source)
    })?;

    fs::rename(&temporary, path).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        cache(path, source)
    })
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;

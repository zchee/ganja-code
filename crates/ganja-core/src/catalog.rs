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

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, PoisonError, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{protocol::Usage, storage};

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
/// channel and one client here, so the version alone says as much.
const USER_AGENT: &str = concat!("ganja/", env!("CARGO_PKG_VERSION"));

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
    pub family: Option<String>,
    /// The day it was published, as the catalog spells it.
    pub release_date: Option<String>,
    /// Whether it can be given tools at all.
    pub tool_call: bool,
    /// How far along its life the catalog says it is.
    pub status: ModelStatus,
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
const DEFAULTS: &[(&str, &str)] = &[("anthropic", "claude-sonnet-5"), ("openai", "gpt-5.6")];

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
        pricing: Pricing {
            input: 2.0,
            output: 10.0,
            cache_read: 0.2,
            cache_write: Some(2.5),
        },
    },
    Row {
        id: "claude-opus-5",
        provider_id: "anthropic",
        name: "Claude Opus 5",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    Row {
        id: "claude-opus-4-8",
        provider_id: "anthropic",
        name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    Row {
        id: "claude-sonnet-4-6",
        provider_id: "anthropic",
        name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: Some(3.75),
        },
    },
    Row {
        id: "claude-haiku-4-5",
        provider_id: "anthropic",
        name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output: 64_000,
        pricing: Pricing {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: Some(1.25),
        },
    },
    Row {
        id: "gpt-5.6",
        provider_id: "openai",
        name: "GPT-5.6",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 30.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    Row {
        id: "gpt-5.4",
        provider_id: "openai",
        name: "GPT-5.4",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 2.5,
            output: 15.0,
            cache_read: 0.25,
            cache_write: None,
        },
    },
    Row {
        id: "gpt-5.4-mini",
        provider_id: "openai",
        name: "GPT-5.4 mini",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 0.75,
            output: 4.5,
            cache_read: 0.075,
            cache_write: None,
        },
    },
    Row {
        id: "gpt-5.4-nano",
        provider_id: "openai",
        name: "GPT-5.4 nano",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 0.2,
            output: 1.25,
            cache_read: 0.02,
            cache_write: None,
        },
    },
    Row {
        id: "gpt-5.3-codex",
        provider_id: "openai",
        name: "GPT-5.3 Codex",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 1.75,
            output: 14.0,
            cache_read: 0.175,
            cache_write: None,
        },
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
    current()
        .models
        .iter()
        .find(|model| model.id == id)
        .cloned()
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

/// The model `provider_id` is asked for when the user names none.
///
/// [`None`] for a provider this build does not pin a default for: answering
/// with some other provider's model would be a silent misconfiguration rather
/// than a default. The pins are compiled in and a refresh does not move them,
/// because the published catalog does not carry the concept.
#[must_use]
pub fn default_model(provider_id: &str) -> Option<&'static str> {
    DEFAULTS
        .iter()
        .find(|(provider, _)| *provider == provider_id)
        .map(|(_, model)| *model)
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
        + priced(
            pricing.cache_write.unwrap_or(pricing.input),
            usage.cache_write_tokens,
        );
    let output_usd = priced(pricing.output, usage.output_tokens);

    Cost {
        input_usd,
        output_usd,
        total_usd: input_usd + output_usd,
    }
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

    Ok(Source {
        url,
        cache,
        read,
        overridden,
    })
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
fn parse(body: &str) -> Result<Catalog, CatalogError> {
    let root: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(body).map_err(|error| CatalogError::Parse(error.to_string()))?;

    let mut models = Vec::new();
    for (provider_id, provider) in root {
        let Some(published) = provider
            .get("models")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };

        for (model_id, published) in published {
            let Ok(wire) = serde_json::from_value::<Wire>(published.clone()) else {
                tracing::debug!(provider_id, model_id, "a catalog row was not readable");
                continue;
            };
            if let Some(info) = wire.into_info(&provider_id, model_id) {
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
    fn into_info(self, provider_id: &str, model_id: &str) -> Option<ModelInfo> {
        let limit = self.limit.unwrap_or_default();
        let context_window = tokens(limit.context)?;
        let max_output = tokens(limit.output)?;
        let cost = self.cost.unwrap_or_default();

        Some(ModelInfo {
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
        })
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
        let outcome = match client
            .get(&url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    response
                        .text()
                        .await
                        .map_err(|error| CatalogError::Request(error.to_string()))
                } else {
                    Err(CatalogError::Status {
                        status: status.as_u16(),
                    })
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
/// it. The scatter is read off the clock rather than from a random-number
/// generator: all it has to do is keep two processes that started together
/// from staying in step, and that is not worth a dependency.
fn backoff(attempt: u32) -> Duration {
    (RETRY_BASE * 2_u32.pow(attempt)).mul_f64(jitter())
}

/// A factor in `0.5..1.5`.
fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());

    0.5 + f64::from(nanos % 1_000_000) / 1_000_000.0
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
    let cache = |path: &Path, source: io::Error| CatalogError::Cache {
        path: path.to_path_buf(),
        source,
    };
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

    let temporary = storage::temporary_beside(path);
    storage::write_new(&temporary, body.as_bytes()).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        cache(&temporary, source)
    })?;

    fs::rename(&temporary, path).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        cache(path, source)
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, time::Duration};

    use super::{
        Cost, DEFAULT_SOURCE, ModelStatus, Pricing, Source, backoff, cache_name, compact_tokens,
        cost, default_model, fresh, model, parse, read_cached, snapshot, write_cache,
    };
    use crate::protocol::Usage;

    /// Dollar amounts are compared with a tolerance because the arithmetic is
    /// binary floating point; a tenth of a cent is far below anything shown.
    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    /// A payload in the shape the catalog publishes: providers keyed by id,
    /// each holding models keyed by id.
    fn payload() -> String {
        r#"{
          "fixture": {
            "id": "fixture",
            "name": "Fixture Inc",
            "env": ["FIXTURE_API_KEY"],
            "npm": "@fixture/sdk",
            "models": {
              "fixture-large": {
                "id": "fixture-large",
                "name": "Fixture Large",
                "family": "fixture",
                "release_date": "2026-01-31",
                "attachment": true,
                "reasoning": true,
                "temperature": false,
                "tool_call": true,
                "status": "beta",
                "modalities": { "input": ["text", "image"], "output": ["text"] },
                "reasoning_options": [{ "type": "toggle" }],
                "a_field_this_build_has_never_heard_of": { "nested": [1, 2, 3] },
                "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.4, "cache_write": 5.0 },
                "limit": { "context": 500000, "input": 400000, "output": 32000 }
              },
              "fixture-small": {
                "id": "fixture-small",
                "limit": { "context": 128000, "output": 8000 }
              },
              "fixture-unsized": {
                "id": "fixture-unsized",
                "cost": { "input": 1.0, "output": 2.0 }
              }
            }
          }
        }"#
        .to_owned()
    }

    #[test]
    fn every_row_is_priced_and_sized() {
        let snapshot = snapshot();
        assert!(!snapshot.models.is_empty());

        for model in &snapshot.models {
            assert!(!model.id.is_empty(), "{model:?}");
            assert!(!model.name.is_empty(), "{model:?}");
            assert!(model.context_window > 0, "{model:?}");
            assert!(
                model.max_output > 0 && model.max_output <= model.context_window,
                "a reply cannot exceed the window: {model:?}"
            );
            assert!(model.pricing.input > 0.0, "{model:?}");
            assert!(
                model.pricing.output >= model.pricing.input,
                "output has never been cheaper than input: {model:?}"
            );
            assert!(
                model.pricing.cache_read < model.pricing.input,
                "a cache read is the discount: {model:?}"
            );
        }
    }

    #[test]
    fn ids_are_unique_so_a_lookup_cannot_be_ambiguous() {
        let snapshot = snapshot();
        let mut ids: Vec<&str> = snapshot
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), total, "duplicate model id in the table");
    }

    #[test]
    fn a_known_model_resolves_and_an_unknown_one_does_not() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

        assert_eq!(sonnet.provider_id, "anthropic");
        assert_eq!(sonnet.context_window, 1_000_000);
        assert!(model("claude-sonnet-3-nonexistent").is_none());
        // The fake provider's model is deliberately absent: nothing canned has
        // a price.
        assert!(model("canned").is_none());
    }

    /// Every provider a session can select has to have a default here, because
    /// this table is what resolves the model when the user names none — a
    /// provider missing from it fails at startup with `NoDefaultModel`. The
    /// list is derived from `provider::PROVIDERS` rather than written out
    /// again: two hand-maintained lists in different modules is precisely how
    /// a provider gets added on one side and forgotten on the other.
    #[test]
    fn every_selectable_provider_has_a_default_this_table_can_price() {
        for provider in crate::provider::PROVIDERS {
            // The fake provider carries its own canned model and is
            // deliberately unpriced; nothing about it is billable.
            if provider == crate::provider::fake::ID {
                continue;
            }

            let id = default_model(provider)
                .unwrap_or_else(|| panic!("{provider} is selectable but has no default model"));
            let info = model(id)
                .unwrap_or_else(|| panic!("{provider}'s default {id} is not in the table"));

            assert_eq!(info.provider_id, provider, "{id} is not {provider}'s");
        }

        assert!(default_model("nonexistent").is_none());
    }

    #[test]
    fn a_turn_with_cache_traffic_prices_every_counter() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            reasoning_tokens: 500_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        };

        let Cost {
            input_usd,
            output_usd,
            total_usd,
        } = cost(&usage, &sonnet);

        // 1 MTok fresh at $2 + 2 MTok cached at $0.20 + 1 MTok written at $2.50.
        assert!(close(input_usd, 2.0 + 0.4 + 2.5), "got {input_usd}");
        // Output is $10/MTok and the reasoning tokens are part of it, not extra.
        assert!(close(output_usd, 10.0), "got {output_usd}");
        assert!(close(total_usd, input_usd + output_usd));
    }

    /// A provider that does not bill cache writes apart still has them priced,
    /// at the plain input rate, rather than silently free.
    #[test]
    fn a_cache_write_without_its_own_price_bills_as_input() {
        let nano = model("gpt-5.4-nano").expect("the snapshot carries nano");
        assert!(nano.pricing.cache_write.is_none());

        let usage = Usage {
            cache_write_tokens: 1_000_000,
            ..Usage::default()
        };

        assert!(close(cost(&usage, &nano).input_usd, nano.pricing.input));
    }

    #[test]
    fn a_token_count_stays_readable_at_every_magnitude() {
        let cases = [
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1_000, "1.0k"),
            (12_345, "12.3k"),
            (999_949, "999.9k"),
            // The boundary rounding would otherwise print as "1000.0k".
            (999_950, "1.0M"),
            (1_000_000, "1.0M"),
            (1_050_000, "1.1M"),
        ];

        for (tokens, expected) in cases {
            assert_eq!(compact_tokens(tokens), expected, "for {tokens}");
        }
    }

    #[test]
    fn an_empty_turn_costs_nothing() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

        assert_eq!(cost(&Usage::default(), &sonnet), Cost::default());
    }

    /// Rounding to the four decimals the status bar shows must not swallow a
    /// short turn: a thousand-token exchange still registers.
    #[test]
    fn a_short_turn_is_still_worth_a_visible_amount() {
        let opus = model("claude-opus-5").expect("the snapshot carries opus");
        let usage = Usage {
            input_tokens: 12_000,
            output_tokens: 800,
            ..Usage::default()
        };

        let total = cost(&usage, &opus).total_usd;

        assert!(close(total, 0.06 + 0.02), "got {total}");
    }

    /// The published payload carries a great deal this build does not read,
    /// and leaves out a great deal it does — neither is a reason to refuse it.
    #[test]
    fn a_published_row_parses_through_fields_this_build_does_not_know() {
        let catalog = parse(&payload()).expect("the fixture is a catalog");

        let large = catalog
            .models
            .iter()
            .find(|model| model.id == "fixture-large")
            .expect("the fixture's large model is in the table");

        assert_eq!(
            large.provider_id, "fixture",
            "the provider is the outer key"
        );
        assert_eq!(large.name, "Fixture Large");
        assert_eq!(large.context_window, 500_000);
        assert_eq!(large.max_output, 32_000);
        assert_eq!(large.input_limit, Some(400_000));
        assert_eq!(large.family.as_deref(), Some("fixture"));
        assert_eq!(large.release_date.as_deref(), Some("2026-01-31"));
        assert_eq!(large.status, ModelStatus::Beta);
        assert!(large.tool_call);
        assert!(close(large.pricing.input, 4.0));
        assert_eq!(large.pricing.cache_write, Some(5.0));

        let small = catalog
            .models
            .iter()
            .find(|model| model.id == "fixture-small")
            .expect("a row carrying only its limits is still a row");

        assert_eq!(small.name, "fixture-small", "an unnamed model is its id");
        assert!(close(small.pricing.input, 0.0), "an unpriced model is free");
        assert_eq!(small.pricing.cache_write, None);
        assert_eq!(small.input_limit, None);
        assert_eq!(small.status, ModelStatus::Active, "absent means active");
        assert!(small.tool_call, "absent means it takes tools");
    }

    /// A row that does not say what it holds cannot size a session, and a zero
    /// window would have every turn compacting against nothing.
    #[test]
    fn a_row_that_names_no_limits_is_left_out() {
        let catalog = parse(&payload()).expect("the fixture is a catalog");

        assert!(
            !catalog
                .models
                .iter()
                .any(|model| model.id == "fixture-unsized"),
            "an unsized row must not reach the table"
        );
    }

    #[test]
    fn a_payload_that_holds_no_usable_row_is_not_a_catalog() {
        for body in [
            "{}",
            r#"{"fixture": {"models": {}}}"#,
            r#"{"fixture": {"models": {"m": {"limit": {"context": 0, "output": 0}}}}}"#,
            "not json at all",
        ] {
            assert!(parse(body).is_err(), "{body} should not be a catalog");
        }
    }

    /// One drifted row is not a reason to throw away every other row that came
    /// with it.
    #[test]
    fn a_row_whose_shape_drifted_is_skipped_rather_than_fatal() {
        let body = r#"{
          "fixture": {
            "models": {
              "drifted": { "limit": "as much as you like" },
              "sound": { "limit": { "context": 1000, "output": 100 } }
            }
          },
          "shapeless": { "models": [] }
        }"#;

        let catalog = parse(body).expect("the sound row still makes a catalog");

        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "sound");
    }

    #[test]
    fn a_source_other_than_the_default_gets_its_own_cache_file() {
        assert_eq!(cache_name(DEFAULT_SOURCE), "models.json");

        let mirror = cache_name("https://models.example.test");
        let other = cache_name("https://models.example.test/v2");

        assert!(
            mirror.starts_with("models-") && mirror.ends_with(".json"),
            "{mirror}"
        );
        assert_ne!(mirror, other, "two mirrors cannot share one cache file");
        assert_eq!(
            mirror,
            cache_name("https://models.example.test"),
            "the same URL names the same file every run"
        );
    }

    #[test]
    fn a_cached_body_is_written_verbatim_and_read_back() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("nested").join("models.json");
        let body = payload();

        write_cache(&path, &body).expect("the cache is writable");

        assert_eq!(
            fs::read_to_string(&path).expect("the cache is readable"),
            body,
            "what is cached is the bytes that arrived"
        );

        let source = Source {
            url: DEFAULT_SOURCE.to_owned(),
            cache: path.clone(),
            read: path,
            overridden: false,
        };
        let catalog = read_cached(&source).expect("the cache holds a catalog");

        assert_eq!(catalog.models.len(), 2);
    }

    /// A cache that cannot be read is the one thing a stale-tolerant reader
    /// must not keep serving, and the one thing it can safely remove.
    #[test]
    fn an_unreadable_cache_is_deleted_and_read_as_absent() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("models.json");
        fs::write(&path, "{\"truncated\": ").expect("the fixture is writable");

        let source = Source {
            url: DEFAULT_SOURCE.to_owned(),
            cache: path.clone(),
            read: path.clone(),
            overridden: false,
        };

        assert!(read_cached(&source).is_none());
        assert!(!path.exists(), "a cache this build wrote is its to remove");
    }

    #[test]
    fn a_cache_the_environment_named_is_read_as_absent_but_never_deleted() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let named = directory.path().join("somebody-elses.json");
        let cache = directory.path().join("models.json");
        fs::write(&named, "{\"truncated\": ").expect("the fixture is writable");
        fs::write(&cache, payload()).expect("the fixture is writable");

        let source = Source {
            url: DEFAULT_SOURCE.to_owned(),
            cache: cache.clone(),
            read: named.clone(),
            overridden: true,
        };

        assert!(read_cached(&source).is_none());
        assert!(
            named.exists(),
            "an overridden path is not this build's file"
        );
        assert!(cache.exists(), "and neither is the cache it stood in for");
    }

    /// The freshness gate is what keeps a refresh from re-fetching a catalog
    /// somebody else just wrote, and what stops it from skipping one that has
    /// aged out.
    #[test]
    fn a_cache_is_fresh_until_the_debounce_has_passed() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("models.json");

        assert!(!fresh(&path), "a cache that does not exist is not fresh");

        fs::write(&path, payload()).expect("the fixture is writable");
        assert!(fresh(&path), "a cache written just now is fresh");

        let file = fs::File::options()
            .write(true)
            .open(&path)
            .expect("the fixture is openable");
        file.set_modified(std::time::SystemTime::now() - super::DEBOUNCE - Duration::from_secs(1))
            .expect("the fixture's timestamp is settable");

        assert!(!fresh(&path), "a cache older than the debounce is not");
    }

    /// A rename onto a non-empty directory cannot succeed, which is the
    /// cheapest way to fail the last step of a write and see what it leaves
    /// behind.
    #[test]
    fn a_cache_write_that_fails_leaves_no_temporary_behind() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("models.json");
        fs::create_dir_all(path.join("occupied")).expect("the obstruction is creatable");

        write_cache(&path, &payload()).expect_err("a rename onto a directory cannot succeed");

        let strays: Vec<_> = fs::read_dir(directory.path())
            .expect("the directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();

        assert!(strays.is_empty(), "a failed write left {strays:?} behind");
    }

    #[test]
    fn the_backoff_grows_and_never_lands_on_the_same_edge() {
        for attempt in 0..3 {
            let waited = backoff(attempt);
            let base = super::RETRY_BASE * 2_u32.pow(attempt);

            assert!(
                waited >= base.mul_f64(0.5) && waited <= base.mul_f64(1.5),
                "attempt {attempt} waited {waited:?}, off the jitter window around {base:?}"
            );
        }

        assert!(
            backoff(2) > backoff(0).mul_f64(1.5),
            "the last wait is longer than the first can ever be"
        );
    }

    /// Nothing in the table is handed out as a borrow of the tier it came
    /// from: a refresh replaces the whole table, and a caller still holding a
    /// row from before it must keep reading the row it asked for. A `&'static`
    /// row could not have survived the tier being dropped, and a copied one
    /// would not have been the same row.
    #[test]
    fn a_row_outlives_the_table_it_came_from() {
        let catalog = parse(&payload()).expect("the fixture is a catalog");
        let held = Arc::clone(&catalog.models[0]);
        let rows = Arc::strong_count(&held);

        drop(catalog);

        assert_eq!(Arc::strong_count(&held), rows - 1, "the table let go of it");
        assert_eq!(held.id, "fixture-large");
        assert_eq!(held.context_window, 500_000);
    }

    #[test]
    fn a_status_this_build_does_not_know_reads_as_active() {
        assert_eq!(super::status(None), ModelStatus::Active);
        assert_eq!(super::status(Some("alpha")), ModelStatus::Alpha);
        assert_eq!(super::status(Some("beta")), ModelStatus::Beta);
        assert_eq!(super::status(Some("deprecated")), ModelStatus::Deprecated);
        assert_eq!(super::status(Some("retired")), ModelStatus::Active);
    }

    /// The snapshot is what makes the table unconditional; it must be a table
    /// on its own, with no filesystem and no network anywhere near it.
    #[test]
    fn the_snapshot_stands_alone() {
        let snapshot = snapshot();

        assert_eq!(snapshot.models.len(), super::SNAPSHOT.len());
        assert!(
            snapshot
                .models
                .iter()
                .all(|model| model.status == ModelStatus::Active && model.tool_call),
            "every compiled-in row is a current tool-using model"
        );
        assert_eq!(
            Pricing {
                input: 2.0,
                output: 10.0,
                cache_read: 0.2,
                cache_write: Some(2.5),
            },
            snapshot.models[0].pricing
        );
    }
}

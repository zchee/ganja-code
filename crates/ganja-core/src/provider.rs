//! Which provider a session runs as, and which model it asks.
//!
//! The wires themselves live in [`ganja_provider`] — every byte of HTTP, every
//! credential, and the catalog that sizes and prices what they serve. This
//! module is the half that stayed, and the line between them is a single
//! type: [`Config`]. `select` is a chain of tiers with the config file as one
//! of them, so it belongs where the config is; everything below it takes plain
//! data, so the wires could leave without it.
//!
//! The whole of `ganja_provider::provider` is re-exported here, so
//! `ganja_core::provider::AnthropicProvider` and every other path a caller
//! already writes still means what it meant. That is the same facade the
//! protocol, permission and tool crates are reached through — see the crate
//! root — and it is why the split cost no caller a rewrite. New code wanting
//! only a wire should depend on `ganja-provider` directly.

use std::{
    env::{self, VarError},
    fmt,
    sync::Arc,
};

// The facade, and the only import this half needs of the other: a glob rather
// than a list because the promise is that every path a caller already writes
// still resolves, and a list is a promise that decays the next time a wire
// grows a type.
pub use ganja_provider::provider::*;

use crate::{
    auth, catalog,
    config::{Config, ProviderConfig, split_model},
};

/// A provider together with the model to ask, and anything the user should be
/// told about how the two were picked.
pub struct Selection {
    /// The provider to drive the session with.
    pub provider: Arc<dyn Provider>,
    /// Model identifier handed to every [`ChatRequest`].
    pub model: String,
    /// Set only when the session fell back to the built-in fake provider — a
    /// real provider defaulted from a stored login is a session running as
    /// somebody the user logged in as, which is not a degradation to warn
    /// about.
    pub notice: Option<String>,
}

impl fmt::Debug for Selection {
    /// Renders what was chosen, never how it authenticates: [`Provider`] has no
    /// way to hand a credential back, so there is nothing here to leak.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Selection")
            .field("provider", &self.provider.id())
            .field("model", &self.model)
            .field("notice", &self.notice)
            .finish()
    }
}

/// The environment does not describe a session this build can run.
#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    /// A tier names a provider that is neither shipped nor configured.
    ///
    /// The message names **both** tiers of what would have been accepted. A
    /// refusal that listed only [`PROVIDERS`] would tell somebody who had just
    /// declared an endpoint in `ganja.jsonc` that their own entry does not
    /// exist, which is the one answer that cannot be acted on. And it names
    /// which tier asked: four of them can put an id here, and "unset the
    /// variable" is no repair for a key sitting in a config file.
    #[error(
        "{named_by} names the unsupported provider {requested:?}; this build ships {}{}",
        PROVIDERS.join(", "),
        also_configured(configured)
    )]
    Unknown {
        /// What the tier said.
        requested: String,
        /// Which tier said it: the flag, [`PROVIDER_ENV`], or one of the two
        /// config keys that can name a provider. Not spelled `source`, which
        /// `thiserror` would read as this error's cause.
        named_by: &'static str,
        /// The ids this config's `provider` table declares, in the order the
        /// table holds them. Empty for a session with no such table, which is
        /// every session that has not asked for one.
        configured: Vec<String>,
    },
    /// The provider was named but cannot be talked to.
    #[error(transparent)]
    Unusable(#[from] ProviderError),
    /// Nothing named a model, and the catalog has no default to supply one.
    ///
    /// For a builtin that is a gap in the table. For a configured endpoint it
    /// is the ordinary case — no published catalog knows a private endpoint —
    /// so the message names every tier that could answer rather than only the
    /// variable.
    #[error(
        "no default model for {provider}; name one with {MODEL_ENV}, with --model, \
         or with the config's `model` key"
    )]
    NoDefaultModel {
        /// Provider the catalog has no default for.
        provider: String,
    },
}

/// The clause a refusal adds when this config declares endpoints of its own.
///
/// Empty where it declares none, so the common message is the one it always
/// was rather than one carrying an empty list.
fn also_configured(configured: &[String]) -> String {
    if configured.is_empty() {
        return String::new();
    }

    format!(", and this config names {}", configured.join(", "))
}

/// Whether a session may run as `provider_id` at all — the first of the two
/// tiers [`PROVIDERS`] describes.
///
/// A builtin, or an id this config's `provider` table declares. The second
/// tier — whether the catalog has rows to size and price it with — is
/// [`catalog::carries`], and the two are deliberately separate: every
/// configured endpoint is selectable and none of them is cataloged, and so is
/// a builtin whose wire ships before its rows do.
#[must_use]
pub fn selectable(config: &Config, provider_id: &str) -> bool {
    PROVIDERS.contains(&provider_id) || config.provider.contains_key(provider_id)
}

/// A model a wire says the current credential may name, for surfaces that
/// list rather than price. [`None`] from [`wire_model_listing`] means the
/// catalog is this provider's source of truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedModel {
    /// The id a request asks for — what `--model` and the chooser hand on.
    pub id: String,
    /// The human name beside it; the id again where the wire carried none.
    pub name: String,
}

/// The live roster for a provider whose *wire*, not the catalog, knows what
/// the stored credential may name — today exactly cursor, whose
/// `GetUsableModels` listing is the only source its uncataloged tier has.
/// Everyone else answers [`None`], and the catalog keeps describing them.
///
/// Live on every call, deliberately: upstream's cursor plugin caches its
/// discovery per process, but both callers here already hold the answer the
/// way they need it — the TUI caches for the App's lifetime and the CLI is
/// one-shot — so a cache at this seam would only be a second staleness to
/// reason about (deviation: cursor-model-listing-uncached-at-the-seam).
///
/// # Errors
///
/// The inner [`ProviderError`] is the wire's own: `Auth` naming
/// `ganja auth login cursor` when no login is stored — the store is read
/// before anything is dialled — and the transport, status and parse classes
/// after that.
pub async fn wire_model_listing(
    provider_id: &str,
) -> Option<Result<Vec<ListedModel>, ProviderError>> {
    if provider_id != cursor::ID {
        return None;
    }

    Some(cursor_models().await)
}

/// The cursor half of [`wire_model_listing`]: the stored login, one listing
/// call, and the entries in the order the server sent them.
async fn cursor_models() -> Result<Vec<ListedModel>, ProviderError> {
    let wire = cursor::CursorWire::from_stored()?;
    let entries = wire.usable_models().await?;

    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            // An entry with no id is nothing a request could ask for. The
            // name is `display_name` — proto field 4, the human string —
            // never `display_model_id`, which is the server-side id again.
            let id = entry.model_id.filter(|id| !id.is_empty())?;
            let name = entry
                .display_name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| id.clone());

            Some(ListedModel { id, name })
        })
        .collect())
}

/// A resolved provider, together with the model to ask when nothing named one.
///
/// The second half is not always [`catalog::default_model`]'s answer, and the
/// exception is the reason this type exists rather than an `Arc<dyn Provider>`.
/// The catalog holds one default per *vendor*, which is the right shape for
/// every vendor here but one: OpenAI serves two backends with different
/// offerings, and a ChatGPT seat handed the vendor-wide default gets a model
/// its backend refuses outright. Making the wire hand back its own default
/// keeps that coupling where the wire was chosen — in [`openai_provider`] — and
/// out of [`select`], which knows only a provider id.
struct Wire {
    /// The provider to drive the session with.
    provider: Arc<dyn Provider>,
    /// The default this wire wants, where it is not the catalog's. [`None`]
    /// means "ask the catalog", which is every wire but the ChatGPT one.
    default_model: Option<&'static str>,
}

impl Wire {
    /// A provider whose default is the catalog's — every provider but the
    /// ChatGPT subscription backend.
    fn catalog(provider: impl Provider + 'static) -> Self {
        Self {
            provider: Arc::new(provider),
            default_model: None,
        }
    }
}

/// Which OpenAI backend a session reaches, decided by the credential it has.
///
/// **One wire, two backends.** Upstream sends every OpenAI model through the
/// Responses API with no reference to the credential at all — the plugin's
/// whole language hook is `evt.language = evt.sdk.responses(evt.model.api.id)`
/// (`plugin/provider/openai.ts:185`) — so the vendor picks the wire and this
/// function only picks where the request goes and what it carries. That is
/// `codex.ts`'s own split: `:356` hands a request whose credential is not OAuth
/// to the unwrapped `fetch`, keeping the platform URL and adding none of the
/// subscription headers, and `:281` returns the model list unfiltered under the
/// same condition.
///
/// - **A key** — exported, or stored, in exactly the order [`key_for`] has
///   always read them — reaches `api.openai.com` with a bearer and nothing
///   else, and is held to no seat's allow-list. This is what makes the newest
///   models usable: chat completions refuses tools on them and the Responses
///   endpoint is what the refusal itself named.
/// - **No key but a stored ChatGPT login** reaches the codex backend that
///   credential was minted for, with the three headers and the allow-list.
///   Only the login's *presence* is read here; the token is resolved per
///   request, so this costs one small file and captures nothing. Its default
///   model comes from the allow-list rather than the catalog — see [`Wire`].
/// - **Neither** is the startup failure it has always been, naming the variable
///   and the login — `require_key`'s message, reached by the same call.
///
/// A store that cannot be read is reported rather than treated as "no
/// credential": those are different situations needing different repairs, and
/// only the second can say what to fix.
fn openai_provider() -> Result<Wire, ProviderError> {
    if key_for(openai::ID)?.is_none() {
        let stored =
            auth::oauth_for(openai::ID).map_err(|error| ProviderError::Auth(error.to_string()))?;

        if stored.is_some() {
            return Ok(Wire {
                provider: Arc::new(ResponsesProvider::from_stored()?),
                default_model: Some(responses::SUBSCRIPTION_DEFAULT),
            });
        }
    }

    // A key, or neither — and the second is `require_key`'s error, unchanged,
    // because it is the same lookup it always was.
    Ok(Wire::catalog(ResponsesProvider::from_env()?))
}

/// The provider a config's `provider` entry describes.
///
/// The whole of the config→wire translation, kept here rather than in
/// [`compat`] because reading a [`Config`] is selection's half of the job:
/// what [`CompatProvider::new`] is handed is a base URL, a credential and a
/// header map, which is data any caller could produce — and now data produced
/// on the other side of a crate boundary, which is the same claim with the
/// compiler behind it.
///
/// # Errors
///
/// Returns [`ProviderError::Auth`] when nothing supplies the endpoint's
/// credential, and [`ProviderError::Transport`] when its `headers` are not
/// headers or its endpoint is somewhere a credential may not travel. All of
/// them fail at startup, where the message is readable.
fn configured_provider(id: &str, entry: &ProviderConfig) -> Result<CompatProvider, ProviderError> {
    let credential = CredentialSource::Key(configured_key(id, entry.key_env.as_deref())?);

    CompatProvider::new(
        id,
        entry.dialect,
        &entry.base_url,
        credential,
        configured_headers(id, &entry.headers)?,
    )
}

/// Resolves the provider and model a session runs on.
///
/// Six tiers, and the first one that says something wins each half of the
/// answer separately — a flag may name the model while the config names the
/// provider:
///
/// 1. `--model`, carried on [`Config::overrides`];
/// 2. [`PROVIDER_ENV`] and [`MODEL_ENV`], where an empty [`MODEL_ENV`] counts
///    as unset and an empty [`PROVIDER_ENV`] is a provider nothing ships;
/// 3. [`Config::model`], `"provider/model"`, split on its first slash;
/// 4. [`Config::default_provider`], which names only a provider and is held
///    to the same two-tier lookup as everything above it;
/// 5. the **oldest stored login** this build can run as — recorded when a
///    credential is stored, ordered by [`auth::stored_logins_oldest_first`] —
///    chosen without a notice, because a provider the user logged into is not
///    a degradation. Exported key variables deliberately do not count as
///    logins: an environment override is for one run, and making it steer the
///    default would have a borrowed shell borrow an identity;
/// 6. the built-in fake provider with a notice saying so, which is now only
///    the machine with no logins at all.
///
/// The model falls through its own tiers as before: the flag, [`MODEL_ENV`],
/// the config's `model` key, then the catalog's default for whichever
/// provider was chosen.
///
/// Whichever tier named the *provider*, the name is resolved against the
/// builtins first and [`Config::provider`] second, so every route into this
/// function reaches a configured endpoint the same way: `GANJA_PROVIDER=<id>`,
/// `--model <id>/<model>`, a config `model` of `"<id>/<model>"` and a config
/// `default_provider` of `"<id>"` are one lookup written four ways.
///
/// # Errors
///
/// Returns [`SelectionError`] when a provider is named that this build neither
/// ships nor finds in the config table, or one whose credentials are missing,
/// or one nothing could supply a model for — and when the login tier is
/// reached and the credential store cannot be read, which is reported rather
/// than treated as "no logins". All of them fail here, before the terminal is
/// put into raw mode, so that the message is readable.
pub fn select(config: &Config) -> Result<Selection, SelectionError> {
    let flag = config.overrides.model.as_deref().map(split_model);
    let file = config.model.as_deref().map(split_model);

    let environment = match env::var(PROVIDER_ENV) {
        // Not `setting`: an exported-but-empty `GANJA_PROVIDER` is a mistake
        // worth naming rather than a variable to look past, and it reaches the
        // "no such provider" refusal below saying exactly what it was set to.
        Ok(requested) => Some(requested),
        Err(VarError::NotUnicode(requested)) => {
            return Err(SelectionError::Unknown {
                requested: requested.to_string_lossy().into_owned(),
                named_by: PROVIDER_ENV,
                configured: config.provider.keys().cloned().collect(),
            });
        }
        Err(VarError::NotPresent) => None,
    };

    // Each half falls through the tiers on its own, the provider's half
    // carrying which tier named it so a refusal can say. A flag naming a bare
    // model leaves the provider to whatever named one next.
    let requested = flag
        .and_then(|(provider, _)| provider)
        .map(|provider| (provider.to_owned(), "--model"))
        .or_else(|| environment.map(|requested| (requested, PROVIDER_ENV)))
        .or_else(|| {
            file.and_then(|(provider, _)| provider)
                .map(|provider| (provider.to_owned(), "the config's `model` key"))
        })
        .or_else(|| {
            config
                .default_provider
                .clone()
                .map(|provider| (provider, "the config's `default_provider` key"))
        });
    let named_model = flag
        .map(|(_, model)| model.to_owned())
        .or_else(|| setting(MODEL_ENV))
        .or_else(|| file.map(|(_, model)| model.to_owned()));

    let (requested, named_by) = match requested {
        Some(named) => named,
        // Nothing named a provider: the oldest stored login, and only a
        // machine with no logins at all falls through to the fake provider —
        // which keeps its notice, because *it* is the degradation.
        None => match oldest_stored_login(config)? {
            Some(stored) => (stored, "the oldest stored login"),
            None => {
                return Ok(Selection {
                    provider: Arc::new(FakeProvider::default()),
                    model: named_model.unwrap_or_else(|| fake::MODEL.to_owned()),
                    notice: Some(format!(
                        "{PROVIDER_ENV} is unset - replying from the built-in {} provider",
                        fake::ID
                    )),
                });
            }
        },
    };

    let wire = match requested.as_str() {
        fake::ID => Wire::catalog(FakeProvider::default()),
        anthropic::ID => Wire::catalog(AnthropicProvider::from_env()?),
        openai::ID => openai_provider()?,
        grok::ID => Wire::catalog(GrokProvider::from_stored()?),
        // Selectable as a bare name: construction reads nothing — grok's
        // posture — and each request reads the stored login as it is needed,
        // refusing by naming `ganja auth login` when there is none.
        // Uncataloged on purpose, so a session must name its model like any
        // config-declared endpoint.
        cursor::ID => Wire::catalog(CursorProvider),
        // Grok's construction shape, and grok's posture with it: neither reads
        // a token here, so a session with no stored login is built and fails at
        // its first request, with the message that names the login. What
        // Copilot does read is which deployment its login was against, because
        // that decides the endpoint rather than the credential.
        copilot::ID => Wire::catalog(CopilotProvider::from_stored()?),
        // The config tier, consulted **after** the builtins so that a table
        // entry can never quietly replace a shipped wire — `config` refuses
        // an entry naming one by name for that reason, which is what keeps
        // this arm from being the place a shadowing is discovered.
        configured => match config.provider.get(configured) {
            Some(entry) => Wire::catalog(configured_provider(configured, entry)?),
            None => {
                return Err(SelectionError::Unknown {
                    requested,
                    named_by,
                    configured: config.provider.keys().cloned().collect(),
                });
            }
        },
    };

    // A model the tiers above *named* never reaches the defaulting at all: an
    // explicit choice is answered or refused, never substituted.
    let model = match named_model {
        Some(model) => model,
        None => defaulted_model(&requested, wire.default_model)?,
    };

    Ok(Selection {
        provider: wire.provider,
        model,
        notice: None,
    })
}

/// The provider a session defaults to when nothing named one: the oldest
/// stored login this build can run as, or [`None`] on a machine with none.
///
/// The store's failure is reported rather than read as "no logins": its own
/// errors say what repairs them — a `chmod`, a corrupt file's position — and
/// silently starting the fake provider over an exposed store would hide
/// exactly the thing the store refuses to hide.
fn oldest_stored_login(config: &Config) -> Result<Option<String>, SelectionError> {
    let stored = auth::stored_logins_oldest_first()
        .map_err(|error| SelectionError::Unusable(ProviderError::Auth(error.to_string())))?;

    Ok(adoptable_login(config, stored))
}

/// The first of `stored` — storage keys, oldest login first — that this
/// session could actually run as, in ganja's own vocabulary.
///
/// Split from [`oldest_stored_login`] so the rule is a thing a test can state
/// without a credential store. Three filters, each with its reason:
///
/// - the key is read back through [`auth::provider_id_for_storage_key`],
///   because the file stores upstream's names — a `grok` login sits under
///   `xai` — and everything from here on speaks ganja's;
/// - [`fake::ID`] never counts: a credential filed under that id is not a
///   login to anything, and a session on the fake provider must keep the
///   notice this tier exists to avoid;
/// - everything else must be [`selectable`] — an id opencode stored for a
///   provider this build has no wire for is a login, just not one this
///   session can use, and skipping it beats refusing to start over somebody
///   else's credential file.
///
/// A cursor login adopts like any other: the stub-era line that steered the
/// default tier away left with the stub, exactly as its comment promised.
/// A session adopted onto it then runs the catalog's cursor pin — `default`,
/// the server-side Auto id the wire's own listing publishes — because the
/// uncataloged tier withholds sizing and pricing, not a default the backend
/// itself names.
fn adoptable_login(config: &Config, stored: impl IntoIterator<Item = String>) -> Option<String> {
    stored
        .into_iter()
        .map(|key| auth::provider_id_for_storage_key(&key).to_owned())
        .find(|id| id != fake::ID && selectable(config, id))
}

/// The model a session asks for when no tier named one.
///
/// Three sources, most specific first, and the order is the whole content of
/// this function:
///
/// 1. the built-in fake provider's canned model, which is deliberately in no
///    catalog because nothing canned has a price;
/// 2. `wire`, the default the *backend just built* wants. Set only where a
///    provider id is not enough to decide — OpenAI's two backends serve
///    different sets, so a ChatGPT seat handed the vendor-wide row below gets a
///    model its backend refuses outright and a session that cannot take a turn;
/// 3. the catalog's per-provider default, which is every other case.
///
/// Split out of [`select`] so the precedence is a thing a test can state.
/// While a wire's default and the catalog's happen to name the same model, the
/// only way to tell the two apart is to hand this a value the catalog could not
/// have produced, which is exactly what its test does.
///
/// # Errors
///
/// Returns [`SelectionError::NoDefaultModel`] where the catalog is asked and
/// has nothing, which is a gap in that table rather than anything the user did.
fn defaulted_model(requested: &str, wire: Option<&'static str>) -> Result<String, SelectionError> {
    if requested == fake::ID {
        return Ok(fake::MODEL.to_owned());
    }
    if let Some(model) = wire {
        return Ok(model.to_owned());
    }

    Ok(catalog::default_model(requested)
        .ok_or_else(|| SelectionError::NoDefaultModel {
            provider: requested.to_owned(),
        })?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        Config, Dialect, PROVIDER_ENV, PROVIDERS, ProviderConfig, SelectionError, adoptable_login,
        cursor, defaulted_model, fake, grok, openai, select, selectable, wire_model_listing,
    };
    use crate::catalog;

    /// A config declaring one endpoint under `id`.
    fn declaring(id: &str) -> Config {
        let mut config = Config::default();
        config.provider.insert(
            id.to_owned(),
            ProviderConfig {
                dialect: Dialect::OpenaiChatCompletions,
                base_url: "http://127.0.0.1:11434/v1".to_owned(),
                key_env: None,
                headers: BTreeMap::new(),
            },
        );

        config
    }

    /// The two tiers, at the boundary that separates them. A session may run
    /// as anything in either, and the catalog knows only some of it — so
    /// neither predicate may be derived from the other.
    #[test]
    fn a_config_named_provider_is_selectable_and_a_builtin_is_not_always_cataloged() {
        let config = declaring("local-llama");

        assert!(selectable(&config, "local-llama"));
        assert!(
            !PROVIDERS.contains(&"local-llama"),
            "the config tier is what makes it selectable, not the shipped list"
        );
        assert!(
            !catalog::carries("local-llama"),
            "no published catalog knows a private endpoint"
        );

        for builtin in PROVIDERS {
            assert!(
                selectable(&config, builtin),
                "{builtin} ships, so it is selectable whatever a config says"
            );
        }
        // The tier boundary inside the builtins themselves: `fake` is
        // selectable and deliberately uncataloged, and `cursor` — the wire
        // that landed before its rows, exactly the shape this comment used to
        // predict — rides the same tier until the real wire brings its rows.
        assert!(!catalog::carries(fake::ID));
        assert!(!catalog::carries(cursor::ID));
        assert!(catalog::carries(openai::ID));

        assert!(!selectable(&config, "gemini"));
        assert!(!selectable(&Config::default(), "local-llama"));
    }

    /// A refusal that listed only the shipped providers would tell somebody
    /// who had just declared an endpoint that their own entry does not exist,
    /// which is the one answer that cannot be acted on — and one that did not
    /// say which tier asked would send them to unset a variable a config key
    /// set.
    #[test]
    fn the_refusal_for_an_unknown_provider_names_both_tiers_and_who_asked() {
        let named = SelectionError::Unknown {
            requested: "gemini".to_owned(),
            named_by: "the config's `default_provider` key",
            configured: vec!["local-llama".to_owned(), "gateway".to_owned()],
        };
        let rendered = named.to_string();

        assert!(rendered.contains("gemini"), "{rendered}");
        assert!(
            rendered.contains("default_provider"),
            "the tier that named the id is the thing to fix: {rendered}"
        );
        for builtin in PROVIDERS {
            assert!(
                rendered.contains(builtin),
                "{builtin} is missing: {rendered}"
            );
        }
        assert!(
            rendered.contains("local-llama") && rendered.contains("gateway"),
            "the config's own endpoints are as selectable as the builtins: {rendered}"
        );

        // A session with no such table gets the message it always had, rather
        // than one carrying an empty list.
        let bare = SelectionError::Unknown {
            requested: "gemini".to_owned(),
            named_by: PROVIDER_ENV,
            configured: Vec::new(),
        }
        .to_string();
        assert!(
            bare.contains(PROVIDER_ENV),
            "the environment tier is named as itself: {bare}"
        );
        assert!(
            !bare.contains("this config names"),
            "nothing was configured, so nothing should be listed: {bare}"
        );
    }

    /// The login tier's own rule, without a credential store: the oldest
    /// login this session can actually run as, in ganja's vocabulary.
    #[test]
    fn the_oldest_login_that_wins_is_the_oldest_one_this_session_can_run_as() {
        let stored = |keys: &[&str]| keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();

        // The file speaks upstream's names: an `xai` login is a grok session.
        assert_eq!(
            adoptable_login(&Config::default(), stored(&["xai", "anthropic"])).as_deref(),
            Some(grok::ID)
        );

        // A login this build has no wire for is skipped, not refused — the
        // file may be shared with opencode, whose logins are its own.
        assert_eq!(
            adoptable_login(&Config::default(), stored(&["gemini", "anthropic"])).as_deref(),
            Some("anthropic")
        );

        // Unless a config declares that very endpoint, which makes its stored
        // login as runnable as a builtin's.
        assert_eq!(
            adoptable_login(&declaring("gemini"), stored(&["gemini", "anthropic"])).as_deref(),
            Some("gemini")
        );

        // A credential filed under the fake id is not a login to anything,
        // and the fake fallback must keep its notice.
        assert_eq!(
            adoptable_login(&Config::default(), stored(&[fake::ID])),
            None
        );
        assert_eq!(adoptable_login(&Config::default(), stored(&[])), None);
    }

    /// The flip the stub-era filter's own comment promised: with the wire
    /// real, a cursor login adopts like any other stored login — seniority
    /// decides, in both directions, and a machine holding only a cursor
    /// login runs as it rather than as the noticed fake. The other side of
    /// the decision — naming cursor explicitly — is pinned unchanged below.
    #[test]
    fn a_cursor_login_adopts_like_any_other_stored_login() {
        let stored = |keys: &[&str]| keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();

        // Oldest wins when cursor is oldest…
        assert_eq!(
            adoptable_login(&Config::default(), stored(&["cursor", "anthropic"])).as_deref(),
            Some("cursor")
        );
        // …and does not win when it is not: adoption is the ordering's
        // verdict, never a preference for the newest wire.
        assert_eq!(
            adoptable_login(&Config::default(), stored(&["anthropic", "cursor"])).as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            adoptable_login(&Config::default(), stored(&["cursor"])).as_deref(),
            Some("cursor")
        );
        // Adopted or named, the id answers the same way everywhere else.
        assert!(selectable(&Config::default(), cursor::ID));
    }

    /// The other side of the same decision: naming cursor is answered, not
    /// filtered — selection builds the wire's identity without reading
    /// anything, and the named model travels through untouched. What the
    /// first request then meets — the stored login, or the refusal naming
    /// `ganja auth login` — is drilled in `ganja-provider`'s own suites
    /// against a credential store they redirect; streaming here would read
    /// whatever store the machine running this test really holds.
    #[test]
    fn an_explicitly_named_cursor_is_answered_not_filtered() {
        // The flag tier, because it outranks every other and so cannot be
        // perturbed by whatever this process's environment holds.
        let mut config = Config::default();
        config.overrides.model = Some("cursor/gpt-5.3-codex".to_owned());

        let selection = select(&config).expect("an explicit cursor selection is not filtered");
        assert_eq!(selection.provider.id(), cursor::ID);
        assert_eq!(selection.model, "gpt-5.3-codex");
        assert!(
            selection.notice.is_none(),
            "the provider was asked for by name, not defaulted"
        );
    }

    /// The listing seam's whole negative half: for everything that is not
    /// cursor — cataloged builtins, the fake provider, config-declared
    /// endpoints, outright typos — the answer is [`None`] before anything is
    /// read or dialled, and the catalog stays the source of truth. The
    /// positive half mutates `XDG_DATA_HOME` and so lives in its own test
    /// binary, `tests/cursor_models_listing.rs`.
    #[tokio::test]
    async fn the_wire_listing_answers_none_where_the_catalog_is_the_source_of_truth() {
        for provider in [
            "anthropic",
            openai::ID,
            grok::ID,
            fake::ID,
            "local-llama",
            "a-provider-nothing-ships",
        ] {
            assert!(
                wire_model_listing(provider).await.is_none(),
                "{provider} is the catalog's to describe, not a wire's"
            );
        }
    }

    /// A model no catalog carries, so an answer naming it can only have come
    /// from the wire.
    const SENTINEL: &str = "a-model-the-catalog-has-never-heard-of";

    /// A backend that serves a narrower set than its vendor's catalog row has
    /// to be able to say so, and this is the precedence that lets it.
    ///
    /// The sentinel is what makes this a real assertion rather than a
    /// coincidence: `openai`'s two defaults currently name the same model, so
    /// comparing the strings would pass whether or not the wire is consulted
    /// at all. Handing it a value the catalog could not produce is the only way
    /// to tell "the wire decided" from "the table did" until the two diverge.
    #[test]
    fn a_backends_own_default_outranks_its_vendors_catalog_row() {
        assert!(
            catalog::model(SENTINEL).is_none(),
            "the sentinel has to be a model no table could have answered with"
        );
        assert_eq!(
            defaulted_model(openai::ID, Some(SENTINEL)).expect("a wire default needs no table"),
            SENTINEL,
            "a session on a backend that named its own default got the vendor's \
             row instead, which is how a ChatGPT seat ends up asking for a model \
             its backend refuses"
        );

        // Naming nothing falls through to the catalog, which is every other
        // provider and the openai key wire.
        assert_eq!(
            defaulted_model(openai::ID, None).expect("openai has a pinned default"),
            catalog::default_model(openai::ID).expect("openai has a pinned default")
        );
        // The fake provider is deliberately in no catalog: nothing canned has a
        // price, so it answers ahead of both.
        assert_eq!(
            defaulted_model(fake::ID, None).expect("the fake provider carries its own"),
            fake::MODEL
        );
        assert!(matches!(
            defaulted_model("nonexistent", None),
            Err(SelectionError::NoDefaultModel { .. })
        ));
        // Cursor left that refusal when the catalog pinned it: uncataloged —
        // no sizing, no pricing — yet defaulted, because the id the pin names
        // is the server-side Auto its own wire publishes.
        assert_eq!(
            defaulted_model(cursor::ID, None).expect("cursor has the wire-published pin"),
            "default"
        );
    }
}

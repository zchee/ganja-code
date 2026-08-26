//! Provider credentials: where they come from, and how they are kept.
//!
//! Two sources, in this order: the environment variable each vendor's own SDK
//! reads, then `auth.json` under the XDG data directory. The environment wins
//! so that `ANTHROPIC_API_KEY=… ganja` is a one-shot override that leaves the
//! stored key alone, which is how every other tool in the terminal behaves.
//!
//! The file mirrors upstream's shape (`packages/opencode/src/auth/index.ts`) so
//! that the two can eventually read each other's storage:
//!
//! ```json
//! {
//!   "anthropic": { "type": "api", "key": "sk-…" },
//!   "openai": { "type": "oauth", "refresh": "…", "access": "…",
//!               "expires": 1785000000000, "accountId": "…" }
//! }
//! ```
//!
//! Reading is deliberately tolerant, and writing is deliberately conservative.
//! Entries this build cannot interpret — upstream's `wellknown` credentials,
//! providers it has never heard of, a credential type invented after this was
//! written — are carried through a rewrite untouched instead of being dropped,
//! so `ganja auth login` can never cost someone a credential it did not
//! understand. The same holds *inside* an OAuth entry: upstream persists one as
//! `{type, access, refresh, expires, ...extra}` (`provider/auth.ts:211-220`),
//! where `...extra` is whatever the login method returned, so the record is
//! open-ended by construction. Fields this build does not model are kept in
//! [`OauthCredential::extra`] and written back as they were found.
//!
//! This is stricter than upstream, which decodes the whole file through a
//! filtering read (`auth/index.ts:65-66`) and then writes that already-filtered
//! map back (`:79`) — an entry it cannot decode is lost on the next write. A
//! shared `auth.json` is somebody else's territory too, so ganja does not.
//!
//! That same filtering write is why the one thing ganja records *about* a
//! credential — when its login first landed, so that selection can default to
//! the oldest one — lives in a sidecar of its own rather than inside the
//! entries: see `STAMPS_FILE`, which carries the evidence.
//!
//! Secrets never reach a log. Key material is held in a [`SecretString`], whose
//! own [`Debug`] is a placeholder and whose contents are wiped when the last
//! handle drops; [`Credential`] and [`OauthCredential`] render as the last four
//! characters of their tokens through both [`Debug`] and [`std::fmt::Display`], and
//! nothing in this module formats a whole secret. [`OauthCredential`]'s `Debug`
//! is hand-written for that reason: the unmodelled extras are exactly where a
//! third party's token would land, so their *keys* are shown and their values
//! never are.
//!
//! The file is replaced by writing a sibling and renaming it into place. That
//! sibling is created exclusively, because its name is predictable and a
//! symbolic link planted at it would otherwise redirect the write.
//!
//! On Windows the same owner-only invariant is a protected DACL sealed onto
//! that exclusive create handle before the first secret byte is written. The
//! DACL grants only the process token's user: SYSTEM and Administrators can
//! take ownership already, so spelling grants for them would only widen the
//! file. The read side nevertheless accepts those two identities so stores
//! made before this protection landed keep working under the profile ACLs
//! Windows normally inherited. Deny ACEs are ignored because they narrow
//! access; an allow ACE for another identity is conservatively refused even
//! when a deny ACE would make that particular grant ineffective.
//!
//! `OpenOptions` has no security-descriptor-at-creation hook, so the Windows
//! create-to-seal interval can expose only an empty file. Keeping the standard
//! exclusive open preserves its reparse-point defence; replacing it with a raw
//! `CreateFileW` just to close that empty-file interval would reimplement the
//! more important no-follow invariant.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    collections::{BTreeMap, HashMap},
    env, fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine as _, alphabet,
    engine::{GeneralPurpose, general_purpose::NO_PAD_INDIFFERENT},
};
use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use futures::future::{BoxFuture, FutureExt as _, Shared};
use secrecy::{ExposeSecret as _, SecretString, zeroize::Zeroize as _};
use serde::Deserialize;
use serde_json::{Map, Value, error::Category};

pub mod copilot;
pub mod cursor;
pub mod device;
pub mod grok;
pub mod loopback;
pub mod mcp_oauth;
pub mod openai;
pub mod pkce;

/// The HTTP client every login flow speaks through.
///
/// Redirects are refused for the reason every credential-carrying request in
/// this build refuses them: these requests hold a code, a verifier or a
/// token, and a client that followed a 3xx would replay it at whatever the
/// redirect named. Each flow keeps its own timeout and its own error shape.
///
/// No response-body size cap is set here, a pre-existing gap every login
/// flow through this client shares — newly relevant since [`mcp_oauth`] is
/// the first login whose counterparty is a config-named,
/// attacker-influenceable origin rather than a fixed vendor endpoint; a
/// follow-up, not a fix made here.
pub(crate) fn login_client(timeout: std::time::Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
}

/// Directory ganja keeps its state in, under the XDG data home.
const DIRECTORY: &str = "ganja";

/// File credentials live in, named after upstream's.
const FILE: &str = "auth.json";

/// Ganja's own sidecar beside [`FILE`], recording when each stored credential
/// first landed: `{ "<storage key>": <milliseconds since the Unix epoch> }`.
/// It is what lets selection default to the *oldest* login when nothing named
/// a provider.
///
/// A sidecar, and deliberately not a field inside the entries themselves.
/// `auth.json` is shared territory, and upstream's `Auth.set`
/// (`auth/index.ts:73-81`) round-trips the WHOLE store through `Auth.all`
/// (`:58-67`), whose `Record.filterMap(data, decode)` (`:66`) rebuilds every
/// entry as an effect `Schema.Class` instance carrying only the declared
/// fields (`Oauth` `:14-21`, `Api` `:23-27`, `WellKnown` `:29-33`) — and `:79`
/// then rewrites `auth.json` from those instances. An in-entry foreign field
/// therefore dies on opencode's next write, and an entry a stricter decoder
/// rejects is dropped from the map entirely (verified 2026-08-08 against the
/// v1.18.22 checkout). A stamp that upstream would silently erase is a default
/// that flips the day someone runs `opencode auth login`, so the stamps live
/// in a file only ganja writes, and `auth.json`'s shape is never touched.
///
/// The file holds provider names and timestamps — no secrets — so reading it
/// takes none of the permission checks the store itself insists on, and losing
/// it costs an ordering preference rather than a credential.
const STAMPS_FILE: &str = "auth-stamps.json";

/// Characters of a secret that any output may show.
const TAIL: usize = 4;

/// Stands in for the part of a secret that stays hidden.
const MASK: &str = "****";

/// Mode the credential file is created with and required to have: readable and
/// writable by its owner, invisible to everyone else.
#[cfg(unix)]
const PRIVATE: u32 = 0o600;

/// Bits that would let someone other than the owner read the file.
#[cfg(unix)]
const SHARED: u32 = 0o077;

/// Environment variables that carry an API key, by provider.
///
/// The names are the ones each vendor's own SDK reads, so a shell already set
/// up for `curl` or an official client needs no further configuration.
pub const KEY_VARS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    // A gateway rather than a model vendor, and the variable is still its own:
    // it is what this one's console, its `curl` examples and the model
    // catalog's `env` for it all name. `provider::openrouter` pins the two
    // spellings against each other.
    ("openrouter", "OPENROUTER_API_KEY"),
    // **Two providers, one variable**, and that is the vendor's own
    // arrangement rather than a shortcut: the catalog names `OPENCODE_API_KEY`
    // in the `env` of both, and one key was measured answering on both hosts.
    // They still store separately, under their own ids — nothing here aliases
    // one to the other, because the vendor's client keys credentials by
    // provider id and a shared `auth.json` is its territory too.
    ("opencode", "OPENCODE_API_KEY"),
    ("opencode-go", "OPENCODE_API_KEY"),
];

/// The environment variable an API key for `provider_id` may be passed in.
#[must_use]
pub fn key_var(provider_id: &str) -> Option<&'static str> {
    KEY_VARS
        .iter()
        .find(|(provider, _)| *provider == provider_id)
        .map(|(_, variable)| *variable)
}

/// Providers ganja names differently from the key their credential is stored
/// under, as `(ganja's id, the key in `auth.json`)`.
///
/// `auth.json` is shared territory: an opencode install pointed at the same
/// file has to find the credential where it put it, and `config
/// import-opencode` translates a config that names providers upstream's way.
/// So the storage key is upstream's, always — xAI's is `xai`
/// (`plugin/xai.ts:509`, which stores under `{ id: "xai" }`) — and ganja's own
/// name for that provider, `grok`, is a name for a module and a command-line
/// argument rather than for a line in a credential file.
const STORAGE_ALIASES: &[(&str, &str)] = &[("grok", "xai")];

/// The key `provider_id`'s credential is stored under.
///
/// The identity for every provider whose name ganja and upstream agree on,
/// which is all but the ones in `STORAGE_ALIASES`.
#[must_use]
pub fn storage_key(provider_id: &str) -> &str {
    STORAGE_ALIASES
        .iter()
        .find(|(ganja, _)| *ganja == provider_id)
        .map_or(provider_id, |(_, stored)| *stored)
}

/// The provider id ganja knows a stored key by — [`storage_key`] backwards.
///
/// Reading a listing back into ganja's own vocabulary is the caller's choice,
/// not this module's: [`list_providers`] reports the keys the file actually
/// holds, because that is what a person comparing it against `opencode auth
/// list` or against the file itself will see.
#[must_use]
pub fn provider_id_for_storage_key(key: &str) -> &str {
    STORAGE_ALIASES
        .iter()
        .find(|(_, stored)| *stored == key)
        .map_or(key, |(ganja, _)| *ganja)
}

/// Where a login nobody stamped ranks, by ganja's name for its provider.
///
/// A store written before [`STAMPS_FILE`] existed — or by opencode, which will
/// never write one — holds logins with no recorded age. They rank **after**
/// every stamped login, in this fixed order, because "I logged into anthropic
/// last year" is information this build arrived too late to have and a stable
/// answer beats a guessed one. An id outside this list ranks after it, in the
/// store's own order.
const UNSTAMPED_PRIORITY: &[&str] = &["anthropic", "openai", "grok", "github-copilot"];

/// Where `key` sorts among stored logins: oldest stamp first, then the
/// unstamped by [`UNSTAMPED_PRIORITY`], then everything else in the order the
/// store holds it — which a stable sort preserves, so the last bucket needs no
/// second component.
fn login_rank(key: &str, stamps: &BTreeMap<String, u64>) -> (u8, u64) {
    if let Some(&stored_at) = stamps.get(key) {
        return (0, stored_at);
    }

    match UNSTAMPED_PRIORITY
        .iter()
        .position(|id| *id == provider_id_for_storage_key(key))
    {
        Some(index) => (1, index as u64),
        None => (2, 0),
    }
}

/// What a stored `expires` of zero means, which is not one answer.
///
/// Two providers write a zero into that field and mean opposite things by it.
/// GitHub Copilot stores the OAuth token as its own credential with `expires:
/// 0` (`copilot.ts:288-295`) and means **never expires** — there is no renewal
/// endpoint for that credential at all, so one that ever reported itself due
/// would be due forever. xAI's loader reads the same zero as **no deadline was
/// recorded** and renews (`xai.ts:491`, `!currentAuth.expires ||`), because its
/// token endpoint does not always answer with an `expires_in` to compute one
/// from; ChatGPT's reads it the same way (`codex.ts:361`, where `0 <
/// Date.now()` is true).
///
/// One field, two meanings, so the meaning belongs to the provider rather than
/// to whoever is holding the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroExpiry {
    /// The credential has no deadline and never becomes due.
    Never,
    /// No deadline was recorded, so the access token's own `exp` is what says
    /// — and a token carrying none says nothing either way.
    Unrecorded,
}

/// Providers whose zero is a promise rather than a gap.
///
/// A list rather than a match arm so that adding a provider is adding a row
/// beside this reason. Everything not named here reads a zero the way both of
/// upstream's other OAuth providers do, which is also the safer default: a
/// deadline nobody wrote down is not a deadline nobody has.
const ZERO_EXPIRY_NEVER: &[&str] = &[copilot::PROVIDER_ID];

/// What a zero in `provider_id`'s stored `expires` means.
///
/// Either spelling of a provider lands on the same rule — ganja's name and the
/// file's must not disagree about the same credential, which is the one way
/// `STORAGE_ALIASES` could turn into a bug here.
#[must_use]
pub fn zero_expiry(provider_id: &str) -> ZeroExpiry {
    if ZERO_EXPIRY_NEVER.contains(&provider_id_for_storage_key(provider_id)) {
        ZeroExpiry::Never
    } else {
        ZeroExpiry::Unrecorded
    }
}

/// The engine a JWT payload is decoded with.
///
/// base64url, which JWS mandates (RFC 7515 §2), and **indifferent about
/// padding** for the reason [`openai`]'s own copy of this constant is:
/// producers mostly emit none, one that pads is not producing something this
/// decode believes anyway, and refusing it would buy strictness with a renewal
/// that silently never happens. The two are the same engine and have to stay
/// so; this one is the parent module's, so the child's can be collapsed into it
/// whenever that file is next open for another reason.
const JWT_CLAIMS: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, NO_PAD_INDIFFERENT);

/// When an access token itself says it stops being accepted, for one that says.
///
/// Spec: `plugin/xai.ts:95-116` (`accessTokenIsExpiring`), whose own comment is
/// the entire warrant for decoding an unsigned token: "We only use this to
/// decide whether to proactively refresh, never to make trust decisions, so
/// unsigned decode is safe."
///
/// **The signature is not checked, and this is not validation** — the same
/// posture [`openai`]'s `claimed_account` is written under, said again here
/// because the value is read for a different purpose and the reasoning has to
/// survive being read on its own. Nothing may be believed on this claim's word;
/// the worst a forged `exp` can do is spend a refresh token early, which costs
/// a round trip rather than an authorization.
///
/// A value that is not a JWS compact serialization contributes nothing, which
/// is upstream's answer too: an opaque token has no deadline inside it, and the
/// stored one is then all there is. Three segments exactly, where upstream
/// takes two or more (`xai.ts:105`) — a two-segment string is not a token any
/// issuer minted, and accepting one would let an arbitrary `a.<base64>` value
/// decide when a credential gets renewed.
fn token_deadline_ms(access: &SecretString) -> Option<u64> {
    // The fourth `expose_secret` in this module, and the second that reads a
    // token in order to say something *about* it rather than to use it: what
    // leaves here is a number, and a token that will not decode leaves nothing.
    let token = access.expose_secret();
    let mut segments = token.split('.');
    segments.next()?;
    let payload = segments.next()?;
    segments.next()?;
    if segments.next().is_some() {
        return None;
    }

    let claims: Value = serde_json::from_slice(&JWT_CLAIMS.decode(payload).ok()?).ok()?;

    claims
        .get("exp")
        // RFC 7519 §2 allows a NumericDate to be non-integer and every issuer
        // met so far emits whole seconds; one that did not would decode as a
        // float here, contribute nothing, and leave the stored deadline in
        // charge — which is the same place a token with no `exp` leaves it.
        .and_then(Value::as_u64)
        .map(|seconds| seconds.saturating_mul(1_000))
}

/// The last few characters of a secret, which is all any output may show.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedTail(String);

impl RedactedTail {
    /// Renders `secret` as a mask followed by its last `TAIL` characters.
    ///
    /// Public so that nothing outside this module has to invent its own idea of
    /// how much of a key may be shown.
    ///
    /// Character counting is deliberate: an API key is ASCII in practice, but
    /// slicing bytes off a value that turned out not to be would panic.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        let characters: Vec<char> = secret.chars().collect();
        let visible: String = characters[characters.len().saturating_sub(TAIL)..]
            .iter()
            .collect();

        Self(format!("{MASK}{visible}"))
    }

    /// Same, for key material that has not been unwrapped.
    ///
    /// Public so that a caller holding a secret never has to expose one to say
    /// which key it is holding.
    #[must_use]
    pub fn of_secret(secret: &SecretString) -> Self {
        Self::of(secret.expose_secret())
    }

    /// The redacted form, for a caller that needs to place it in a table.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedTail {
    /// Through [`fmt::Formatter::pad`] for the same reason
    /// [`CredentialKind`]'s is: this is printed in a column beside values of
    /// other widths, and a `write_str` would drop the width it was given.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(&self.0)
    }
}

impl fmt::Debug for RedactedTail {
    /// Same as [`std::fmt::Display`]: a redacted value that grows quotes in a debug dump
    /// is still redacted, and one that grows the key back is a leak.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Whether a secret carries nothing but whitespace, which is not a credential.
fn is_blank(secret: &SecretString) -> bool {
    secret.expose_secret().trim().is_empty()
}

/// An API key, and the only thing a provider needs to authenticate.
///
/// The key is held in a [`SecretString`], so reading it back takes an explicit
/// `expose_secret` — this module has four: [`RedactedTail::of_secret`],
/// `is_blank` and `token_deadline_ms`, which read a secret in order to say
/// something *about* it rather than to use it, and `Store::set`, which has to
/// hand the plaintext to the serializer that writes it to disk — and the
/// material is wiped when the last handle drops
/// along every path this module controls. There is deliberately no `PartialEq`:
/// comparing secrets is not something this crate needs, and an implementation
/// of it would be a timing oracle nobody asked for.
#[derive(Clone)]
pub struct Credential {
    /// The key itself, as the provider expects it in a header.
    pub api_key: SecretString,
}

impl Credential {
    /// The key as it may be shown.
    #[must_use]
    pub fn tail(&self) -> RedactedTail {
        RedactedTail::of_secret(&self.api_key)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("api_key", &self.tail())
            .finish()
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.tail().fmt(formatter)
    }
}

/// An OAuth credential, in the shape upstream writes it.
///
/// Spec: `packages/opencode/src/auth/index.ts:14-21` for the schema —
/// `{type:"oauth", refresh, access, expires, accountId?, enterpriseUrl?}` — and
/// `packages/opencode/src/provider/auth.ts:211-220` for how it reaches disk:
/// the four required fields plus `...extra`, whatever else the login method
/// returned. The two optional fields upstream names are modelled here because
/// two shipped providers need them by name — `accountId` is the ChatGPT account
/// a Codex request is billed to (`plugin/openai/codex.ts:365`, `:404`) and
/// `enterpriseUrl` is the GitHub deployment a Copilot request goes to
/// (`plugin/github-copilot/copilot.ts:65`) — and everything else stays in
/// [`extra`](Self::extra), unread and unharmed.
///
/// Both tokens are secrets. Copilot's are the same string: the GitHub OAuth
/// token *is* the credential, so it is stored as both `refresh` and `access`
/// with `expires: 0` (`copilot.ts:288-295`), and this build measured that
/// against the live API before writing any of this down.
#[derive(Clone, Deserialize)]
pub struct OauthCredential {
    /// The long-lived token a new access token is obtained with. For a
    /// credential that never expires this is the credential itself.
    pub refresh: SecretString,
    /// The token a request carries, until [`expires`](Self::expires).
    pub access: SecretString,
    /// When [`access`](Self::access) stops being accepted, in milliseconds
    /// since the Unix epoch.
    ///
    /// **Zero is never "expired in 1970", and it does not mean one thing.** It
    /// is Copilot's *never expires* (`copilot.ts:294`) and xAI's *no deadline
    /// was recorded* (`xai.ts:491`) written into the same field by two
    /// providers, so which of them a zero means is [`zero_expiry`]'s to say and
    /// not a reader's to assume. [`needs_refresh`] and [`needs_refresh_for`]
    /// are the only things that should be reading this directly.
    ///
    /// [`needs_refresh`]: Self::needs_refresh
    /// [`needs_refresh_for`]: Self::needs_refresh_for
    pub expires: u64,
    /// The account a request is billed to, where the provider has more than one.
    #[serde(rename = "accountId", default)]
    pub account_id: Option<String>,
    /// The GitHub Enterprise deployment this credential belongs to.
    #[serde(rename = "enterpriseUrl", default)]
    pub enterprise_url: Option<String>,
    /// Everything else the entry carried.
    ///
    /// Never interpreted, never dropped: an entry written by opencode, by a
    /// third-party plugin, or by a later version of ganja keeps whatever it
    /// put here across a rewrite. It cannot collide with a modelled field —
    /// serde matches those first — so writing this back can only restore what
    /// was already there.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OauthCredential {
    /// A credential holding just the pair of tokens and an expiry.
    ///
    /// The optional fields are set afterwards by the login flow that knows
    /// whether its provider has them; a struct literal would have to name them
    /// all, and `..Default::default()` on a type with two secrets in it is a
    /// default credential nobody asked to exist.
    #[must_use]
    pub fn new(refresh: SecretString, access: SecretString, expires: u64) -> Self {
        Self {
            refresh,
            access,
            expires,
            account_id: None,
            enterprise_url: None,
            extra: Map::new(),
        }
    }

    /// What may be shown of the credential: the tail of the token a request
    /// would carry, or of the refresh token when there is no access token to
    /// show — Copilot's two are the same string, and a credential mid-refresh
    /// may have only the one.
    #[must_use]
    pub fn tail(&self) -> RedactedTail {
        if is_blank(&self.access) {
            RedactedTail::of_secret(&self.refresh)
        } else {
            RedactedTail::of_secret(&self.access)
        }
    }

    /// Whether there is any token here at all.
    ///
    /// An entry whose tokens are blank is not a credential, the same way an
    /// `api` entry storing an empty key is not one: it would fail at the
    /// provider with a message about the request rather than about the login.
    fn is_usable(&self) -> bool {
        !is_blank(&self.access) || !is_blank(&self.refresh)
    }

    /// Whether the *stored* deadline is spent, or close enough to it that a
    /// request started now might outlive it.
    ///
    /// `expires == 0` reads as "never" here — Copilot's meaning, and the one
    /// this predicate has always had. It answers the narrow question the
    /// stored field alone can answer, and nothing more: whether the record
    /// itself calls the token spent.
    ///
    /// **The renewal decision is [`needs_refresh_for`](Self::needs_refresh_for)**,
    /// which reads a zero the way the credential's own provider writes one and
    /// falls back to the deadline inside the access token. A caller deciding
    /// whether to spend a refresh token wants that one; this one cannot tell
    /// Copilot's promise from xAI's silence, and telling them apart is the
    /// whole reason there are two.
    ///
    /// `skew_ms` is the margin: upstream refreshes two minutes early so that a
    /// single long tool call does not have to recover from a mid-flight 401
    /// (`xai.ts:44`).
    #[must_use]
    pub fn needs_refresh(&self, now_ms: u64, skew_ms: u64) -> bool {
        self.expires != 0 && self.expires <= now_ms.saturating_add(skew_ms)
    }

    /// Whether this credential is due for renewal, under `provider_id`'s own
    /// reading of what it carries.
    ///
    /// This is the renewal decision, and [`Refresher`] is what asks it. Two
    /// things separate it from [`needs_refresh`](Self::needs_refresh):
    ///
    /// - a zero in [`expires`](Self::expires) means whatever [`zero_expiry`]
    ///   says it means for this provider, rather than Copilot's "never" for
    ///   everybody;
    /// - where the stored deadline says nothing, the deadline *inside the
    ///   access token* does — upstream's own comment (`xai.ts:485-490`) calls
    ///   that check "the load-bearing one for tokens that lack a fresh stored
    ///   deadline", because xAI's token endpoint does not always send an
    ///   `expires_in` to compute one from.
    ///
    /// The token's own claim is **only ever a reason to renew**. Nothing here
    /// is a trust decision — see `token_deadline_ms`, which checks no
    /// signature — and [`needs_refresh`](Self::needs_refresh), the reading of
    /// the stored record alone, deliberately does not consult it: a forged
    /// `exp` must not be able to make a credential the store calls live look
    /// spent.
    #[must_use]
    pub fn needs_refresh_for(&self, provider_id: &str, now_ms: u64, skew_ms: u64) -> bool {
        let horizon = now_ms.saturating_add(skew_ms);

        if self.expires == 0 && zero_expiry(provider_id) == ZeroExpiry::Never {
            // A promise, and nothing may talk it out of one: this provider has
            // no renewal endpoint at all, so a credential that ever reported
            // itself due would report it forever.
            return false;
        }
        if self.expires != 0 && self.expires <= horizon {
            return true;
        }

        token_deadline_ms(&self.access).is_some_and(|deadline| deadline <= horizon)
    }

    /// This credential, taking from `previous` whatever it does not carry
    /// itself.
    ///
    /// A refresh returns tokens, not an identity: upstream keeps the account id
    /// across one explicitly (`codex.ts:365`, `extractAccountId(tokens) ||
    /// authWithAccount.accountId`), and the same reasoning covers the enterprise
    /// deployment and every unmodelled field — a token endpoint that does not
    /// echo them back has not revoked them.
    #[must_use]
    pub fn inheriting(mut self, previous: &Self) -> Self {
        if self.account_id.is_none() {
            self.account_id = previous.account_id.clone();
        }
        if self.enterprise_url.is_none() {
            self.enterprise_url = previous.enterprise_url.clone();
        }
        for (field, value) in &previous.extra {
            self.extra
                .entry(field.clone())
                .or_insert_with(|| value.clone());
        }

        self
    }

    /// The entry as it goes on disk, exposing both tokens exactly once.
    ///
    /// Unmodelled fields are laid down first so that a modelled one can only
    /// ever overwrite a stale copy of itself, never the other way round.
    fn to_value(&self) -> Value {
        let mut entry = self.extra.clone();
        entry.insert("type".to_owned(), Value::from("oauth"));
        entry.insert(
            "refresh".to_owned(),
            Value::from(self.refresh.expose_secret()),
        );
        entry.insert(
            "access".to_owned(),
            Value::from(self.access.expose_secret()),
        );
        entry.insert("expires".to_owned(), Value::from(self.expires));
        if let Some(account_id) = &self.account_id {
            entry.insert("accountId".to_owned(), Value::from(account_id.clone()));
        }
        if let Some(enterprise_url) = &self.enterprise_url {
            entry.insert(
                "enterpriseUrl".to_owned(),
                Value::from(enterprise_url.clone()),
            );
        }

        Value::Object(entry)
    }
}

impl fmt::Debug for OauthCredential {
    /// Hand-written because [`extra`](OauthCredential::extra) holds values this
    /// module has never seen. A third-party plugin's own token lands there, and
    /// a derived `Debug` would print it: the field names are useful for working
    /// out what is stored, the values are not worth the risk.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OauthCredential")
            .field("refresh", &RedactedTail::of_secret(&self.refresh))
            .field("access", &RedactedTail::of_secret(&self.access))
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .field("enterprise_url", &self.enterprise_url)
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl fmt::Display for OauthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.tail().fmt(formatter)
    }
}

/// Which of the things a provider can be authenticated with an entry holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// A key sent as-is on every request.
    ApiKey,
    /// A pair of tokens, the sent one with a lifetime.
    Oauth,
}

impl fmt::Display for CredentialKind {
    /// Through [`fmt::Formatter::pad`], not `write_str`: the two words differ
    /// in length and the only caller prints them in a column, so a `{:<n}`
    /// that a `write_str` silently ignored would misalign every row against
    /// its own header.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(match self {
            Self::ApiKey => "api",
            Self::Oauth => "oauth",
        })
    }
}

/// Where a credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The environment variable named here.
    Environment(&'static str),
    /// The stored credential file.
    File,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(variable) => formatter.write_str(variable),
            Self::File => formatter.write_str(FILE),
        }
    }
}

/// One provider that can be authenticated, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Key the credential is stored under, which is upstream's name for the
    /// provider — see [`storage_key`].
    pub provider_id: String,
    /// What may be shown of the key.
    pub tail: RedactedTail,
    /// Where [`credential_for`] would read it.
    pub source: Source,
    /// What kind of credential it is, since not every one is a key.
    pub kind: CredentialKind,
    /// The environment variable that outranks this one, when one does.
    ///
    /// Always [`None`] on an environment entry — nothing outranks the
    /// environment — so this is also how a caller tells a losing row from the
    /// winning one without re-deriving the precedence rule for itself.
    pub shadowed_by: Option<&'static str>,
}

/// A credential could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The store could not be reached.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, naming the path where there is one.
        context: String,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// The store is not the JSON object it has to be.
    ///
    /// The parser's own message is deliberately thrown away, and there is no
    /// `#[source]` to walk to it: `serde_json` quotes the offending value back
    /// — `invalid type: string "sk-…", expected a map` — and in this file every
    /// value is a secret. The position says where to look without saying what
    /// is there.
    #[error(
        "{} is not valid credential storage: {kind} at line {line}, column {column}",
        .path.display()
    )]
    Malformed {
        /// The file that could not be understood.
        path: PathBuf,
        /// How it failed to make sense, in words that quote nothing.
        kind: &'static str,
        /// Line the parser stopped on.
        line: usize,
        /// Column the parser stopped on.
        column: usize,
    },
    /// The store is exposed to other users of the machine.
    #[cfg_attr(
        not(windows),
        error(
        "{path} is readable by users other than its owner (mode {mode:04o}); \
         a leaked key cannot be un-leaked, so nothing was read from it - \
         run `chmod 600 {path}`, or rotate the key and store it again",
        path = .path.display()
        )
    )]
    #[cfg_attr(
        windows,
        error(
            "{path} grants {grantee} access to stored credentials; \
             a leaked key cannot be un-leaked, so nothing was read from it - \
             run `icacls \"{path}\" /inheritance:r /grant:r \"%USERNAME%:F\"`, \
             or rotate the key and store it again",
            path = .path.display()
        )
    )]
    Permissions {
        /// The file with the permissions.
        path: PathBuf,
        /// The mode it was found with.
        #[cfg(not(windows))]
        mode: u32,
        /// The identity an allow ACE lets reach the file.
        #[cfg(windows)]
        grantee: String,
    },
    /// An OAuth credential was asked for and the provider has none.
    #[error(
        "{provider_id} has no OAuth credential stored ({found}); \
         run `ganja auth login {provider_id}`"
    )]
    NotOauth {
        /// The provider that was asked about.
        provider_id: String,
        /// What is stored instead, in words that quote nothing.
        found: &'static str,
    },
    /// A refresh ran, and the provider refused it. Only a new login fixes this.
    #[error(
        "the stored {provider_id} credential was refused when it was renewed \
         ({reason}); run `ganja auth login {provider_id}`"
    )]
    ReauthRequired {
        /// The provider that refused.
        provider_id: String,
        /// Why, in the provider's terms. Never carries token material: a
        /// caller building one of these passes a status and an error code, not
        /// a response body.
        reason: String,
    },
    /// A refresh could not be carried out, and the stored credential is fine.
    ///
    /// The difference from [`ReauthRequired`](Self::ReauthRequired) is the
    /// whole reason this variant exists: telling someone whose network dropped
    /// to log in again costs them a browser round trip they did not need.
    #[error(
        "the {provider_id} access token could not be renewed right now \
         ({reason}); the stored credential is still good - try again"
    )]
    RefreshUnavailable {
        /// The provider whose token could not be renewed.
        provider_id: String,
        /// What got in the way. Never carries token material.
        reason: String,
    },
}

/// What a caller can do about an [`AuthError`], without matching every variant.
///
/// Four, since P22 (`flp`). There was a fifth — `Expired`, "the access token
/// is spent and renewable" — retired with the variant it classified: nothing
/// in production ever built one. A spent token is not a failure any caller
/// sees, because [`Refresher::usable`] renews it and reports only what came
/// back, so the outcomes a caller acts on are a refusal, an absence, a store
/// to repair, or a renewal that never got that far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthErrorKind {
    /// The store could not be read or written. Repair the file.
    Storage,
    /// There is no OAuth credential here. Log in.
    NotOauth,
    /// The credential is dead. Log in again.
    ReauthRequired,
    /// The renewal did not happen. Retry.
    RefreshUnavailable,
}

impl AuthError {
    /// Reports a parse failure by position and kind, never by content.
    fn malformed(path: &Path, error: &serde_json::Error) -> Self {
        Self::Malformed {
            path: path.to_path_buf(),
            kind: match error.classify() {
                Category::Io => "the file could not be read",
                Category::Syntax => "the JSON is malformed",
                Category::Data => "the JSON is not the shape a credential store has",
                Category::Eof => "the JSON ends early",
            },
            line: error.line(),
            column: error.column(),
        }
    }

    /// What went wrong, in the four terms a caller acts on.
    #[must_use]
    pub fn kind(&self) -> AuthErrorKind {
        match self {
            Self::Io { .. } | Self::Malformed { .. } | Self::Permissions { .. } => {
                AuthErrorKind::Storage
            }
            Self::NotOauth { .. } => AuthErrorKind::NotOauth,
            Self::ReauthRequired { .. } => AuthErrorKind::ReauthRequired,
            Self::RefreshUnavailable { .. } => AuthErrorKind::RefreshUnavailable,
        }
    }
}

/// A stored credential, as far as this build understands it.
///
/// Upstream also stores `wellknown` credentials, and nothing says a later
/// version of either tool will not invent a fourth kind; both decode as
/// [`Stored::Unusable`] so that a rewrite keeps them.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Stored {
    /// A plain API key.
    Api {
        /// The key.
        key: SecretString,
    },
    /// A pair of OAuth tokens, plus whatever else the entry carried.
    Oauth(OauthCredential),
    /// Something this build cannot authenticate with.
    #[serde(other)]
    Unusable,
}

impl Stored {
    /// How to describe this entry to someone who asked for a different kind.
    ///
    /// Deliberately not the entry itself: every value in it is a secret.
    fn describe(entry: Option<&Value>) -> &'static str {
        match entry.map(|value| serde_json::from_value::<Self>(value.clone())) {
            None => "nothing is stored",
            Some(Ok(Self::Api { .. })) => "an API key is stored",
            Some(Ok(Self::Oauth(_))) => "the stored OAuth credential has no tokens in it",
            Some(Ok(Self::Unusable) | Err(_)) => {
                "a credential this build does not understand is stored"
            }
        }
    }
}

/// The credential file, wherever it turned out to be.
struct Store {
    path: PathBuf,
}

impl Store {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Resolves the store's location from the XDG data directory.
    ///
    /// XDG conventions are used on every platform, macOS included, matching
    /// upstream's own `~/.local/share/opencode` behaviour there.
    fn open() -> Result<Self, AuthError> {
        let base = Xdg::new().map_err(|source| AuthError::Io {
            context: "the home directory holding the credential store could not be located"
                .to_owned(),
            source: io::Error::other(source),
        })?;

        Ok(Self::new(base.data_dir().join(DIRECTORY).join(FILE)))
    }

    fn io(&self, attempt: &str, source: io::Error) -> AuthError {
        AuthError::Io {
            context: format!("{} {attempt}", self.path.display()),
            source,
        }
    }

    /// Reads the file as it stands, entries this build cannot use included.
    ///
    /// A missing file is the first run, not a failure.
    fn read(&self) -> Result<BTreeMap<String, Value>, AuthError> {
        #[cfg(windows)]
        let mut bytes = {
            use std::{io::Read as _, os::windows::fs::OpenOptionsExt as _};

            use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;

            let mut file = match fs::OpenOptions::new()
                .read(true)
                .access_mode(FILE_GENERIC_READ)
                .open(&self.path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(BTreeMap::new());
                }
                Err(source) => return Err(self.io("could not be read", source)),
            };
            check_private(&self.path, &file)?;

            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|source| self.io("could not be read", source))?;
            bytes
        };

        #[cfg(not(windows))]
        let mut bytes = {
            let metadata = match fs::metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(BTreeMap::new());
                }
                Err(source) => return Err(self.io("could not be inspected", source)),
            };
            check_private(&self.path, &metadata)?;

            fs::read(&self.path).map_err(|source| self.io("could not be read", source))?
        };
        let parsed = serde_json::from_slice(&bytes);
        // This held every stored key in plaintext; the parse has taken what it
        // needs, so there is no reason to leave it in the heap to be handed to
        // the next allocation or written to a core dump.
        bytes.zeroize();

        parsed.map_err(|error| AuthError::malformed(&self.path, &error))
    }

    /// Replaces the file's contents.
    ///
    /// The bytes land in a sibling file that is renamed into place, so an
    /// interrupted write cannot leave a truncated store behind — losing every
    /// stored key to a crash would be a worse bug than any this method has.
    fn write(&self, data: &BTreeMap<String, Value>) -> Result<(), AuthError> {
        let parent = self.path.parent().ok_or_else(|| {
            self.io(
                "has no directory to be created in",
                io::Error::from(io::ErrorKind::NotFound),
            )
        })?;
        fs::create_dir_all(parent).map_err(|source| AuthError::Io {
            context: format!("{} could not be created", parent.display()),
            source,
        })?;

        let mut json = serde_json::to_vec_pretty(data)
            .map_err(|error| AuthError::malformed(&self.path, &error))?;
        json.push(b'\n');

        let temporary = self
            .path
            .with_file_name(format!("{FILE}.{}.tmp", std::process::id()));
        let written = write_private(&temporary, &json);
        // Wiped whether or not the write landed, and before the `?`: the buffer
        // holds every stored key in plaintext and the file now has its own copy.
        json.zeroize();
        written.map_err(|source| AuthError::Io {
            context: format!("{} could not be written", temporary.display()),
            source,
        })?;

        fs::rename(&temporary, &self.path).map_err(|source| {
            // A rename that fails leaves the temporary file holding a copy of
            // every key, which is exactly what must not be left lying around.
            let _ = fs::remove_file(&temporary);
            self.io("could not be replaced", source)
        })
    }

    fn get(&self, provider_id: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self
            .read()?
            .get(storage_key(provider_id))
            .and_then(usable_key)
            .map(|api_key| Credential { api_key }))
    }

    /// The OAuth credential stored for `provider_id`, whole.
    ///
    /// The entry's unmodelled fields come back with it, which is what makes
    /// [`Self::set_oauth`] able to put them back.
    fn oauth(&self, provider_id: &str) -> Result<Option<OauthCredential>, AuthError> {
        Ok(self
            .read()?
            .get(storage_key(provider_id))
            .and_then(usable_oauth))
    }

    /// Stores `credential` as `provider_id`'s, replacing whatever it had.
    ///
    /// Replacement rather than a merge, the way upstream's `set` is
    /// (`auth/index.ts:73-81`): what the caller holds is the whole credential,
    /// including the extras it read out of the previous one. Merging here
    /// instead would resurrect a field from an account that has since been
    /// logged out of.
    ///
    /// This is the **login** path — `ganja auth login` is its only caller
    /// outside the tests — so it stamps. A renewal writes through
    /// [`Self::renew_oauth`] instead, which never does.
    fn set_oauth(&self, provider_id: &str, credential: &OauthCredential) -> Result<(), AuthError> {
        let key = storage_key(provider_id).to_owned();
        let mut data = self.read()?;
        data.insert(key.clone(), credential.to_value());

        self.write(&data)?;
        self.record_stamps(&data, Some(&key));

        Ok(())
    }

    /// Stores a renewed `credential` without treating the write as a login.
    ///
    /// The one difference from [`Self::set_oauth`] is the stamp: a refresh
    /// rewrites the same login it was given, so minting a stamp here would
    /// walk a pre-feature credential into the stamped tier at whatever moment
    /// its token happened to expire — and the oldest-login default would flip
    /// under whoever was relying on it. A stamp the login already has is kept,
    /// by the same rule.
    fn renew_oauth(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<(), AuthError> {
        let mut data = self.read()?;
        data.insert(storage_key(provider_id).to_owned(), credential.to_value());

        self.write(&data)?;
        self.record_stamps(&data, None);

        Ok(())
    }

    /// Stores `api_key`, exposing it exactly once: the serializer that puts it
    /// on disk needs the plaintext, and there is no way to write a file without
    /// the bytes that go in it.
    fn set(&self, provider_id: &str, api_key: impl Into<SecretString>) -> Result<(), AuthError> {
        let key = storage_key(provider_id).to_owned();
        let api_key = api_key.into();
        let mut data = self.read()?;
        data.insert(
            key.clone(),
            serde_json::json!({ "type": "api", "key": api_key.expose_secret() }),
        );

        self.write(&data)?;
        self.record_stamps(&data, Some(&key));

        Ok(())
    }

    fn remove(&self, provider_id: &str) -> Result<bool, AuthError> {
        let mut data = self.read()?;
        if data.remove(storage_key(provider_id)).is_none() {
            return Ok(false);
        }
        self.write(&data)?;
        // A logout ends the login's seniority with it: logging in again later
        // is a new login and earns a fresh stamp, where a credential merely
        // *replaced* in place keeps the one it had.
        self.record_stamps(&data, None);

        Ok(true)
    }

    /// Every stored provider this build could authenticate with, sorted, and
    /// what it would authenticate with.
    fn stored(&self) -> Result<Vec<(String, RedactedTail, CredentialKind)>, AuthError> {
        Ok(self
            .read()?
            .iter()
            .filter_map(|(provider_id, value)| {
                if let Some(key) = usable_key(value) {
                    return Some((
                        provider_id.clone(),
                        RedactedTail::of_secret(&key),
                        CredentialKind::ApiKey,
                    ));
                }

                usable_oauth(value).map(|credential| {
                    (
                        provider_id.clone(),
                        credential.tail(),
                        CredentialKind::Oauth,
                    )
                })
            })
            .collect())
    }

    /// Where this store's stamps live: [`STAMPS_FILE`], beside it.
    fn stamps_path(&self) -> PathBuf {
        self.path.with_file_name(STAMPS_FILE)
    }

    /// The stamps as they stand, and never an error: the sidecar holds no
    /// secrets, so a file that is missing, unreadable or not the shape it
    /// should be degrades to "nobody is stamped" — the [`UNSTAMPED_PRIORITY`]
    /// order — rather than costing a session its startup. The store itself
    /// hard-fails on corruption because reading past it could hide a
    /// credential; there is nothing here for a failure to hide.
    fn read_stamps(&self) -> BTreeMap<String, u64> {
        let path = self.stamps_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return BTreeMap::new(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the login stamps could not be read");
                return BTreeMap::new();
            }
        };

        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "the login stamps could not be parsed");
            BTreeMap::new()
        })
    }

    /// Brings the stamps up to date with `data`, the store as it was just
    /// written: entries the store no longer holds lose their stamps — opencode
    /// removes credentials without knowing this file exists, so orphans are
    /// pruned here rather than trusted to [`Self::remove`] — and `minted`, when
    /// given, is stamped now **unless it already is**. A credential replaced in
    /// place keeps its seniority; only a logout (which dropped the stamp) makes
    /// the next login a new one.
    ///
    /// A failure is logged and swallowed. The credential write this follows has
    /// already landed, and a login must not be reported failed over the
    /// metadata that merely orders a default.
    fn record_stamps(&self, data: &BTreeMap<String, Value>, minted: Option<&str>) {
        let mut stamps = self.read_stamps();
        stamps.retain(|key, _| data.contains_key(key));
        if let Some(key) = minted {
            stamps.entry(key.to_owned()).or_insert_with(now_ms);
        }

        if let Err(error) = self.write_stamps(&stamps) {
            tracing::warn!(
                path = %self.stamps_path().display(),
                %error,
                "the login stamp could not be recorded; the stored credential is unaffected",
            );
        }
    }

    /// Replaces the sidecar the way [`Self::write`] replaces the store —
    /// sibling plus rename — so a crash cannot leave it torn. The content is
    /// not secret, but the file lives in the store's directory and gets the
    /// store's posture.
    fn write_stamps(&self, stamps: &BTreeMap<String, u64>) -> io::Result<()> {
        let mut json = serde_json::to_vec_pretty(stamps)?;
        json.push(b'\n');

        let temporary = self
            .stamps_path()
            .with_file_name(format!("{STAMPS_FILE}.{}.tmp", std::process::id()));
        write_private(&temporary, &json)?;

        fs::rename(&temporary, self.stamps_path()).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
    }

    /// Every stored provider this build could authenticate with, oldest login
    /// first: stamped entries by their stamps, then the unstamped by
    /// [`UNSTAMPED_PRIORITY`], then the rest in the store's own order.
    fn logins_oldest_first(&self) -> Result<Vec<String>, AuthError> {
        let stamps = self.read_stamps();
        let mut keys: Vec<String> = self
            .stored()?
            .into_iter()
            .map(|(provider_id, _, _)| provider_id)
            .collect();
        keys.sort_by_key(|key| login_rank(key, &stamps));

        Ok(keys)
    }
}

/// The API key an entry carries, when it carries one this build can use.
///
/// An entry storing an empty key is treated as absent rather than as a
/// credential that will fail at the provider with a confusing message.
///
/// The whole file has already been parsed into [`Value`]s by the time this is
/// called, which is what carrying unknown entries through a rewrite costs: for
/// the length of a read, every key in the file exists as a plain `String`
/// inside `serde_json`. Wrapping starts here because this is the first point
/// at which one value is known to be a credential.
fn usable_key(value: &Value) -> Option<SecretString> {
    match serde_json::from_value::<Stored>(value.clone()) {
        // An entry that does not decode at all is somebody else's — upstream
        // filters the same way rather than failing the whole read.
        Ok(Stored::Api { key }) if !is_blank(&key) => Some(key),
        _ => None,
    }
}

/// The OAuth credential an entry carries, when it carries one with a token in
/// it.
///
/// Same tolerance as [`usable_key`], for the same reason: an `oauth` entry
/// whose `expires` is a string, or whose `access` is a number, is a record
/// somebody else's schema wrote and this build has no business failing over.
fn usable_oauth(value: &Value) -> Option<OauthCredential> {
    match serde_json::from_value::<Stored>(value.clone()) {
        Ok(Stored::Oauth(credential)) if credential.is_usable() => Some(credential),
        _ => None,
    }
}

/// The clock `expires` is measured against: milliseconds since the Unix epoch.
///
/// Public because every login flow computes an expiry the same way — `now +
/// expires_in * 1000`, which is what upstream writes (`codex.ts:371`,
/// `xai.ts:571`) — and three of them agreeing by accident is one of them
/// getting it wrong.
///
/// A clock set before 1970 reads as zero rather than failing: a stored expiry
/// then looks like the future, which errs towards using a credential that may
/// have expired instead of refusing one that has not.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// How long before an access token actually expires it is renewed.
///
/// Two minutes, upstream's margin (`plugin/xai.ts:44`): a tool call that starts
/// with a token about to expire would otherwise have to recover from a 401 in
/// the middle of a turn.
pub const REFRESH_SKEW_MS: u64 = 120_000;

/// Trades a spent OAuth credential for a fresh one.
///
/// The implementation is a login flow's: this crate knows when a token needs
/// renewing and where the result is stored, and nothing about which endpoint
/// renews it. Errors should be [`AuthError::ReauthRequired`] when the provider
/// refused the refresh token and [`AuthError::RefreshUnavailable`] when the
/// attempt never got that far, because those are the two things a caller can
/// do something different about.
#[async_trait::async_trait]
pub trait RefreshOauth: Send + Sync {
    /// Renews `credential`, which belongs to `provider_id`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the provider refused or could not be reached.
    async fn refresh(
        &self,
        provider_id: &str,
        credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError>;
}

/// A refresh that failed, as every caller that was waiting on it sees it.
///
/// Cloneable because one refresh serves every caller that asked for it, and an
/// [`AuthError`] carrying an [`io::Error`] is not. The error itself is reached
/// through [`AsRef`], [`Self::kind`], or the `source` chain.
#[derive(Clone, Debug, thiserror::Error)]
#[error(transparent)]
pub struct RefreshError(Arc<AuthError>);

impl RefreshError {
    /// What went wrong, in the terms a caller acts on.
    #[must_use]
    pub fn kind(&self) -> AuthErrorKind {
        self.0.kind()
    }
}

impl AsRef<AuthError> for RefreshError {
    fn as_ref(&self) -> &AuthError {
        &self.0
    }
}

impl From<AuthError> for RefreshError {
    fn from(error: AuthError) -> Self {
        Self(Arc::new(error))
    }
}

/// A refresh in progress, shared by everyone who asked for one.
type Pending = Shared<BoxFuture<'static, Result<OauthCredential, RefreshError>>>;

/// Runs the credential store's blocking file I/O off the async runtime.
///
/// [`Refresher`] sits on the per-request path of every OAuth provider, and
/// every question it answers begins by reading `auth.json` — a `stat`, an open
/// and a read against whatever filesystem the home directory happens to live
/// on, which on a network mount is not a bounded wait. Doing that inline stalls
/// the executor thread it is polled on, and on a single-threaded runtime that
/// is the only thread there is: the frontend's render loop and every other
/// request in the turn stop with it.
///
/// **This is not a cache, and the read must not become one.** Re-reading per
/// request is load-bearing: it is what closes the window in which another ganja
/// process — or another turn in this one — has already renewed the credential
/// this caller is about to spend, and with a rotating refresh token spending a
/// stale one logs somebody out mid-turn. Moving the read off the executor keeps
/// that property exactly and costs a thread hop.
///
/// Key providers are untouched by this: [`credential_for`] is synchronous and
/// is called once at startup, where blocking is what a startup does.
async fn blocking<T, F>(work: F) -> Result<T, AuthError>
where
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        // The pool cancels a task only as the runtime shuts down, and a panic
        // inside `work` is a bug in this module rather than a state the store
        // is in. Either way the file was not read, which is a storage failure
        // and is reported as one: "there is nothing stored" is the single
        // answer that would be wrong, because it reads as "log in again".
        Err(source) => Err(AuthError::Io {
            context: "the credential store could not be read off the runtime".to_owned(),
            source: io::Error::other(source),
        }),
    }
}

/// Renews expired OAuth credentials, once per provider however many callers ask.
///
/// A turn can have several requests in the air at once, and an access token
/// that expires under them would otherwise have each of them mint a new one:
/// with a rotating refresh token — xAI's rotates — the second exchange presents
/// a token the first has already spent, and the provider is right to refuse it.
/// Upstream guards this with a module-scoped promise per provider plugin
/// (`plugin/xai.ts:494-521`, `plugin/openai/codex.ts:362-386`, cleared in a
/// `finally`); this is the same thing, keyed by provider so two providers
/// refreshing at once do not queue behind each other.
///
/// [`Self::shared`] is the process-wide one, which is the scope upstream's
/// module-level promise has. A test wanting its own takes [`Self::new`].
#[derive(Default)]
pub struct Refresher {
    in_flight: Mutex<HashMap<String, Pending>>,
}

impl Refresher {
    /// A refresher with nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The one every provider in this process shares.
    #[must_use]
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<Refresher> = OnceLock::new();

        SHARED.get_or_init(Self::new)
    }

    /// `provider_id`'s stored credential, renewed first if it is due.
    ///
    /// A credential that is still good is returned without `refresh` being
    /// consulted at all. One that is due is renewed exactly once no matter how
    /// many callers arrive while that is happening; they all receive the same
    /// result, and the renewed credential is stored before any of them get it.
    /// The renewal reads the store again before it spends anything, so a
    /// credential renewed since this caller read it — by a caller that was
    /// descheduled past its own turn, or by another process — is used rather
    /// than replaced.
    ///
    /// A store that cannot be written is logged and not returned as a failure.
    /// The credential in hand is valid, the turn depending on it should not die
    /// for a filesystem, and upstream makes the same call explicitly
    /// (`plugin/xai.ts:501-506`: "an auth.set failure leaves the on-disk state
    /// stale but the in-memory result is still valid for this turn"). What is
    /// lost is durability, and the next process to read a stale credential
    /// renews it again.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::NotOauth`] when the provider has no OAuth
    /// credential, a storage error when the file cannot be read, and whatever
    /// `refresh` returned when the renewal failed.
    pub async fn usable(
        &self,
        provider_id: &str,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<OauthCredential, RefreshError> {
        let key = storage_key(provider_id).to_owned();
        // Read once rather than through `Store::oauth`: saying what *is* stored
        // when there is no OAuth credential needs the same entry, and a second
        // read would be a second trip through the permission check as well.
        // Through `blocking`, because this is a file and this line is on the
        // per-request path of every OAuth provider.
        let data = blocking(|| Store::open().and_then(|store| store.read())).await?;
        let entry = data.get(&key);
        let Some(current) = entry.and_then(usable_oauth) else {
            return Err(AuthError::NotOauth {
                provider_id: provider_id.to_owned(),
                found: Stored::describe(entry),
            }
            .into());
        };
        if !current.needs_refresh_for(provider_id, now_ms(), REFRESH_SKEW_MS) {
            return Ok(current);
        }

        let pending = self.enqueue(key.clone(), provider_id.to_owned(), current, refresh);
        let renewed = pending.clone().await;
        self.retire(&key, &pending);

        renewed
    }

    /// The refresh for `key`, joining one already running rather than starting
    /// a second.
    ///
    /// The lock is held only across the map lookup — the future is built, not
    /// awaited, so nothing can block here — and the future itself is not polled
    /// until a caller awaits it.
    fn enqueue(
        &self,
        key: String,
        provider_id: String,
        current: OauthCredential,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Pending {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|poisoned| {
            // A panic while the map was held says nothing about the map: it
            // holds futures, and the panic came from whatever was building one.
            poisoned.into_inner()
        });

        in_flight
            .entry(key.clone())
            .or_insert_with(|| {
                async move {
                    // The credential this was built from was read before the
                    // map was locked, and the two are not one step. A caller
                    // descheduled in between — or another process entirely,
                    // which is a case upstream names and lives with
                    // (`xai.ts:501-506`) — can have renewed it since, and
                    // presenting a refresh token that has already been rotated
                    // away is how a live login gets refused. Reading again
                    // costs one small file and settles it.
                    let reread = blocking({
                        let key = key.clone();
                        move || Store::open().and_then(|store| store.oauth(&key))
                    })
                    .await;
                    let current = match reread {
                        Ok(Some(stored))
                            if !stored.needs_refresh_for(
                                &provider_id,
                                now_ms(),
                                REFRESH_SKEW_MS,
                            ) =>
                        {
                            return Ok(stored);
                        }
                        Ok(Some(stored)) => stored,
                        // Nothing readable to correct it with; the credential
                        // in hand is what there is, and refusing to renew
                        // because a file moved would be the worse answer.
                        Ok(None) | Err(_) => current,
                    };

                    let renewed = refresh
                        .refresh(&provider_id, &current)
                        .await
                        .map_err(RefreshError::from)?
                        .inheriting(&current);

                    // Off the executor for the reason the reads are: this is a
                    // create, a write, an fsync and a rename, and it happens
                    // while every caller that joined this renewal is waiting
                    // on it. Through `renew_oauth`, not `set_oauth`: a refresh
                    // is not a login, and must not mint the stamp a login
                    // would.
                    let written = blocking({
                        let key = key.clone();
                        let renewed = renewed.clone();
                        move || Store::open().and_then(|store| store.renew_oauth(&key, &renewed))
                    })
                    .await;
                    if let Err(error) = written {
                        tracing::warn!(
                            provider = %provider_id,
                            %error,
                            "the renewed credential could not be stored; it is still \
                             good for this process",
                        );
                    }

                    Ok(renewed)
                }
                .boxed()
                .shared()
            })
            .clone()
    }

    /// Drops a finished refresh, so the next caller starts a new one.
    ///
    /// Only if it is still the same one: a caller cancelled mid-flight leaves
    /// its `Shared` in the map for whoever comes next to drive to completion,
    /// and by the time this runs the entry may already have been replaced.
    fn retire(&self, key: &str, pending: &Pending) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if in_flight
            .get(key)
            .is_some_and(|current| Shared::ptr_eq(current, pending))
        {
            in_flight.remove(key);
        }
    }
}

/// Rejects a file anyone but its owner can read.
#[cfg(unix)]
fn check_private(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    let mode = metadata.permissions().mode() & 0o777;

    if mode & SHARED == 0 {
        return Ok(());
    }

    Err(AuthError::Permissions {
        path: path.to_path_buf(),
        mode,
    })
}

/// Rejects a Windows DACL that lets an identity outside the accepted system
/// set reach credential bytes.
#[cfg(windows)]
fn check_private(path: &Path, file: &fs::File) -> Result<(), AuthError> {
    match windows_acl::exposed_grantee(file) {
        Ok(None) => Ok(()),
        Ok(Some(grantee)) => Err(AuthError::Permissions {
            path: path.to_path_buf(),
            grantee,
        }),
        Err(source) => Err(AuthError::Io {
            context: format!("{} could not be inspected", path.display()),
            source,
        }),
    }
}

/// Platforms without unix modes or Windows DACLs retain the old no-op.
#[cfg(not(any(unix, windows)))]
fn check_private(_path: &Path, _metadata: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

/// Creates `path`, failing if anything is already there.
///
/// `create_new` is `O_CREAT | O_EXCL`, which does not follow a symbolic link at
/// the final component. That is the whole point: the temporary file's name is
/// derived from the process id, so anyone sharing the machine can predict it
/// and plant a link pointing at a file of their choosing, and an opening that
/// followed it would write every stored key wherever the link led.
#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    // The mode is set at creation rather than afterwards so that the file is
    // never, even briefly, readable by anyone else.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE)
        .open(path)
}

/// Creates a Windows file with the right to replace its inherited DACL.
#[cfg(windows)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_WRITE, WRITE_DAC};

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_WRITE | WRITE_DAC)
        .open(path)
}

/// Platforms without unix modes or Windows DACLs retain the old open.
#[cfg(not(any(unix, windows)))]
fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Writes `bytes` to a newly created file only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = match create_private(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Either a write that crashed before its rename, or something
            // planted to catch this one. Unlinking the name and creating it
            // again exclusively settles both without widening the window: what
            // is removed is the name, never whatever it pointed at, and a
            // second link planted in between fails the retry outright.
            fs::remove_file(path)?;
            create_private(path)?
        }
        result => result?,
    };
    // `open` masks the mode with the process umask, so a narrow umask could
    // leave the file unreadable to the owner that has to rename and reread it.
    // This is on the descriptor, not the path, so it cannot be redirected.
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(PRIVATE))?;
    // The handle carries WRITE_DAC from its exclusive open, so inheritance is
    // severed before there is a secret byte for another identity to race for.
    #[cfg(windows)]
    windows_acl::seal_private(&file)?;
    file.write_all(bytes)?;

    file.sync_all()
}

#[cfg(windows)]
mod windows_acl {
    use std::{ffi::c_void, fs, io, mem::size_of, os::windows::io::AsRawHandle as _, ptr};

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LocalFree, WIN32_ERROR,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx,
            Authorization::{
                ConvertSidToStringSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GetExplicitEntriesFromAclW,
                GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo, TRUSTEE_IS_NAME, TRUSTEE_IS_SID,
            },
            CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING,
            GetLengthSid, GetTokenInformation, InitializeAcl, IsValidSid, LookupAccountSidW,
            MapGenericMask, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
            SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE,
            WinBuiltinAdministratorsSid, WinLocalSystemSid,
        },
        Storage::FileSystem::{
            FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_READ_ATTRIBUTES, FILE_READ_EA, READ_CONTROL, SYNCHRONIZE,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    /// A kernel handle which is not a borrowed file or process pseudo-handle.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper is constructed only from a successful
            // OpenProcessToken call and owns that one handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Memory an authorization API allocated with LocalAlloc.
    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetSecurityInfo, GetExplicitEntriesFromAclW and
                // ConvertSidToStringSidW return LocalAlloc-owned pointers.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    /// An aligned, self-contained SID.
    pub(super) struct OwnedSid {
        words: Box<[usize]>,
    }

    impl OwnedSid {
        fn zeroed(bytes: u32) -> Self {
            let words = (bytes as usize).div_ceil(size_of::<usize>());
            Self {
                words: vec![0; words].into_boxed_slice(),
            }
        }

        pub(super) fn as_psid(&self) -> PSID {
            self.words.as_ptr().cast_mut().cast()
        }

        pub(super) fn len(&self) -> u32 {
            // SAFETY: every constructor validates or asks Windows to create
            // the SID held by this aligned allocation.
            unsafe { GetLengthSid(self.as_psid()) }
        }

        fn copy_from(source: PSID) -> io::Result<Self> {
            // SAFETY: source comes from a successful token-information call.
            if unsafe { IsValidSid(source) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows returned an invalid user SID",
                ));
            }

            // SAFETY: IsValidSid established that this is a readable SID.
            let length = unsafe { GetLengthSid(source) };
            let sid = Self::zeroed(length);
            // SAFETY: the destination is length bytes and source is a valid SID
            // of exactly that reported length.
            if unsafe { CopySid(length, sid.as_psid(), source) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(sid)
        }

        pub(super) fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Self> {
            let mut length = SECURITY_MAX_SID_SIZE;
            let sid = Self::zeroed(length);
            // SAFETY: the output allocation is SECURITY_MAX_SID_SIZE bytes,
            // the documented upper bound for a well-known SID.
            if unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid.as_psid(), &mut length) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(sid)
        }
    }

    /// An aligned ACL whose length is carried in its own header.
    pub(super) struct OwnedAcl {
        words: Box<[usize]>,
    }

    impl OwnedAcl {
        fn as_mut_ptr(&mut self) -> *mut ACL {
            self.words.as_mut_ptr().cast()
        }

        pub(super) fn as_ptr(&self) -> *const ACL {
            self.words.as_ptr().cast()
        }
    }

    /// The current process token's user, copied out before its token closes.
    pub(super) fn process_user() -> io::Result<OwnedSid> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a borrowed pseudo-handle and token is a
        // valid out pointer receiving an owned handle on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);

        let mut length = 0;
        // SAFETY: a zero-sized first query is how GetTokenInformation reports
        // the required TOKEN_USER allocation.
        let queried =
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
        if queried != 0 {
            return Err(io::Error::other(
                "Windows returned token-user data without a buffer",
            ));
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(source);
        }

        let words = (length as usize).div_ceil(size_of::<usize>());
        let mut information = vec![0usize; words];
        // SAFETY: the aligned buffer is at least length bytes and the token is
        // live for the call.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                information.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: a successful TokenUser query initializes a TOKEN_USER at the
        // start of the suitably aligned buffer.
        let user = unsafe { information.as_ptr().cast::<TOKEN_USER>().read() };
        OwnedSid::copy_from(user.User.Sid)
    }

    /// Builds the one-ACE DACL written by ganja.
    pub(super) fn private_acl(user: &OwnedSid) -> io::Result<OwnedAcl> {
        let bytes = size_of::<ACL>()
            .checked_add(size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>())
            .and_then(|bytes| bytes.checked_add(user.len() as usize))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("the private DACL is too large"))?;
        let words = (bytes as usize).div_ceil(size_of::<usize>());
        let mut acl = OwnedAcl {
            words: vec![0; words].into_boxed_slice(),
        };

        // SAFETY: acl owns an aligned allocation of bytes bytes.
        if unsafe { InitializeAcl(acl.as_mut_ptr(), bytes, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the initialized ACL has room for this SID-bearing ACE and
        // user is a live, valid SID.
        if unsafe {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr(),
                ACL_REVISION,
                0,
                FILE_ALL_ACCESS,
                user.as_psid(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(acl)
    }

    /// Severs inheritance and grants the process user alone full control.
    pub(super) fn seal_private(file: &fs::File) -> io::Result<()> {
        let user = process_user()?;
        let acl = private_acl(&user)?;
        // SAFETY: file is live, its create access included WRITE_DAC, and acl
        // stays live for the duration of SetSecurityInfo.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl.as_ptr(),
                ptr::null(),
            )
        };
        win32(status)
    }

    /// Names the first allow grant that can reach credential bytes and is not
    /// made to one of the three accepted Windows identities.
    pub(super) fn exposed_grantee(file: &fs::File) -> io::Result<Option<String>> {
        let mut dacl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: file is a live handle with READ_CONTROL, and every requested
        // out pointer is either valid or deliberately null.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        let _descriptor = LocalAllocation(descriptor);
        win32(status)?;

        exposed_grantee_in_dacl(dacl)
    }

    pub(super) fn exposed_grantee_in_dacl(dacl: *const ACL) -> io::Result<Option<String>> {
        if dacl.is_null() {
            return Ok(Some("Everyone (NULL DACL)".to_owned()));
        }

        let accepted = [
            process_user()?,
            OwnedSid::well_known(WinLocalSystemSid)?,
            OwnedSid::well_known(WinBuiltinAdministratorsSid)?,
        ];
        let mut count = 0;
        let mut entries: *mut EXPLICIT_ACCESS_W = ptr::null_mut();
        // SAFETY: dacl came from GetSecurityInfo or a validated test builder;
        // the API owns allocation of the returned entry array.
        let status = unsafe { GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries) };
        let _entries = LocalAllocation(entries.cast());
        win32(status)?;
        if count != 0 && entries.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned ACL entries without their storage",
            ));
        }

        for index in 0..count as usize {
            // SAFETY: the API returned count contiguous EXPLICIT_ACCESS_W
            // records and entries is non-null whenever count is non-zero.
            let entry = unsafe { &*entries.add(index) };
            if entry.grfAccessMode == DENY_ACCESS || !reaches_secret(entry.grfAccessPermissions) {
                continue;
            }

            if entry.Trustee.TrusteeForm != TRUSTEE_IS_SID {
                return Ok(Some(trustee_name(entry)));
            }
            let sid = entry.Trustee.ptstrName.cast();
            if accepted
                .iter()
                .any(|allowed| unsafe { EqualSid(sid, allowed.as_psid()) } != 0)
            {
                continue;
            }
            return Ok(Some(account_name(sid)));
        }

        Ok(None)
    }

    fn reaches_secret(mask: u32) -> bool {
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        let mut mapped = mask;
        // SAFETY: both pointers refer to initialized stack values.
        unsafe {
            MapGenericMask(&mut mapped, &mapping);
        }

        // These rights expose ACLs and file metadata, not credential bytes.
        // Everything else is conservatively meaningful: WRITE_DAC or
        // WRITE_OWNER, for example, can be turned into a later read grant.
        let noise = SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_READ_EA;
        mapped & !noise != 0
    }

    fn trustee_name(entry: &EXPLICIT_ACCESS_W) -> String {
        if entry.Trustee.TrusteeForm == TRUSTEE_IS_NAME && !entry.Trustee.ptstrName.is_null() {
            // SAFETY: the authorization API returned a NUL-terminated trustee
            // name for TRUSTEE_IS_NAME.
            return unsafe { wide_z(entry.Trustee.ptstrName) };
        }
        "an unrecognized identity".to_owned()
    }

    fn account_name(sid: PSID) -> String {
        lookup_account_name(sid)
            .or_else(|| sid_string(sid))
            .unwrap_or_else(|| "an unrecognized identity".to_owned())
    }

    fn lookup_account_name(sid: PSID) -> Option<String> {
        let mut name_length = 0;
        let mut domain_length = 0;
        let mut use_kind: SID_NAME_USE = 0;
        // SAFETY: the zero-sized first query asks Windows for both lengths.
        unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid,
                ptr::null_mut(),
                &mut name_length,
                ptr::null_mut(),
                &mut domain_length,
                &mut use_kind,
            );
        }
        if name_length == 0 {
            return None;
        }

        let mut name = vec![0u16; name_length as usize];
        let mut domain = vec![0u16; domain_length as usize];
        let domain_pointer = if domain.is_empty() {
            ptr::null_mut()
        } else {
            domain.as_mut_ptr()
        };
        // SAFETY: both buffers have the lengths supplied by the first query.
        if unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid,
                name.as_mut_ptr(),
                &mut name_length,
                domain_pointer,
                &mut domain_length,
                &mut use_kind,
            )
        } == 0
        {
            return None;
        }

        let name = wide_buffer(&name, name_length);
        let domain = wide_buffer(&domain, domain_length);
        Some(if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        })
    }

    fn sid_string(sid: PSID) -> Option<String> {
        let mut rendered = ptr::null_mut();
        // SAFETY: sid came from a Windows ACL and rendered is a valid out
        // pointer receiving LocalAlloc-owned UTF-16 on success.
        if unsafe { ConvertSidToStringSidW(sid, &mut rendered) } == 0 {
            return None;
        }
        let _rendered = LocalAllocation(rendered.cast());
        // SAFETY: ConvertSidToStringSidW returned a NUL-terminated string.
        Some(unsafe { wide_z(rendered) })
    }

    fn wide_buffer(buffer: &[u16], reported: u32) -> String {
        let used = (reported as usize).min(buffer.len());
        let used = buffer[..used]
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(used);
        String::from_utf16_lossy(&buffer[..used])
    }

    unsafe fn wide_z(value: *const u16) -> String {
        let mut length = 0;
        // SAFETY: the caller supplies a readable NUL-terminated UTF-16 string.
        while unsafe { *value.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the scan above established this many initialized code units.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) })
    }

    fn win32(status: WIN32_ERROR) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

/// The API key `provider_id`'s environment variable carries.
///
/// Surrounding whitespace is trimmed, because a key read out of a file with
/// `$(cat …)` arrives with a newline that would corrupt the request header. An
/// exported-but-empty variable reads as unset: that is how a shell says "not
/// for this command", and it must not shadow a stored key.
fn key_from_env(provider_id: &str) -> Option<SecretString> {
    let mut value = env::var(key_var(provider_id)?).ok()?;
    let trimmed = value.trim();
    let key = (!trimmed.is_empty()).then(|| SecretString::from(trimmed));
    // The copy `env::var` handed back is wiped, so that this module's own
    // plaintext does not outlive the call. The environment block itself still
    // holds the value — that is how it was passed in, and not this module's to
    // clear — so this narrows the exposure rather than ending it.
    value.zeroize();

    key
}

/// Where credentials are stored.
///
/// # Errors
///
/// Returns [`AuthError::Io`] when there is no home directory to resolve the
/// path against.
pub fn store_path() -> Result<PathBuf, AuthError> {
    Ok(Store::open()?.path)
}

/// Where the login stamps live: `STAMPS_FILE`, beside the store. Public for
/// the same reason [`store_path`] is — somebody clearing ganja's state, or a
/// test arranging one, should not have to guess the name.
///
/// # Errors
///
/// Returns [`AuthError::Io`] when there is no home directory to resolve the
/// path against.
pub fn stamps_path() -> Result<PathBuf, AuthError> {
    Ok(Store::open()?.stamps_path())
}

/// Every stored provider this build could authenticate with, by storage key,
/// **oldest login first** — the order selection defaults through when nothing
/// named a provider.
///
/// Stamped logins come first, oldest stamp leading. Logins with no stamp —
/// stored before `STAMPS_FILE` existed, or by opencode, which will never
/// write one — follow in `UNSTAMPED_PRIORITY`'s fixed order, and anything
/// outside that list comes last in the store's own order. Environment keys are
/// deliberately not consulted: an exported variable is a one-shot override,
/// not a login, and [`credential_for`]'s precedence still applies once a
/// provider is chosen.
///
/// # Errors
///
/// Returns [`AuthError`] when the store cannot be read — reported rather than
/// read as "no logins", because "log in again" is the one wrong answer to a
/// store that is sitting right there.
pub fn stored_logins_oldest_first() -> Result<Vec<String>, AuthError> {
    Store::open()?.logins_oldest_first()
}

/// The credential to authenticate `provider_id` with, if there is one.
///
/// The environment is consulted first; only then the stored file.
///
/// # Errors
///
/// Returns [`AuthError`] when the stored file exists but cannot be read,
/// cannot be understood, or is readable by other users. A provider with no
/// credential at all is `Ok(None)`, not an error: choosing what to say about
/// it belongs to the caller.
pub fn credential_for(provider_id: &str) -> Result<Option<Credential>, AuthError> {
    if let Some(api_key) = key_from_env(provider_id) {
        return Ok(Some(Credential { api_key }));
    }

    Store::open()?.get(provider_id)
}

/// Stores `api_key` as `provider_id`'s credential, replacing any it had.
///
/// Credentials belonging to providers this build does not know are left as they
/// were.
///
/// # Errors
///
/// Returns [`AuthError`] when the existing file cannot be read or the new one
/// cannot be written.
pub fn set_credential(
    provider_id: &str,
    api_key: impl Into<SecretString>,
) -> Result<(), AuthError> {
    Store::open()?.set(provider_id, api_key)
}

/// The OAuth credential stored for `provider_id`, if there is one.
///
/// There is no environment variable in front of this the way there is for an
/// API key: a pair of OAuth tokens with an expiry is not something anyone types
/// into a shell, and upstream has no variable for one either.
///
/// The credential comes back whole, unmodelled fields included, so that a
/// caller which stores it again with [`set_oauth`] puts back what it found.
///
/// # Errors
///
/// Returns [`AuthError`] when the stored file cannot be read, understood, or is
/// readable by other users. A provider with no OAuth credential is `Ok(None)`.
pub fn oauth_for(provider_id: &str) -> Result<Option<OauthCredential>, AuthError> {
    Store::open()?.oauth(provider_id)
}

/// Stores `credential` as `provider_id`'s, replacing any credential it had.
///
/// Credentials belonging to other providers are left as they were, including
/// ones this build cannot interpret.
///
/// # Errors
///
/// Returns [`AuthError`] when the existing file cannot be read or the new one
/// cannot be written.
pub fn set_oauth(provider_id: &str, credential: &OauthCredential) -> Result<(), AuthError> {
    Store::open()?.set_oauth(provider_id, credential)
}

/// Stores `credential` as `provider_id`'s renewal, without stamping it as a
/// fresh login the way [`set_oauth`] does.
///
/// [`Refresher`] already writes through this path for the refresh it drives
/// itself; this is the same write exposed for a caller that renews *outside*
/// [`Refresher::usable`]'s own due-check — [`mcp_oauth`]'s 401-triggered
/// re-dial, which forces a refresh because a server's answer said the stored
/// token was bad, not because [`OauthCredential::needs_refresh_for`] agreed
/// yet.
///
/// # Errors
///
/// Returns [`AuthError`] when the existing file cannot be read or the new one
/// cannot be written.
pub fn renew_oauth(provider_id: &str, credential: &OauthCredential) -> Result<(), AuthError> {
    Store::open()?.renew_oauth(provider_id, credential)
}

/// Forgets `provider_id`'s stored credential, reporting whether there was one.
///
/// An environment variable is not this function's to clear, so a provider
/// authenticated that way keeps working; [`list_providers`] shows where a
/// credential is coming from.
///
/// # Errors
///
/// Returns [`AuthError`] when the file cannot be read or rewritten.
pub fn remove_credential(provider_id: &str) -> Result<bool, AuthError> {
    Store::open()?.remove(provider_id)
}

/// Every credential this build holds, and where [`credential_for`] finds each.
///
/// **A provider can appear twice**, once per place it has a credential, with
/// the environment entry first and the stored one carrying the variable that
/// outranks it in [`Entry::shadowed_by`]. The listing used to drop the stored
/// row whenever a variable was exported, which made a login somebody had just
/// completed invisible to the command whose whole job is "what credentials do I
/// have" — measured live with three OAuth records on disk and two rows printed.
/// Precedence is unchanged and lives where it always did, in
/// [`credential_for`]; what changed is that being outranked is now something
/// this reports rather than something it hides.
///
/// Rows are ordered by provider and then by who wins, so a caller taking the
/// first row for a provider still gets the credential that would be used.
///
/// Providers are named by the key they are stored under, which is upstream's
/// name for them — see [`storage_key`], and [`provider_id_for_storage_key`] for
/// a caller that would rather show ganja's.
///
/// # Errors
///
/// Returns [`AuthError`] when the stored file cannot be read.
pub fn list_providers() -> Result<Vec<Entry>, AuthError> {
    // The exported keys, kept as their own list because they answer two
    // questions: they are rows in their own right, and their presence is what
    // makes a stored credential a shadowed one. `key_var` alone could not — it
    // names the variable whether or not anybody exported it.
    let exported: Vec<(&str, &'static str, RedactedTail)> = KEY_VARS
        .iter()
        .filter_map(|(provider_id, variable)| {
            key_from_env(provider_id)
                .map(|key| (*provider_id, *variable, RedactedTail::of_secret(&key)))
        })
        .collect();
    let mut entries: Vec<Entry> = exported
        .iter()
        .map(|(provider_id, variable, tail)| Entry {
            provider_id: (*provider_id).to_owned(),
            tail: tail.clone(),
            source: Source::Environment(variable),
            kind: CredentialKind::ApiKey,
            shadowed_by: None,
        })
        .collect();

    for (provider_id, tail, kind) in Store::open()?.stored()? {
        entries.push(Entry {
            shadowed_by: exported
                .iter()
                .find(|(exported_id, _, _)| *exported_id == provider_id)
                .map(|(_, variable, _)| *variable),
            provider_id,
            tail,
            source: Source::File,
            kind,
        });
    }
    entries.sort_by(|left, right| {
        left.provider_id
            .cmp(&right.provider_id)
            .then(rank(left.source).cmp(&rank(right.source)))
    });

    Ok(entries)
}

/// Where a source sorts among the credentials one provider has: the one that
/// would be used first.
///
/// The same order [`credential_for`] resolves in, written once so that a
/// listing cannot drift from the lookup it describes.
const fn rank(source: Source) -> u8 {
    match source {
        Source::Environment(_) => 0,
        Source::File => 1,
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

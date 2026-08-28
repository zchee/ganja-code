//! What a vendor's rate-limit headers say is left (**D484**,
//! `rate-limit-visibility`).
//!
//! No upstream counterpart: opencode v1.18.22 reads no rate-limit header
//! anywhere — its only use of a rate signal is `retry-after` on a refusal,
//! which [`super::retry`] ports. This module exists because P14 left two holes
//! that wanted a *usage* API ganja holds no credential tier for (**D471**: the
//! 5h/weekly plan meters), and the honest thing every wire already receives on
//! every successful response is this: the account's own remaining budget, said
//! by the vendor, in headers.
//!
//! # Two families, one table
//!
//! The two families spell the same three facts in opposite orders, which is
//! the whole reason `FAMILIES` is a table rather than two parsers:
//!
//! - Anthropic Messages — `anthropic-ratelimit-<kind>-<field>`, e.g.
//!   `anthropic-ratelimit-input-tokens-remaining`, with `reset` an RFC 3339
//!   instant (`2026-08-14T12:34:56Z`).
//! - The `x-ratelimit-*` family every OpenAI-shaped endpoint uses —
//!   `x-ratelimit-<field>-<kind>`, e.g. `x-ratelimit-remaining-tokens`, with
//!   `reset` a *duration from now* the platform spells Go-style (`6m0s`,
//!   `500ms`) and other endpoints spell as bare seconds (`60`).
//!
//! # The other kind of budget: plan limits (**D485**)
//!
//! [`RateWindow`] is a *rate* limit — the tokens-per-minute a request is
//! throttled against. What P14 left unbuilt (**D471**) is the other thing a
//! subscription meters: the plan's own 5h and weekly buckets, which no vendor
//! serves ganja through a usage API it holds a credential tier for. The W-A1
//! probe of 2026-08-14 (`.omc/plans/2026-08-14-usage-meters-cursor-exec.md`)
//! read the header *names* every credential's own responses carry and found
//! two backends saying it in headers after all, so [`PlanWindow`] is that
//! answer's shape — a **sibling** of [`RateWindow`], never a widening of it,
//! because the two measure different things and spell them incompatibly: a
//! rate window is `limit`/`remaining` counts against a clock, a plan window is
//! a percentage against a rolling window that may carry no clock at all.
//!
//! Two families, sourced from the vendors' own public clients rather than
//! guessed from the names the probe saw:
//!
//! - **codex** — `x-codex-{primary,secondary}-{used-percent,window-minutes,
//!   reset-at}`, plus `x-codex-limit-name`, and the same shape under any
//!   `x-<limit-id>-` infix (the probe observed `x-codex-bengalfox-…`). Spec:
//!   `openai/codex`, `codex-rs/codex-api/src/rate_limits.rs`
//!   (`parse_rate_limit_for_limit`, `parse_rate_limit_window`,
//!   `header_name_to_limit_id`) and `codex-rs/protocol/src/protocol.rs`
//!   (`struct RateLimitWindow`), read at tag `rust-v0.148.0-alpha.15`. That
//!   source fixes every value type: `used-percent` is an `f64` **0–100 of the
//!   window consumed** ("Percentage (0-100) of the window that has been
//!   consumed"), `window-minutes` an `i64` of minutes, `reset-at` an `i64` of
//!   **unix seconds** — not RFC 3339 — and `limit-name` a trimmed string.
//! - **github-copilot** — `x-quota-snapshot-<kind>`, whose value is a URL
//!   query string (`&`-joined, `=` between key and value), *not* the
//!   `;`-joined list its shape suggests. Spec:
//!   `microsoft/vscode-copilot-chat`,
//!   `src/platform/chat/common/chatQuotaServiceImpl.ts`
//!   (`ChatQuotaService.processQuotaHeaders`), which reads `ent` (int
//!   entitlement, `-1` meaning unlimited), `ov` (float overage used), `ovPerm`
//!   (`"true"`/`"false"`), `rem` (**float 0–100 of the entitlement
//!   remaining**) and `rst` (a percent-encoded RFC 3339 instant); corroborated
//!   byte-for-byte by captured live responses in `CaddyGlow/ccproxy-api`,
//!   `tests/data/endpoint_samples/copilot_chat_completions*.json`
//!   (`ent=-1&ov=0.0&ovPerm=false&rem=100.0&rst=2025-10-01T00%3A00%3A00Z`).
//!   GitHub documents none of this; the grammar is only as good as those
//!   sources, which is why a snapshot that does not parse is dropped whole.
//!
//! The two disagree about which direction the percentage runs — codex sends
//! *used*, copilot sends *remaining* — so [`PlanWindow::used_percent`] is
//! normalized here, once, and no rendering site ever flips a sign.
//!
//! # What is not invented
//!
//! A backend that sends nothing yields nothing — the D470 rule, restated here
//! because this is the module a lie would start in. A bucket is a bucket when
//! `limit` and `remaining` parse; anything short of those two is dropped with
//! a debug log naming the bucket and nothing else — a header value is a fact
//! about somebody's account.
//!
//! `reset` is **not** one of those two, since P22. It was, and the reasoning
//! was that a window which could never expire is the frozen live-looking meter
//! the P16 pre-mortem names — but the P17 probe read xAI's own headers and
//! found `x-ratelimit-{limit,remaining}-{tokens,requests}` arriving with no
//! `-reset-` field at all, so that rule was throwing away a whole vendor's
//! figures to guard against a staleness that vendor never claimed. A bucket it
//! dated not at all is kept, [`RateWindow::expired`] answers `false` for it,
//! and it lives until [`RateWindows::record`] replaces the set — which is the
//! decay story [`PlanWindow`] has kept since D485, now told once for both.
//!
//! A reset that *was* sent in a spelling this build cannot read is a different
//! thing and still drops its bucket whole: the vendor dated the window, so
//! drawing it as undated would say something false about what arrived.
//!
//! A [`PlanWindow`] keeps that rule where its vendor gives it the material and
//! says so where it does not: codex sends a reset and the window decays past
//! it exactly as a rate bucket does, while a copilot snapshot's `rst` is
//! optional and one that arrives without it has **no clock at all**. Such a
//! window is kept until a later response replaces the set — which
//! [`RateWindows::record`] does wholesale — and every surface that draws it
//! says the vendor reported no reset rather than inventing one. What is never
//! done is manufacturing an expiry so the shape looks uniform.
//!
//! # Per-wire, not per-session
//!
//! The store is the wire's, not a conversation's, because what these headers
//! measure is the *credential's* budget: the same account, the same limits,
//! across every session that credential opens. So it survives a resume — there
//! is nothing session-shaped to clear — and staleness is answered by
//! [`RateWindow::expired`] rather than by session identity.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use jiff::fmt::friendly::SpanParser;
use reqwest::header::HeaderMap;

/// One vendor bucket: how much of one budget is left, and when it refills.
///
/// `kind` is the vendor's own word for the bucket (`requests`, `tokens`,
/// `input-tokens`, `output-tokens`) rather than an enum of ours, because a
/// vendor that adds a fourth bucket tomorrow should show up rather than be
/// discarded by a parser that had never heard of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateWindow {
    /// What the vendor calls this budget, lowercased and hyphenated as sent.
    pub kind: String,
    /// The budget's size for the window.
    pub limit: u64,
    /// What is left of it.
    pub remaining: u64,
    /// When the window refills, when the vendor said.
    ///
    /// **D484**, amended P22: reset became optional for the vendors that send
    /// none — grok, per the P17 probe
    /// (`.omc/plans/2026-08-14-usage-meters-cursor-exec.md`), whose
    /// `x-ratelimit-*` headers carry the two counts and no `-reset-` field.
    /// [`None`] is that vendor's own answer and means this bucket has **no
    /// clock** rather than that it refills now: nothing dated it, so nothing
    /// expires it, and it lives until [`RateWindows::record`] replaces the
    /// whole set. Every surface that draws one says so rather than inventing
    /// an instant.
    pub reset: Option<SystemTime>,
}

impl RateWindow {
    /// Whether `now` is past the moment this window said it would refill.
    ///
    /// An expired bucket is not deleted — the number it carried was true when
    /// it was said — but every surface renders it as expired rather than as a
    /// live figure, which is the whole of the staleness guard.
    ///
    /// A bucket whose vendor sent no reset is **never** expired: nothing dated
    /// it, so nothing may call it stale. It goes when the next response that
    /// speaks replaces the set — [`PlanWindow::expired`]'s answer, for the
    /// same reason, on the sibling shape.
    #[must_use]
    pub fn expired(&self, now: SystemTime) -> bool {
        self.reset.is_some_and(|reset| reset <= now)
    }

    /// How much of the budget is gone, 0.0 through 1.0.
    ///
    /// A `limit` of zero is a vendor saying the budget has no size, which is
    /// not a denominator: it meters as full rather than dividing by nothing.
    #[must_use]
    pub fn used(&self) -> f64 {
        if self.limit == 0 {
            return 1.0;
        }

        1.0 - (self.remaining.min(self.limit) as f64 / self.limit as f64)
    }
}

/// One plan bucket: how much of a *subscription's* own budget is spent, and
/// when — if ever — it refills (**D485**).
///
/// A sibling of [`RateWindow`], not a variant of it: the module docs say why.
/// The fields are the four the two vendor families agree on, each [`Option`]
/// exactly where its vendor may say nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanWindow {
    /// What this bucket is, in the vendor's own words: codex's `primary` and
    /// `secondary` (its 5h and weekly analogues), an infixed family's
    /// `<family> primary`, or a copilot snapshot's kind (`chat`,
    /// `premium_interactions`).
    pub name: String,
    /// How much of the budget is gone, 0 through 100 — **always used**,
    /// whichever direction its vendor sent (see the module docs). Not clamped
    /// at parse, because a vendor saying 103 has said something true about an
    /// account in overage; [`PlanWindow::used`] is what meters it.
    pub used_percent: f64,
    /// How long the rolling window is, when the vendor said. Copilot says
    /// nothing here, so a quota snapshot carries [`None`] rather than a
    /// guessed month.
    pub window_minutes: Option<u64>,
    /// When the window refills, when the vendor said. [`None`] is a real
    /// answer — a copilot snapshot may carry no `rst` — and means this window
    /// has no clock rather than that it refills now.
    pub resets_at: Option<SystemTime>,
    /// The vendor's own label for the limit this window belongs to, when it
    /// sent one (codex's `-limit-name`, e.g. a model family).
    pub limit_name: Option<String>,
}

impl PlanWindow {
    /// Whether `now` is past the moment this window said it would refill.
    ///
    /// A window with no reset is **never** expired: nothing said it would end,
    /// so nothing may say it has. It is replaced by the next response that
    /// speaks, which is the whole of its staleness story.
    #[must_use]
    pub fn expired(&self, now: SystemTime) -> bool {
        self.resets_at.is_some_and(|reset| reset <= now)
    }

    /// How much of the budget is gone, 0.0 through 1.0 — [`RateWindow::used`]'s
    /// shape, so both kinds of budget meter through one rendering path.
    #[must_use]
    pub fn used(&self) -> f64 {
        (self.used_percent / 100.0).clamp(0.0, 1.0)
    }
}

/// The latest buckets one wire has seen, shared with whoever polls it.
///
/// A wire holds one and hands it to `super::open`; the engine reads it back
/// through [`super::Provider::rate_windows`] and
/// [`super::Provider::plan_windows`]. Cheap to clone — every clone is the same
/// store — so a wire's constructor can hand copies out without thinking about
/// it.
///
/// The two sets are held apart because they are refreshed apart: a response
/// carrying rate headers and no plan headers is a vendor saying one thing and
/// not the other, and one lock over both would make the quieter family's
/// answer depend on the louder one's.
#[derive(Clone, Debug, Default)]
pub struct RateWindows {
    latest: Arc<Mutex<Vec<RateWindow>>>,
    plans: Arc<Mutex<Vec<PlanWindow>>>,
}

impl RateWindows {
    /// Replaces what this store holds with what `headers` said.
    ///
    /// A response carrying no rate headers at all **leaves the store alone**
    /// rather than clearing it: a proxy that strips them, or an endpoint that
    /// never sent them, is not a vendor saying the budget is unknown, and the
    /// buckets already held still expire on their own clock. What a response
    /// *does* say replaces the whole set, because the vendor sends its buckets
    /// together and a half-updated set would mix two moments.
    pub fn record(&self, headers: &HeaderMap, now: SystemTime) {
        // The W-A1 probe, on the one seam every wire's response already passes
        // through: names only, never values. See [`header_names`].
        tracing::debug!(
            names = ?header_names(headers),
            "a provider response carried these header names"
        );

        let windows = parse(headers, now);
        if !windows.is_empty() {
            *self.latest.lock().expect("a rate-window store is never poisoned") = windows;
        }

        // Per family, for [`RateWindows`]'s own reason: a backend that sends
        // rate headers and no plan headers has said nothing about the plan.
        let plans = parse_plans(headers, now);
        if !plans.is_empty() {
            *self.plans.lock().expect("a rate-window store is never poisoned") = plans;
        }
    }

    /// What the wire last heard, newest set first-hand.
    #[must_use]
    pub fn latest(&self) -> Vec<RateWindow> {
        self.latest.lock().expect("a rate-window store is never poisoned").clone()
    }

    /// The plan buckets the wire last heard (**D485**), the same way.
    #[must_use]
    pub fn latest_plans(&self) -> Vec<PlanWindow> {
        self.plans.lock().expect("a rate-window store is never poisoned").clone()
    }
}

/// One vendor's header spelling: the prefix, and how the three fields and the
/// bucket name are arranged after it.
struct Family {
    /// What every header in the family starts with.
    prefix: &'static str,
    /// Whether the field name comes before the bucket name
    /// (`x-ratelimit-remaining-tokens`) or after it
    /// (`anthropic-ratelimit-input-tokens-remaining`).
    field_first: bool,
}

/// Every family this build reads, in the order it tries them.
///
/// Probed rather than assumed, and the probe is structural: the Anthropic
/// family is what `api.anthropic.com` documents and sends, the `x-ratelimit-*`
/// family is what every OpenAI-shaped endpoint here can send (the platform
/// backend, xAI, Copilot, a config-declared compat endpoint). A backend whose
/// answer carries neither — the ChatGPT codex backend as observed, cursor's
/// Connect wire, which does not pass through [`super::open`] at all, and the
/// fake provider, which makes no request — parses to nothing and renders
/// nothing. That is the finding, not a gap: the table is prefix-driven, so a
/// backend that starts sending one of these is picked up with no code change.
const FAMILIES: [Family; 2] = [
    Family { prefix: "anthropic-ratelimit-", field_first: false },
    Family { prefix: "x-ratelimit-", field_first: true },
];

/// The three things a bucket needs, in the spelling both families use.
const FIELDS: [&str; 3] = ["limit", "remaining", "reset"];

/// The **names** of the headers a response carried, each once, in the map's
/// own order — the W-A1 probe of
/// `.omc/plans/2026-08-14-usage-meters-cursor-exec.md`.
///
/// **A value is never returned and never logged.** That is a hard rule rather
/// than a style choice: a response header is a place auth-adjacent material
/// arrives — a rotated token, a `set-cookie`, an id that names somebody's
/// account — and the whole of this module's discipline is that a header value
/// is a fact about somebody's account, said above [`parse`] and kept here.
/// `crates/ganja-core/tests/secrets_env.rs`'s canary is only as good as the
/// modules that hand it nothing to catch.
///
/// A name is also the whole of the question the probe asks. Whether the
/// plan-limit meters D471 left unbuilt are implementable per credential
/// (**D485**) is decided by *which spellings* a backend sends: [`FAMILIES`] is
/// prefix-driven, so the next family row is chosen by a name, and how much of
/// anybody's budget is left decides nothing about whether the row exists.
fn header_names(headers: &HeaderMap) -> Vec<&str> {
    // `keys`, not the `(name, value)` iteration `parse` uses: a multi-valued
    // header would otherwise be listed once per value, which reads as a
    // backend sending more than it did.
    headers.keys().map(|name| name.as_str()).collect()
}

/// Every complete bucket `headers` describes, relative to `now`.
///
/// Buckets come back in the order their vendor's names sort, so two responses
/// carrying the same buckets render in the same order.
#[must_use]
pub fn parse(headers: &HeaderMap, now: SystemTime) -> Vec<RateWindow> {
    // Keyed by bucket name so the three headers of one bucket meet, whichever
    // order the response listed them in. `BTreeMap` for the stable order.
    let mut seen: std::collections::BTreeMap<String, [Option<&str>; 3]> =
        std::collections::BTreeMap::new();

    for (name, value) in headers {
        let name = name.as_str();
        let Some((kind, field)) = FAMILIES
            .iter()
            .find_map(|family| family.split(name))
            .filter(|(kind, _)| !kind.is_empty())
        else {
            continue;
        };
        let Ok(value) = value.to_str() else {
            // A header value that is not text cannot be a count or an
            // instant. Named, never quoted.
            tracing::debug!(header = name, "a rate-limit header was not text");
            continue;
        };

        seen.entry(kind.to_owned()).or_default()[field] = Some(value.trim());
    }

    seen.into_iter().filter_map(|(kind, fields)| window(kind, fields, now)).collect()
}

impl Family {
    /// Splits `name` into `(bucket, field index)` when it belongs to this
    /// family, and [`None`] when it does not.
    fn split<'a>(&self, name: &'a str) -> Option<(&'a str, usize)> {
        let rest = name.strip_prefix(self.prefix)?;

        if self.field_first {
            let (field, kind) = rest.split_once('-')?;
            Some((kind, FIELDS.iter().position(|known| *known == field)?))
        } else {
            let (kind, field) = rest.rsplit_once('-')?;
            Some((kind, FIELDS.iter().position(|known| *known == field)?))
        }
    }
}

/// Builds one bucket from its raw values, dropping it — named — when what
/// makes it a bucket is missing or unreadable.
///
/// The two counts are what make it one; the reset is optional on the wire and
/// therefore optional here (see the module docs). What a vendor sent and this
/// build could not read is the one case that still costs the whole bucket.
fn window(kind: String, fields: [Option<&str>; 3], now: SystemTime) -> Option<RateWindow> {
    let [limit, remaining, reset] = fields;
    let (Some(limit), Some(remaining)) = (limit, remaining) else {
        tracing::debug!(bucket = kind, "a rate-limit bucket was incomplete");
        return None;
    };

    let (Ok(limit), Ok(remaining)) = (limit.parse::<u64>(), remaining.parse::<u64>()) else {
        tracing::debug!(bucket = kind, "a rate-limit bucket could not be read");
        return None;
    };

    let reset = match reset {
        Some(sent) => match instant(sent, now) {
            Some(reset) => Some(reset),
            // Dated by its vendor in a spelling this build does not know. Not
            // a clockless bucket — that is a vendor saying nothing, and this
            // one said something — so the whole bucket goes rather than being
            // drawn as undated.
            None => {
                tracing::debug!(bucket = kind, "a rate-limit bucket could not be read");
                return None;
            }
        },
        None => None,
    };

    Some(RateWindow { kind, limit, remaining, reset })
}

/// Every plan bucket `headers` describes, relative to `now` (**D485**).
///
/// The codex families first, in their limit ids' own order, then the copilot
/// snapshots in theirs — both stable, so two responses carrying the same
/// buckets render in the same order.
#[must_use]
pub fn parse_plans(headers: &HeaderMap, now: SystemTime) -> Vec<PlanWindow> {
    let mut plans = codex_plans(headers, now);
    plans.extend(copilot_plans(headers));

    plans
}

/// What every codex header in a family sits under, for the family the account
/// itself is metered by.
const CODEX_PREFIX: &str = "x-codex";

/// The suffix that names a codex limit family, and the whole of how one is
/// discovered — `openai/codex`'s own `header_name_to_limit_id`, which strips
/// exactly this and takes what is left as the family's id. Mirrored rather
/// than narrowed to a known list, because the shadow family the probe saw
/// (`x-codex-bengalfox-…`) is precisely the case a known list would miss.
const CODEX_FAMILY_SUFFIX: &str = "-primary-used-percent";

/// The two windows a codex family carries: the account's short rolling budget
/// and its long one — Claude's 5h and weekly meters, in this vendor's words.
const CODEX_WINDOWS: [&str; 2] = ["primary", "secondary"];

/// Every codex-family plan window `headers` carries.
fn codex_plans(headers: &HeaderMap, now: SystemTime) -> Vec<PlanWindow> {
    // The default family is always tried, exactly as the vendor's client
    // tries it; the rest are whatever the response named. `BTreeSet` for the
    // stable order, and because a family named twice is one family.
    let mut families: std::collections::BTreeSet<&str> =
        std::collections::BTreeSet::from([CODEX_PREFIX]);
    for name in headers.keys() {
        if let Some(family) = name.as_str().strip_suffix(CODEX_FAMILY_SUFFIX)
            && family.len() > "x-".len()
            && family.starts_with("x-")
        {
            families.insert(family);
        }
    }

    let mut plans = Vec::new();
    for family in families {
        let limit_name = header(headers, &format!("{family}-limit-name"))
            .filter(|name| !name.is_empty())
            .map(str::to_owned);

        for window in CODEX_WINDOWS {
            let Some(used_percent) = header(headers, &format!("{family}-{window}-used-percent"))
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|percent| percent.is_finite())
            else {
                continue;
            };
            let window_minutes = header(headers, &format!("{family}-{window}-window-minutes"))
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|minutes| u64::try_from(minutes).ok());
            let resets_at = codex_reset(headers, family, window, now);

            // The vendor's own `has_data` rule: a window that is zero percent
            // spent, over no window, resetting never is a placeholder rather
            // than a budget, and metering it would draw an empty bar for an
            // account nobody has told us anything about.
            if used_percent <= 0.0 && window_minutes.unwrap_or(0) == 0 && resets_at.is_none() {
                tracing::debug!(family, window, "a plan window carried no data");
                continue;
            }

            plans.push(PlanWindow {
                name: codex_name(family, window),
                used_percent,
                window_minutes,
                resets_at,
                limit_name: limit_name.clone(),
            });
        }
    }

    plans
}

/// When a codex window refills.
///
/// `-reset-at` first, which the vendor's own client reads as unix seconds —
/// with RFC 3339 accepted beside it, since the two spellings cannot be
/// mistaken for one another and a backend that switched would otherwise go
/// quiet. `-reset-after-seconds` is the fallback: the probe saw that header on
/// the wire and the vendor's client never reads it, so its grammar is taken
/// from the field of that name in the backend's own OpenAPI model
/// (`openai/codex`,
/// `codex-rs/codex-backend-openapi-models/src/models/rate_limit_window_snapshot.rs`,
/// `reset_after_seconds: i32`) — a count of seconds from now.
fn codex_reset(
    headers: &HeaderMap,
    family: &str,
    window: &str,
    now: SystemTime,
) -> Option<SystemTime> {
    if let Some(at) = header(headers, &format!("{family}-{window}-reset-at")) {
        if let Ok(seconds) = at.parse::<i64>() {
            return u64::try_from(seconds)
                .ok()
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
        }
        if let Some(absolute) = rfc3339(at) {
            return Some(absolute);
        }
    }

    header(headers, &format!("{family}-{window}-reset-after-seconds"))
        .and_then(|after| after.parse::<u64>().ok())
        .and_then(|seconds| now.checked_add(Duration::from_secs(seconds)))
}

/// What to call a codex window on screen.
///
/// The default family is the account's own plan, so its windows are named by
/// the window alone: `codex primary` would name the vendor twice on a bar that
/// already knows which provider it is talking to. Any other family keeps its
/// own id, minus the `codex-` the vendor prefixes it with.
fn codex_name(family: &str, window: &str) -> String {
    let family = family.strip_prefix("x-").unwrap_or(family);
    let family =
        if family == "codex" { "" } else { family.strip_prefix("codex-").unwrap_or(family) };

    if family.is_empty() { window.to_owned() } else { format!("{family} {window}") }
}

/// What every copilot quota header starts with; what follows is the kind of
/// quota the snapshot is about (`chat`, `premium_interactions`).
const QUOTA_SNAPSHOT_PREFIX: &str = "x-quota-snapshot-";

/// Every copilot quota snapshot `headers` carries, in their kinds' own order.
fn copilot_plans(headers: &HeaderMap) -> Vec<PlanWindow> {
    let mut plans: std::collections::BTreeMap<&str, PlanWindow> = std::collections::BTreeMap::new();

    for (name, value) in headers {
        let Some(kind) = name.as_str().strip_prefix(QUOTA_SNAPSHOT_PREFIX) else {
            continue;
        };
        let Ok(value) = value.to_str() else {
            tracing::debug!(bucket = kind, "a quota snapshot was not text");
            continue;
        };

        match quota_snapshot(kind, value) {
            Some(plan) => {
                plans.insert(kind, plan);
            }
            // Dropped whole rather than read halfway: this grammar is
            // documented nowhere GitHub publishes, so a value that does not
            // fit the sourced shape is a value this build cannot claim to
            // understand.
            None => tracing::debug!(bucket = kind, "a quota snapshot could not be read"),
        }
    }

    plans.into_values().collect()
}

/// One `x-quota-snapshot-<kind>` value, in the query-string grammar the module
/// docs cite.
///
/// `rem` is the whole of what makes a bucket: without a percentage there is
/// nothing to meter. `ent`'s `-1` is the vendor's own "unlimited" (VS Code
/// derives its `unlimited` flag as exactly `ent === -1`), and an unlimited
/// entitlement is not a budget — it would meter as permanently empty, which is
/// the same lie in the other direction. `ov`/`ovPerm` are read by that client
/// for an overage display this build does not have, and are left alone rather
/// than parsed into a field nothing renders.
fn quota_snapshot(kind: &str, value: &str) -> Option<PlanWindow> {
    let mut remaining_percent = None;
    let mut entitlement = None;
    let mut resets_at = None;

    for field in value.split('&') {
        let Some((key, raw)) = field.split_once('=') else {
            continue;
        };
        match key {
            "rem" => remaining_percent = raw.parse::<f64>().ok().filter(|value| value.is_finite()),
            "ent" => entitlement = raw.parse::<i64>().ok(),
            "rst" => resets_at = rfc3339(&percent_decode(raw)),
            _ => {}
        }
    }

    let remaining_percent = remaining_percent?;
    if entitlement.is_some_and(|entitlement| entitlement < 0) {
        tracing::debug!(bucket = kind, "a quota snapshot is unlimited, so unmetered");
        return None;
    }

    Some(PlanWindow {
        name: kind.to_owned(),
        // The one place the two families' opposite conventions meet.
        used_percent: 100.0 - remaining_percent,
        // This vendor sends no window length and no name for the limit.
        window_minutes: None,
        resets_at,
        limit_name: None,
    })
}

/// A query-string value with its `%XX` escapes resolved.
///
/// Deliberately *not* a full `application/x-www-form-urlencoded` decode: a
/// `+` stays a `+`, because the only field this build decodes is an RFC 3339
/// instant, whose offset is spelled with one and whose grammar has nowhere to
/// put a space. That is the whole reason this is a named seam rather than the
/// call itself — `serve`'s reader of the other dialect sits one crate away.
fn percent_decode(value: &str) -> Cow<'_, str> {
    percent_encoding::percent_decode_str(value).decode_utf8_lossy()
}

/// One header's value as trimmed text, or [`None`] when it is absent or is not
/// text at all.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

/// Reads a `reset` value in whichever spelling its vendor uses.
///
/// Three are accepted because three are sent: an RFC 3339 instant (Anthropic),
/// a Go-style duration from now (`6m0s`, `500ms` — the OpenAI platform), and
/// bare seconds from now (`60`, what a plainer `x-ratelimit-*` endpoint sends).
/// Anything else is not guessed at.
fn instant(value: &str, now: SystemTime) -> Option<SystemTime> {
    if let Some(absolute) = rfc3339(value) {
        return Some(absolute);
    }

    now.checked_add(elapsed(value)?)
}

/// A duration from now, Go's spelling or a bare count of seconds.
fn elapsed(value: &str) -> Option<Duration> {
    // An empty value is a header the vendor sent without filling in, which is
    // not "refills now" — it is a bucket this build cannot read.
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_millis((seconds * 1_000.0).ceil() as u64));
    }

    let parsed = SpanParser::new().parse_unsigned_duration(value).ok()?;
    let millis = u64::try_from(parsed.as_millis()).ok()?;
    let millis = millis.checked_add(u64::from(parsed.subsec_nanos() % 1_000_000 != 0))?;

    Some(Duration::from_millis(millis))
}

/// Parses the RFC 3339 spelling Anthropic sends: `2026-08-14T12:34:56Z`, with
/// optional fractional seconds and an optional numeric offset.
///
/// Jiff validates the calendar and offset. Fractions are removed before it
/// parses because this meter has always resolved resets to whole seconds; its
/// parser also clamps `:60` to `:59`, so that one spelling is advanced back to
/// the instant the vendor named.
fn rfc3339(value: &str) -> Option<SystemTime> {
    let (date, rest) = value.split_once(['T', 't'])?;
    let (clock, zone) = match rest.find(['Z', 'z']) {
        Some(index) if index + 1 == rest.len() => (&rest[..index], &rest[index..]),
        _ => {
            let index = rest.rfind(['+', '-'])?;
            let (hours, minutes) = rest[index + 1..].split_once(':')?;
            hours.parse::<u32>().ok()?;
            minutes.parse::<u32>().ok()?;
            (&rest[..index], &rest[index..])
        }
    };

    let mut clock = clock.split(':');
    let hour = clock.next()?;
    let minute = clock.next()?;
    let field = clock.next()?;
    if clock.next().is_some() {
        return None;
    }
    // A window that refills 300ms after the named whole second is not useful
    // at finer precision, and dropping it before parsing also preserves the
    // old reader's acceptance of any opaque fractional tail.
    let second = field.split_once('.').map_or(field, |(whole, _)| whole);
    let leap_second = second == "60";
    let normalized = format!("{date}T{hour}:{minute}:{second}{zone}");
    let mut timestamp = normalized.parse::<Timestamp>().ok()?;
    if leap_second {
        timestamp = timestamp.checked_add(Duration::from_secs(1)).ok()?;
    }

    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(timestamp.as_second()).ok()?))
}

#[cfg(test)]
#[path = "rate_tests.rs"]
mod tests;

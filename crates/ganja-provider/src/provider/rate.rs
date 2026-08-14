//! What a vendor's rate-limit headers say is left (**D484**,
//! `rate-limit-visibility`).
//!
//! No upstream counterpart: opencode v1.18.13 reads no rate-limit header
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
//! the whole reason [`FAMILIES`] is a table rather than two parsers:
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

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
/// A wire holds one and hands it to [`super::open`]; the engine reads it back
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
            *self
                .latest
                .lock()
                .expect("a rate-window store is never poisoned") = windows;
        }

        // Per family, for [`RateWindows`]'s own reason: a backend that sends
        // rate headers and no plan headers has said nothing about the plan.
        let plans = parse_plans(headers, now);
        if !plans.is_empty() {
            *self
                .plans
                .lock()
                .expect("a rate-window store is never poisoned") = plans;
        }
    }

    /// What the wire last heard, newest set first-hand.
    #[must_use]
    pub fn latest(&self) -> Vec<RateWindow> {
        self.latest
            .lock()
            .expect("a rate-window store is never poisoned")
            .clone()
    }

    /// The plan buckets the wire last heard (**D485**), the same way.
    #[must_use]
    pub fn latest_plans(&self) -> Vec<PlanWindow> {
        self.plans
            .lock()
            .expect("a rate-window store is never poisoned")
            .clone()
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
    Family {
        prefix: "anthropic-ratelimit-",
        field_first: false,
    },
    Family {
        prefix: "x-ratelimit-",
        field_first: true,
    },
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

    seen.into_iter()
        .filter_map(|(kind, fields)| window(kind, fields, now))
        .collect()
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

    Some(RateWindow {
        kind,
        limit,
        remaining,
        reset,
    })
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
    let family = if family == "codex" {
        ""
    } else {
        family.strip_prefix("codex-").unwrap_or(family)
    };

    if family.is_empty() {
        window.to_owned()
    } else {
        format!("{family} {window}")
    }
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
/// put a space. An escape that is not two hex digits is left as written rather
/// than guessed at.
fn percent_decode(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(index) = rest.find('%') {
        decoded.push_str(&rest[..index]);
        match rest
            .get(index + 1..index + 3)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            Some(byte) => {
                decoded.push(char::from(byte));
                rest = &rest[index + 3..];
            }
            None => {
                decoded.push('%');
                rest = &rest[index + 1..];
            }
        }
    }
    decoded.push_str(rest);

    decoded
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

    // `1h2m3.5s`, `6m0s`, `500ms`: number-then-unit, repeated, no separators.
    let mut millis = 0f64;
    let mut rest = value;
    while !rest.is_empty() {
        let digits = rest
            .find(|character: char| !character.is_ascii_digit() && character != '.')
            .filter(|split| *split > 0)?;
        let (count, tail) = rest.split_at(digits);
        let count: f64 = count.parse().ok()?;
        let unit = tail
            .find(|character: char| character.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit);

        millis += count
            * match unit {
                "ms" => 1.0,
                "s" => 1_000.0,
                "m" => 60_000.0,
                "h" => 3_600_000.0,
                _ => return None,
            };
        rest = tail;
    }

    (millis.is_finite() && millis >= 0.0).then(|| Duration::from_millis(millis.ceil() as u64))
}

/// Parses the RFC 3339 spelling Anthropic sends: `2026-08-14T12:34:56Z`, with
/// optional fractional seconds and an optional numeric offset.
///
/// Hand-rolled for [`super::retry::retry_after`]'s reason — the workspace
/// carries no date crate, and the grammar a vendor actually emits is this
/// narrow.
fn rfc3339(value: &str) -> Option<SystemTime> {
    let (date, rest) = value.split_once(['T', 't'])?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // The offset first, so what is left is only the clock.
    let (clock, offset) = match rest.find(['Z', 'z']) {
        Some(index) if index + 1 == rest.len() => (&rest[..index], 0i64),
        _ => {
            let index = rest.rfind(['+', '-'])?;
            let (hours, minutes) = rest[index + 1..].split_once(':')?;
            let seconds = i64::from(hours.parse::<u32>().ok()?) * 3_600
                + i64::from(minutes.parse::<u32>().ok()?) * 60;
            (
                &rest[..index],
                if rest.as_bytes()[index] == b'-' {
                    -seconds
                } else {
                    seconds
                },
            )
        }
    };

    let mut clock = clock.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    // Fractional seconds are dropped rather than refused: a window that
    // refills 300ms later than the header's whole second is a window this
    // build has no reason to be precise about.
    let field = clock.next()?;
    let second: u64 = field
        .split_once('.')
        .map_or(field, |(whole, _)| whole)
        .parse()
        .ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // `retry`'s, not a copy of it: the two modules read the same dates off
    // the same responses, and one arithmetic is what keeps them agreeing.
    let seconds = super::retry::days_from_civil(year, month, day).checked_mul(86_400)?
        + i64::try_from(hour * 3_600 + minute * 60 + second).ok()?
        - offset;

    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).ok()?))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{PlanWindow, RateWindow, RateWindows, header_names, parse, parse_plans, rfc3339};

    /// A fixed "now" so a duration-spelled reset lands somewhere a test can
    /// name, rather than wherever the clock happens to be.
    const NOW: SystemTime = UNIX_EPOCH;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }

        headers
    }

    /// Anthropic's family: the bucket name sits before the field, and the
    /// reset is an absolute instant.
    #[test]
    fn the_anthropic_family_parses_its_buckets_with_the_field_last() {
        let windows = parse(
            &headers(&[
                ("anthropic-ratelimit-requests-limit", "1000"),
                ("anthropic-ratelimit-requests-remaining", "999"),
                ("anthropic-ratelimit-requests-reset", "1970-01-01T00:01:00Z"),
                ("anthropic-ratelimit-input-tokens-limit", "80000"),
                ("anthropic-ratelimit-input-tokens-remaining", "60000"),
                (
                    "anthropic-ratelimit-input-tokens-reset",
                    "1970-01-01T00:00:30Z",
                ),
            ]),
            NOW,
        );

        assert_eq!(
            windows,
            vec![
                RateWindow {
                    kind: "input-tokens".to_owned(),
                    limit: 80_000,
                    remaining: 60_000,
                    reset: Some(NOW + Duration::from_secs(30)),
                },
                RateWindow {
                    kind: "requests".to_owned(),
                    limit: 1_000,
                    remaining: 999,
                    reset: Some(NOW + Duration::from_secs(60)),
                },
            ],
            "both buckets are read, in their names' own order"
        );
    }

    /// The `x-ratelimit-*` family: the field sits before the bucket name, and
    /// the reset is a duration from now — in either spelling.
    #[test]
    fn the_x_ratelimit_family_parses_its_buckets_with_the_field_first() {
        let windows = parse(
            &headers(&[
                ("x-ratelimit-limit-tokens", "150000"),
                ("x-ratelimit-remaining-tokens", "149000"),
                ("x-ratelimit-reset-tokens", "6m0s"),
                ("x-ratelimit-limit-requests", "500"),
                ("x-ratelimit-remaining-requests", "499"),
                ("x-ratelimit-reset-requests", "120"),
            ]),
            NOW,
        );

        assert_eq!(
            windows,
            vec![
                RateWindow {
                    kind: "requests".to_owned(),
                    limit: 500,
                    remaining: 499,
                    reset: Some(NOW + Duration::from_secs(120)),
                },
                RateWindow {
                    kind: "tokens".to_owned(),
                    limit: 150_000,
                    remaining: 149_000,
                    reset: Some(NOW + Duration::from_secs(360)),
                },
            ],
            "the bare-seconds and Go-duration spellings both land"
        );
    }

    /// A sub-second Go duration is still a duration.
    #[test]
    fn a_millisecond_reset_is_read_as_one() {
        let windows = parse(
            &headers(&[
                ("x-ratelimit-limit-requests", "10"),
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "500ms"),
            ]),
            NOW,
        );

        assert_eq!(windows[0].reset, Some(NOW + Duration::from_millis(500)));
    }

    /// The rule this module exists to keep: nothing is invented.
    #[test]
    fn a_response_carrying_no_rate_headers_yields_no_buckets() {
        assert!(
            parse(&headers(&[("content-type", "text/event-stream")]), NOW).is_empty(),
            "a headerless backend meters nothing"
        );
    }

    /// The two counts make a bucket; the reset is the vendor's to send or not.
    ///
    /// This is grok's shape as the P17 probe read it off `api.x.ai`: the
    /// `x-ratelimit-*` family with both counts per bucket and no `-reset-`
    /// header anywhere. Before P22 the three-field rule dropped every one of
    /// them, so a whole vendor metered as silent.
    #[test]
    fn a_bucket_its_vendor_never_dated_is_kept_clockless() {
        let windows = parse(
            &headers(&[
                ("x-ratelimit-limit-tokens", "150000"),
                ("x-ratelimit-remaining-tokens", "149000"),
                ("x-ratelimit-limit-requests", "500"),
                ("x-ratelimit-remaining-requests", "499"),
            ]),
            NOW,
        );

        assert_eq!(
            windows,
            vec![
                RateWindow {
                    kind: "requests".to_owned(),
                    limit: 500,
                    remaining: 499,
                    reset: None,
                },
                RateWindow {
                    kind: "tokens".to_owned(),
                    limit: 150_000,
                    remaining: 149_000,
                    reset: None,
                },
            ],
            "both of grok's buckets are read, dated by nobody"
        );
    }

    /// One count is still not a bucket: nothing meters against a limit alone.
    #[test]
    fn a_bucket_missing_a_count_is_dropped() {
        for lonely in [
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "999"),
        ] {
            assert!(
                parse(&headers(&[lonely]), NOW).is_empty(),
                "{} alone is half a bucket",
                lonely.0
            );
        }
    }

    /// A full triple is untouched by P22's widening: dated as it always was.
    #[test]
    fn a_bucket_its_vendor_dated_still_carries_that_date() {
        let windows = parse(
            &headers(&[
                ("anthropic-ratelimit-requests-limit", "1000"),
                ("anthropic-ratelimit-requests-remaining", "999"),
                ("anthropic-ratelimit-requests-reset", "1970-01-01T00:01:00Z"),
            ]),
            NOW,
        );

        assert_eq!(windows[0].reset, Some(NOW + Duration::from_secs(60)));
    }

    /// Mixed sets: one vendor answering about two buckets may date one of them
    /// and not the other, and each keeps its own answer.
    #[test]
    fn a_dated_bucket_and_a_clockless_one_survive_the_same_response() {
        let windows = parse(
            &headers(&[
                ("x-ratelimit-limit-requests", "500"),
                ("x-ratelimit-remaining-requests", "499"),
                ("x-ratelimit-reset-requests", "60"),
                ("x-ratelimit-limit-tokens", "150000"),
                ("x-ratelimit-remaining-tokens", "149000"),
            ]),
            NOW,
        );

        assert_eq!(windows.len(), 2, "neither is dropped for the other's sake");
        assert_eq!(windows[0].reset, Some(NOW + Duration::from_secs(60)));
        assert_eq!(windows[1].reset, None);
    }

    /// Garbage in one bucket drops that bucket and leaves its neighbour.
    #[test]
    fn an_unreadable_value_drops_only_its_own_bucket() {
        let windows = parse(
            &headers(&[
                ("anthropic-ratelimit-requests-limit", "not-a-number"),
                ("anthropic-ratelimit-requests-remaining", "999"),
                ("anthropic-ratelimit-requests-reset", "1970-01-01T00:01:00Z"),
                ("anthropic-ratelimit-output-tokens-limit", "16000"),
                ("anthropic-ratelimit-output-tokens-remaining", "8000"),
                (
                    "anthropic-ratelimit-output-tokens-reset",
                    "1970-01-01T00:01:00Z",
                ),
            ]),
            NOW,
        );

        assert_eq!(windows.len(), 1, "the readable bucket survives");
        assert_eq!(windows[0].kind, "output-tokens");
    }

    /// A reset in neither spelling is not guessed at — and, since P22, is not
    /// quietly demoted to a clockless bucket either: this vendor *dated* the
    /// window, so drawing it as undated would misreport what arrived.
    #[test]
    fn a_reset_in_no_known_spelling_drops_its_bucket() {
        for spelling in ["tomorrow", "6x0s", "", "-30"] {
            let mut map = HeaderMap::new();
            map.insert("x-ratelimit-limit-requests", HeaderValue::from_static("10"));
            map.insert(
                "x-ratelimit-remaining-requests",
                HeaderValue::from_static("9"),
            );
            map.insert(
                "x-ratelimit-reset-requests",
                HeaderValue::from_str(spelling).expect("a header value"),
            );

            assert!(
                parse(&map, NOW).is_empty(),
                "{spelling:?} is not a reset this build claims to understand"
            );
        }
    }

    /// A header inside a family but naming no field of ours is ignored rather
    /// than mistaken for a bucket called `overhead`.
    #[test]
    fn a_family_header_naming_no_known_field_is_ignored() {
        assert!(
            parse(
                &headers(&[("x-ratelimit-overhead-tokens", "3"), ("x-ratelimit-", "3")]),
                NOW,
            )
            .is_empty()
        );
    }

    /// The RFC 3339 shapes a vendor actually emits, and the ones it does not.
    #[test]
    fn the_rfc3339_reader_takes_offsets_and_fractions_and_refuses_the_rest() {
        assert_eq!(
            rfc3339("1970-01-01T00:00:01.500Z"),
            Some(UNIX_EPOCH + Duration::from_secs(1)),
            "a fraction is dropped, not refused"
        );
        assert_eq!(
            rfc3339("1970-01-01T01:00:00+01:00"),
            Some(UNIX_EPOCH),
            "an offset is subtracted"
        );
        assert_eq!(
            rfc3339("2026-08-14T12:34:56Z"),
            Some(UNIX_EPOCH + Duration::from_secs(1_786_710_896)),
            "a real instant lands where the civil arithmetic says"
        );
        for refused in ["1970-13-01T00:00:00Z", "1970-01-01 00:00:00Z", "not a date"] {
            assert_eq!(rfc3339(refused), None, "{refused:?} is refused");
        }
    }

    /// The staleness guard, on a bucket manufactured already past its reset.
    #[test]
    fn a_bucket_past_its_reset_reports_itself_expired() {
        let window = RateWindow {
            kind: "requests".to_owned(),
            limit: 100,
            remaining: 3,
            reset: Some(NOW + Duration::from_secs(60)),
        };

        assert!(!window.expired(NOW), "before its reset it is live");
        assert!(
            window.expired(NOW + Duration::from_secs(61)),
            "past its reset it is expired"
        );
    }

    /// The other half of that guard, since P22: a bucket nobody dated cannot
    /// go stale, however long the clock runs.
    #[test]
    fn a_bucket_its_vendor_never_dated_never_expires() {
        let window = RateWindow {
            kind: "requests".to_owned(),
            limit: 100,
            remaining: 3,
            reset: None,
        };

        assert!(!window.expired(NOW), "nothing dated it");
        assert!(
            !window.expired(NOW + Duration::from_secs(86_400 * 365)),
            "and a year of clock does not date it either: only the next \
             response that speaks replaces it"
        );
    }

    /// A limit of zero is a vendor with nothing to divide by.
    #[test]
    fn a_bucket_with_no_size_meters_full_rather_than_dividing_by_zero() {
        let window = RateWindow {
            kind: "requests".to_owned(),
            limit: 0,
            remaining: 0,
            reset: Some(NOW),
        };

        assert!((window.used() - 1.0).abs() < f64::EPSILON);
    }

    /// The W-A1 probe's own rule, pinned on the shape the log line renders:
    /// what the instrument yields is names, and a value never rides along —
    /// neither in the returned list nor in the `?`-formatted debug field
    /// [`RateWindows::record`] logs it through.
    #[test]
    fn the_header_probe_yields_names_and_never_the_values_beside_them() {
        // Header names a real backend sends beside material nobody wants in a
        // log file, each paired with the value that must not appear.
        let sensitive = [
            ("set-cookie", "session=sk-live-do-not-log-me"),
            ("authorization", "Bearer sk-ant-not-a-real-key"),
            ("anthropic-organization-id", "org-0123456789abcdef"),
            ("x-ratelimit-remaining-requests", "9"),
        ];

        let map = headers(&sensitive);
        let names = header_names(&map);
        let rendered = format!("{names:?}");

        for (name, value) in sensitive {
            assert!(
                names.contains(&name),
                "{name} is what the probe exists to report"
            );
            assert!(
                !rendered.contains(value),
                "{name}'s value must not reach a log line"
            );
        }
        assert_eq!(
            names.len(),
            sensitive.len(),
            "each header is named once and nothing else is added"
        );
    }

    /// The store keeps the newest complete set, and a response that said
    /// nothing does not erase what a response that spoke had said.
    #[test]
    fn the_store_keeps_the_newest_set_and_a_silent_response_erases_nothing() {
        let store = RateWindows::default();
        assert!(store.latest().is_empty(), "a fresh store holds nothing");

        store.record(
            &headers(&[
                ("x-ratelimit-limit-requests", "10"),
                ("x-ratelimit-remaining-requests", "9"),
                ("x-ratelimit-reset-requests", "60"),
            ]),
            NOW,
        );
        assert_eq!(store.latest()[0].remaining, 9);

        store.record(
            &headers(&[
                ("x-ratelimit-limit-requests", "10"),
                ("x-ratelimit-remaining-requests", "8"),
                ("x-ratelimit-reset-requests", "60"),
            ]),
            NOW,
        );
        assert_eq!(store.latest()[0].remaining, 8, "the newer set wins");

        store.record(&headers(&[("content-type", "application/json")]), NOW);
        assert_eq!(
            store.latest()[0].remaining,
            8,
            "a response with no rate headers leaves the last real answer alone"
        );
    }

    /// Every value below is manufactured. A real captured percentage is a fact
    /// about the owner's own account and belongs in no repository.
    ///
    /// The codex family, in the shape `openai/codex`'s own client reads: a
    /// percentage of the window *consumed*, a window length in minutes, and a
    /// reset spelled as unix seconds.
    #[test]
    fn the_codex_family_reads_both_its_windows_and_dates_them_from_unix_seconds() {
        let plans = parse_plans(
            &headers(&[
                ("x-codex-primary-used-percent", "12.5"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-at", "3600"),
                ("x-codex-secondary-used-percent", "40"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-at", "86400"),
            ]),
            NOW,
        );

        assert_eq!(
            plans,
            vec![
                PlanWindow {
                    name: "primary".to_owned(),
                    used_percent: 12.5,
                    window_minutes: Some(300),
                    resets_at: Some(UNIX_EPOCH + Duration::from_secs(3_600)),
                    limit_name: None,
                },
                PlanWindow {
                    name: "secondary".to_owned(),
                    used_percent: 40.0,
                    window_minutes: Some(10_080),
                    resets_at: Some(UNIX_EPOCH + Duration::from_secs(86_400)),
                    limit_name: None,
                },
            ],
            "the account's short and long budgets, in the vendor's own words"
        );
    }

    /// The shadow family the probe saw: discovered by its own
    /// `-primary-used-percent`, named by what the vendor infixed, and carrying
    /// the family's `-limit-name` on every window of it.
    #[test]
    fn an_infixed_codex_family_is_discovered_by_its_own_primary_header() {
        let plans = parse_plans(
            &headers(&[
                ("x-codex-primary-used-percent", "10"),
                ("x-codex-bengalfox-primary-used-percent", "80"),
                ("x-codex-bengalfox-limit-name", "  a-model-family  "),
            ]),
            NOW,
        );

        assert_eq!(plans.len(), 2, "both families are read; got {plans:?}");
        assert_eq!(
            plans[1],
            PlanWindow {
                name: "bengalfox primary".to_owned(),
                used_percent: 80.0,
                window_minutes: None,
                resets_at: None,
                limit_name: Some("a-model-family".to_owned()),
            },
            "the infixed family keeps its own id and its trimmed limit name"
        );
        assert_eq!(
            plans[0].name, "primary",
            "and the default family is still named by its window alone"
        );
    }

    /// The header the vendor's client never reads and the wire sends anyway:
    /// seconds from now, tried only once `-reset-at` has said nothing.
    #[test]
    fn a_codex_window_dates_itself_from_reset_after_seconds_when_no_reset_at_arrives() {
        let plans = parse_plans(
            &headers(&[
                ("x-codex-primary-used-percent", "5"),
                ("x-codex-primary-reset-after-seconds", "1800"),
            ]),
            NOW,
        );

        assert_eq!(plans[0].resets_at, Some(NOW + Duration::from_secs(1_800)));
    }

    /// A window that is zero percent spent, over no window, resetting never is
    /// a placeholder — and an empty bar drawn for an account nobody said
    /// anything about is exactly what this module refuses.
    #[test]
    fn a_codex_window_of_nothing_but_zeroes_is_a_placeholder_rather_than_a_budget() {
        assert!(
            parse_plans(
                &headers(&[
                    ("x-codex-primary-used-percent", "0"),
                    ("x-codex-primary-window-minutes", "0"),
                ]),
                NOW,
            )
            .is_empty()
        );
    }

    /// Copilot says how much is *left*; this module stores how much is *gone*,
    /// so no rendering site ever flips a sign. Its reset arrives
    /// percent-encoded.
    #[test]
    fn a_copilot_snapshot_is_read_as_used_where_the_vendor_said_remaining() {
        let plans = parse_plans(
            &headers(&[(
                "x-quota-snapshot-premium_interactions",
                "ent=300&ov=0.0&ovPerm=false&rem=88.5&rst=1970-01-02T00%3A00%3A00Z",
            )]),
            NOW,
        );

        assert_eq!(plans.len(), 1, "got {plans:?}");
        assert_eq!(plans[0].name, "premium_interactions");
        assert!(
            (plans[0].used_percent - 11.5).abs() < 1e-9,
            "88.5 remaining is 11.5 used; got {}",
            plans[0].used_percent
        );
        assert_eq!(
            plans[0].resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(86_400)),
            "the `%3A`-escaped instant is decoded before it is read"
        );
        assert_eq!(
            plans[0].window_minutes, None,
            "this vendor sends no window length, so none is invented"
        );
    }

    /// `rst` is optional in the sourced grammar. A snapshot without it has no
    /// clock — which is a real answer, not a reason to drop the numbers or to
    /// guess a month the way the vendor's own UI does.
    #[test]
    fn a_copilot_snapshot_without_a_reset_keeps_its_numbers_and_no_clock() {
        let plans = parse_plans(
            &headers(&[("x-quota-snapshot-chat", "ent=1000&ov=0.0&rem=25.0")]),
            NOW,
        );

        assert_eq!(plans[0].resets_at, None);
        assert!((plans[0].used_percent - 75.0).abs() < 1e-9);
        assert!(
            !plans[0].expired(NOW + Duration::from_secs(86_400 * 365)),
            "a window nothing dated cannot go stale on its own"
        );
    }

    /// Half a grammar is not half a bucket: a value that does not fit the
    /// sourced shape is dropped whole rather than read as far as it goes.
    #[test]
    fn a_copilot_snapshot_whose_grammar_does_not_parse_is_dropped_whole() {
        for value in [
            // No `rem` at all: nothing to meter.
            "ent=300&ov=0.0&ovPerm=false",
            // `rem` present and unreadable.
            "ent=300&rem=most-of-it",
            // Not this grammar at all — the `;`-joined shape it is not.
            "ent=300;rem=50.0",
            "",
        ] {
            let mut map = HeaderMap::new();
            map.insert(
                "x-quota-snapshot-chat",
                HeaderValue::from_str(value).expect("a header value"),
            );

            assert!(
                parse_plans(&map, NOW).is_empty(),
                "{value:?} is not a snapshot this build claims to understand"
            );
        }
    }

    /// The vendor's own `-1` sentinel: an unlimited entitlement is not a
    /// budget, and metering one would draw a permanently empty bar.
    #[test]
    fn an_unlimited_copilot_entitlement_meters_nothing() {
        assert!(
            parse_plans(
                &headers(&[(
                    "x-quota-snapshot-chat",
                    "ent=-1&ov=0.0&ovPerm=false&rem=100.0",
                )]),
                NOW,
            )
            .is_empty()
        );
    }

    /// D484's decay posture, on the sibling shape: a dated window expires on
    /// its own clock, and an undated one is replaced rather than expiring.
    #[test]
    fn a_plan_window_past_its_reset_reports_itself_expired() {
        let dated = PlanWindow {
            name: "primary".to_owned(),
            used_percent: 90.0,
            window_minutes: Some(300),
            resets_at: Some(NOW + Duration::from_secs(60)),
            limit_name: None,
        };

        assert!(!dated.expired(NOW), "before its reset it is live");
        assert!(dated.expired(NOW + Duration::from_secs(61)));
        assert!(
            (dated.used() - 0.9).abs() < 1e-9,
            "the meter reads the fraction spent"
        );
    }

    /// A percentage past the end of the scale is a vendor talking about an
    /// account in overage; the number is kept and the *meter* is what clamps.
    #[test]
    fn a_plan_window_over_a_hundred_percent_meters_full_without_losing_the_figure() {
        let overage = PlanWindow {
            name: "chat".to_owned(),
            used_percent: 103.0,
            window_minutes: None,
            resets_at: None,
            limit_name: None,
        };

        assert!((overage.used() - 1.0).abs() < f64::EPSILON);
        assert!((overage.used_percent - 103.0).abs() < f64::EPSILON);
    }

    /// The D470 rule on the new family: the backends the probe found silent
    /// meter nothing, and the rate family alone is not a plan family.
    #[test]
    fn a_response_carrying_no_plan_headers_yields_no_plan_windows() {
        assert!(
            parse_plans(
                &headers(&[
                    ("content-type", "text/event-stream"),
                    ("anthropic-ratelimit-requests-limit", "1000"),
                    ("anthropic-ratelimit-requests-remaining", "999"),
                    ("anthropic-ratelimit-requests-reset", "1970-01-01T00:01:00Z"),
                ]),
                NOW,
            )
            .is_empty(),
            "a rate-limit header is not a plan meter"
        );
    }

    /// The two sets are refreshed apart: a response that spoke about one says
    /// nothing about the other, and must not clear it.
    #[test]
    fn the_store_holds_the_two_families_apart() {
        let store = RateWindows::default();
        assert!(
            store.latest_plans().is_empty(),
            "a fresh store holds nothing"
        );

        store.record(
            &headers(&[
                ("x-codex-primary-used-percent", "20"),
                ("x-codex-primary-reset-at", "3600"),
            ]),
            NOW,
        );
        assert_eq!(store.latest_plans().len(), 1);
        assert!(
            store.latest().is_empty(),
            "a plan-only response invents no rate bucket"
        );

        store.record(
            &headers(&[
                ("x-ratelimit-limit-requests", "10"),
                ("x-ratelimit-remaining-requests", "9"),
                ("x-ratelimit-reset-requests", "60"),
            ]),
            NOW,
        );
        assert_eq!(
            store.latest_plans().len(),
            1,
            "a rate-only response leaves the plan set alone"
        );
        assert_eq!(store.latest().len(), 1, "and lands its own buckets");

        store.record(
            &headers(&[
                ("x-codex-primary-used-percent", "35"),
                ("x-codex-primary-reset-at", "3600"),
            ]),
            NOW,
        );
        assert!(
            (store.latest_plans()[0].used_percent - 35.0).abs() < f64::EPSILON,
            "the newer plan set wins"
        );
    }
}

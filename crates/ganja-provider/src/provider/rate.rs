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
//! # What is not invented
//!
//! A backend that sends nothing yields nothing — the D470 rule, restated here
//! because this is the module a lie would start in. A bucket is only a bucket
//! when all three of `limit`, `remaining` and `reset` parse: a window with no
//! reset could never expire, and a number that can never expire is exactly the
//! frozen live-looking meter the P16 pre-mortem names. Anything short of three
//! is dropped with a debug log naming the bucket and nothing else — a header
//! value is a fact about somebody's account.
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
    /// When the window refills. Always present: see the module docs.
    pub reset: SystemTime,
}

impl RateWindow {
    /// Whether `now` is past the moment this window said it would refill.
    ///
    /// An expired bucket is not deleted — the number it carried was true when
    /// it was said — but every surface renders it as expired rather than as a
    /// live figure, which is the whole of the staleness guard.
    #[must_use]
    pub fn expired(&self, now: SystemTime) -> bool {
        self.reset <= now
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

/// The latest buckets one wire has seen, shared with whoever polls it.
///
/// A wire holds one and hands it to [`super::open`]; the engine reads it back
/// through [`super::Provider::rate_windows`]. Cheap to clone — every clone is
/// the same store — so a wire's constructor can hand copies out without
/// thinking about it.
#[derive(Clone, Debug, Default)]
pub struct RateWindows {
    latest: Arc<Mutex<Vec<RateWindow>>>,
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
        if windows.is_empty() {
            return;
        }

        *self
            .latest
            .lock()
            .expect("a rate-window store is never poisoned") = windows;
    }

    /// What the wire last heard, newest set first-hand.
    #[must_use]
    pub fn latest(&self) -> Vec<RateWindow> {
        self.latest
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

/// Builds one bucket from its three raw values, dropping it — named — when any
/// of them is missing or unreadable.
fn window(kind: String, fields: [Option<&str>; 3], now: SystemTime) -> Option<RateWindow> {
    let [limit, remaining, reset] = fields;
    let (Some(limit), Some(remaining), Some(reset)) = (limit, remaining, reset) else {
        tracing::debug!(bucket = kind, "a rate-limit bucket was incomplete");
        return None;
    };

    let (Ok(limit), Ok(remaining), Some(reset)) = (
        limit.parse::<u64>(),
        remaining.parse::<u64>(),
        instant(reset, now),
    ) else {
        tracing::debug!(bucket = kind, "a rate-limit bucket could not be read");
        return None;
    };

    Some(RateWindow {
        kind,
        limit,
        remaining,
        reset,
    })
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

    let seconds = days_from_civil(year, month, day).checked_mul(86_400)?
        + i64::try_from(hour * 3_600 + minute * 60 + second).ok()?
        - offset;

    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// Days between 1970-01-01 and `year-month-day`, by Howard Hinnant's
/// `days_from_civil` — the same arithmetic [`super::retry`] uses for HTTP
/// dates, repeated rather than shared because that one is private to a module
/// whose subject is refusals.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * i64::from(if month > 2 { month - 3 } else { month + 9 }) + 2) / 5
        + i64::from(day)
        - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{RateWindow, RateWindows, header_names, parse, rfc3339};

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
                    reset: NOW + Duration::from_secs(30),
                },
                RateWindow {
                    kind: "requests".to_owned(),
                    limit: 1_000,
                    remaining: 999,
                    reset: NOW + Duration::from_secs(60),
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
                    reset: NOW + Duration::from_secs(120),
                },
                RateWindow {
                    kind: "tokens".to_owned(),
                    limit: 150_000,
                    remaining: 149_000,
                    reset: NOW + Duration::from_secs(360),
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

        assert_eq!(windows[0].reset, NOW + Duration::from_millis(500));
    }

    /// The rule this module exists to keep: nothing is invented.
    #[test]
    fn a_response_carrying_no_rate_headers_yields_no_buckets() {
        assert!(
            parse(&headers(&[("content-type", "text/event-stream")]), NOW).is_empty(),
            "a headerless backend meters nothing"
        );
    }

    /// Three fields or no bucket — a window that could never expire is the
    /// frozen meter the pre-mortem names.
    #[test]
    fn a_bucket_missing_its_reset_is_dropped_rather_than_left_unexpiring() {
        assert!(
            parse(
                &headers(&[
                    ("anthropic-ratelimit-requests-limit", "1000"),
                    ("anthropic-ratelimit-requests-remaining", "999"),
                ]),
                NOW,
            )
            .is_empty(),
            "two of three fields is not a bucket"
        );
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

    /// A reset in neither spelling is not guessed at.
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
            reset: NOW + Duration::from_secs(60),
        };

        assert!(!window.expired(NOW), "before its reset it is live");
        assert!(
            window.expired(NOW + Duration::from_secs(61)),
            "past its reset it is expired"
        );
    }

    /// A limit of zero is a vendor with nothing to divide by.
    #[test]
    fn a_bucket_with_no_size_meters_full_rather_than_dividing_by_zero() {
        let window = RateWindow {
            kind: "requests".to_owned(),
            limit: 0,
            remaining: 0,
            reset: NOW,
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
}

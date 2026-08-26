use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};

use super::{
    PlanWindow, RateWindow, RateWindows, elapsed, header_names, parse, parse_plans, percent_decode,
    rfc3339,
};

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

/// The OpenAI-shaped reset grammar, including its ceiling to the next
/// whole millisecond.
#[test]
fn the_elapsed_reader_keeps_the_vendor_grammar_and_rounding() {
    for (value, expected) in [
        ("1h2m3.5s", Some(Duration::from_millis(3_723_500))),
        ("500ms", Some(Duration::from_millis(500))),
        ("60", Some(Duration::from_secs(60))),
        ("1.5", Some(Duration::from_millis(1_500))),
        ("0.0001s", Some(Duration::from_millis(1))),
        ("", None),
        ("6x0s", None),
        ("-1", None),
        ("NaN", None),
        ("inf", None),
    ] {
        assert_eq!(elapsed(value), expected, "parsing {value:?}");
    }
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
    assert_eq!(
        rfc3339("1970-01-01T00:00:60Z"),
        Some(UNIX_EPOCH + Duration::from_secs(60)),
        "a leap-second-shaped field names the next minute"
    );
    for refused in [
        "1970-13-01T00:00:00Z",
        "1970-01-01T00:00:61Z",
        "1970-01-01 00:00:00Z",
        "not a date",
    ] {
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

/// The escapes carry whatever the vendor put in the field, and a field is
/// bytes rather than code points: reading each escaped byte as its own
/// character turns any multi-byte sequence into mojibake.
#[test]
fn a_percent_encoded_utf8_sequence_decodes_to_its_character() {
    assert_eq!(percent_decode("%E2%9C%93"), "\u{2713}");
    assert_eq!(
        percent_decode("1970-01-02T00%3A00%3A00Z"),
        "1970-01-02T00:00:00Z",
        "the ASCII escapes this build actually meets are unchanged"
    );
    assert_eq!(
        percent_decode("+00%3A00%zz"),
        "+00:00%zz",
        "a `+` is not a space here, and half an escape is not a guess"
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

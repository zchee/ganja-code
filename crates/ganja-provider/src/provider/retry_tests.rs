use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};

use super::{MAX_ATTEMPTS, MAX_DELAY, delay, http_date, jitter, retry_after, scattered, summarize};
use crate::provider::ProviderError;

/// One `retry-after` case: what it is called, the headers a response
/// carried, and how long they ask to be left alone for.
type HeaderCase = (
    &'static str,
    Vec<(&'static str, &'static str)>,
    Option<Duration>,
);

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(*name, HeaderValue::from_str(value).expect("a header value"));
    }

    headers
}

/// Pins the schedule ported from upstream `session/retry.ts`.
#[test]
fn the_backoff_doubles_from_two_seconds_and_stops_at_thirty() {
    let scheduled: Vec<Duration> = (1..=8).map(|attempt| delay(attempt, None)).collect();

    assert_eq!(
        scheduled,
        vec![
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ]
    );

    // The last scheduled retry is the one that touches the ceiling, so
    // upstream's larger header-present cap changes at most that delay.
    assert!(
        (1..MAX_ATTEMPTS - 1).all(|attempt| delay(attempt, None) < MAX_DELAY)
            && delay(MAX_ATTEMPTS - 1, None) == MAX_DELAY,
        "the attempt budget reaches the cap exactly once, at its tail"
    );
}

#[test]
fn a_requested_delay_wins_over_the_schedule_but_not_over_the_ceiling() {
    assert_eq!(
        delay(1, Some(Duration::from_secs(7))),
        Duration::from_secs(7)
    );
    assert_eq!(delay(1, Some(Duration::from_secs(600))), MAX_DELAY);
}

#[test]
fn jitter_only_ever_extends_a_delay() {
    for _ in 0..64 {
        let extended = jitter(Duration::from_secs(10));

        assert!(
            (Duration::from_secs(10)..=Duration::from_millis(12_500)).contains(&extended),
            "jitter should add at most a quarter, got {extended:?}"
        );
    }

    assert_eq!(
        jitter(Duration::from_millis(3)),
        Duration::from_millis(3),
        "a delay too short to jitter is left alone"
    );
}

/// Both ends of the span, which sampling a live draw sixty-four times can
/// only ever suggest.
#[test]
fn the_span_a_draw_walks_is_the_scheduled_quarter_and_no_more() {
    let base = Duration::from_secs(10);

    assert_eq!(scattered(base, 0), base, "the smallest draw adds nothing");
    assert_eq!(
        scattered(base, 2_500),
        Duration::from_millis(12_500),
        "a draw at the top of the span adds the whole quarter"
    );
    assert_eq!(
        scattered(base, 2_501),
        base,
        "and the span wraps rather than spilling past it"
    );
    assert!(
        scattered(MAX_DELAY, u64::MAX) <= MAX_DELAY,
        "the ceiling outranks the scatter"
    );
}

/// The message classification ported from upstream's
/// `RETRYABLE_MESSAGE_PATTERNS` (v1.18.22): each row is one pattern
/// family, and the refusals below are the reason the list is patterns
/// rather than substrings.
#[test]
fn a_message_naming_a_transient_condition_is_worth_another_attempt() {
    for transient in [
        "Provider returned error (status 502)",
        "rate increased too quickly, please slow down",
        "The model is currently overloaded. Try again shortly.",
        "socket hang up",
        "getaddrinfo ENOTFOUND api.example.test",
        "timeout",
        "the request timed out after 30s",
        "Please try again later.",
        "Our servers are temporarily at capacity.",
        "Resource exhausted: out of quota for this minute",
    ] {
        assert!(
            super::transient_message(transient),
            "upstream retries this: {transient}"
        );
    }

    for lasting in [
        "invalid api key",
        "model not found",
        "context length exceeded",
        // The anchored `^timeout$` and the bounded phrases must not turn
        // every sentence containing the word into a retry.
        "set the timeout in your config",
        "do not try again with the same key",
    ] {
        assert!(
            !super::transient_message(lasting),
            "no pattern should match: {lasting}"
        );
    }
}

#[test]
fn the_retry_after_headers_are_read_the_way_upstream_reads_them() {
    // Sixty seconds before the date the cases below quote.
    let now = UNIX_EPOCH + Duration::from_secs(784_111_717);

    let cases: Vec<HeaderCase> = vec![
        ("nothing to read", vec![], None),
        (
            "milliseconds win over seconds",
            vec![("retry-after-ms", "1500"), ("retry-after", "60")],
            Some(Duration::from_millis(1_500)),
        ),
        (
            "fractional milliseconds round up",
            vec![("retry-after-ms", "1500.5")],
            Some(Duration::from_millis(1_501)),
        ),
        (
            "seconds",
            vec![("retry-after", "3")],
            Some(Duration::from_secs(3)),
        ),
        (
            "fractional seconds",
            vec![("retry-after", "0.25")],
            Some(Duration::from_millis(250)),
        ),
        (
            "an http date in the future",
            vec![("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT")],
            Some(Duration::from_secs(60)),
        ),
        (
            "an http date already past",
            vec![("retry-after", "Sun, 06 Nov 1994 08:47:37 GMT")],
            None,
        ),
        ("nonsense", vec![("retry-after", "soon")], None),
        (
            "a negative count is not a delay",
            vec![("retry-after", "-5")],
            None,
        ),
    ];

    for (name, pairs, expected) in cases {
        assert_eq!(retry_after(&headers(&pairs), now), expected, "{name}");
    }
}

#[test]
fn http_dates_parse_across_leap_years_and_centuries() {
    let cases = [
        ("Thu, 01 Jan 1970 00:00:00 GMT", Some(0)),
        ("Sun, 06 Nov 1994 08:49:37 GMT", Some(784_111_777)),
        ("Tue, 29 Feb 2000 12:00:00 GMT", Some(951_825_600)),
        ("Mon, 01 Mar 2100 00:00:00 GMT", Some(4_107_542_400)),
        ("Sun, 06 Nov 1994 08:49:37 UTC", None),
        ("Sun, 06 Nov 1994 08:49 GMT", None),
        ("Sun, 06 Xyz 1994 08:49:37 GMT", None),
        ("", None),
    ];

    for (value, expected) in cases {
        let parsed = http_date(value).map(|time| {
            time.duration_since(UNIX_EPOCH)
                .expect("the fixtures are all after the epoch")
                .as_secs()
        });

        assert_eq!(parsed, expected, "parsing {value:?}");
    }
}

/// RFC 9110 lets a sender spell the leap second, and this build reads it
/// as the instant it names: a refusal here would turn a real `Retry-After`
/// into no delay at all, which is the one answer the header rules out.
#[test]
fn a_leap_second_names_the_instant_past_the_minute_rather_than_a_refusal() {
    let parsed = http_date("Sat, 31 Dec 2016 23:59:60 GMT").map(|time| {
        time.duration_since(UNIX_EPOCH)
            .expect("the fixture is after the epoch")
            .as_secs()
    });

    assert_eq!(parsed, Some(1_483_228_800));
    assert!(
        httpdate::parse_http_date("Sat, 31 Dec 2016 23:59:60 GMT").is_err(),
        "the branch above is only here because the crate refuses this form"
    );
}

#[test]
fn only_transient_failures_are_worth_repeating() {
    assert!(
        ProviderError::Status {
            status: 429,
            message: String::new()
        }
        .is_retryable()
    );
    assert!(
        ProviderError::Status {
            status: 529,
            message: String::new()
        }
        .is_retryable()
    );
    assert!(
        !ProviderError::Status {
            status: 400,
            message: String::new()
        }
        .is_retryable()
    );
    assert!(
        !ProviderError::Status {
            status: 401,
            message: String::new()
        }
        .is_retryable()
    );
    assert!(ProviderError::Transport(String::new()).is_retryable());
    assert!(!ProviderError::Auth(String::new()).is_retryable());
    assert!(!ProviderError::Parse(String::new()).is_retryable());
}

#[test]
fn an_error_body_is_trimmed_to_something_a_status_bar_can_hold() {
    assert_eq!(summarize("   "), "no error body");
    assert_eq!(summarize(" overloaded "), "overloaded");

    let long = summarize(&"é".repeat(1_000));
    assert!(long.ends_with('…'));
    assert_eq!(long.chars().count(), 401);
}

#[test]
fn the_clock_only_matters_relative_to_now() {
    let now = SystemTime::now();
    let soon = retry_after(&headers(&[("retry-after", "2")]), now);

    assert_eq!(soon, Some(Duration::from_secs(2)));
}

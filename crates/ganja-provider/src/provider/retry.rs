//! Retry policy for the request that opens a turn.
//!
//! Only the initial request is retried, and only before the first byte of the
//! response body arrives. Once a provider has started streaming, a failure is
//! reported as [`ProviderEvent::Failed`](super::ProviderEvent::Failed) instead:
//! replaying the request would either duplicate the text already rendered or
//! silently discard it, and the session layer is the only thing that knows
//! which of those the user wants. Upstream retries the whole stream because its
//! session layer owns the transcript rewind; ganja's does not, yet.
//!
//! One bounded exception (**D475**): a retryable failure that arrives *inside*
//! the stream but **before any content event** — the codex backend's overload
//! wears exactly that shape — has rendered nothing to duplicate or discard,
//! so `provider::settled` reopens the request up to three times on this
//! module's schedule before the failure is allowed to end the turn.
//!
//! The delays are ported from upstream `packages/opencode/src/session/retry.ts`
//! (v1.18.22): 2s initial, doubling, jittered by up to a quarter, capped at
//! 30s and at five retries, with `retry-after-ms` and `retry-after` honoured
//! ahead of the schedule — and, past the statuses, that file's
//! `RETRYABLE_MESSAGE_PATTERNS`, because a transient condition does not
//! always arrive under a status anyone can classify.

use std::{
    fmt::Write as _,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use regex::Regex;
use reqwest::header::HeaderMap;
use secrecy::zeroize::Zeroize as _;
use tokio_util::sync::CancellationToken;

use crate::provider::{Presented, ProviderError};

/// Delay before the first retry. Upstream `RETRY_INITIAL_DELAY`.
const INITIAL_DELAY: Duration = Duration::from_millis(2_000);

/// Multiplier applied per attempt. Upstream `RETRY_BACKOFF_FACTOR`.
const BACKOFF_FACTOR: u32 = 2;

/// Ceiling on a scheduled delay. Upstream `RETRY_MAX_DELAY_NO_HEADERS`.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// Attempts, including the first.
///
/// Upstream capped its schedule at v1.18.22 (`RETRY_MAX_RETRIES`: five
/// retries after the first attempt); until then ganja bounded what upstream
/// left unbounded, one retry short of this number. Five retries spend at most
/// 2 + 4 + 8 + 16 + 30 = 60 seconds of scheduled delay.
pub const MAX_ATTEMPTS: u32 = 6;

/// Statuses worth sending the same request again for: rate limits, transient
/// gateway failures, the two gateway-timeout spellings upstream's own pattern
/// list names (504, and Cloudflare's 524), and Anthropic's 529 "overloaded".
pub const RETRYABLE_STATUS: [u16; 7] = [429, 500, 502, 503, 504, 524, 529];

/// Fraction of the scheduled delay that jitter may add, in percent.
///
/// Upstream's `RETRY_JITTER_FACTOR` (0.25, v1.18.22); ganja carried its own
/// ten percent before upstream had any, and now follows the vendor's number.
/// Jitter only ever extends a delay — and never a server-requested
/// `retry-after`, which upstream's `exponential` leaves exact too.
const JITTER_PERCENT: u32 = 25;

/// The error messages worth retrying on regardless of status, ported verbatim
/// from upstream `session/retry.ts` (`RETRYABLE_MESSAGE_PATTERNS`, v1.18.22):
/// a vendor's transient condition often arrives with a status this build
/// cannot classify — an in-body error object, a gateway's own spelling — and
/// the message is then the only thing that says "again might work".
static TRANSIENT_MESSAGES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"429|500|502|503|504|524",
        r"rate increased too quickly|rate limit|rate-limit|rate_limit|too many requests",
        concat!(
            "overloaded|service unavailable|service_unavailable|service-unavailable|",
            "internal error|internal_error|internal server error|server error|server_error|",
            "server-error|provider returned error|provider_returned_error|provider-returned-error",
        ),
        concat!(
            r"terminated|fetch failed|failed to fetch|network[-_\s]error|upstream connect|",
            "connection error|connection refused|connection lost|socket connection was closed|",
            "socket hang up|reset before headers|getaddrinfo|enotfound|eai_again|econnrefused|",
            "econnreset|etimedout",
        ),
        concat!(
            r"^timeout$",
            r"|\b(?:request|response|connection|network|stream|read) (?:timeout|timed out|time out)\b",
        ),
        r"try your request again|retry your request|resource exhausted|resource_exhausted",
        r"\btry again (?:later|in\b)|\b(?:currently|temporarily) at capacity\b",
    ]
    .into_iter()
    .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("upstream's own patterns compile"))
    .collect()
});

/// Whether `message` names a condition upstream's pattern list calls
/// transient.
fn transient_message(message: &str) -> bool {
    TRANSIENT_MESSAGES
        .iter()
        .any(|pattern| pattern.is_match(message))
}

/// Longest error body kept for a status message; a status bar cannot hold more
/// and a provider's HTML error page is not worth a megabyte of transcript.
const BODY_LIMIT: usize = 400;

/// The delay before in-body retry `attempt` (1-based), for the one exception
/// the module doc names (**D475**): the ported schedule, headerless, because
/// an error that arrived inside a 200 stream carried no `retry-after`.
pub(super) fn stream_backoff(attempt: u32) -> Duration {
    INITIAL_DELAY
        .saturating_mul(BACKOFF_FACTOR.saturating_pow(attempt.saturating_sub(1)))
        .min(MAX_DELAY)
}

impl ProviderError {
    /// Whether sending the request again could plausibly succeed.
    ///
    /// This classifies the error, not the moment: the retry driver applies it
    /// only to the request that opens a turn.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            // By the status, or by what the body said (v1.18.22's message
            // patterns): an in-body failure is synthesized as a 500 here, but
            // a vendor proxying somebody else's refusal can surface a
            // transient condition under a status this list has never met.
            Self::Status { status, message } => {
                RETRYABLE_STATUS.contains(status) || transient_message(message)
            }
            Self::Transport(_) => true,
            Self::Auth(_) | Self::Parse(_) => false,
        }
    }
}

/// Sends `request`, retrying it while the failure looks transient.
///
/// `presented` is scrubbed from anything that ends up in the returned error, so
/// a provider that echoes the credential it rejected cannot leak it into a log.
///
/// # Errors
///
/// Returns [`ProviderError::Status`] once a non-2xx response is final,
/// [`ProviderError::Transport`] when the request never completes, and the same
/// on cancellation — callers check the token and turn that into an empty
/// stream, because a cancelled turn is not a failed one.
pub(super) async fn send(
    client: &reqwest::Client,
    request: reqwest::Request,
    presented: &Presented,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1;

    loop {
        // A body that cannot be replayed cannot be retried. Buffered bodies
        // clone and take the retry loop below; a streamed body (cursor's
        // duplex run) lands here by design and is sent exactly once — its
        // wire owns the retry schedule instead.
        let Some(replay) = request.try_clone() else {
            return match client.execute(request).await {
                Ok(response) if response.status().is_success() => Ok(response),
                Ok(response) => Err(refusal(response, presented).await),
                Err(error) => Err(transport(error)),
            };
        };

        let wait = match client.execute(replay).await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status().as_u16();
                // Read before the body consumes the response, whichever way
                // the classification below goes.
                let requested = retry_after(response.headers(), SystemTime::now());
                if attempt >= MAX_ATTEMPTS {
                    return Err(refusal(response, presented).await);
                }
                if !RETRYABLE_STATUS.contains(&status) {
                    // The status alone says final, but the body may still
                    // name a transient condition — upstream classifies the
                    // error it built, message included (v1.18.22), so the
                    // body is read before the request is given up on.
                    let refused = refusal(response, presented).await;
                    let transient = matches!(
                        &refused,
                        ProviderError::Status { message, .. } if transient_message(message)
                    );
                    if !transient {
                        return Err(refused);
                    }
                }
                match requested {
                    // A server-mandated pause is honoured exactly; jitter is
                    // the schedule's own, as upstream's `exponential` has it.
                    Some(_) => delay(attempt, requested),
                    None => jitter(delay(attempt, None)),
                }
            }
            Err(error) => {
                if attempt >= MAX_ATTEMPTS
                    || !(is_retryable_transport(&error) || transient_message(&chained(&error)))
                {
                    return Err(transport(error));
                }

                jitter(delay(attempt, None))
            }
        };

        tracing::debug!(attempt, ?wait, "retrying the request that opens the turn");
        pause(wait, cancel).await?;
        attempt += 1;
    }
}

/// Turns a final non-2xx response into the error the turn reports.
async fn refusal(response: reqwest::Response, presented: &Presented) -> ProviderError {
    let status = response.status().as_u16();
    let mut body = response.text().await.unwrap_or_default();
    // A provider that quotes the credential it refused is a real shape, and
    // this is the one place that text becomes an error message and a log line,
    // so it is the one place the quote has to be masked.
    let message = presented.redact(&summarize(&body));
    // The unmasked body was one of those quotes; the masked copy is the only
    // one anything downstream should be able to find.
    body.zeroize();

    // The masked copy, and only after the masking: this is the one place a
    // provider's own words about a refusal exist, and a status bar shows the
    // last of them while a log file keeps every one. Warn rather than debug —
    // a turn that did not happen is worth reading about without `-v`.
    tracing::warn!(status, message, "the provider refused the request");

    ProviderError::Status { status, message }
}

/// Trims an error body to something a status bar can hold, on a char boundary.
fn summarize(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return "no error body".to_owned();
    }

    match body.char_indices().nth(BODY_LIMIT) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_owned(),
    }
}

/// Describes a transport failure, following the cause chain because
/// `reqwest::Error` alone says only that a request failed, never why.
///
/// The URL is dropped first: it is the one part of the rendering that echoes
/// configuration back, and a base URL is allowed to carry credentials in its
/// userinfo. What went wrong is in the causes either way.
pub(super) fn transport(error: reqwest::Error) -> ProviderError {
    ProviderError::Transport(chained(&error.without_url()))
}

/// The error and every cause under it, flattened into one line.
///
/// Also what the transient-message classification reads — over the error as
/// it arrived, URL included, because the classification never leaves this
/// module while [`transport`] scrubs what the caller is handed.
fn chained(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);

    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }

    message
}

/// Whether a failed send never reached the provider, which is the only kind of
/// transport failure that is safe to repeat.
fn is_retryable_transport(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

/// Sleeps, unless the turn is cancelled first.
async fn pause(wait: Duration, cancel: &CancellationToken) -> Result<(), ProviderError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ProviderError::Transport(
            "the turn was cancelled before the provider answered".to_owned(),
        )),
        () = tokio::time::sleep(wait) => Ok(()),
    }
}

/// How long to wait before `attempt`'s retry, ported from upstream `delay`.
///
/// `attempt` is 1-based and names the attempt that just failed.
#[must_use]
pub fn delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(requested) = retry_after {
        return requested.min(MAX_DELAY);
    }

    let scheduled = INITIAL_DELAY
        .as_millis()
        .saturating_mul(u128::from(BACKOFF_FACTOR).saturating_pow(attempt.saturating_sub(1)));

    Duration::from_millis(u64::try_from(scheduled).unwrap_or(u64::MAX)).min(MAX_DELAY)
}

/// Extends `base` by up to [`JITTER_PERCENT`], off the operating system's
/// entropy rather than the clock the processes being spread apart share.
fn jitter(base: Duration) -> Duration {
    scattered(base, crate::jitter::draw())
}

/// [`jitter`] over a draw the caller holds, so the span can be walked end to
/// end instead of sampled.
fn scattered(base: Duration, entropy: u64) -> Duration {
    let span =
        u64::try_from(base.as_millis()).unwrap_or(u64::MAX) / u64::from(100 / JITTER_PERCENT);
    if span == 0 {
        return base;
    }

    (base + Duration::from_millis(entropy % (span + 1))).min(MAX_DELAY)
}

/// How long a response asked to be left alone for.
///
/// `retry-after-ms` wins over `retry-after` because that is the order upstream
/// reads them in, and because it is the more precise of the two. A `retry-after`
/// is either a count of seconds or an HTTP date.
#[must_use]
pub fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

    if let Some(millis) =
        header("retry-after-ms").and_then(|value| value.trim().parse::<f64>().ok())
        && millis.is_finite()
        && millis >= 0.0
    {
        return Some(Duration::from_millis(millis.ceil() as u64));
    }

    let value = header("retry-after")?.trim();

    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_millis((seconds * 1_000.0).ceil() as u64));
    }

    http_date(value)?.duration_since(now).ok()
}

/// An HTTP date: the IMF-fixdate RFC 9110 requires senders to use, plus the
/// two legacy spellings it requires recipients to read.
///
/// The grammar is held to rather than approximated: a one-digit day, an absent
/// weekday and a weekday naming the wrong day of the week are each refused,
/// where a parser that merely splits on spaces lets all three through.
///
/// The one form [`httpdate`] refuses and a reader here may not is the leap
/// second, which keeps the branch below.
fn http_date(value: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(value)
        .ok()
        .or_else(|| http_leap_second(value))
}

/// The `:60` of a leap second, read as the instant it names.
///
/// A sender that spells 60 is telling the truth about a second that happened,
/// and the alternative — refusing the header — turns a `Retry-After` into no
/// delay at all. The second before it is a date every reader agrees on, so
/// that is what gets parsed and then stepped past.
fn http_leap_second(value: &str) -> Option<SystemTime> {
    let head = value.trim_end().strip_suffix(":60 GMT")?;

    httpdate::parse_http_date(&format!("{head}:59 GMT"))
        .ok()?
        .checked_add(Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{
        MAX_ATTEMPTS, MAX_DELAY, delay, http_date, jitter, retry_after, scattered, summarize,
    };
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
}

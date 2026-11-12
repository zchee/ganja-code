//! Count-only launch diagnostics; remote error text never enters these records.
//!
//! The client already owns transport and decoding. A separate classification
//! crate would still need this projection of ganja-client's own error variants.

use std::io::ErrorKind;

use ganja_client::ClientError;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Kind {
    ConnectionRefused,
    ConnectionReset,
    TimedOut,
    Http3xx,
    Http4xx,
    Http5xx,
    HttpOther,
    Decode,
    Other,
}

fn transport_kind(mut error: &(dyn std::error::Error + 'static)) -> Kind {
    let mut kind = Kind::Other;
    loop {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            match (io.kind(), kind) {
                (ErrorKind::ConnectionReset, _) => kind = Kind::ConnectionReset,
                (ErrorKind::ConnectionRefused, Kind::Other | Kind::TimedOut) => {
                    kind = Kind::ConnectionRefused;
                }
                (ErrorKind::TimedOut, Kind::Other) => kind = Kind::TimedOut,
                _ => {}
            }
            // io::Error::source delegates to its payload's source, skipping
            // the payload itself. Inspect that payload before continuing.
            if let Some(inner) = io.get_ref() {
                error = inner;
                continue;
            }
        }
        let Some(source) = error.source() else { return kind };
        error = source;
    }
}

fn classify(error: &ClientError) -> Kind {
    match error {
        ClientError::Transport { source, .. } => match transport_kind(source) {
            Kind::Other if source.is_timeout() => Kind::TimedOut,
            kind => kind,
        },
        ClientError::Unauthorized { .. } => Kind::Http4xx,
        ClientError::Refused { status: 300..=399, .. } => Kind::Http3xx,
        ClientError::Refused { status: 400..=499, .. } => Kind::Http4xx,
        ClientError::Refused { status: 500..=599, .. } => Kind::Http5xx,
        ClientError::Refused { .. } => Kind::HttpOther,
        ClientError::Skew { .. } => Kind::Decode,
        _ => Kind::Other,
    }
}

pub(crate) struct HealthWait<'a> {
    name: &'a str,
    started: Instant,
    attempts: u64,
    failures: u64,
    kinds: [u64; 9],
    in_flight: bool,
    outcome: &'static str,
}

impl<'a> HealthWait<'a> {
    pub(crate) fn new(name: &'a str) -> Self {
        Self {
            name,
            started: Instant::now(),
            attempts: 0,
            failures: 0,
            kinds: [0; 9],
            in_flight: false,
            outcome: "cancelled",
        }
    }

    pub(crate) fn begin(&mut self) {
        self.attempts += 1;
        self.in_flight = true;
    }

    pub(crate) fn failed(&mut self, error: &ClientError) {
        self.failures += 1;
        self.kinds[classify(error) as usize] += 1;
        self.in_flight = false;
    }

    pub(crate) fn ready(&mut self) {
        self.in_flight = false;
        self.outcome = "ready";
    }

    pub(crate) fn deadline(&mut self) {
        self.outcome = "deadline_reached";
        // A deadline can interrupt a request, or just the spacing between requests.
        // Only the former is an additional failed attempt.
        if self.in_flight {
            self.failures += 1;
            self.kinds[Kind::TimedOut as usize] += 1;
            self.in_flight = false;
        }
    }
}

impl Drop for HealthWait<'_> {
    fn drop(&mut self) {
        // Only an observed timeout records a deadline. Dropping an unmarked
        // future (including abort or unwind) leaves any pending probe unfinished.
        tracing::info!(
            name = self.name,
            attempts = self.attempts,
            failures = self.failures,
            unfinished = u64::from(self.in_flight),
            connection_refused = self.kinds[Kind::ConnectionRefused as usize],
            connection_reset = self.kinds[Kind::ConnectionReset as usize],
            timed_out = self.kinds[Kind::TimedOut as usize],
            http_status = self.kinds[Kind::Http3xx as usize]
                + self.kinds[Kind::Http4xx as usize]
                + self.kinds[Kind::Http5xx as usize]
                + self.kinds[Kind::HttpOther as usize],
            http_3xx = self.kinds[Kind::Http3xx as usize],
            http_4xx = self.kinds[Kind::Http4xx as usize],
            http_5xx = self.kinds[Kind::Http5xx as usize],
            http_other = self.kinds[Kind::HttpOther as usize],
            decode = self.kinds[Kind::Decode as usize],
            other = self.kinds[Kind::Other as usize],
            elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            outcome = self.outcome,
            "launch health wait ended"
        );
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;

//! The draw both of this crate's backoff ladders scatter from.
//!
//! Two schedules here wait before asking again — the retry policy's and the
//! catalog fetcher's — and both scatter the wait so that processes which
//! started together do not come back in step. Both used to take that scatter
//! off the clock, which is the one source such processes share: machines
//! booted from an image and started by the same command land within
//! microseconds of one another, so the nanosecond field they read is as
//! correlated as the delay it was there to decorrelate. The operating system's
//! entropy is the source that actually answers the question, and this crate
//! already draws from it for every login.
//!
//! Each ladder keeps its own arithmetic — one adds a bounded fraction, the
//! other multiplies by a factor around one — and takes the draw as an
//! argument, so a test can hold it still and walk the whole span rather than
//! sampling a live one and hoping.

/// Eight bytes from the operating system, as a number.
///
/// A draw that fails leaves the schedule standing rather than failing a
/// request: zero is still a legal wait, adding nothing to the request retry's
/// delay and selecting the catalog retry's shortest multiplicative factor. It
/// is worth a line in the log, because a platform whose entropy source has
/// gone away has a larger problem than a backoff.
pub(crate) fn draw() -> u64 {
    let mut bytes = [0_u8; 8];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::debug!(%error, "the platform's entropy source refused a backoff draw");
        return 0;
    }

    u64::from_le_bytes(bytes)
}

#[cfg(test)]
#[path = "jitter_tests.rs"]
mod tests;

//! A delivered trace proves only the retained interval it names. Inspection
//! events before or after that interval cannot repair a missing frame within it.

use serde::{Deserialize, Serialize};

/// Value-free identity of a journal frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceFrame {
    /// Process incarnation that minted the frame.
    pub epoch: String,
    /// Monotonic sequence within the incarnation.
    pub seq: u64,
    /// Protocol event discriminator, never a text delta or tool argument.
    pub event_type: String,
    /// Assistant message, where the event is attached to one.
    pub message_id: Option<String>,
}

/// The explicitly bounded interval a consumer claims it received.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveredTrace {
    /// Initial connected head. It precedes, rather than belongs to, the interval.
    pub initial: TraceFrame,
    /// Last expected terminal event in the acceptance run.
    pub terminal: TraceFrame,
    /// Message frames actually yielded to the consumer, in delivery order.
    pub frames: Vec<TraceFrame>,
}

/// A trace failed to prove its declared interval.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    /// Incarnations cannot be spliced into an exactly-once claim.
    #[error("the trace crosses a process incarnation")]
    Epoch,
    /// The declared terminal must be a finish after the initial head.
    #[error("the trace does not name a terminal event after its initial head")]
    Interval,
    /// A retained journal is needed all the way back to the initial head.
    #[error("the journal no longer retains the full evidence interval")]
    Retention,
    /// Duplicated, reordered, or missing delivery cannot be treated as success.
    #[error("delivered frames differ from the retained journal interval")]
    Delivery,
    /// A scripted expectation is independent evidence of which events should exist.
    #[error("the journal interval differs from the expected event manifest")]
    Manifest,
}

/// Compares delivered frames, the retained journal, and an independent script manifest.
///
/// `manifest` is the event-type sequence derived from the acceptance script; both
/// it and the journal must name the entire interval, not just its terminal event.
///
/// # Errors
/// Refuses gaps, duplicates, reordering, an evicted beginning, and epoch changes.
pub fn check(
    trace: &DeliveredTrace,
    journal: &[TraceFrame],
    manifest: &[String],
) -> Result<(), TraceError> {
    if trace.initial.epoch != trace.terminal.epoch {
        return Err(TraceError::Epoch);
    }
    if trace.terminal.seq <= trace.initial.seq || trace.terminal.event_type != "message_finished" {
        return Err(TraceError::Interval);
    }
    let first = trace.initial.seq.checked_add(1).ok_or(TraceError::Interval)?;
    let start = journal
        .iter()
        .position(|frame| frame.epoch == trace.initial.epoch && frame.seq == first)
        .ok_or(TraceError::Retention)?;
    let count = usize::try_from(trace.terminal.seq - trace.initial.seq)
        .map_err(|_| TraceError::Interval)?;
    let retained = journal
        .get(start..start.checked_add(count).ok_or(TraceError::Interval)?)
        .ok_or(TraceError::Retention)?;
    for (offset, frame) in retained.iter().enumerate() {
        if frame.epoch != trace.initial.epoch || frame.seq != first + offset as u64 {
            return Err(TraceError::Retention);
        }
    }
    if retained.last() != Some(&trace.terminal) || trace.frames != retained {
        return Err(TraceError::Delivery);
    }
    if retained.iter().map(|frame| &frame.event_type).ne(manifest.iter()) {
        return Err(TraceError::Manifest);
    }
    Ok(())
}

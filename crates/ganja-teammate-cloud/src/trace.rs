//! A delivered trace proves only the retained interval it names. Inspection
//! events before or after that interval cannot repair a missing frame within it.

use std::io::{BufRead, BufWriter, Write as _};

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
    /// The trace could not be read completely.
    #[error("the trace could not be read")]
    Read,
    /// A line is not a value-free trace frame.
    #[error("the trace contains an invalid frame")]
    Decode,
    /// Declared loss cannot become an exactly-once claim.
    #[error("the trace contains a declared gap")]
    Gap,
}

/// Loads JSON-line trace frames through the last recorded terminal event.
///
/// # Errors
/// Rejects unreadable or malformed data, a declared gap, and missing boundaries.
pub fn read(reader: impl BufRead) -> Result<DeliveredTrace, TraceError> {
    let mut initial = None;
    let mut frames = Vec::new();
    for line in reader.lines() {
        let frame: TraceFrame = serde_json::from_str(&line.map_err(|_| TraceError::Read)?)
            .map_err(|_| TraceError::Decode)?;
        if frame.event_type == "gap" {
            return Err(TraceError::Gap);
        }
        if frame.event_type == "connected" {
            initial.get_or_insert(frame);
        } else {
            frames.push(frame);
        }
    }
    let initial = initial.ok_or(TraceError::Interval)?;
    let terminal_position = frames
        .iter()
        .rposition(|frame| frame.event_type == "message_finished")
        .ok_or(TraceError::Interval)?;
    frames.truncate(terminal_position + 1);
    let terminal = frames.last().cloned().ok_or(TraceError::Interval)?;
    Ok(DeliveredTrace { initial, terminal, frames })
}

pub(crate) struct TraceLog(BufWriter<std::fs::File>);

impl TraceLog {
    pub(crate) fn from_env(name: &str) -> std::io::Result<Option<Self>> {
        let Some(directory) = std::env::var_os("GANJA_CLOUD_TRACE") else { return Ok(None) };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory)?;
            let metadata = std::fs::symlink_metadata(&directory)?;
            // Do not silently change a shared directory's access policy, or follow
            // a symlink when deciding where private trace identities are written.
            if !metadata.is_dir() || metadata.permissions().mode() & 0o7077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the trace directory must be private",
                ));
            }
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            let path = std::path::PathBuf::from(directory).join(format!("{name}.jsonl"));
            let file =
                std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            Ok(Some(Self(BufWriter::new(file))))
        }
        #[cfg(not(unix))]
        {
            let _ = (directory, name);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "private trace permissions require a Unix filesystem",
            ))
        }
    }

    pub(crate) fn frame(
        &mut self,
        id: &ganja_client::EventId,
        event_type: &str,
        message_id: Option<String>,
    ) -> std::io::Result<()> {
        let frame = TraceFrame {
            epoch: id.epoch().to_owned(),
            seq: id.seq(),
            event_type: event_type.to_owned(),
            message_id,
        };
        serde_json::to_writer(&mut self.0, &frame)?;
        self.0.write_all(b"\n")?;
        self.0.flush()
    }

    pub(crate) fn event(
        &mut self,
        id: &ganja_client::EventId,
        event: &ganja_protocol::Event,
    ) -> std::io::Result<()> {
        use ganja_protocol::Event;
        let (kind, message) = match event {
            Event::MessageStarted { message, .. } => ("message_started", Some(message.id.as_str())),
            Event::PartStarted { message_id, .. } => ("part_started", Some(message_id.as_str())),
            Event::PartDelta { message_id, .. } => ("part_delta", Some(message_id.as_str())),
            Event::PartUpdated { message_id, .. } => ("part_updated", Some(message_id.as_str())),
            Event::MessageFinished { message_id, .. } => {
                ("message_finished", Some(message_id.as_str()))
            }
            Event::PermissionRequested { .. } => ("permission_requested", None),
            Event::PermissionReplied { .. } => ("permission_replied", None),
            Event::SteerConsumed { .. } => ("steer_consumed", None),
            Event::QuestionAsked { .. } => ("question_asked", None),
            Event::QuestionReplied { .. } => ("question_replied", None),
            Event::QuestionRejected { .. } => ("question_rejected", None),
            Event::RevertChanged { .. } => ("revert_changed", None),
            Event::AgentChanged { .. } => ("agent_changed", None),
            Event::EffortChanged { .. } => ("effort_changed", None),
            Event::FastChanged { .. } => ("fast_changed", None),
            Event::PermissionModeChanged { .. } => ("permission_mode_changed", None),
            Event::PeerHeld { .. } => ("peer_held", None),
            Event::PeerHoldSettled { .. } => ("peer_hold_settled", None),
            Event::PeerReceipt { .. } => ("peer_receipt", None),
            Event::CompactionProgress { .. } => ("compaction_progress", None),
        };
        self.frame(id, kind, message.map(str::to_owned))
    }
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

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;

//! Carrying a shim pane teammate's answers back to its lead (**D515**).
//!
//! Neither upstream opencode nor Claude Code has a counterpart: no other build
//! here runs somebody else's CLI as a teammate, so every sentence is ganja's
//! own. **D512** shipped the pane as send-only and said so in every place a
//! person reads — the spawn dialog, the `/team` ring, the preamble the pane is
//! handed — and recorded the missing half as bead `ganja-code-9u1`. This is
//! that half, on a user directive of 2026-08-24, and it is the reason those
//! sentences changed rather than grew a footnote.
//!
//! # Why the transcript, and not the pane
//!
//! A shim teammate is a foreign CLI in a pty. It has no `send_message` tool,
//! no mailbox, and — for codex and grok — a read-only sandbox that would
//! refuse to write one, so **the agent itself cannot answer**: nothing this
//! side asks it to do would reach the lead. What each of these CLIs does do,
//! unasked, is write its own conversation to its own home. So the answer road
//! is ganja reading that file and mailing what it finds, exactly as the
//! headless shim mails what its child printed — the lead's side is unchanged,
//! and a relayed answer arrives as the `PartBody::Peer` every teammate's words
//! already arrive as.
//!
//! Scraping the pane's screen was the other candidate and is deliberately not
//! this: `capture-pane` shows a TUI's chrome — frames, spinners, folded tool
//! output, a composer — and telling a finished answer from a thinking line
//! there is a guess that would change under every vendor's next release. A
//! transcript is that CLI's own structured record of what it said.
//!
//! # Finding *this member's* session
//!
//! By fingerprint, never by "the newest file": two panes may open in one
//! directory a second apart, and a teammate reading another teammate's
//! conversation would mail somebody else's words to the lead under its own
//! name. The fingerprint is [`ganja_core::teammate::preamble::opening`] — the
//! sentence naming this member and its team, which this side pasted and the
//! CLI recorded verbatim — so a match is proof rather than inference. Until
//! the CLI has written that first record there is simply no session yet, and
//! the loop asks again on its next poll.
//!
//! # What is carried, per CLI
//!
//! [`answers_clause`](crate::readback::answers_clause) is the one place that says it, and both preambles are
//! composed from it, so the sentence a teammate is told and the records its
//! reader actually yields cannot come apart:
//!
//! | CLI | record | carried |
//! |---|---|---|
//! | codex | `response_item` / `message` / `role: assistant` | **every** message, in arrival order |
//! | grok | the last `agent_message_chunk` before a `turn_completed` | one answer per finished turn |
//! | agy | a `PLANNER_RESPONSE` carrying `content` | **every** such record, in order (its headless door mails one per turn — [`Road`](crate::readback::Road)) |
//!
//! Those are each CLI's own shape. grok's matches what its **headless** driver
//! already mails ([`crate::shim`]), for the reason that module
//! gives: it narrates as it works, into a channel it marks turns in, so the
//! answer can be told from the commentary. codex and agy write no end-of-turn
//! marker a reader could wait for, so waiting for one would mean a teammate
//! asked a single question is heard from only when it is asked a second — and
//! every record they wrote as something they said is carried instead. What
//! that costs is bounded where it lands rather than at the source: a poll's
//! answers become **one** mail ([`crate::shim_tui`]'s relay), so a
//! narrating turn is one peer message and not seven.
//!
//! # What v1 does not do
//!
//! It does not read a teammate's *thinking*, its tool calls or its errors —
//! only what the CLI recorded as something it said. It carries no answer from
//! before the spawn: a resumed conversation's history is not this member's to
//! forward. And it is one-way still, in the sense that matters for consent:
//! the lead hears the pane, and nothing the lead reads is written back into
//! that CLI's session except through the paste door that was always there.

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;

/// How many records into a candidate transcript the fingerprint is looked
/// for.
///
/// **Records, not bytes**, and that is a finding rather than a preference:
/// codex writes its own instructions, the project's `AGENTS.md` and whatever
/// its plugins add before the first user message, which on the machine this
/// was built on put the pasted sentence at byte 265,806 of a 273KB rollout —
/// past any byte window small enough to be worth having. What does not vary
/// is that the message is one of the conversation's first *records*: a
/// handful of the CLI's own, then the one this side pasted.
const FINGERPRINT_RECORDS: usize = 400;

/// A ceiling on the reading anyway, for a file that is one enormous line.
const FINGERPRINT_BYTES: u64 = 8 * 1024 * 1024;

/// Where one CLI's answers are read from, and how far a reader has got.
///
/// Both fields are here rather than one per reader because the two shapes are
/// real: a transcript that is only ever appended to is tailed from a byte
/// offset, and one its CLI may rewrite — agy chunks and re-emits its own — is
/// re-read whole with the answers already carried skipped. Each reader's doc
/// says which field it advances and why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Bytes of the file already scanned, for an append-only transcript.
    pub bytes: u64,
    /// Answers already carried, for a transcript that may be rewritten.
    pub answers: usize,
}

/// One CLI's own record of what it said, as this side reads it.
///
/// Two questions, and they are separate because they fail separately: which
/// file is this member's conversation (asked until it is answered, since a CLI
/// writes nothing before its first turn), and what has it said since last time
/// (asked on every poll after that).
///
/// Synchronous, like everything that touches `ganja-team`'s documents: a
/// transcript read is a file read, and the caller wraps it in
/// `blocking_io` the way every other one here is wrapped.
pub trait Transcript: Send + Sync {
    /// The session file whose own **pasted message** carries `mark`, if this
    /// CLI has written one yet.
    ///
    /// `cwd` is the directory the pane was opened in, which is what narrows
    /// the search for the CLIs that shard their sessions by it; `since` is
    /// when this member was spawned, which drops every conversation that
    /// existed before it.
    fn find(&self, mark: &str, cwd: &Path, since: SystemTime) -> Option<PathBuf>;

    /// Whether this record is the **user's** own message and carries `mark`.
    ///
    /// The fingerprint is only proof if it is read where the CLI says a
    /// person spoke. A raw substring over the whole file is not: the sentence
    /// naming this member also appears in its mailbox document, in this
    /// repository's own fixtures, and therefore in the transcript of any
    /// *other* session that read one of them — and preferring the busiest
    /// match would then mail a stranger's answers to the lead under this
    /// member's name. So each CLI says which of its records a person's words
    /// arrive in, and the search asks only those.
    fn user_said(&self, record: &serde_json::Value, mark: &str) -> bool;

    /// What this member has said since `cursor`, advancing it.
    ///
    /// An answer is whatever this CLI records as a message of its own; the
    /// module doc's table is the contract, and [`answers_clause`] is the
    /// sentence the teammate was told it by.
    fn answers(&self, path: &Path, cursor: &mut Cursor) -> Vec<String>;
}

/// The reader for `cli`, by an exhaustive match rather than a registration.
///
/// The same shape [`ganja_core::teammate::posture_line`] and
/// [`crate::shim_tui::pane_line`] use, for the same reason: a
/// fourth CLI that forgot to say how its answers are read would be a build
/// failure rather than a teammate that silently never answers.
#[must_use]
pub fn of(cli: ShimCli) -> &'static dyn Transcript {
    match cli {
        ShimCli::Codex => &crate::codex::TRANSCRIPT,
        ShimCli::Grok => &crate::grok::TRANSCRIPT,
        ShimCli::Agy => &crate::agy::TRANSCRIPT,
    }
}

/// Which road a teammate's answers travel, since a CLI's two doors do not
/// always carry the same amount (**D515**).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Road {
    /// A headless child, whose stdout that CLI's driver parses.
    Headless,
    /// A native TUI in a pane, whose own transcript this module reads.
    Pane,
}

/// What a teammate on `backend` is told its answers do, on the road it is
/// actually travelling — the sentence and the contract in one place
/// (**D515**).
///
/// [`None`] for the three surfaces that are not a foreign CLI: they hold
/// ganja's own `send_message` or Claude's, and their preambles say so.
///
/// Two roads rather than one sentence per CLI, because for one of the three
/// they differ and a shared sentence would have to be wrong for a door: a
/// **headless** agy child answers on a `result` record its driver waits for,
/// one per turn, while its **pane** writes no end-of-turn marker at all, so
/// the reader carries each record it wrote as something said rather than
/// waiting for a marker that never comes. codex says the same on both roads,
/// and grok says the same on both because it marks its turns on either.
#[must_use]
pub fn answers_clause(backend: MemberBackend, road: Road) -> Option<&'static str> {
    /// What a teammate is told when everything it says is carried.
    const EVERY: &str =
        "every message you print in answer is carried to the lead as mail, in order";
    /// What a teammate is told when one answer per turn is.
    const FINAL: &str = "only your final answer for the turn is carried to the lead, as one mail \
         — so put the whole of it in your last message";

    match (backend, road) {
        (MemberBackend::InProcess | MemberBackend::Ganja | MemberBackend::Claude, _) => None,
        (MemberBackend::Codex, _) => Some(EVERY),
        (MemberBackend::Grok, _) | (MemberBackend::Agy, Road::Headless) => Some(FINAL),
        (MemberBackend::Agy, Road::Pane) => Some(EVERY),
    }
}

/// The `sessions` directory of a CLI that keeps its home under `home_env`,
/// else under `~/<fallback>` — the one layout codex and grok share, each
/// naming only its own variable and dot-directory.
pub(crate) fn home_sessions(home_env: &str, fallback: &str) -> Option<PathBuf> {
    let home = match std::env::var_os(home_env) {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(fallback),
    };

    Some(home.join("sessions"))
}

/// Whether one of `path`'s first [`FINGERPRINT_RECORDS`] records is a message
/// the *user* sent carrying `mark`, under a [`FINGERPRINT_BYTES`] ceiling.
///
/// **Streamed**, never slurped: a candidate is read a line at a time and the
/// reading stops at the first match or at the record ceiling, so a directory
/// of multi-megabyte rollouts costs the head of each rather than all of them
/// (measured: 113 rollouts on the machine this was written on come to 192MB,
/// which a two-second poll must not read).
pub(crate) fn holds(path: &Path, mark: &str, reader: &dyn Transcript) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };

    BufReader::new(file.take(FINGERPRINT_BYTES))
        .lines()
        .take(FINGERPRINT_RECORDS)
        // A line that is not UTF-8 is a line this cannot read, and **not** the
        // end of the reading: `lines()` reports one as an error, and stopping
        // there would make a member's own transcript permanently unmatchable
        // over a stray byte in some tool's output it recorded ahead of the
        // paste — silently, since the only thing that would say so is one
        // ring line 90 seconds later. Skipped as an empty line, which parses
        // as no record and costs one of the four hundred.
        .map_while(|line| match line {
            Ok(line) => Some(line),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Some(String::new()),
            Err(_) => None,
        })
        // The substring first, the parse only where it could matter: these
        // rollouts average twelve kilobytes a line, and a member with several
        // panes beside it would otherwise parse megabytes of JSON every two
        // seconds until it latched. What decides is still the parse — a
        // sentence appearing anywhere in a record proves nothing until
        // [`Transcript::user_said`] says a person sent it.
        .filter(|line| line.contains(mark))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .any(|record| reader.user_said(&record, mark))
}

/// The complete lines added since `cursor.bytes`, advancing it past them.
///
/// **Complete** is the whole of the care here: a transcript is being written
/// while it is read, so a trailing fragment is left where it is and read again
/// next time with the rest of its line. The cursor therefore only ever moves
/// past bytes that ended in a newline.
pub(crate) fn appended(path: &Path, cursor: &mut Cursor) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(cursor.bytes)).is_err() {
        return Vec::new();
    }
    let mut fresh = Vec::new();
    if file.read_to_end(&mut fresh).is_err() {
        return Vec::new();
    }
    let Some(end) = fresh.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let whole = &fresh[..=end];
    cursor.bytes += whole.len() as u64;

    String::from_utf8_lossy(whole).lines().map(str::to_owned).collect()
}

/// Every line of `path`, for a transcript its CLI may rewrite.
pub(crate) fn whole(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The answers past `cursor.answers` of everything `found` yielded, advancing
/// it.
///
/// The re-read shape's other half: a reader that recomputes every answer in
/// the file hands them all here, and what comes back is the tail nobody has
/// carried yet. A file that *shrank* — a CLI that compacted its own record —
/// yields nothing rather than repeating itself, because the count is what is
/// remembered and it only moves forward.
pub(crate) fn beyond(found: Vec<String>, cursor: &mut Cursor) -> Vec<String> {
    if found.len() <= cursor.answers {
        return Vec::new();
    }
    let fresh = found[cursor.answers..].to_vec();
    cursor.answers = found.len();

    fresh
}

/// The newest entry of `candidates` that this member's own pasted message
/// opened, of the ones a CLI could have written since `since`.
///
/// Two filters, and neither is the ordering. The **time** bound drops every
/// conversation that existed before this member did, so a resumed session
/// that once read the sentence somewhere cannot be latched. The
/// **fingerprint** is then read only where that CLI records a person's own
/// words ([`Transcript::user_said`]), which is what makes a match proof
/// rather than the observation that the bytes appear on disk. Newest-first is
/// only the order of the search.
pub(crate) fn matching(
    mut candidates: Vec<PathBuf>,
    mark: &str,
    reader: &dyn Transcript,
    since: SystemTime,
) -> Option<PathBuf> {
    // A file nothing has written since this member was spawned cannot hold a
    // message this member sent. `mtime` rather than a creation time because
    // it is the one every filesystem here answers, and a transcript is
    // written to for as long as its conversation lives.
    candidates.retain(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|written| written >= since)
    });
    candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok());

    candidates.into_iter().rev().find(|path| holds(path, mark, reader))
}

/// The files directly under `directory` that `keep` accepts.
///
/// Answers an empty list for a directory that is not there, which is the
/// ordinary case before a CLI's first run on this machine — not an error, and
/// not something to log on a two-second poll.
pub(crate) fn listing(directory: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    entries.flatten().map(|entry| entry.path()).filter(|path| keep(path)).collect()
}

#[cfg(test)]
#[path = "readback_tests.rs"]
mod tests;

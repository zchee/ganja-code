//! Which name a session answers to: the registration record beside its
//! socket (**D527**).
//!
//! No upstream counterpart: opencode has no cross-session addressing at all.
//! The specification is Claude Code's local session registry — a per-process
//! record in a `0700` directory, removed on exit and updated in place (v2
//! §"Local session registration", evidence 155042-155235) — re-keyed onto
//! ganja's own scheme: one `<stem>.json` per registered session beside the
//! `<stem>.sock` the binder bound, the stem being the id-prefix name the
//! bind walk settled. The reference keys its records by pid because its
//! liveness is a pid-plus-start-time probe; ganja's liveness is the flock'd
//! `.lock` the binder already holds, which makes pid recycling irrelevant
//! and the filename the whole correlation — record, socket and session tied
//! by construction, no path field needed. It lives in this crate for
//! `socket`'s own reason: the lowest reader owns the spelling, and the
//! engine's resolver, the binary's lister and the frontend's writer all sit
//! above it.
//!
//! # The name is self-asserted, and the record never crosses the wire
//!
//! Any same-uid process can write any record, so the name in one is
//! **self-asserted display-and-routing data, never an authenticated
//! identity** — the axiom every reader is built on, and same-uid visibility
//! is likewise not secrecy: the records are exactly as private as the
//! sockets beside them, `0700`-directory private and no more. Registration
//! never refuses a collision — an exclusivity nobody can enforce would be a
//! fake boundary — so the registering side surfaces a notice and registers
//! anyway, and *resolution* refuses duplicate live names as ambiguous. That
//! collision rule is ganja's own, **user-ratified 2026-08-26**, because
//! v2's collision behavior is flag-gated and therefore unportable. And the
//! record never crosses the wire: a lead's `from` on a socket message stays
//! its team identity, a record-less teamless session's self-name crosses as
//! unauthenticated `from` display data (**D530**), and nothing a receiver
//! trusts is fed from here — the registry changes how a *sender* finds an
//! address, never what a *receiver* may believe.
//!
//! # Who registers, and what the name is
//!
//! Lead sessions only (**user-ratified 2026-08-26**): the record rides the
//! bound socket, and only leads bind one. The name is `--name`'s or
//! `/rename`'s, both through [`vet_name`], or — neither given — the project
//! root's basename through [`sanitize`] (**user-ratified 2026-08-26**; the
//! source table mirrors v2 §"Session names and `[ref]`", which records no
//! evidence ranges of its own, so that citation is section-name-only). The
//! grammar itself is **ganja-inferred**: each clause exists so a name
//! survives the surfaces that read it, with the one v2-recorded clause —
//! the 64-code-point cap of v2 §"Attribute semantics and trust", evidence
//! 153113-153175 — applied as a refusal rather than the reference's
//! truncation, because the name is the person's own input at their own
//! prompt; the control-character refusal applies the same section's
//! sanitization classes the same way.
//!
//! # Tolerant read, exact write
//!
//! This is ganja's own cross-version format — neither `ganja-protocol`'s
//! refuse-unknown wire nor `ganja-team`'s foreign passthrough — because its
//! one hazard is version skew among ganja's own builds: an older reader must
//! skip a newer record without mis-parsing it, and nothing foreign ever
//! writes one, so there is no unknown key to preserve in place and no wire
//! to hold exact. A reader therefore ignores unknown fields, skips with a
//! trace any record whose [`FORMAT`] it does not know, caps what it will
//! read, and drops what fails shape validation — the tolerant-reader
//! posture of v2 §"Liveness validation and garbage collection" (evidence
//! 221136-221268) with the checks re-derived — while the writer writes
//! exactly the declared fields, atomically ([`write()`]). The stored name's
//! typed case is preserved end to end: comparison folds ([`same_name`]),
//! storage does not.
//!
//! # Liveness is the flock, and the probe never unlinks
//!
//! A record is live exactly while its stem's `.lock` is held ([`is_live`]):
//! a non-blocking try-lock on the file [`socket::open_lock`] opens, dropped
//! the instant it answers. The probe reads and never removes — the
//! claim-then-unlink discipline stays the binder's and the lister's, in
//! `ganja-serve` and `ganja sessions --live`, so a read path here can never
//! become a directory mutation. Its two side costs are the documented ones:
//! a probe may create an absent `.lock` (the lister's standing price — lock
//! files are never removed), and holding the lock for its one instant may
//! cost a concurrently walking binder a digit of stem extension. The lock
//! is `std`'s own file lock, the `NameLock` precedent in `ganja-serve`, and
//! that reuse is what satisfies the workspace's prefer-a-crate rule here:
//! nothing is hand-rolled that a crate would provide, and no dependency was
//! added.
//!
//! # Deliberately absent fields
//!
//! - Start-time tokens (the reference's `procStart`): the flock makes pid
//!   recycling irrelevant — a divergence from v2 §"Liveness validation and
//!   garbage collection"'s process checks, ganja keeping only the spirit of
//!   its lock check; the [`Record::pid`] that remains is informational.
//! - `formerNames`: its consumption is untraced in citable evidence, so no
//!   rename grace is built on it.
//! - `messagingSocketPath`: the filename *is* the correlation.
//! - `kind`: only TUI leads register today; the field appears when a second
//!   kind exists.
//! - Collision auto-suffixing: flag-gated in v2; ganja's rule is the
//!   notice-and-resolution-refusal above, user-ratified 2026-08-26.
//! - The `[ref]` disambiguator hash: provisional in v2 itself; the socket
//!   stem serves the role with no new derivation (**D528**).
//! - Registration for `serve`-led sessions: `ganja serve` leads no team
//!   today, so no serve path reaches a bind this record would advertise —
//!   the day it leads one, its bind needs this lifecycle (a filed bead
//!   holds the question).

use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::socket;

/// The extension a registration record carries, beside the socket's `.sock`
/// and the lock's `.lock`, so a listing can tell the three apart by name.
pub const EXTENSION: &str = "json";

/// The one record format this build writes, and the only one it reads: a
/// record naming any other is skipped with a trace rather than mis-parsed.
pub const FORMAT: u32 = 1;

/// The most bytes of a record file a reader will take. A real record is a
/// few hundred; the cap is what keeps a bogus giant at a stem-shaped name
/// from being slurped whole by every listing.
const MOST_RECORD_BYTES: u64 = 64 * 1024;

/// The most code points a name may carry — the cap v2 records for
/// display-name serialization (v2 §"Attribute semantics and trust",
/// evidence 153113-153175), applied here as a refusal rather than the
/// reference's truncation: the name is the person's own input at their own
/// prompt, and cutting it silently would register a name nobody typed.
pub const MOST_NAME_POINTS: usize = 64;

/// What derivation falls back to when the sanitized basename has nothing
/// left in it that the grammar admits.
pub const FALLBACK_NAME: &str = "ganja";

/// Where a session's name came from, kept so a listing can label a typed
/// name differently from a defaulted one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NameSource {
    /// The person's own, from `--name` or `/rename`.
    User,
    /// The project root's basename, sanitized (**user-ratified 2026-08-26**).
    Derived,
}

/// One session's registration, as its own writer spells it.
///
/// Tolerant read, exact write — the module doc says which and why. Every
/// field a reader consults for anything but display is structural: liveness
/// is the sibling lock, never [`Record::pid`] or [`Record::started_at`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// [`FORMAT`]; a reader skips, with a trace, formats it does not know.
    pub format: u32,
    /// The full bare UUIDv7 the session runs under (**D493**) — what a
    /// resolver excludes its own session by.
    pub session_id: String,
    /// The self-asserted name, stored as typed; comparison folds through
    /// [`same_name`], storage never does.
    pub name: String,
    /// Where [`Record::name`] came from.
    pub name_source: NameSource,
    /// The launch directory the session serves, for a listing's rows.
    pub cwd: PathBuf,
    /// The project root the derived name came from.
    pub root: PathBuf,
    /// Informational only — liveness is the flock, never the pid.
    pub pid: u32,
    /// Milliseconds since the epoch, display and sort only.
    pub started_at: u64,
}

/// A record as the listing walk found it: the stem that names it — the
/// bound socket's, and therefore the session's disambiguator — beside what
/// the file said.
#[derive(Clone, Debug, PartialEq)]
pub struct Registered {
    /// The filename's stem, equal to the bound socket's.
    pub stem: String,
    /// What the record holds.
    pub record: Record,
}

/// Where `stem`'s record lives under `directory` — the one spelling of the
/// name, so the writer, the walk and the collector cannot drift.
#[must_use]
pub fn record_path(directory: &Path, stem: &str) -> PathBuf {
    directory.join(format!("{stem}.{EXTENSION}"))
}

/// Writes `record` at `stem`'s name atomically: staged whole, then
/// `rename(2)` onto [`record_path`] — the `.shims` precedent.
///
/// The staging name is deliberately **not** stem-shaped — a leading dot in
/// front of the stem — so [`list`]'s own filter can never read a
/// half-written record, and a crash between the write and the rename
/// leaves a dot-file no listing ever shows rather than a torn record. A
/// `stem` that is not a session socket's is refused: a record at any other
/// name would be one the walk never lists, which is a bug at the caller,
/// not a file to leave behind.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] for a non-stem `stem`; otherwise what
/// the filesystem said.
pub fn write(directory: &Path, stem: &str, record: &Record) -> io::Result<()> {
    if !socket::is_session_stem(stem) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{stem:?} is not a session socket stem, so no record may take its name"),
        ));
    }

    let staging = directory.join(format!(".{stem}.{EXTENSION}"));
    std::fs::write(&staging, serde_json::to_vec_pretty(record)?)?;

    std::fs::rename(&staging, record_path(directory, stem))
}

/// Every readable record under `directory`, in stem order: only
/// `<8–32 hex>.json` names are considered ([`socket::is_session_stem`]),
/// a file over the read cap or failing shape validation or naming a
/// [`FORMAT`] this build does not know is skipped with a trace — the
/// tolerant reader — and liveness is deliberately **not** judged here:
/// pairing the walk with [`is_live`] is the caller's, because a resolver
/// refuses on what a notice merely skips.
///
/// Traces carry paths and stems, never a record's contents.
///
/// # Errors
///
/// A directory that cannot be read, or a walk that fails partway: an
/// incomplete listing is an error, never a shorter answer, so a caller can
/// refuse rather than guess.
pub fn list(directory: &Path) -> io::Result<Vec<Registered>> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_suffix(EXTENSION)
            .and_then(|it| it.strip_suffix('.'))
        else {
            continue;
        };
        if !socket::is_session_stem(stem) {
            continue;
        }

        let path = entry.path();
        let size = match entry.metadata() {
            Ok(found) => found.len(),
            Err(error) => {
                tracing::trace!(path = %path.display(), %error, "skipping an unreadable record");
                continue;
            }
        };
        if size > MOST_RECORD_BYTES {
            tracing::trace!(path = %path.display(), size, "skipping an oversized record");
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::trace!(path = %path.display(), %error, "skipping an unreadable record");
                continue;
            }
        };
        // Two steps, so an unknown format is skipped as the format it names
        // rather than as whatever shape difference it happens to carry.
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                tracing::trace!(path = %path.display(), %error, "skipping a record that is not JSON");
                continue;
            }
        };
        match value.get("format").and_then(serde_json::Value::as_u64) {
            Some(format) if format == u64::from(FORMAT) => {}
            format => {
                tracing::trace!(
                    path = %path.display(),
                    ?format,
                    "skipping a record whose format this build does not read"
                );
                continue;
            }
        }
        let record: Record = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(error) => {
                tracing::trace!(path = %path.display(), %error, "skipping a record that fails shape validation");
                continue;
            }
        };

        records.push(Registered {
            stem: stem.to_owned(),
            record,
        });
    }
    records.sort_by(|left, right| left.stem.cmp(&right.stem));

    Ok(records)
}

/// Whether `stem`'s name is live under `directory`: held ⇒ live, free ⇒
/// stale — the one liveness token, read without being taken for more than
/// the answer's own instant.
///
/// The probe is a non-blocking try-lock on the file [`socket::open_lock`]
/// opens, dropped immediately on success; it **never unlinks** — a stale
/// verdict is the caller's to filter on, and removal stays behind the
/// binder's and the lister's claimed lock. The module doc accounts for its
/// two side costs.
///
/// # Errors
///
/// What opening or locking the lock file said.
#[cfg(unix)]
pub fn is_live(directory: &Path, stem: &str) -> io::Result<bool> {
    let lock = socket::open_lock(&directory.join(format!("{stem}.{}", socket::EXTENSION)))?;
    match lock.try_lock() {
        // Acquired: nobody live holds the name. The descriptor is the guard
        // and dropping it here — the function's end — is the release.
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// The **live** records holding `name` under the shared predicate, this
/// session's own excluded — the collision scan behind the registration
/// notice and the incumbent's re-scan, both of which warn and never refuse
/// (**user-ratified 2026-08-26**).
///
/// A record whose liveness cannot be judged is skipped with a trace rather
/// than refused over: the scan feeds a notice, and a notice missing one
/// holder warns less, where a resolver guessing would deliver wrong — that
/// stricter composition is the resolver's own, over [`list`] and
/// [`is_live`] directly.
///
/// # Errors
///
/// [`list`]'s: a listing that cannot be taken whole.
#[cfg(unix)]
pub fn holders(directory: &Path, name: &str, own_session: &str) -> io::Result<Vec<Registered>> {
    let mut holders = list(directory)?;
    holders.retain(|held| {
        held.record.session_id != own_session
            && same_name(&held.record.name, name)
            && match is_live(directory, &held.stem) {
                Ok(live) => live,
                Err(error) => {
                    tracing::trace!(
                        stem = held.stem,
                        %error,
                        "skipping a holder whose liveness could not be judged"
                    );
                    false
                }
            }
    });

    Ok(holders)
}

/// Why a name was refused, clause by clause — each with the sentence the
/// person who typed it reads at `--name` or `/rename`.
///
/// The grammar is **ganja-inferred** (the module doc says from what):
/// every clause exists so a name survives a surface that reads it, and each
/// names that surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameRefusal {
    /// Nothing was typed.
    #[error("a name needs something in it; this one is empty")]
    Empty,
    /// A second line could never be typed at a mention or a flag.
    #[error("a name is one line, and this one has more than one")]
    MultiLine,
    /// The mention grammar ends a token at whitespace, so a name holding
    /// any could never be pointed at.
    #[error(
        "a name carries no whitespace: an @-mention ends at the first space, so a name holding one could never be pointed at"
    )]
    Whitespace,
    /// [`MOST_NAME_POINTS`]'s cap, refused rather than truncated.
    #[error("a name is at most 64 code points, and this one has {points}")]
    TooLong {
        /// How many the refused name has.
        points: usize,
    },
    /// `*` is `send_message`'s broadcast token.
    #[error("`*` is the broadcast token, which a name may not be")]
    Broadcast,
    /// An `@` scopes an address at the send ladder's rung 4.
    #[error("a name carries no `@`, which scopes an address")]
    Scoped,
    /// A `:` reads as an address scheme at the send ladder's parser.
    #[error("a name carries no `:`, which reads as an address scheme")]
    Scheme,
    /// A leading `/` reads as a socket path at the same parser.
    #[error("a name does not begin with `/`, which reads as a socket path")]
    LeadingSlash,
    /// v2's sanitization classes, applied as refusal — the module doc's
    /// citation.
    #[error("a name carries no control characters")]
    Control,
}

/// The name grammar, one predicate for every door — `--name`, `/rename`,
/// and the sanitizer's own final check — refusing each clause by name.
///
/// Non-ASCII names are admitted; only the surfaces' own reserved characters
/// and shapes are refused. The most-specific clause answers first, so a
/// newline is refused as a second line rather than as the whitespace and
/// control character it also is.
///
/// # Errors
///
/// The first [`NameRefusal`] the name earns.
pub fn vet_name(name: &str) -> Result<(), NameRefusal> {
    if name.is_empty() {
        return Err(NameRefusal::Empty);
    }
    if name.contains('\n') || name.contains('\r') {
        return Err(NameRefusal::MultiLine);
    }
    if name.chars().any(char::is_whitespace) {
        return Err(NameRefusal::Whitespace);
    }
    let points = name.chars().count();
    if points > MOST_NAME_POINTS {
        return Err(NameRefusal::TooLong { points });
    }
    if name == "*" {
        return Err(NameRefusal::Broadcast);
    }
    if name.contains('@') {
        return Err(NameRefusal::Scoped);
    }
    if name.contains(':') {
        return Err(NameRefusal::Scheme);
    }
    if name.starts_with('/') {
        return Err(NameRefusal::LeadingSlash);
    }
    if name.chars().any(char::is_control) {
        return Err(NameRefusal::Control);
    }

    Ok(())
}

/// The derived name: `candidate` — the project root's basename
/// (**user-ratified 2026-08-26**) — run through the same grammar rather
/// than a second one. Characters a clause refuses are dropped, leading
/// slashes stripped, the rest cut at [`MOST_NAME_POINTS`], the typed case
/// of what survives preserved; anything [`vet_name`] still refuses —
/// nothing left, or the bare broadcast token — falls back to
/// [`FALLBACK_NAME`]. Ganja-inferred where the reference's slug rule is its
/// own (the module doc's section-name-only citation).
#[must_use]
pub fn sanitize(candidate: &str) -> String {
    let kept: String = candidate
        .chars()
        .filter(|point| {
            !point.is_whitespace() && !point.is_control() && *point != '@' && *point != ':'
        })
        .collect();
    let kept: String = kept
        .trim_start_matches('/')
        .chars()
        .take(MOST_NAME_POINTS)
        .collect();

    if vet_name(&kept).is_err() {
        FALLBACK_NAME.to_owned()
    } else {
        kept
    }
}

/// The one name-comparison predicate, shared by the collision scan, the
/// resolver's match and the menu's duplicate detection so the three cannot
/// disagree: `eq_ignore_ascii_case`, the identity comparison the send
/// ladder's rung 8 already makes of the lead's name.
///
/// The fold is **ASCII-only while the grammar admits non-ASCII names**, on
/// purpose: two names differing only in non-ASCII case are two names. That
/// is the precedent's own behavior extended, not a new folding regime
/// invented.
#[must_use]
pub fn same_name(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        FALLBACK_NAME, FORMAT, MOST_NAME_POINTS, NameRefusal, NameSource, Record, list,
        record_path, same_name, sanitize, vet_name, write,
    };

    /// A record for `stem`, distinct enough that a test mixing several can
    /// tell them apart.
    fn record(stem: &str, name: &str, session_id: &str) -> Record {
        Record {
            format: FORMAT,
            session_id: session_id.to_owned(),
            name: name.to_owned(),
            name_source: NameSource::User,
            cwd: format!("/work/{stem}").into(),
            root: format!("/work/{stem}").into(),
            pid: 4242,
            started_at: 1_756_150_000_000,
        }
    }

    /// AC-1's serde half: storage preserves the typed case byte for byte,
    /// and the round trip loses nothing.
    #[test]
    fn a_record_round_trips_with_its_typed_case_preserved() {
        let written = record(
            "0198c1a2",
            "MiXeD-Case",
            "0198c1a2-0000-7000-8000-000000000001",
        );

        let json = serde_json::to_string(&written).expect("a record serializes");
        let read: Record = serde_json::from_str(&json).expect("and reads back");

        assert_eq!(read, written);
        assert_eq!(read.name, "MiXeD-Case", "the typed case is storage's");
    }

    /// AC-1's walk half: unknown extra fields still read (tolerant), an
    /// unknown format and a torn or foreign name are skipped, and nothing
    /// that is not `<hex stem>.json` is ever read — the staging dot-name
    /// included.
    #[test]
    fn the_listing_walk_reads_tolerantly_and_skips_what_it_does_not_know() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        // A good record carrying a field this build never wrote.
        let mut good = serde_json::to_value(record(
            "0198c1a2",
            "worker",
            "0198c1a2-0000-7000-8000-000000000001",
        ))
        .expect("a record is JSON");
        good["a_field_from_the_future"] = json!("still reads");
        std::fs::write(
            record_path(dir.path(), "0198c1a2"),
            serde_json::to_vec(&good).expect("json"),
        )
        .expect("the fixture writes");

        // A record from a format this build does not know.
        let mut newer = serde_json::to_value(record(
            "0299d2b3",
            "future",
            "0299d2b3-0000-7000-8000-000000000002",
        ))
        .expect("a record is JSON");
        newer["format"] = json!(2);
        std::fs::write(
            record_path(dir.path(), "0299d2b3"),
            serde_json::to_vec(&newer).expect("json"),
        )
        .expect("the fixture writes");

        // A half-written record at the staging spelling, a foreign name, a
        // record-shaped file that is not JSON, and one that is JSON of the
        // wrong shape.
        std::fs::write(dir.path().join(".0398e3c4.json"), b"{\"format\":1,\"trunc")
            .expect("the fixture writes");
        std::fs::write(dir.path().join("notes.json"), b"{}").expect("the fixture writes");
        std::fs::write(dir.path().join("0398e3c4.json"), b"not json at all")
            .expect("the fixture writes");
        std::fs::write(
            dir.path().join("0498f4d5.json"),
            b"{\"format\":1,\"name\":7}",
        )
        .expect("the fixture writes");

        let listed = list(dir.path()).expect("the directory lists");

        assert_eq!(listed.len(), 1, "one readable record: {listed:?}");
        assert_eq!(listed[0].stem, "0198c1a2");
        assert_eq!(listed[0].record.name, "worker");
    }

    /// The size cap: a stem-named giant is skipped, not slurped.
    #[test]
    fn an_oversized_record_is_skipped_rather_than_read() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(record_path(dir.path(), "0198c1a2"), vec![b' '; 65 * 1024])
            .expect("the fixture writes");

        assert!(
            list(dir.path()).expect("the directory lists").is_empty(),
            "over the cap is over the cap"
        );
    }

    /// An incomplete search refuses rather than answers short: a directory
    /// that cannot be read is an error, never an empty listing.
    #[test]
    fn an_unreadable_directory_is_an_error_not_an_empty_listing() {
        assert!(list(std::path::Path::new("/nonexistent-ganja-registry")).is_err());
    }

    /// AC-2's mechanism half (F8): the write stages under a leading dot the
    /// stem filter can never match, lands atomically at the stem's name, and
    /// refuses a stem the walk would never list.
    #[test]
    fn a_write_lands_atomically_and_its_staging_name_is_never_a_session_stem() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let written = record("0198c1a2", "worker", "0198c1a2-0000-7000-8000-000000000001");

        write(dir.path(), "0198c1a2", &written).expect("a record writes");

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("the directory lists")
            .map(|entry| {
                entry
                    .expect("an entry reads")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["0198c1a2.json".to_owned()],
            "the staging file is gone and only the record remains"
        );
        // The listing walk's filter is what makes the staging spelling safe,
        // so the shape claim is asserted against that filter itself.
        assert!(
            !crate::socket::is_session_stem(".0198c1a2"),
            "a dot-led stem can never pass the walk"
        );

        let listed = list(dir.path()).expect("the directory lists");
        assert_eq!(listed[0].record, written);

        assert_eq!(
            write(dir.path(), "not-a-stem", &written)
                .expect_err("a non-stem name is refused")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    /// AC-5: every clause of the grammar refuses by name, at one predicate
    /// every door shares.
    #[test]
    fn the_name_grammar_refuses_each_clause_by_name() {
        assert_eq!(vet_name(""), Err(NameRefusal::Empty));
        assert_eq!(vet_name("two\nlines"), Err(NameRefusal::MultiLine));
        assert_eq!(vet_name("a b"), Err(NameRefusal::Whitespace));
        assert_eq!(
            vet_name(&"x".repeat(MOST_NAME_POINTS + 1)),
            Err(NameRefusal::TooLong {
                points: MOST_NAME_POINTS + 1
            })
        );
        assert_eq!(vet_name("*"), Err(NameRefusal::Broadcast));
        assert_eq!(vet_name("name@scope"), Err(NameRefusal::Scoped));
        assert_eq!(vet_name("uds:name"), Err(NameRefusal::Scheme));
        assert_eq!(vet_name("/leading"), Err(NameRefusal::LeadingSlash));
        assert_eq!(vet_name("bell\u{7}"), Err(NameRefusal::Control));

        assert_eq!(vet_name("worker-1"), Ok(()));
        assert_eq!(
            vet_name("日本語の名前"),
            Ok(()),
            "non-ASCII names are admitted"
        );
        assert_eq!(
            vet_name(&"あ".repeat(MOST_NAME_POINTS)),
            Ok(()),
            "the cap counts code points, not bytes"
        );

        // Every refusal is one single-spaced sentence, like the socket
        // gate's own.
        for refusal in [
            NameRefusal::Empty,
            NameRefusal::MultiLine,
            NameRefusal::Whitespace,
            NameRefusal::TooLong { points: 65 },
            NameRefusal::Broadcast,
            NameRefusal::Scoped,
            NameRefusal::Scheme,
            NameRefusal::LeadingSlash,
            NameRefusal::Control,
        ] {
            let sentence = refusal.to_string();
            assert!(!sentence.contains("  "), "single-spaced: {sentence:?}");
        }
    }

    /// AC-4's sanitizer half: the basename through the same grammar —
    /// invalid characters dropped, typed case preserved, nothing valid left
    /// falling back — and whatever comes out always passes the predicate.
    #[test]
    fn a_derived_name_is_the_basename_run_through_the_same_grammar() {
        assert_eq!(sanitize("ganja-code"), "ganja-code");
        assert_eq!(sanitize("MyProject"), "MyProject", "typed case survives");
        assert_eq!(sanitize("my project"), "myproject");
        assert_eq!(sanitize("a@b:c"), "abc");
        assert_eq!(sanitize("/leading/kept"), "leading/kept");
        assert_eq!(sanitize(""), FALLBACK_NAME);
        assert_eq!(sanitize("///"), FALLBACK_NAME);
        assert_eq!(sanitize("*"), FALLBACK_NAME);
        assert_eq!(sanitize(" \t\n"), FALLBACK_NAME);
        assert_eq!(
            sanitize(&"x".repeat(MOST_NAME_POINTS + 20)).chars().count(),
            MOST_NAME_POINTS,
            "an over-long basename is cut at the cap rather than refused"
        );

        for hostile in ["", "*", "///", "a b@c:d\n", "\u{7}\u{8}", &"y".repeat(200)] {
            assert_eq!(
                vet_name(&sanitize(hostile)),
                Ok(()),
                "whatever {hostile:?} became, the grammar admits it"
            );
        }
    }

    /// The one comparison predicate: ASCII case folds, non-ASCII case does
    /// not — the rung-8 precedent extended, not a new folding regime.
    #[test]
    fn name_comparison_folds_ascii_case_and_only_ascii_case() {
        assert!(same_name("Worker", "wORKER"));
        assert!(!same_name("worker", "worker-1"));
        assert!(
            !same_name("É", "é"),
            "two names differing only in non-ASCII case are two names"
        );
    }

    /// AC-9: a record is live exactly while its stem's lock is held, the
    /// probe's only footprint is the documented one — an absent `.lock`
    /// created, nothing ever unlinked.
    #[cfg(unix)]
    #[test]
    fn a_record_is_live_exactly_while_its_lock_is_held_and_the_probe_unlinks_nothing() {
        use super::is_live;

        let dir = tempfile::tempdir().expect("a scratch directory");
        let stem = "0198c1a2";
        write(
            dir.path(),
            stem,
            &record(stem, "worker", "0198c1a2-0000-7000-8000-000000000001"),
        )
        .expect("a record writes");

        // Held, as a binder holds it: a second descriptor's try-lock blocks.
        let socket = dir.path().join(format!("{stem}.sock"));
        let held = crate::socket::open_lock(&socket).expect("the lock file opens");
        held.try_lock().expect("nothing else holds a fresh lock");

        let names = |dir: &std::path::Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .expect("the directory lists")
                .map(|entry| {
                    entry
                        .expect("an entry reads")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        };

        let before = names(dir.path());
        assert!(
            is_live(dir.path(), stem).expect("the probe answers"),
            "a held lock is a live name"
        );
        assert_eq!(names(dir.path()), before, "the probe touched nothing");

        drop(held);
        assert!(
            !is_live(dir.path(), stem).expect("the probe answers"),
            "a freed lock is a stale name"
        );
        assert_eq!(
            names(dir.path()),
            before,
            "stale is a verdict, not an unlink"
        );

        // A name never bound: the probe creates the absent `.lock` — the
        // lister's standing price, lock files being never removed — and
        // reads stale. Everything already there survives.
        assert!(
            !is_live(dir.path(), "0299d2b3").expect("the probe answers"),
            "an unbound name is stale"
        );
        let after = names(dir.path());
        assert!(after.contains(&"0299d2b3.lock".to_owned()), "{after:?}");
        for kept in before {
            assert!(after.contains(&kept), "{kept} survived the probe");
        }
    }

    /// The collision scan behind the notice: live same-named holders under
    /// the folding predicate, the stale and this session's own excluded.
    #[cfg(unix)]
    #[test]
    fn the_collision_scan_reports_live_same_named_holders_and_never_this_session() {
        use super::holders;

        let dir = tempfile::tempdir().expect("a scratch directory");
        let live_id = "0198c1a2-0000-7000-8000-000000000001";
        write(
            dir.path(),
            "0198c1a2",
            &record("0198c1a2", "Worker", live_id),
        )
        .expect("a record writes");
        write(
            dir.path(),
            "0299d2b3",
            &record("0299d2b3", "worker", "0299d2b3-0000-7000-8000-000000000002"),
        )
        .expect("a record writes");

        // Only the first is live.
        let held = crate::socket::open_lock(&dir.path().join("0198c1a2.sock"))
            .expect("the lock file opens");
        held.try_lock().expect("nothing else holds a fresh lock");

        let found = holders(dir.path(), "wORKER", "some-other-session").expect("the scan answers");
        assert_eq!(
            found
                .iter()
                .map(|held| held.stem.as_str())
                .collect::<Vec<_>>(),
            vec!["0198c1a2"],
            "the live holder matches case-insensitively; the stale one is no collision"
        );
        assert_eq!(found[0].record.name, "Worker", "reported as typed");

        assert!(
            holders(dir.path(), "worker", live_id)
                .expect("the scan answers")
                .is_empty(),
            "a session is never its own collision"
        );
    }
}

//! An inbox: the file one teammate writes into and another drains.
//!
//! **Upstream opencode has no counterpart.** The specification is Claude
//! Code's, read out of the reference document — §2.3 for the message schema
//! and its envelope, §2.4 for validation and corruption handling, §2.5 for the
//! seed-and-rewrite shape of a write, §3.1 for what delivery does (**D497**).
//!
//! Four things about this file are decisions rather than transcription.
//!
//! *A mailbox is a queue, not a history.* §3.1 corrects an earlier reading:
//! delivered messages are **pruned**, not flagged. [`prune_delivered`] is
//! `retain(|m| !m.read && !delivered.contains(identity(m)))`, and `read` is a
//! tombstone nothing ever writes `true`. The depth of an inbox is therefore
//! genuine backlog, which is the better design and is kept for that reason as
//! much as for compatibility.
//!
//! *The identity a delivery is reconciled by is not `msg_id`.* It is §2.3's
//! composite `from|timestamp|text` ([`identity`]), which is why the timestamp's
//! exact spelling is a compatibility surface and not a cosmetic choice — see
//! [`crate::record::now_iso8601`].
//!
//! *Corruption is survivable and never silent.* A bad entry is dropped, not
//! fatal, and what is reported about it names the **field, the expectation and
//! the JSON type** — never a value. Reports are deduplicated and capped the way
//! §2.4 caps them, so one permanently broken inbox polled twice a second cannot
//! become the log.
//!
//! *Nothing here logs a message body.* What a teammate wrote is user content;
//! log lines carry counts, paths and ids. `tests/no_bodies_in_logs.rs` is the
//! canary that keeps it true, and the deliberately partial
//! [`Debug`](std::fmt::Debug) on [`Identity`], [`MailboxMessage`] and the record
//! shapes beside it is the same rule expressed as a type.
//!
//! *An append rewrites the file.* Every mutation is a whole read-modify-write
//! (the private `update`), so appending N messages moves on the order of **N² bytes** in
//! total — the tenth message rewrites the nine before it. That is the format's
//! shape rather than this module's choice: one JSON array in one file is what a
//! peer parses, and an append-only log beside it would be a second format
//! nobody else reads. At an inbox's real depth — a backlog a teammate drains,
//! not a history it accumulates (§3.1) — the quadratic is a handful of
//! kilobytes, and it is named here so nobody has to rediscover it from a
//! profile.

use std::{
    collections::HashSet,
    fmt, fs,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    io::{self, Write as _},
    path::Path,
    sync::{LazyLock, Mutex},
};

use serde_json::Value;
use tempfile::NamedTempFile;

use crate::{
    lock::{self, LockError},
    record::{MESSAGE_TYPE, MESSAGE_VERSION, MailboxMessage, SCHEMA_KEYS, document, shadowed},
};

/// What a brand-new inbox holds (§2.5's `writeExclusive(path, "[]")`).
pub const EMPTY_INBOX: &str = "[]";

/// How many *distinct* dropped entries are ever reported (§2.4's
/// `MAX_REPORTED`).
///
/// **A budget for the whole process, not per inbox and not per read.** The
/// memory behind it is one process-wide set, so the hundredth distinct damage
/// anywhere exhausts it for every inbox this process will ever open, for the
/// life of the process — after that a dropped entry is still dropped and still
/// counted in [`Contents::dropped`], it simply stops being narrated. That is
/// §2.4's own design and the point of it: the alternative is a permanently
/// broken inbox, polled twice a second, becoming the log.
pub const MAX_REPORTED: usize = 100;

/// How much of a dropped entry goes into its report key (§2.4's `kSS`).
const REPORT_KEY_CAP: usize = 2_048;

/// Why a write was refused for the one field type §2.4 calls out separately.
pub const REFUSED_NON_STRING_TEXT: &str = "a message's `text` is a string";

/// Why a write was refused for anything else.
pub const REFUSED_SCHEMA: &str = "a message does not match the inbox schema";

/// Why an inbox file yielded nothing, when it did not parse at all.
pub const DROPPED_NOT_JSON: &str =
    "the inbox file is not JSON, so nothing in it could be read as a message";

/// Why an inbox file yielded nothing, when it parsed as something other than an
/// array (§2.4).
pub const DROPPED_NOT_AN_ARRAY: &str =
    "the inbox file's top level is not an array, so nothing in it could be read as a message";

/// Why an entry was dropped when every field checked out and it still would not
/// decode.
///
/// Deliberately says nothing more. The underlying decoder's message can quote
/// the value it choked on, and a value is a message body.
pub const DROPPED_UNDECODABLE: &str =
    "the entry passed its field checks and still would not decode as a message";

/// A refusal or a failure on the way to an inbox.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    /// `text` was present and was not a string — §2.4's distinctly reported
    /// case, kept distinct here for the same reason: it is the one field whose
    /// type failure means the sender built the message wrong rather than that
    /// the file is damaged.
    #[error("{REFUSED_NON_STRING_TEXT}: this one holds {found}")]
    TextNotAString {
        /// The JSON type that was there instead. A type name, never a value.
        found: &'static str,
    },
    /// Some other field did not check out.
    #[error("{REFUSED_SCHEMA}: {}", issues.join("; "))]
    SchemaInvalid {
        /// One sentence per field that failed, naming the field, the
        /// expectation and the type found — never the value found.
        issues: Vec<String>,
    },
    /// The inbox is at its caller-supplied [`Ceiling`], and the append was
    /// refused rather than grown past it (**D526**). The counts are what the
    /// check observed — the state this append would have left — and the file
    /// is exactly as it was: a ceiling refusal happens under the hold and
    /// writes nothing, so not even the rewrite's usual prune of unreadable
    /// neighbours runs. Counts and bounds only, never a body.
    #[error(
        "the inbox is full: this append would leave it holding {held} messages in {bytes} \
         bytes, past its ceiling of {max_messages} messages / {max_bytes} bytes; nothing was \
         written"
    )]
    Full {
        /// How many messages the inbox would hold with this append.
        held: usize,
        /// How many bytes the rewritten file would hold with this append.
        bytes: usize,
        /// The bound on messages, as the caller supplied it.
        max_messages: usize,
        /// The bound on bytes, as the caller supplied it.
        max_bytes: usize,
    },
    /// The inbox could not be held while it was rewritten.
    ///
    /// Carried transparently because [`LockError`] already says everything
    /// there is to say — which path, and after how many retries — and because
    /// the two are not really different failures to a caller: the write did not
    /// happen, and the file is exactly as it was.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The file would not be read, written or replaced.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A message would not encode. Unreachable for the shapes in this crate,
    /// and carried rather than unwrapped because "unreachable" is a claim about
    /// today's shapes.
    #[error("an inbox could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// What one read of an inbox found (§2.4's `{valid, droppedCount}`).
///
/// The [`Debug`](std::fmt::Debug) here is derived and still carries no message
/// body, because the one field that could is a `Vec<`[`MailboxMessage`]`>` and
/// *that* type's `Debug` is deliberately partial. The safety is therefore
/// inherited rather than restated — writing a hand-rolled impl identical to the
/// derive would only add a second place to get it wrong — and it is pinned from
/// the outside by `tests/no_bodies_in_logs.rs`, which renders a `Contents`
/// holding a canary and searches it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contents {
    /// The entries that checked out, in file order.
    pub valid: Vec<MailboxMessage>,
    /// How many did not. A whole file that is not an array counts as one.
    pub dropped: usize,
    /// One sentence per *newly* reported drop — deduplicated and capped per
    /// [`MAX_REPORTED`], so a repeatedly polled broken inbox reports once.
    pub reports: Vec<String>,
}

/// What one prune removed and what it left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// How many entries the prune took out.
    pub pruned: usize,
    /// How many remain.
    pub remaining: usize,
}

/// A caller-supplied bound on what one inbox file may hold (**D526**).
///
/// The mechanics live here and the numbers do not: this crate enforces
/// whatever bound it is handed and decides nothing — the same posture as
/// [`crate::lock`], whose schedule is likewise somebody else's. Every ganja
/// writer passes the one ceiling `ganja-core` keeps beside its postbox;
/// [`write()`] passes none, which is the unbounded behavior a peer's own
/// tooling and this crate's fixtures still get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ceiling {
    /// The most messages the file may hold once an append lands.
    pub max_messages: usize,
    /// The most bytes the rewritten file may hold once an append lands,
    /// measured on the document this build would write.
    pub max_bytes: usize,
}

/// §2.3's `messageIdentityKey`: `from|timestamp|text`, and **not** `msg_id`.
///
/// A message's id is minted by whoever wrote it, so two builds looking at one
/// message would agree about it only if both had written it. The composite is
/// derivable by any reader, which is why delivery reconciliation uses it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Identity(String);

impl fmt::Debug for Identity {
    /// Renders the addressing half and the *size* of the body half.
    ///
    /// A derived `Debug` would put a message body into every `{:?}` — an error
    /// context, a `tracing` field, a panic message — which is exactly the leak
    /// `tests/no_bodies_in_logs.rs` exists to catch. Keeping the sender and the
    /// timestamp is what still makes a delivery mismatch debuggable.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (from, rest) = self.0.split_once('|').unwrap_or((self.0.as_str(), ""));
        let (timestamp, text) = rest.split_once('|').unwrap_or((rest, ""));

        write!(
            formatter,
            "Identity({from}|{timestamp}|<{} bytes>)",
            text.len()
        )
    }
}

/// The identity a delivery is reconciled by.
#[must_use]
pub fn identity(message: &MailboxMessage) -> Identity {
    Identity(format!(
        "{}|{}|{}",
        message.from, message.timestamp, message.text
    ))
}

/// Creates the inbox holding `[]`, and does nothing at all if it is already
/// there (§2.5's step 1).
///
/// This is `project::write_new`'s fifteen lines
/// (`ganja-permission/src/project.rs`) written out again rather than reached
/// for, and the duplication is deliberate: widening that crate's internal
/// allowlist so a mailbox could call it would trade a boundary CI asserts for
/// fifteen lines of `OpenOptions`. The **`EEXIST` branch is inverted**, which
/// is the other half of why sharing was never really available — `write_new`
/// unlinks and recreates, because its callers are about to rename over the
/// result anyway, while a seed that did that would delete an inbox somebody's
/// messages were sitting in.
///
/// # Errors
///
/// Whatever the directory creation, the create or the flush returned.
pub fn seed(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        // A team directory and its `inboxes/` are made on the way in: the
        // alternative is every caller remembering to, and forgetting reads as
        // "that teammate has no messages" rather than as an error.
        fs::create_dir_all(parent)?;
    }

    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        result => result?,
    };
    file.write_all(EMPTY_INBOX.as_bytes())?;

    file.sync_all()
}

/// Everything currently in the inbox, with what would not read counted.
///
/// An inbox that is not there yet is empty rather than an error — §2.5
/// swallows `ENOENT` as "nothing to do" throughout, and a teammate nobody has
/// written to yet is exactly that case.
///
/// # Errors
///
/// Whatever reading the file returned, other than its absence. A file that is
/// present and unreadable is a real failure; a file that is present and
/// *corrupt* is not, and comes back as drops.
pub fn read(path: &Path) -> io::Result<Contents> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Contents::default()),
        Err(error) => return Err(error),
    };

    Ok(parse(path, &text))
}

/// [`write_bounded`] with no ceiling — the unbounded append, kept under the
/// old name because a bound is the caller's to have (**D526**): this crate
/// holds no number of its own, and a peer's tooling or a test fixture appends
/// here without one.
///
/// # Errors
///
/// [`write_bounded`]'s, less [`MailboxError::Full`].
pub fn write(path: &Path, message: MailboxMessage) -> Result<String, MailboxError> {
    write_bounded(path, message, None)
}

/// Writes a message into the inbox, stamping §2.3's envelope, and answers with
/// the `msg_id` it stamped — refusing first when `ceiling` says the inbox is
/// full.
///
/// **A write is also a destructive prune.** It is a read-modify-write, and the
/// read keeps only what §2.4's validation accepts, so any *neighbouring* entry
/// this build cannot read is gone from the file once this returns — a write is
/// §2.4's `pruneInvalidMailboxEntries` with a message appended, whether or not
/// the caller wanted that. That is the original's own posture (it rewrites the
/// file without them) rather than an accident, and the drops are reported the
/// way every other drop is, once per distinct damage. The consequence worth
/// stating: an entry a *newer* peer wrote in a shape this build refuses is not
/// merely skipped, it is **deleted** by the next write here. Nothing in the
/// survey has that shape, and the passthrough exists precisely so a grown entry
/// stays readable — but that is the thing keeping this safe, so it is named
/// rather than assumed.
///
/// **The ceiling is checked under the hold** (**D526**), on the document this
/// write would produce — the entries that read cleanly plus this message — so
/// two writers racing an almost-full inbox cannot both squeeze past the bound.
/// A refusal is [`MailboxError::Full`] naming the counts it observed, and it
/// leaves the file **byte-identical**: nothing is written, not even the
/// rewrite that would have pruned an unreadable neighbour. The check encodes
/// the prospective document once beside the write's own encode; at any depth a
/// ceiling permits, that is noise under the fsync beneath it.
///
/// # Errors
///
/// [`MailboxError::SchemaInvalid`] when the message's passthrough map would
/// shadow one of the schema's own keys, [`MailboxError::Full`] for an append
/// the ceiling refuses, and whatever the read-modify-write returned.
pub fn write_bounded(
    path: &Path,
    mut message: MailboxMessage,
    ceiling: Option<Ceiling>,
) -> Result<String, MailboxError> {
    // §2.3: the write is what decides these three, not the sender. `read` is
    // forced false because §3.1 never writes it true — see the module note.
    message.kind = Some(MESSAGE_TYPE.to_owned());
    message.read = Some(false);
    message.msg_v = Some(MESSAGE_VERSION);
    let msg_id = message
        .msg_id
        .clone()
        .unwrap_or_else(ganja_protocol::uuidv7);
    message.msg_id = Some(msg_id.clone());

    // The guard `record::shadowed` documents: a schema key arriving through
    // the passthrough would be emitted twice. Nothing in this build puts one
    // there, and it is checked anyway because the cost of being wrong is a
    // corrupt shared file.
    let issues = shadowed(&message.extra, &SCHEMA_KEYS);
    if !issues.is_empty() {
        return Err(MailboxError::SchemaInvalid { issues });
    }

    let outcome = update(path, move |messages| {
        messages.push(message);
        if let Some(ceiling) = ceiling {
            let held = messages.len();
            let bytes = document(&*messages)?.len();
            if held > ceiling.max_messages || bytes > ceiling.max_bytes {
                return Err(MailboxError::Full {
                    held,
                    bytes,
                    max_messages: ceiling.max_messages,
                    max_bytes: ceiling.max_bytes,
                });
            }
        }

        Ok(())
    });
    if let Err(MailboxError::Full { held, bytes, .. }) = &outcome {
        // Counts and a path, never a body (the module's own rule): the
        // refusal is the caller's to surface, and this line is where the
        // inbox it protected is named.
        tracing::warn!(
            inbox = %path.display(),
            held,
            bytes,
            "an append was refused at the inbox's ceiling"
        );
    }
    outcome?;
    tracing::debug!(inbox = %path.display(), %msg_id, "a message joined an inbox");

    Ok(msg_id)
}

/// §3.1's `markMessagesAsRead`: delivered messages are removed, not flagged.
///
/// Like [`write()`], this is a read-modify-write and therefore **also a
/// destructive prune of anything unreadable** — see that function's note. The
/// [`Pruned`] count answers for delivered messages only; an entry dropped for
/// being damaged is not in it, and is gone from the file all the same.
///
/// # Errors
///
/// Whatever the read-modify-write returned.
pub fn prune_delivered(path: &Path, delivered: &[Identity]) -> Result<Pruned, MailboxError> {
    let delivered: HashSet<Identity> = delivered.iter().cloned().collect();
    let pruned = update(path, |messages| {
        let before = messages.len();
        messages.retain(|message| {
            !message.read.unwrap_or(false) && !delivered.contains(&identity(message))
        });

        Ok(Pruned {
            pruned: before - messages.len(),
            remaining: messages.len(),
        })
    })?;
    tracing::debug!(
        inbox = %path.display(),
        pruned = pruned.pruned,
        remaining = pruned.remaining,
        "pruned delivered messages",
    );

    Ok(pruned)
}

/// Read, change, write back — the shape every mutation here has (§2.5).
///
/// The closure is handed the entries that read cleanly, and whatever it leaves
/// behind is what lands on disk. **A closure that refuses stops the write**:
/// it leaves through the `?` below before [`write_atomically`] runs, so the
/// file is byte-identical to what the read found. Exactly one refusal lives
/// there — [`write_bounded`]'s ceiling check (**D526**) — and it sits inside
/// rather than in front of the hold on purpose: judged outside it, two
/// writers racing an almost-full inbox would both observe room and both land.
/// Every *other* precondition still refuses before getting here, so the hold
/// is never taken for a write that its own arguments doomed. The I/O after
/// the closure can fail too — the read, the encode and the atomic replace
/// each return — and every early exit releases the hold through
/// [`Guard`](crate::lock::Guard)'s `Drop` on the way out.
///
/// Because the write-back is whatever the read produced, **every mutation here
/// prunes what would not read** — see [`write_bounded`]'s own note, which is
/// where that consequence is stated for callers rather than for this function
/// — while a mutation that *refused* prunes nothing, which is the other half
/// of the same contract.
///
/// The two steps below are §2.5's own order and not an implementation detail:
/// the seed comes first because the lock is on `realpath(target) + ".lock"` and
/// a file that is not there has no real path, and the hold covers the read as
/// well as the write because the whole point is that nothing lands on top of an
/// entry that arrived after this read. Everything under it — [`crate::lock`]'s
/// protocol, its ladder and its stale break — is somebody else's decision,
/// reproduced so a real `claude` sharing this inbox agrees about who holds it.
///
/// The binding is named rather than `_`, which would drop the guard where it
/// stands and hold nothing at all.
fn update<T>(
    path: &Path,
    change: impl FnOnce(&mut Vec<MailboxMessage>) -> Result<T, MailboxError>,
) -> Result<T, MailboxError> {
    seed(path)?;
    let _hold = lock::acquire(path)?;

    let mut messages = read(path)?.valid;
    let outcome = change(&mut messages)?;
    write_atomically(path, document(&messages)?.as_bytes())?;

    Ok(outcome)
}

/// Replaces the file's contents in one step, keeping the mode it had.
///
/// `NamedTempFile` creates its file `0600`, and an inbox is shared: a rewrite
/// that left `0600` behind would lock a peer out of a file it had been reading
/// until something else recreated it, and one that widened the mode would hand
/// out a conversation. So the target's existing bits are copied onto the temp
/// file before the rename. There is always a target to copy from, because
/// [`seed`] ran first.
///
/// **The parent directory is deliberately not fsynced**, and the asymmetry is
/// worth naming because the temp file *is*. `sync_all` guarantees the new bytes
/// are durable; only an `fsync` on the directory would guarantee the **rename**
/// is. So a power loss in the window between `persist` and the next directory
/// flush can leave a reader seeing the *old* array — the previous message set,
/// intact and parseable, minus the message just written. That is accepted rather
/// than fixed: the lost outcome is indistinguishable from the message never
/// having been sent, which is a state every caller already handles, and the fix
/// would put a second fsync on the hot path of every mailbox write to buy
/// durability for an inbox whose whole contents are transient backlog. A crash
/// can never produce a *torn* file, which is the property that would actually
/// matter — `persist` is a `rename(2)`, so a reader sees one array or the other.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "an inbox path names a file inside a directory",
        )
    })?;

    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if let Ok(existing) = fs::metadata(path) {
        temp.as_file().set_permissions(existing.permissions())?;
    }
    temp.persist(path)?;

    Ok(())
}

/// §2.4's `safeParse`, in ganja's vocabulary.
///
/// Checks the fields the schema declares and says nothing about any other key,
/// which is what makes the shape a passthrough. Every sentence it produces
/// names a field, an expectation and a JSON type — the diagnostic is
/// field-level for the same reason the original's is, so a dropped entry says
/// which field failed and how, and none of them carries a value.
///
/// # Errors
///
/// [`MailboxError::TextNotAString`] for §2.4's distinctly reported case, and
/// [`MailboxError::SchemaInvalid`] for everything else. The `text` check runs
/// first, so an entry that is wrong in several ways is reported as that one.
pub(crate) fn validate(entry: &Value) -> Result<(), MailboxError> {
    let Value::Object(fields) = entry else {
        return Err(MailboxError::SchemaInvalid {
            issues: vec![format!(
                "an entry is an object; this one is {}",
                type_name(entry)
            )],
        });
    };

    match fields.get("text") {
        Some(Value::String(_)) => {}
        Some(other) => {
            return Err(MailboxError::TextNotAString {
                found: type_name(other),
            });
        }
        None => {
            return Err(MailboxError::SchemaInvalid {
                issues: vec!["text: required, and absent".to_owned()],
            });
        }
    }

    let mut issues = Vec::new();
    for key in ["from", "timestamp"] {
        match fields.get(key) {
            Some(Value::String(_)) => {}
            Some(other) => issues.push(format!(
                "{key}: expected a string, found {}",
                type_name(other)
            )),
            None => issues.push(format!("{key}: required, and absent")),
        }
    }
    for key in ["type", "color", "summary", "msg_id"] {
        if let Some(present) = fields.get(key)
            && !present.is_string()
        {
            issues.push(format!(
                "{key}: expected a string when present, found {}",
                type_name(present)
            ));
        }
    }
    if let Some(present) = fields.get("read")
        && !present.is_boolean()
    {
        issues.push(format!(
            "read: expected a boolean when present, found {}",
            type_name(present)
        ));
    }
    if let Some(present) = fields.get("msgV")
        && !present.is_u64()
    {
        issues.push(format!(
            "msgV: expected a whole number when present, found {}",
            type_name(present)
        ));
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(MailboxError::SchemaInvalid { issues })
    }
}

/// A JSON value's type, as a word a sentence can use. Never its contents.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// One inbox file's text, read as far as it can be (§2.4).
///
/// Entries are held as [`RawValue`](serde_json::value::RawValue) between the
/// two passes rather than as `Value`s, and that is not an optimization: a
/// `Value`'s object is a `BTreeMap`, so decoding through one would hand every
/// unknown key back alphabetized and quietly break the byte identity the whole
/// format contract rests on. The raw bytes go to the decoder, and the `Value`
/// is only ever looked at.
fn parse(path: &Path, text: &str) -> Contents {
    let entries = match serde_json::from_str::<Vec<&serde_json::value::RawValue>>(text) {
        Ok(entries) => entries,
        Err(_) => {
            // Told apart so the sentence is true: a file of prose and a file
            // holding one JSON object fail differently and a reader acts on
            // them differently.
            let sentence = match serde_json::from_str::<Value>(text) {
                Ok(value) => format!("{DROPPED_NOT_AN_ARRAY}, found {}", type_name(&value)),
                Err(_) => DROPPED_NOT_JSON.to_owned(),
            };

            return Contents {
                valid: Vec::new(),
                dropped: 1,
                reports: report(path, text, sentence).into_iter().collect(),
            };
        }
    };

    let mut contents = Contents::default();
    for entry in entries {
        let raw = entry.get();
        let sentence = match serde_json::from_str::<Value>(raw) {
            Ok(value) => match validate(&value) {
                Ok(()) => match serde_json::from_str::<MailboxMessage>(raw) {
                    Ok(message) => {
                        contents.valid.push(message);
                        continue;
                    }
                    Err(_) => DROPPED_UNDECODABLE.to_owned(),
                },
                Err(refusal) => refusal.to_string(),
            },
            // Unreachable through the array parse above, which already decoded
            // every element far enough to find its end.
            Err(_) => DROPPED_NOT_JSON.to_owned(),
        };

        contents.dropped += 1;
        contents.reports.extend(report(path, raw, sentence));
    }

    contents
}

/// The process's memory of what it has already complained about (§2.4's
/// `reportedDroppedEntries`).
static REPORTED: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// `sentence`, unless this exact damage has already been reported.
///
/// §2.4 keys the memory by `` `${path}\0${len}:${serialized[..2048]}` ``. That
/// key holds a message body, so what is kept here is its **hash**: the dedupe
/// is identical and the process does not sit on a copy of a conversation for
/// its lifetime.
///
/// [`Option`] rather than a `Vec` of at most one, because "this was reported, or
/// it was not" is the whole answer and a length is a question a caller should
/// not have to ask. Both call sites collect it the same way a `Vec` was
/// collected, since [`Option`] is itself an iterator of nought or one.
fn report(path: &Path, serialized: &str, sentence: String) -> Option<String> {
    let mut hasher = DefaultHasher::new();
    (path, serialized.len(), clamp(serialized, REPORT_KEY_CAP)).hash(&mut hasher);
    let key = hasher.finish();

    let Ok(mut reported) = REPORTED.lock() else {
        // A poisoned set means some other thread panicked mid-report. Saying
        // it once more is a better failure than going quiet about corruption.
        return Some(sentence);
    };
    if !first_report(&mut reported, key) {
        return None;
    }
    drop(reported);

    tracing::warn!(inbox = %path.display(), reason = %sentence, "dropped an unreadable inbox entry");

    Some(sentence)
}

/// Whether this key is new *and* there is still room to remember it.
///
/// Separated from the global above so the cap is testable without a test
/// depending on what every other test in the binary has already reported.
fn first_report(reported: &mut HashSet<u64>, key: u64) -> bool {
    if reported.contains(&key) || reported.len() >= MAX_REPORTED {
        return false;
    }
    reported.insert(key);

    true
}

/// The first `cap` bytes of `text`, cut on a character boundary.
fn clamp(text: &str, cap: usize) -> &str {
    &text[..text.floor_char_boundary(cap)]
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs};

    use serde_json::json;

    use super::{
        Ceiling, Contents, MailboxError, first_report, identity, prune_delivered, read, seed,
        validate, write, write_bounded,
    };
    use crate::record::{MailboxMessage, SCHEMA_KEYS};

    const WHEN: &str = "2026-08-17T00:00:00.000Z";

    fn inbox() -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().expect("a temp directory");
        let path = home.path().join("teams/t/inboxes/worker.json");

        (home, path)
    }

    /// The body of the entry [`damaged_pair`] plants beside a clean one.
    const DAMAGED_BODY: &str = "s3cret-body";

    /// An inbox holding one entry that reads and one that does not — its
    /// `timestamp` is a number — with [`DAMAGED_BODY`] in the damaged one.
    fn damaged_pair(path: &std::path::Path) {
        seed(path).expect("the inbox seeds");
        fs::write(
            path,
            serde_json::to_string(&json!([
                {"from": "w", "text": "kept", "timestamp": WHEN},
                {"from": "w", "text": DAMAGED_BODY, "timestamp": 7},
            ]))
            .expect("the fixture encodes"),
        )
        .expect("the inbox is writable");
    }

    #[test]
    fn a_delivered_message_does_not_remain() {
        let (_home, path) = inbox();
        write(&path, MailboxMessage::new("team-lead", "first", WHEN)).expect("a message writes");
        write(&path, MailboxMessage::new("team-lead", "second", WHEN)).expect("a message writes");

        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.valid.len(), 2);
        assert_eq!(held.dropped, 0);

        let delivered = vec![identity(&held.valid[0])];
        let pruned = prune_delivered(&path, &delivered).expect("the prune writes");
        assert_eq!(pruned.pruned, 1);
        assert_eq!(pruned.remaining, 1);

        let left = read(&path).expect("the inbox reads");
        assert_eq!(left.valid.len(), 1);
        assert_eq!(left.valid[0].text, "second");
        // A tombstone that is never written: §3.1's whole correction.
        assert_eq!(left.valid[0].read, Some(false));
    }

    #[test]
    fn a_write_stamps_the_envelope_and_seeds_an_absent_inbox() {
        let (_home, path) = inbox();
        let id = write(&path, MailboxMessage::new("w", "hello", WHEN)).expect("a message writes");

        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.valid[0].msg_id.as_deref(), Some(id.as_str()));
        assert_eq!(held.valid[0].msg_v, Some(1));
        assert_eq!(held.valid[0].kind.as_deref(), Some("message"));

        // Seeding an inbox that already holds messages must not empty it.
        seed(&path).expect("a second seed is a no-op");
        assert_eq!(read(&path).expect("the inbox reads").valid.len(), 1);
    }

    #[test]
    fn a_non_array_top_level_reads_as_one_dropped_entry() {
        let (_home, path) = inbox();
        seed(&path).expect("the inbox seeds");

        fs::write(&path, "{\"from\": \"w\"}").expect("the inbox is writable");
        let held = read(&path).expect("the inbox reads");
        assert_eq!(
            held,
            Contents {
                valid: Vec::new(),
                dropped: 1,
                reports: vec![format!("{}, found an object", super::DROPPED_NOT_AN_ARRAY)],
            }
        );

        // And any mutation's rewrite leaves a file that reads clean: the
        // non-array is gone, and only what was written over it remains.
        write(&path, MailboxMessage::new("w", "over it", WHEN)).expect("a message writes");
        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.dropped, 0);
        assert_eq!(held.valid.len(), 1);

        // A file that is not JSON at all fails differently, and says so.
        fs::write(&path, "not json").expect("the inbox is writable");
        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.dropped, 1);
        assert_eq!(held.reports, vec![super::DROPPED_NOT_JSON.to_owned()]);
    }

    #[test]
    fn a_non_string_text_is_refused_as_its_own_case_and_every_other_field_by_name() {
        // §2.4's one distinctly reported field: the type is named, the value
        // is not.
        let refusal = validate(&json!({"from": "w", "text": 42, "timestamp": WHEN}))
            .expect_err("a number is not a message body");
        assert!(
            matches!(refusal, MailboxError::TextNotAString { found: "a number" }),
            "{refusal:?}"
        );
        assert!(refusal.to_string().contains("holds a number"));

        // Everything else is the other refusal, one sentence per field.
        let refusal = validate(&json!({"text": "hi", "timestamp": 7}))
            .expect_err("a message needs a sender and a timestamp");
        let MailboxError::SchemaInvalid { issues } = refusal else {
            panic!("expected a schema refusal, got {refusal:?}");
        };
        assert_eq!(
            issues,
            [
                "from: required, and absent".to_owned(),
                "timestamp: expected a string, found a number".to_owned(),
            ]
        );
    }

    #[test]
    fn a_message_whose_extra_shadows_a_schema_key_is_refused_before_the_file_is_touched() {
        let (_home, path) = inbox();
        write(&path, MailboxMessage::new("w", "kept", WHEN)).expect("a message writes");
        let before = fs::read_to_string(&path).expect("the inbox is readable");

        // Two shadowing keys, so the refusal is pinned as naming every
        // offender rather than the first one it met.
        let mut shadowing = MailboxMessage::new("w", "impostor", WHEN);
        shadowing
            .extra
            .insert("text".to_owned(), json!("a second body"));
        shadowing.extra.insert("read".to_owned(), json!(true));
        let refusal = write(&path, shadowing).expect_err("a shadowed schema key is refused");
        let MailboxError::SchemaInvalid { issues } = refusal else {
            panic!("expected a schema refusal, got {refusal:?}");
        };
        assert_eq!(
            issues,
            [
                "text: the shape declares this key, so a passthrough map may not also carry it"
                    .to_owned(),
                "read: the shape declares this key, so a passthrough map may not also carry it"
                    .to_owned(),
            ]
        );

        // Refused before the file was touched, which is the half that matters:
        // a rejected write must not cost the messages already queued, and it
        // must not have taken a hold to find out.
        assert_eq!(
            fs::read_to_string(&path).expect("the inbox is readable"),
            before
        );
        assert!(
            !std::path::PathBuf::from(format!("{}.lock", path.display())).exists(),
            "a refusal that never reached the disk took no lock"
        );
    }

    #[test]
    fn an_entry_that_passes_its_field_checks_and_still_will_not_decode_is_dropped_by_name() {
        let (_home, path) = inbox();
        seed(&path).expect("the inbox seeds");
        // `msgV` is checked as a whole number (§2.4's `is_u64`), and 2^32 is
        // one — it is also one more than the `u32` the envelope declares, so
        // the field check passes and the typed decode does not.
        fs::write(
            &path,
            r#"[{"from": "w", "text": "s3cret-body", "timestamp": "t", "msgV": 4294967296}]"#,
        )
        .expect("the inbox is writable");

        let held = read(&path).expect("the inbox reads");
        assert!(held.valid.is_empty());
        assert_eq!(held.dropped, 1);
        // The constant and nothing else: the decoder's own sentence can quote
        // the value it choked on, and a value is a message body.
        assert_eq!(held.reports, [super::DROPPED_UNDECODABLE.to_owned()]);
    }

    #[test]
    fn a_dropped_entry_names_the_field_and_never_the_value() {
        let (_home, path) = inbox();
        damaged_pair(&path);

        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.valid.len(), 1);
        assert_eq!(held.dropped, 1);
        assert_eq!(held.reports.len(), 1);
        assert!(held.reports[0].contains("timestamp: expected a string, found a number"));
        assert!(
            !held.reports[0].contains(DAMAGED_BODY),
            "a drop report names fields and types, never a body: {}",
            held.reports[0]
        );
    }

    #[test]
    fn drop_reports_dedupe_and_stop_at_one_hundred() {
        // Driven against a set of its own rather than the process-wide one, so
        // the cap is what is under test and not what every other test in this
        // binary has already reported.
        let mut reported = HashSet::new();
        for key in 0..super::MAX_REPORTED {
            let key = u64::try_from(key).expect("an index under a hundred fits");
            assert!(first_report(&mut reported, key), "{key} is new");
            assert!(!first_report(&mut reported, key), "{key} is not new twice");
        }
        assert_eq!(reported.len(), super::MAX_REPORTED);
        assert!(
            !first_report(&mut reported, 1_000),
            "past the cap, a new key is not reported either"
        );

        // And the process-wide memory really is consulted: the same damage in
        // the same file reports once and then goes quiet.
        let (_home, path) = inbox();
        seed(&path).expect("the inbox seeds");
        fs::write(&path, "{\"unique\": \"to this temp path\"}").expect("the inbox is writable");
        assert_eq!(read(&path).expect("the inbox reads").reports.len(), 1);
        assert!(read(&path).expect("the inbox reads").reports.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_rewrite_keeps_the_inboxes_existing_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_home, path) = inbox();
        seed(&path).expect("the inbox seeds");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("the mode is settable");

        write(&path, MailboxMessage::new("w", "hello", WHEN)).expect("a message writes");

        let mode = fs::metadata(&path)
            .expect("the inbox is there")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o640,
            "a rewrite neither tightens nor loosens what the peer wrote"
        );
    }

    #[test]
    fn the_schema_key_list_is_exactly_what_a_message_serializes() {
        // `SCHEMA_KEYS` is the shadow guard's list and `validate`'s field lists
        // are the drop diagnostics'. Both are written out by hand beside a
        // struct that is written out by hand, so nothing but this test stops a
        // tenth field from being declared and silently governed by neither.
        let whole = MailboxMessage {
            from: "w".to_owned(),
            text: "hi".to_owned(),
            summary: Some("s".to_owned()),
            timestamp: WHEN.to_owned(),
            color: Some("blue".to_owned()),
            msg_v: Some(1),
            msg_id: Some("x".to_owned()),
            kind: Some("message".to_owned()),
            read: Some(false),
            extra: indexmap::IndexMap::new(),
        };
        let serde_json::Value::Object(fields) =
            serde_json::to_value(&whole).expect("a message encodes")
        else {
            panic!("a message is an object");
        };

        assert_eq!(
            fields.keys().map(String::as_str).collect::<HashSet<_>>(),
            SCHEMA_KEYS.iter().copied().collect::<HashSet<_>>(),
            "every declared field is a schema key and every schema key is a field",
        );

        // And every one of them is a key `validate` actually checks: give each
        // in turn a type no field of the schema accepts, and the refusal has to
        // name that key.
        for key in SCHEMA_KEYS {
            let mut broken = fields.clone();
            broken[key] = json!([]);
            let refusal = super::validate(&serde_json::Value::Object(broken))
                .expect_err("an array is not any of the schema's types");
            assert!(
                refusal.to_string().contains(key),
                "validate ignores {key}: {refusal}",
            );
        }
    }

    #[test]
    fn a_write_also_deletes_a_damaged_neighbour() {
        let (_home, path) = inbox();
        damaged_pair(&path);

        // A read changes nothing.
        read(&path).expect("the inbox reads");
        assert!(
            fs::read_to_string(&path)
                .expect("the inbox is readable")
                .contains(DAMAGED_BODY),
            "a read is not a rewrite",
        );

        // One ordinary write, and the neighbour is *gone from the file*. This is
        // the destructive half `write`'s own doc names: nobody asked for a
        // prune, and one happened.
        write(&path, MailboxMessage::new("w", "new", WHEN)).expect("a message writes");

        let after = fs::read_to_string(&path).expect("the inbox is readable");
        assert!(
            !after.contains(DAMAGED_BODY),
            "a write rewrites only what read cleanly, so the damaged entry is deleted: {after}",
        );
        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.dropped, 0, "and nothing unreadable is left to drop");
        assert_eq!(
            held.valid
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["kept", "new"],
            "the readable neighbour and the new message both survive, in order",
        );
    }

    #[test]
    fn a_report_key_is_clamped_on_a_character_boundary() {
        // Three-byte characters, so the cap falls inside one: 2048 is not a
        // multiple of three, and a byte-indexed cut there would panic.
        let wide = "€".repeat(1_000);
        let cut = super::clamp(&wide, super::REPORT_KEY_CAP);
        assert_eq!(cut.len(), 2_046);
        assert!(cut.chars().all(|character| character == '€'));
        assert_eq!(super::clamp("short", super::REPORT_KEY_CAP), "short");

        // And through the door that clamps: an entry longer than the cap with
        // such a character straddling it is dropped and reported, not a panic.
        // The raw entry is what `report` clamps, so it is the raw entry whose
        // cap must fall mid-character.
        let entry = format!(r#"{{"from":"ww","text":"{wide}","timestamp":7}}"#);
        assert!(
            !entry.is_char_boundary(super::REPORT_KEY_CAP),
            "the fixture straddles the cap"
        );
        let (_home, path) = inbox();
        seed(&path).expect("the inbox seeds");
        fs::write(&path, format!("[{entry}]")).expect("the inbox is writable");
        let held = read(&path).expect("the inbox reads");
        assert_eq!(held.dropped, 1);
        assert_eq!(held.reports.len(), 1);
    }

    /// A ceiling no test here ever reaches on the axis it is not testing.
    const ROOMY: usize = usize::MAX;

    #[test]
    fn an_append_past_the_message_bound_is_refused_naming_the_counts() {
        let (_home, path) = inbox();
        let ceiling = Some(Ceiling {
            max_messages: 2,
            max_bytes: ROOMY,
        });
        write_bounded(&path, MailboxMessage::new("w", "first", WHEN), ceiling)
            .expect("an inbox under its ceiling takes a message");
        write_bounded(&path, MailboxMessage::new("w", "second", WHEN), ceiling)
            .expect("an inbox at its last slot takes a message");
        let before = fs::read_to_string(&path).expect("the inbox is readable");

        let refusal = write_bounded(&path, MailboxMessage::new("w", "third", WHEN), ceiling)
            .expect_err("an inbox at its message bound refuses the append");
        let sentence = refusal.to_string();
        assert!(
            sentence.contains("3 messages") && sentence.contains("2 messages"),
            "the refusal names the observed count and the bound: {sentence}"
        );
        let MailboxError::Full {
            held,
            max_messages,
            max_bytes,
            ..
        } = refusal
        else {
            panic!("expected a full refusal, got {refusal:?}");
        };
        assert_eq!(held, 3, "the counts are what the append would have left");
        assert_eq!(max_messages, 2);
        assert_eq!(max_bytes, ROOMY);

        // The refusal wrote nothing: the file is byte-identical, and a read
        // still finds exactly the two admitted messages.
        assert_eq!(
            fs::read_to_string(&path).expect("the inbox is readable"),
            before
        );
        assert_eq!(read(&path).expect("the inbox reads").valid.len(), 2);
    }

    #[test]
    fn an_append_past_the_byte_bound_is_refused_naming_the_counts() {
        let (_home, path) = inbox();
        let ceiling = Some(Ceiling {
            max_messages: ROOMY,
            max_bytes: 256,
        });
        write_bounded(&path, MailboxMessage::new("w", "short", WHEN), ceiling)
            .expect("a small message fits under the byte bound");
        let before = fs::read_to_string(&path).expect("the inbox is readable");

        let oversized = "x".repeat(300);
        let refusal = write_bounded(&path, MailboxMessage::new("w", oversized, WHEN), ceiling)
            .expect_err("an append past the byte bound is refused");
        let MailboxError::Full { held, bytes, .. } = refusal else {
            panic!("expected a full refusal, got {refusal:?}");
        };
        assert_eq!(held, 2);
        assert!(
            bytes > 256,
            "the observed byte count is the document the append would have written: {bytes}"
        );

        assert_eq!(
            fs::read_to_string(&path).expect("the inbox is readable"),
            before
        );
    }

    #[test]
    fn a_ceiling_refusal_leaves_even_a_damaged_neighbour_untouched() {
        // The half of "byte-identical" that is easy to lose: an ordinary
        // write destructively prunes what would not read (the test above
        // this module's write doc), so a refusal that still wrote the file
        // back would delete the neighbour as a side effect of refusing.
        let (_home, path) = inbox();
        damaged_pair(&path);
        let before = fs::read_to_string(&path).expect("the inbox is readable");

        let refusal = write_bounded(
            &path,
            MailboxMessage::new("w", "one too many", WHEN),
            Some(Ceiling {
                max_messages: 1,
                max_bytes: ROOMY,
            }),
        )
        .expect_err("the valid entry already fills the one slot");
        assert!(matches!(refusal, MailboxError::Full { held: 2, .. }));

        let after = fs::read_to_string(&path).expect("the inbox is readable");
        assert_eq!(after, before, "a refusal is not a rewrite");
        assert!(
            after.contains(DAMAGED_BODY),
            "the unreadable neighbour survives a refused append"
        );
    }

    #[test]
    fn no_ceiling_keeps_the_unbounded_append() {
        let (_home, path) = inbox();
        for n in 0..4 {
            write_bounded(&path, MailboxMessage::new("w", format!("{n}"), WHEN), None)
                .expect("an unbounded append always lands");
        }
        // And `write` is that spelling under the old name.
        write(&path, MailboxMessage::new("w", "fifth", WHEN)).expect("a message writes");

        assert_eq!(read(&path).expect("the inbox reads").valid.len(), 5);
    }

    #[test]
    fn an_identity_debug_renders_no_body() {
        let rendered = format!(
            "{:?}",
            identity(&MailboxMessage::new("w", "s3cret-body", WHEN))
        );

        assert_eq!(rendered, format!("Identity(w|{WHEN}|<11 bytes>)"));
    }
}

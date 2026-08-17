//! Where sessions live between runs: one SQLite database per project.
//!
//! Spec: upstream `packages/core/src/database/database.ts` for the connection
//! and its pragmas, `packages/core/src/database/migration.ts` for the journal
//! and the three-way branch a first open takes, and
//! `packages/core/src/database/schema.gen.ts` for the shape of the three
//! tables a session needs. What is ported is the *machinery*, not upstream's
//! 38 migrations: those are its history, and this tree starts at its own base
//! schema.
//!
//! ```text
//! session (id, parent_id, time_created, time_updated, data)
//! message (session_id, id, time_created, time_updated, data)
//! part    (session_id, message_id, id, time_created, time_updated, data)
//! ```
//!
//! `data` is opaque JSON, exactly as upstream keeps it: a part is not shredded
//! into columns, so the serde shape stays the one every other module speaks
//! and a schema change is not owed for every new part variant. The columns
//! beside it are identity and ordering only.
//!
//! Parts live in their own rows rather than inside their envelope for the same
//! reason they used to live in their own files: a streaming turn rewrites one
//! small row per fragment instead of the whole message. Ascending ids double
//! as ordering — reassembly is `ORDER BY id`, never by a timestamp — and that
//! is still true now that an id is a lowercase hyphenated UUIDv7 (**D493**):
//! the four hyphens sit at fixed positions in every one of them so they never
//! discriminate, and `'0'..='9'` precedes `'a'..='f'` in ASCII exactly as
//! their values order, so a lexicographic sort of these strings is a sort by
//! the millisecond timestamp they open with.
//!
//! What changed underneath is the guarantee, not the ordering. The
//! `<millis hex><counter hex>` layout ids used to have counted from zero in
//! each *process*, so two of them reaching one millisecond minted the same id
//! — which is why a store holding such rows is **set aside** rather than
//! minted into, both as a database — `sessions.db.preuuid-<millis>` — and as
//! an older store's tree, `storage.preuuid-<millis>`. Nothing is deleted
//! either way: the file or the directory is renamed and a fresh, empty store
//! takes the name.
//!
//! Every record still carries a `version` field — [`SessionInfo`] inline,
//! message and part rows through an envelope `{"version":1,"payload":…}` — and
//! it is what a build reads before it decodes anything: a row a newer build
//! wrote is left exactly where it is. A row this build cannot decode at all is
//! skipped with a warning and left in place, which is the database's version
//! of the rename-aside `quarantine` does to a file: nothing is destroyed and
//! one unreadable session costs that session, never the ability to start one.
//!
//! # Where the writes go
//!
//! A [`rusqlite::Connection`] is [`Send`] but not [`Sync`], so the shape has to
//! be chosen rather than assumed. Writes go to a dedicated thread over an
//! `mpsc` queue: it preserves the ordering guarantee the turn task used to
//! hold by convention — two writes racing out of order would persist stale
//! content — *structurally*, in the queue, so it survives a caller that one
//! day stops waiting for its answer. Reads take a second connection, which is
//! what WAL buys: a listing never blocks the turn that is writing.
//!
//! # Why `bundled`
//!
//! `libsqlite3-sys` is built from vendored sources rather than linked against
//! whatever the machine has, because the pragma *defaults* differ between
//! builds — this one compiles `SQLITE_DEFAULT_FOREIGN_KEYS=1` and
//! `SQLITE_DEFAULT_WAL_SYNCHRONOUS=2`, stock SQLite does neither — and pinning
//! the library is what pins the semantics. Nothing here relies on those
//! defaults; every pragma is set explicitly, on every connection.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::protocol::{
    Message, MessageId, Part, PartBody, PartId, REASONING_TAG, Usage, is_uuidv7, now,
};

/// The record format this build writes.
pub const VERSION: u32 = 1;

/// The database file, beside the `storage/` directory a store is anchored on.
///
/// Suffixed on a debug build the way upstream suffixes by release channel
/// (`database.ts:48-54`): a build being worked on must not write into the file
/// an installed one reads, because the schema under development is exactly the
/// thing that has not settled yet.
const DATABASE: &str = if cfg!(debug_assertions) {
    "sessions-dev.db"
} else {
    "sessions.db"
};

/// Directory every session artefact used to hang under, and the one a first
/// open converts from.
const SESSION: &str = "session";

/// Directory that held one info file per session.
const INFO: &str = "info";

/// Directory that held one directory of message envelopes per session.
const MESSAGE: &str = "message";

/// Directory that held one directory of part files per message.
const PART: &str = "part";

/// Extension every stored file carried, and the only one a listing read.
const EXTENSION: &str = "json";

/// What a file or a database that could not be read is renamed to, ahead of
/// the moment it was set aside: `<name>.corrupt-<millis>`.
const QUARANTINE: &str = "corrupt";

/// What a converted `storage/` tree is renamed to: `storage.migrated-<millis>`.
const MIGRATED: &str = "migrated";

/// What a store minted before UUIDv7 ids is renamed to:
/// `sessions.db.preuuid-<millis>`, or `storage.preuuid-<millis>` for an older
/// store's tree.
///
/// A second suffix beside [`QUARANTINE`] rather than that one reused, because
/// the two say different things to whoever reads the directory afterwards: a
/// `corrupt-` file is one nothing could read, while a `preuuid-` file reads
/// perfectly and is set aside only because its ids came from the
/// process-local counter [`crate::protocol::uuidv7`] replaced (**D493**) —
/// superseded, not unreadable.
const PREUUID: &str = "preuuid";

/// What the lock that quarantine is decided under is named:
/// `sessions.db.quarantine.lock`.
///
/// Beside the database rather than inside the data home, because the thing
/// being coordinated is this one file and the processes that race for it are
/// the ones that opened this project. See [`QuarantineLock`].
const QUARANTINE_LOCK: &str = "quarantine.lock";

/// Every connection sets these, immediately after open.
///
/// `journal_mode` is a property of the file and persists; the other four are
/// properties of the *connection* and reset on each new one. `synchronous` is
/// the trap in that family: this build compiles `DEFAULT_WAL_SYNCHRONOUS=2`,
/// so a connection that forgets it silently runs at `FULL` — safe, four times
/// slower, and with nothing to say why.
///
/// `NORMAL` rather than `FULL` is proven rather than assumed: 128 `SIGKILL`
/// iterations against 2.3M acknowledged rows lost none of them, because
/// `synchronous` governs *fsync*, and fsync defends against power loss and
/// kernel panic — not against a process dying and leaving its already-written
/// WAL frames in the OS page cache. Process death is the only crash this
/// program can cause, and it is the one the resume drill fires.
///
/// `foreign_keys` is not SQLite's own default and is per-connection: without
/// it the `ON DELETE CASCADE` that carries a message's parts away with it
/// silently does nothing, and the orphans are immediately visible to a
/// connection that did set it.
///
/// **`busy_timeout` goes first**, which is one of two places this diverges
/// from upstream's order (`database.ts:27-31`), and it is not cosmetic: a
/// connection that meets a busy database before its timeout is set fails
/// outright instead of waiting. The other is `journal_mode`, which is not in
/// this list at all — it is a property of the *file* rather than of the
/// connection, and it needs the handling [`wal`] gives it.
const PRAGMAS: &[&str] = &[
    "PRAGMA busy_timeout = 5000",
    "PRAGMA synchronous  = NORMAL",
    "PRAGMA foreign_keys = ON",
    "PRAGMA cache_size   = -64000",
];

/// How many times the switch into WAL is attempted before the error is the
/// caller's. Generous, because each attempt is a lock the winner holds for
/// microseconds; see [`wal`].
const WAL_ATTEMPTS: u32 = 50;

/// How long to wait between those attempts.
const WAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// The base schema, and the only migration this build has.
///
/// Three deliberate departures from upstream's table shapes, each because
/// ganja's ids and ganja's operations are not upstream's:
///
/// 1. **The primary keys are composite**, `(session_id, id)` on a message and
///    `(session_id, message_id, id)` on a part, which is exactly the key the
///    file layout used: `message/<sid>/<mid>.json`. A session id is the
///    namespace a message id is unique *in*, and the key says so. It was
///    written when ganja's ids were `<millis hex><process-local counter hex>`,
///    where two `ganja` processes in one project minting the same `msg_…` in
///    one millisecond was not a risk but a certainty; ids are UUIDv7 now
///    (**D493**) and that collision is merely astronomically unlikely. The key
///    stays, for two reasons that outlive the arithmetic: it is the index the
///    read path uses — `ORDER BY id` within a session, and the cascade's child
///    lookup, both served by its prefix, as the paragraph below says — and it
///    is what a collision costs nothing to survive. Under upstream's bare `id`
///    an upsert that met one would silently overwrite the *other* session's
///    message; here it lands in its own row, as it always did.
/// 2. **A message names its session without a foreign key to it.** The only
///    deletion this store performs is [`Storage::delete_message`]; there is no
///    delete-session, so a cascade from `session` would never fire, and a
///    constraint that never fires only forbids something that is currently
///    allowed — writing a message before its session record, which seeding
///    fixtures do.
/// 3. **The part → message cascade is the one foreign key**, and it is what
///    replaces the manual parts-then-envelope ordering the file layout needed:
///    an interrupted deletion had to leave a message short of content rather
///    than parts with no envelope, and a cascade inside one statement cannot
///    be interrupted between the two.
///
/// Upstream's four read-path indices (`schema.gen.ts:245-271`) reduce to one
/// here, and the other three are not missing but subsumed: `ORDER BY id`
/// within a session is served by the `message` primary key, a message's parts
/// and the cascade's child lookup by the `part` primary key's prefix, and
/// `session_project_idx` has no column to index in a database that holds one
/// project. `session_parent_idx` is the one that stands on its own — a listing
/// that shows roots reads [`SessionInfo::parent`].
const SCHEMA: &str = "
    CREATE TABLE session (
        id           TEXT    NOT NULL PRIMARY KEY,
        parent_id    TEXT,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data         TEXT    NOT NULL
    );

    CREATE TABLE message (
        id           TEXT    NOT NULL,
        session_id   TEXT    NOT NULL,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data         TEXT    NOT NULL,
        PRIMARY KEY (session_id, id)
    );

    CREATE TABLE part (
        id           TEXT    NOT NULL,
        message_id   TEXT    NOT NULL,
        session_id   TEXT    NOT NULL,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data         TEXT    NOT NULL,
        PRIMARY KEY (session_id, message_id, id),
        FOREIGN KEY (session_id, message_id)
            REFERENCES message (session_id, id) ON DELETE CASCADE
    );

    CREATE INDEX session_parent_idx ON session (parent_id);
";

/// The journal every applied migration is recorded in.
///
/// Upstream's table verbatim (`migration.ts:30`), string ids and all: the ids
/// are `<UTC timestamp>_<name>`, so the order they sort in is the order they
/// were written in, and a build knows what it has *by name* rather than by a
/// count it could disagree about.
const JOURNAL: &str = "
    CREATE TABLE migration (
        id             TEXT    NOT NULL PRIMARY KEY,
        time_completed INTEGER NOT NULL
    );
";

/// The table a database must have for this build to recognize it as a store.
const SESSION_TABLE: &str = "session";

/// The journal table, by the name `sqlite_master` knows it by.
const JOURNAL_TABLE: &str = "migration";

/// One step from an empty file to the schema this build reads.
struct Migration {
    /// What the journal records, and what a newer build's journal is compared
    /// against.
    id: &'static str,
    /// The statements that apply it.
    up: &'static str,
}

/// Every migration this build has, oldest first.
///
/// One entry, and it is the base schema. Upstream's 38 are its history, not a
/// specification: nothing has ever written a ganja database in an older shape,
/// so there is nothing to migrate *from*. The machinery is here for the second
/// entry, whenever it is owed.
const MIGRATIONS: &[Migration] = &[Migration {
    id: "20260805000000_session_message_part",
    up: SCHEMA,
}];

/// The session id began life here, beside the rows it names, and moved to
/// [`crate::protocol`] when events started carrying it — a wire type has to
/// live with the wire. The re-export keeps `storage::SessionId` meaning what
/// it always meant to every caller that reads it here.
pub use crate::protocol::SessionId;

/// Everything known about a session apart from its transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Names the session, in rows and in the picker.
    pub id: SessionId,
    /// Format the session was written with; see [`VERSION`].
    pub version: u32,
    /// What the session is about, once a title call has said. Absent until
    /// then, and absent forever on sessions the fake provider ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Milliseconds since the Unix epoch when the session was created.
    pub created: u64,
    /// Milliseconds since the Unix epoch when the session last changed.
    pub updated: u64,
    /// What every turn so far spent, summed.
    #[serde(default)]
    pub usage: Usage,
    /// Input tokens the most recent model request carried, which is what the
    /// compaction trigger compares against the model's context window. Zero
    /// until a provider reports one.
    #[serde(default)]
    pub context_tokens: u64,
    /// The message that opens the live context window, once compaction has
    /// replaced everything before it with a summary. A request carries only
    /// messages from this one onward; absent means the window is the whole
    /// transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<MessageId>,
    /// Agent this session was last running as, so that reopening it reopens
    /// the same session rather than one that merely has the same transcript.
    /// Absent on every session written before agents existed, and on any
    /// session an engine with no agent registry created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model this session was last asking. Absent for the same reasons as
    /// above; restored only when the provider this process holds still serves
    /// it, since the provider is fixed when the engine is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Catalog effort this session was last running under. Absent on every
    /// session written before efforts existed — the serde default is what
    /// keeps those rows readable — and on every session running upstream's
    /// "Default"; restored only when the resumed model's catalog row still
    /// carries the name, the same rule a live model switch applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Deferred-roster names this session has activated — by a `tool_search`
    /// hit, by an executed `mcp__*` call, or by resume seeding (**D492**).
    ///
    /// Insert-only in memory; made durable at each root-side activating
    /// call's `finish` and re-read on resume, where it is unioned with every
    /// `mcp__*` call name in the stored transcript. An empty set writes no
    /// key — a pre-feature-shaped row stays byte-stable — and an absent key
    /// reads as empty, so the tolerance runs both ways.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub activated_tools: BTreeSet<String>,
    /// The session that delegated this one, when a `task` call created it.
    ///
    /// Present on subagent sessions and absent on every other, which is what
    /// lets a listing tell a conversation somebody had from one a tool call
    /// spawned. Absent from the wire when there is none, so an ordinary
    /// session's bytes are what they always were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    /// How far back an `/undo` walked, when one did.
    ///
    /// Stored rather than held in memory because a revert deletes nothing: the
    /// messages it hid are still stored, and a session reopened tomorrow has
    /// to know that it is looking at a transcript with a hidden tail rather
    /// than at the whole conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<crate::snapshot::RevertState>,
}

/// A write the storage could not perform, or a database it refuses to touch.
///
/// Reads still do not produce errors for content: a missing record is [`None`]
/// and one that will not decode is skipped, so the failures left are the
/// filesystem refusing to act, SQLite refusing a statement, and the two
/// refusals a whole database can earn.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The filesystem refused a read, create, or rename.
    #[error("failed to access {path}: {source}")]
    Io {
        /// What was being accessed.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
    /// A value would not serialize, which is a bug rather than user data.
    #[error("failed to encode {path}: {source}")]
    Encode {
        /// What was being written.
        path: PathBuf,
        /// What serde said.
        #[source]
        source: serde_json::Error,
    },
    /// SQLite refused a statement.
    ///
    /// Deliberately narrow in effect as well as in name: a statement that
    /// fails for one session must not be allowed to fail a listing for the
    /// rest, which is why the read paths decode row by row.
    #[error("failed to query {path}: {source}")]
    Sql {
        /// The database that refused.
        path: PathBuf,
        /// What SQLite said.
        #[source]
        source: rusqlite::Error,
    },
    /// The database has a migration this build has never heard of.
    ///
    /// The one refusal with no analogue in the file layout, where a newer
    /// build's file was simply left alone. A database is not one file per
    /// session: it is a single artefact for the whole project, so leaving it
    /// alone means refusing to open it — loudly — rather than migrating it
    /// *down* and taking every session in it along.
    #[error(
        "{path} was written by a newer build: it carries migration {unknown}, \
         which this build does not have"
    )]
    Newer {
        /// The database that was refused.
        path: PathBuf,
        /// The first unrecognized migration id found in its journal.
        unknown: String,
    },
    /// The file is a database, but not one this build recognizes as a store.
    ///
    /// Upstream's `Database is not empty and has no session table`
    /// (`migration.ts:25`): guessing at somebody else's tables is worse than
    /// stopping.
    #[error("{path} is not empty and is not a session store; refusing to touch it")]
    Foreign {
        /// The database that was refused.
        path: PathBuf,
    },
}

/// What a message or part row holds: the value, and the format it is in.
///
/// [`SessionInfo`] carries its own `version` field, so only these two need
/// wrapping. The order of the fields is the order they reach the column, which
/// puts the version first — where something reading a truncated value, or a
/// person reading the column in a shell, meets it before anything it governs.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    /// Format `payload` is written in; see [`VERSION`].
    version: u32,
    /// The stored value.
    payload: T,
}

/// The version field alone, read before the rest of a record is decoded.
///
/// Deciding what a record *is* has to come before decoding it. One a later
/// build wrote may be shaped in ways this one cannot parse at all, and a build
/// that treated "does not decode" as "is corrupt" would set aside the newer
/// build's sessions on sight — which is the one outcome this module exists to
/// prevent. So the version is read first, and a record that is not this
/// build's is never touched.
#[derive(Deserialize)]
struct Versioned {
    /// Format the rest of the record is written in. Deliberately not
    /// defaulted: every build stamps it, so a record without one was not
    /// written by any of them.
    version: u32,
}

/// What came back from a stored record: something usable, or one of the two
/// ways to come back with nothing that are deliberately not the same thing.
enum Decoded<T> {
    /// This build wrote it, and it decoded.
    Usable(T),
    /// A later build wrote it. Left exactly where it is.
    Newer(u32),
    /// Nothing this build can read. Set aside if it is a file; skipped and
    /// left in place if it is a row, since a row has no name to lose.
    Unreadable(serde_json::Error),
}

/// One project's session store, in a database beside the `storage/` directory
/// it is anchored on.
///
/// Opening is free of I/O: the connection, the schema and the conversion of an
/// older store all wait for the first operation, so resolving a project never
/// creates a database for a project nobody used.
#[derive(Clone)]
pub struct Storage {
    /// Shared with every clone, because the writer thread and its queue are
    /// the store rather than a detail of one handle on it.
    inner: Arc<Inner>,
}

/// The half of a [`Storage`] every clone shares.
struct Inner {
    /// The `storage/` directory this store is anchored on, which is where an
    /// older store's files are and beside which the database sits.
    root: PathBuf,
    /// The database file.
    database: PathBuf,
    /// [`None`] until the first operation opens it, and back to [`None`] on a
    /// failed open so the next operation tries again rather than remembering a
    /// verdict about a disk that may since have come back.
    open: Mutex<Option<Handles>>,
    /// The writer thread, kept so that dropping the last handle on a store
    /// closes it rather than merely asking it to close.
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

/// What an opened store is reached through.
#[derive(Clone)]
struct Handles {
    /// The writer thread's queue. A `Sender` is cheap to clone, and the order
    /// things enter it is the order the thread applies them, which is the
    /// whole point of it.
    writer: mpsc::Sender<Job>,
    /// The read connection, shared: WAL means it never blocks the writer, and
    /// the lock only keeps two readers off one connection.
    reader: Arc<Mutex<Connection>>,
}

/// One write, and where its answer goes.
struct Job {
    /// What to apply.
    work: Work,
    /// Where the outcome goes. The caller waits on it, so a failed write is
    /// still observed at the call site it came from — the turn task warns and
    /// carries on exactly as it did when the write was a `rename`.
    reply: mpsc::Sender<Result<(), StorageError>>,
}

/// A write, already serialized.
///
/// Encoding happens on the calling thread rather than the writer's, so a
/// [`StorageError::Encode`] — which is a bug about the value, not about the
/// disk — is raised where the value came from.
enum Work {
    /// Upsert one session record.
    Session {
        /// Its id.
        id: String,
        /// The session that delegated it, when one did.
        parent: Option<String>,
        /// When it was created.
        created: u64,
        /// When it last changed.
        updated: u64,
        /// The [`SessionInfo`] as JSON.
        data: String,
    },
    /// Upsert one message envelope.
    Message {
        /// The session it belongs to.
        session: String,
        /// Its id.
        id: String,
        /// When it was created.
        created: u64,
        /// When it was written.
        updated: u64,
        /// The message envelope as JSON.
        data: String,
    },
    /// Upsert one part.
    Part {
        /// The session it belongs to.
        session: String,
        /// The message that owns it.
        message: String,
        /// Its id.
        id: String,
        /// When it was written.
        updated: u64,
        /// The part envelope as JSON.
        data: String,
    },
    /// Remove one message, and every part it owns with it.
    Delete {
        /// The session it belongs to.
        session: String,
        /// Its id.
        id: String,
    },
}

impl std::fmt::Debug for Storage {
    /// Names the database rather than the connection: what a reader of a log
    /// wants is which project's store this is, and a connection has nothing to
    /// say about that.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Storage")
            .field("root", &self.inner.root)
            .field("database", &self.inner.database)
            .finish_non_exhaustive()
    }
}

impl Storage {
    /// A store anchored at `root`, the `storage/` directory itself.
    ///
    /// The argument is the directory rather than the database so that every
    /// caller that already knows where a project's sessions go keeps working,
    /// and so that the older store this one converts from is found without
    /// being asked for: it *is* `root`.
    #[must_use]
    pub fn open(root: PathBuf) -> Self {
        let database = root.with_file_name(DATABASE);

        Self {
            inner: Arc::new(Inner {
                root,
                database,
                open: Mutex::new(None),
                thread: Mutex::new(None),
            }),
        }
    }

    /// The database file this store reads and writes.
    ///
    /// Named rather than derived by callers so the debug suffix is decided in
    /// one place: a test that goes looking for a store on disk has to find the
    /// same file the binary under test wrote.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.inner.database
    }

    /// Writes a session's record.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the value will not encode or the database
    /// refuses the write.
    pub fn save_info(&self, info: &SessionInfo) -> Result<(), StorageError> {
        let data = self.encode(info)?;

        self.write(Work::Session {
            id: info.id.as_str().to_owned(),
            parent: info.parent.as_ref().map(|id| id.as_str().to_owned()),
            created: info.created,
            updated: info.updated,
            data,
        })
    }

    /// Reads one session's record, or [`None`] when it does not exist or this
    /// build cannot read it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database refuses the read.
    pub fn load_info(&self, id: &SessionId) -> Result<Option<SessionInfo>, StorageError> {
        let handles = self.handles()?;
        let stored: Option<String> = {
            let reader = self.reader(&handles);

            reader
                .query_row(
                    "SELECT data FROM session WHERE id = ?1",
                    params![id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|source| self.sql(source))?
        };

        Ok(stored.and_then(|data| self.usable(id.as_str(), &data)))
    }

    /// Every stored session, newest [`SessionInfo::updated`] first.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database refuses the listing; a store
    /// nothing has written lists as empty.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, StorageError> {
        let handles = self.handles()?;
        // Newest first, and by id when two carry the same instant: ids ascend,
        // so the one minted later still sorts first and a picker never shows
        // two sessions in an order that changes between listings.
        let stored = {
            let reader = self.reader(&handles);

            rows(
                &reader,
                "SELECT id, data FROM session ORDER BY time_updated DESC, id DESC",
                None,
            )
            .map_err(|source| self.sql(source))?
        };

        // Row by row rather than all at once: one session whose bytes rotted
        // must cost that session, not the listing every other one is in.
        Ok(stored
            .into_iter()
            .filter_map(|(id, data)| self.usable(&id, &data))
            .collect())
    }

    /// Writes a message's envelope, its parts stripped: parts have their own
    /// rows, and this row says the message exists, who spoke it, and when.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the value will not encode or the database
    /// refuses the write.
    pub fn save_message(&self, session: &SessionId, message: &Message) -> Result<(), StorageError> {
        // Stripping happens here rather than at the call site because a caller
        // that had to remember would eventually forget, and forgetting writes
        // every part twice — once inline, once in its own row — which is how a
        // transcript comes back doubled.
        let mut stored = message.clone();
        stored.parts.clear();

        let data = self.encode(&Envelope {
            version: VERSION,
            payload: &stored,
        })?;

        self.write(Work::Message {
            session: session.as_str().to_owned(),
            id: message.id.as_str().to_owned(),
            created: message.time.created,
            updated: now(),
            data,
        })
    }

    /// Writes one part, replacing what was there: a streaming text part is
    /// rewritten as it grows, and its row is small precisely so this stays
    /// cheap.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the value will not encode, or when the
    /// database refuses the write — which now includes a part whose message
    /// was never stored, since nothing would ever read it back.
    pub fn save_part(
        &self,
        session: &SessionId,
        message: &MessageId,
        part: &Part,
    ) -> Result<(), StorageError> {
        let data = self.encode(&Envelope {
            version: VERSION,
            payload: part,
        })?;

        self.write(Work::Part {
            session: session.as_str().to_owned(),
            message: message.as_str().to_owned(),
            id: part.id.as_str().to_owned(),
            updated: now(),
            data,
        })
    }

    /// Removes one message and every part it owns.
    ///
    /// What the prompt after an `/undo` does to the messages the undo hid: a
    /// transcript that kept them would hand the next request a conversation
    /// the user has taken back. A message that is not there is not an error —
    /// the caller is asking for it to be gone, and it is.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database refuses the removal.
    pub fn delete_message(
        &self,
        session: &SessionId,
        message: &MessageId,
    ) -> Result<(), StorageError> {
        self.write(Work::Delete {
            session: session.as_str().to_owned(),
            id: message.as_str().to_owned(),
        })
    }

    /// Reads a session's whole transcript back: envelopes in id order, each
    /// carrying its parts in id order. Records this build cannot read are
    /// skipped; a message whose envelope is gone takes its parts with it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the database refuses a read; a session
    /// with no stored messages loads as empty.
    pub fn load_transcript(&self, session: &SessionId) -> Result<Vec<Message>, StorageError> {
        let handles = self.handles()?;
        let (stored, owned) = {
            let reader = self.reader(&handles);
            let stored = rows(
                &reader,
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id",
                Some(session.as_str()),
            )
            .map_err(|source| self.sql(source))?;

            // One query for every part in the session rather than one per
            // message: a transcript is read whole, and the grouping below
            // keeps the rule the directory layout used to keep by itself — a
            // part reaches a transcript only through an envelope that was
            // read, so a lost message costs its parts too.
            let owned = rows(
                &reader,
                "SELECT message_id || ' ' || id, data FROM part \
                 WHERE session_id = ?1 ORDER BY message_id, id",
                Some(session.as_str()),
            )
            .map_err(|source| self.sql(source))?;

            (stored, owned)
        };

        let mut transcript: Vec<Message> = stored
            .into_iter()
            .filter_map(|(id, data)| {
                self.usable::<Envelope<Message>>(&id, &data)
                    .map(|envelope| envelope.payload)
            })
            .collect();

        for (key, data) in owned {
            let Some((owner, id)) = key.split_once(' ') else {
                continue;
            };
            let Some(message) = transcript
                .iter_mut()
                .find(|message| message.id.as_str() == owner)
            else {
                continue;
            };
            // A row that did not decode is not always a row that costs only
            // itself; see [`Self::lost_reasoning`] for which ones do not.
            let part = self.usable::<Envelope<Part>>(id, &data).map_or_else(
                || self.lost_reasoning(id, &data),
                |envelope| Some(envelope.payload),
            );
            if let Some(part) = part {
                message.parts.push(part);
            }
        }

        Ok(transcript)
    }

    /// Serializes a value into the JSON one column holds.
    fn encode<T: Serialize>(&self, value: &T) -> Result<String, StorageError> {
        serde_json::to_string(value).map_err(|source| StorageError::Encode {
            path: self.inner.database.clone(),
            source,
        })
    }

    /// Names the database on whatever SQLite refused.
    fn sql(&self, source: rusqlite::Error) -> StorageError {
        StorageError::Sql {
            path: self.inner.database.clone(),
            source,
        }
    }

    /// Decodes one row's `data`, or says why it came back with nothing.
    ///
    /// A row a newer build wrote and one this build cannot read at all are
    /// both left exactly where they are — there is no rename to give a row,
    /// and deleting it would destroy the only copy of what it held. So the
    /// blast radius is what the file layout's was: one record.
    ///
    /// What that record *costs* is a second question, and for part rows it is
    /// [`Self::lost_reasoning`]'s.
    fn usable<T: DeserializeOwned>(&self, id: &str, data: &str) -> Option<T> {
        match decode(data.as_bytes()) {
            Decoded::Usable(value) => Some(value),
            Decoded::Newer(version) => {
                tracing::warn!(
                    database = %self.inner.database.display(),
                    record = id,
                    version,
                    understands = VERSION,
                    "a stored record was written by a newer build and was left alone"
                );

                None
            }
            Decoded::Unreadable(error) => {
                tracing::warn!(
                    database = %self.inner.database.display(),
                    record = id,
                    %error,
                    "a stored record could not be read and was skipped"
                );

                None
            }
        }
    }

    /// The marker a part row leaves in the transcript when this build could
    /// not decode it, or [`None`] where dropping the row whole is still the
    /// right answer.
    ///
    /// # The granularity ruling
    ///
    /// Dropping an undecodable record whole is right for everything a part has
    /// ever held: a lost text part costs a line of a conversation nobody can
    /// re-render anyway, and the row stays on disk for the build that *can*
    /// read it. [`PartBody::Reasoning`] is the first part that is not like
    /// that. It is **request-affecting state**: the next request is built from
    /// the transcript, so a reasoning record that silently disappears changes
    /// what the model is asked next — its own sealed thinking gone while the
    /// tool calls that thinking produced remain — and nothing anywhere would
    /// say so. That is the one shape of loss this store must not have.
    ///
    /// So for those rows the granularity moves from *the record* to *the
    /// record and a marker in its place*: the message keeps every other part,
    /// and where the unreadable one stood the transcript carries a reasoning
    /// part with no state. The encoder that builds the next request already
    /// drops a stateless reasoning item rather than sending it (upstream's own
    /// rule under `store: false`, `openai-responses.ts:446-451`), so the
    /// marker cannot become a bad request; what it does is make the loss a
    /// thing the transcript *says* — beside a warning naming the row — instead
    /// of an absence.
    ///
    /// Three decisions are worth naming, because each had a plausible
    /// alternative:
    ///
    /// - **Recognition is by the record's `type` prefix**
    ///   ([`REASONING_TAG`]), read out of the raw JSON, because a record this
    ///   build cannot decode is precisely one whose fields it cannot trust.
    ///   The prefix is the protocol's stated contract for every later variant
    ///   of this part, which is what makes reading it here sound rather than a
    ///   guess about a name.
    /// - **Nothing is salvaged from the record**, not even a field that looks
    ///   like the blob. The shape is what this build failed to understand, so
    ///   a value read out of it is read under an assumption the failure
    ///   already disproved — and a wrong blob is a refused request that fails
    ///   the whole turn, where a missing one only costs the model a memory.
    /// - **The marker keeps the row's id and is never written back.** The only
    ///   path that rewrites a stored part is the interrupted-call closure in
    ///   `engine.rs`, which touches tool parts alone; every other write belongs
    ///   to a part the running turn minted. So the newer build's bytes stay
    ///   exactly where they are, which is the promise the rest of this module
    ///   makes.
    fn lost_reasoning(&self, id: &str, data: &str) -> Option<Part> {
        let raw: serde_json::Value = serde_json::from_str(data).ok()?;
        let payload = &raw["payload"];
        if !payload["type"].as_str()?.starts_with(REASONING_TAG) {
            return None;
        }

        tracing::warn!(
            database = %self.inner.database.display(),
            record = id,
            "a stored reasoning record could not be read; the message kept its \
             other parts and the next request carries no reasoning for this step"
        );

        Some(Part {
            id: PartId::from(id.to_owned()),
            body: PartBody::Reasoning {
                // Best-effort provenance: whichever of these the record still
                // spells plainly says *whose* continuity was lost, and an
                // empty one says even that is unknown. Neither is ever sent.
                provider: payload["provider"].as_str().unwrap_or_default().to_owned(),
                item: payload["item"].as_str().unwrap_or_default().to_owned(),
                encrypted: None,
            },
        })
    }

    /// Hands one write to the writer thread and waits for its answer.
    ///
    /// Waiting is what keeps the call site's contract: every caller of these
    /// methods already deals with a `Result`, and a turn that could no longer
    /// see a failed write would carry on believing a conversation was stored.
    /// The queue is what preserves the *order*, and it does so whether or not
    /// anybody waits.
    fn write(&self, work: Work) -> Result<(), StorageError> {
        let handles = self.handles()?;
        let (reply, answer) = mpsc::channel();

        handles
            .writer
            .send(Job { work, reply })
            .map_err(|_| self.gone())?;

        answer.recv().map_err(|_| self.gone())?
    }

    /// What a caller is told when the writer thread is not there to answer —
    /// it failed to open its connection, or the process is on its way down.
    ///
    /// An error rather than a panic, because the contract every write site
    /// holds is that losing the disk must not kill the conversation.
    fn gone(&self) -> StorageError {
        StorageError::Io {
            path: self.inner.database.clone(),
            source: io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the storage writer is no longer running",
            ),
        }
    }

    /// The store's handles, opening it if this is the first operation.
    fn handles(&self) -> Result<Handles, StorageError> {
        let mut open = self
            .inner
            .open
            .lock()
            .expect("the storage handles are never poisoned");

        if let Some(handles) = open.as_ref() {
            return Ok(handles.clone());
        }

        let handles = self.inner.start()?;
        *open = Some(handles.clone());

        Ok(handles)
    }

    /// Borrows the read connection.
    fn reader<'handles>(
        &self,
        handles: &'handles Handles,
    ) -> std::sync::MutexGuard<'handles, Connection> {
        handles
            .reader
            .lock()
            .expect("the read connection is never poisoned")
    }
}

/// Every `(key, data)` a two-column query returns, optionally narrowed to one
/// session.
///
/// Collected before anything is decoded, so the statement — and with it the
/// borrow of the connection — is finished with before the decode loop runs.
fn rows(
    connection: &Connection,
    sql: &str,
    session: Option<&str>,
) -> Result<Vec<(String, String)>, rusqlite::Error> {
    let read = |row: &rusqlite::Row<'_>| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?));

    let mut statement = connection.prepare(sql)?;
    match session {
        Some(session) => statement
            .query_map(params![session], read)?
            .collect::<Result<Vec<_>, _>>(),
        None => statement
            .query_map([], read)?
            .collect::<Result<Vec<_>, _>>(),
    }
}

impl Inner {
    /// Opens the store: the connection, the schema, and — on the first open a
    /// project ever has — the conversion of whatever the file layout left.
    fn start(&self) -> Result<Handles, StorageError> {
        // Asked before the open, because opening creates the file: the
        // question is "has this project ever had a database", and after
        // `connect` the answer is always yes.
        let mut first = !self.database.exists();

        // Opening never fails on a corrupt database — it reads no pages — so
        // one of these two is the first moment damage can be seen, and setting
        // the file aside is the same reversible move a corrupt session file
        // used to get. What replaces it is empty, which is worse than the
        // sessions it held and better than a store that cannot open.
        let mut sound = None;
        match connect(&self.database) {
            Ok(connection) => match integrity(&connection) {
                Ok(()) => sound = Some(connection),
                Err(reason) => {
                    drop(connection);
                    set_aside(&self.database, &reason, QUARANTINE);
                }
            },
            // A file so damaged that the pragmas cannot even be set: the same
            // damage `integrity` reports, arriving one statement earlier.
            // Anything else — a full disk, a directory that will not open — is
            // a passing condition and must not cost the file.
            //
            // Whether the move succeeded is not consulted on this path: what
            // follows is an open of whatever is at the name, which is the
            // right next step either way — a fresh file when it moved, and the
            // damaged one failing loudly when it did not.
            Err(error) if damaged(&error) => {
                set_aside(&self.database, &error.to_string(), QUARANTINE);
            }
            Err(error) => return Err(error),
        }

        let mut connection = match sound {
            Some(connection) => connection,
            None => {
                first = true;
                connect(&self.database)?
            }
        };

        migrate(&mut connection, &self.database)?;
        if first {
            convert(&mut connection, &self.root, &self.database)?;
        }
        // After both, never beside the checks above: at that point the
        // `session` table may not exist at all, and on a first open `convert`
        // has not yet written the older store's ids — a probe placed there
        // would pass on an empty file and then rename the store `convert` had
        // just filled.
        let connection = set_aside_preuuid(connection, &self.database)?;

        let (writer, thread) = spawn_writer(self.database.clone())?;
        *self
            .thread
            .lock()
            .expect("the writer thread handle is never poisoned") = Some(thread);

        Ok(Handles {
            writer,
            reader: Arc::new(Mutex::new(connection)),
        })
    }
}

impl Drop for Inner {
    /// Closes the store rather than merely asking it to close.
    ///
    /// Dropping the queue is what ends the writer's loop, and joining is what
    /// makes the moment the last handle goes the moment both connections are
    /// closed. Without it those two moments are merely close together, and a
    /// process that exits on the near side of that gap leaves a thread holding
    /// a database. Nothing is ever waiting in the queue here — a write is
    /// answered before its caller returns — so the join is bounded by whatever
    /// statement is in flight.
    ///
    /// It does *not* promise a checkpointed file: SQLite decides for itself
    /// whether a closing connection folds the write-ahead log back in, and a
    /// log left beside a database is a database that is perfectly well.
    fn drop(&mut self) {
        let handles = self.open.get_mut().ok().and_then(Option::take);
        drop(handles);

        if let Some(thread) = self.thread.get_mut().ok().and_then(Option::take) {
            let _ = thread.join();
        }
    }
}

/// Opens `path`, creating the directory it needs, and sets every pragma.
///
/// The read connection is opened read-write like the writer's rather than with
/// `SQLITE_OPEN_READ_ONLY`: WAL reads go through a shared-memory index, and a
/// read-only connection cannot create one — so a reader that arrived before
/// any writer would fail to open a database that is perfectly well.
fn connect(path: &Path) -> Result<Connection, StorageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StorageError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let named = |source: rusqlite::Error| StorageError::Sql {
        path: path.to_path_buf(),
        source,
    };
    let connection = Connection::open(path).map_err(named)?;

    // One at a time rather than one batch, so a failure names the pragma that
    // failed: these are the settings everything below assumes, and "the
    // pragmas did not apply" is not a diagnosis anybody can act on.
    for pragma in PRAGMAS {
        connection.execute_batch(pragma).map_err(named)?;
    }
    wal(&connection).map_err(named)?;

    Ok(connection)
}

/// Puts the database into WAL.
///
/// Kept apart from [`PRAGMAS`] because it is not the same kind of thing. WAL
/// is recorded in the file, so this is a **one-time transition** that only the
/// first connection a project ever has really performs — and it is the one
/// statement here that `busy_timeout` cannot cover. Switching journal mode
/// needs a brief exclusive lock that SQLite reaches for **without** running
/// the busy handler, so a second connection arriving mid-switch is told
/// `SQLITE_BUSY` outright rather than made to wait. Two `ganja` processes
/// opening one project at the same moment is not a rare case; it is two
/// terminals.
///
/// So the mode is *asked for* before it is set — a connection that already
/// finds `wal` does nothing at all — and a switch that loses the race is
/// retried, because the connection that won it finishes in microseconds and
/// the next look finds a file that already says `wal`.
fn wal(connection: &Connection) -> Result<(), rusqlite::Error> {
    let mode = |connection: &Connection| {
        connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
    };

    for attempt in 0..WAL_ATTEMPTS {
        if mode(connection)?.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        match connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(_) => return Ok(()),
            Err(error) if attempt + 1 == WAL_ATTEMPTS => return Err(error),
            Err(_) => thread::sleep(WAL_BACKOFF),
        }
    }

    Ok(())
}

/// Whether the database is structurally sound, and what is wrong when it is
/// not.
///
/// `PRAGMA integrity_check` rather than a cheap query on purpose: a
/// `SELECT COUNT(*)` is answered from an index and reports a healthy count on
/// a database whose rows cannot be read at all. Two shapes of failure have to
/// be caught, because they do not arrive the same way: a damaged page makes
/// the check *return* its complaint, while a damaged header makes even
/// `sqlite_master` unreadable, so the check itself errors.
///
/// What this cannot see is content: SQLite does not checksum page payloads, so
/// a single flipped byte inside a `data` column passes here and arrives as a
/// serde error at decode. That is why the per-record tolerance above still has
/// a job.
fn integrity(connection: &Connection) -> Result<(), String> {
    match connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
        Ok(verdict) if verdict == "ok" => Ok(()),
        Ok(verdict) => Err(verdict),
        Err(error) => Err(error.to_string()),
    }
}

/// Renames a database out of the way under `kind`, with the two files SQLite
/// keeps beside it, and says whether it went.
///
/// The write-ahead log has to travel with the database it belongs to. Left
/// behind, it is recovered into the *fresh* file that takes the old name —
/// which would pour the store that was just set aside straight back in.
///
/// `kind` is [`QUARANTINE`] or [`PREUUID`]: the same reversible move for two
/// different reasons, told apart in the name so a log reader can see which
/// happened without reading the log.
fn set_aside(database: &Path, reason: &str, kind: &str) -> bool {
    let stamp = now();
    let name = database.file_name().unwrap_or_default().to_string_lossy();
    // The name the database itself lands under, which is the one worth
    // logging: the other two are derived from it the way SQLite derives them.
    let kept = database.with_file_name(format!("{name}.{kind}-{stamp}"));

    for suffix in ["", "-wal", "-shm"] {
        let from = with_suffix(database, suffix);
        if !from.exists() {
            continue;
        }
        let to = from.with_file_name(format!("{name}.{kind}-{stamp}{suffix}"));
        // Refused rather than overwritten. A name already taken means another
        // process set a store aside in this same millisecond, and the only
        // thing worse than not moving this file is moving it onto the one
        // copy of somebody else's.
        if to.exists() {
            tracing::warn!(
                path = %from.display(),
                taken = %to.display(),
                "a database could not be moved aside: the name it would take is in use"
            );

            return false;
        }
        if let Err(failure) = fs::rename(&from, &to) {
            tracing::warn!(
                path = %from.display(),
                %failure,
                "a database could not be moved aside"
            );

            return false;
        }
    }

    tracing::warn!(
        path = %database.display(),
        kept = %kept.display(),
        reason,
        "the session database was moved aside; this project starts with an empty store"
    );

    true
}

/// Sets a store whose sessions predate UUIDv7 ids aside, and hands back the
/// connection to go on with (**D493**).
///
/// The question is data-shaped rather than schema-shaped — there is no version
/// to bump for it, and a store from before the change is structurally
/// identical to one from after — so it is asked of the rows: does any session
/// carry an id [`crate::protocol::is_uuidv7`] refuses? Those ids came from a
/// counter that started at zero in each *process*, so two processes reaching
/// one millisecond minted the same one; mixing them with ids that cannot
/// collide would fuse two sessions into one row, which is the outcome this
/// exists to prevent. The store is renamed, never read further and never
/// deleted, and a fresh one takes its name.
///
/// **A SQLite transaction cannot make this decision safely**, and the first
/// version of it tried to. `IMMEDIATE` serializes the *reading*; the damage is
/// done by the *renaming*, which is not a database operation and which no
/// SQLite lock spans. Two processes can both read old ids, both be right, and
/// then the second one renames the fresh store the first one had already put
/// in the old one's place — leaving two `preuuid-` files, the second of them a
/// clean store, and the first process's writer thread writing into a file no
/// path names any more. Comparing inodes just before the rename narrows that
/// window without closing it: the check and the rename are still two steps.
///
/// So the whole sequence — decide, rename, create fresh — happens under
/// [`QuarantineLock`], and **the decision is taken again inside it**, against
/// the file the path names at that moment rather than the one this connection
/// was opened on. A process that waited for the lock finds a store with
/// nothing old in it and does nothing, which is the right answer and the one
/// it could not reach before. The cheap read that gets us here is done first
/// and unlocked, so a store with nothing old in it — every store, after the
/// first open that converts one — costs no lock and creates no lock file.
///
/// [`still_named_by`] is kept as the last word before the rename. It is no
/// longer what makes this correct, but it is not dead either: [`Inner::start`]
/// also renames this database when it is *unreadable*, and that path holds no
/// lock, so a rename here is still checked against the file it means to move.
///
/// A move that is *refused* — the name it would take is already in use, the
/// rename fails, or the lock cannot be had at all — leaves the store where it
/// is and the session goes on reading it, old ids and all. That is worse than
/// a quarantine and better than a project that will not open, and every one of
/// those paths says so at `warn`.
///
/// The reopened store is migrated but deliberately **not** converted: the
/// older `storage/` tree a first open would have read was either carried into
/// the store that has just been set aside, or set aside itself by [`convert`]
/// for holding the same old ids. Converting again would be importing the very
/// sessions that were just moved out of the way.
fn set_aside_preuuid(connection: Connection, database: &Path) -> Result<Connection, StorageError> {
    // The cheap question first, on the connection that is already open and
    // taking nothing: this is asked on every open a project ever has, and all
    // but one of them answer no.
    if preuuid_id(&connection, database)?.is_none() {
        return Ok(connection);
    }

    // Let go of the store *before* waiting, and open it again afterwards.
    //
    // Two reasons, and the first is not obvious. Waiting with this connection
    // open would pin the very file the winner is about to rename, and SQLite
    // deletes a write-ahead log **by the path it remembers**: when this
    // process finally closed the last handle on the set-aside inode it would
    // checkpoint that file correctly and then unlink
    // `<database>-wal`/`-shm` — which by then name the *winner's fresh
    // store*, leaving two processes writing one database through two
    // different logs. Holding nothing while waiting makes the winner's own
    // close the last one, so what it sets aside is settled and self-contained.
    // The second reason is the ordinary one: a descriptor does not follow a
    // rename, so the question has to be asked of the path again anyway.
    drop(connection);

    let lock = QuarantineLock::take(database);
    // Whether or not the lock was had, this process needs a store to go on
    // with, and the path is where one is.
    let mut connection = connect(database)?;
    let held = fs::File::open(database).ok();
    migrate(&mut connection, database)?;

    // Held until this function returns, which is after the fresh store exists:
    // whoever was waiting must find a finished store rather than a gap.
    let Some(_lock) = lock else {
        return Ok(connection);
    };

    let old = {
        // `IMMEDIATE` still, though the lock is what serializes us now: it
        // keeps the row set from moving under the read while another process
        // writes a session, which is a different race and a real one.
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StorageError::Sql {
                path: database.to_path_buf(),
                source,
            })?;
        let old = preuuid_id(&transaction, database)?;
        transaction.commit().map_err(|source| StorageError::Sql {
            path: database.to_path_buf(),
            source,
        })?;

        old
    };

    let Some(old) = old else {
        // Somebody else did it while we waited. What we are holding is their
        // replacement store, which is exactly what we would have made.
        return Ok(connection);
    };

    let ours = held
        .as_ref()
        .is_some_and(|file| still_named_by(file, database));
    drop(connection);
    drop(held);

    if ours {
        set_aside(
            database,
            &format!("session {old} predates UUIDv7 ids"),
            PREUUID,
        );
    } else {
        tracing::warn!(
            path = %database.display(),
            "the session database was replaced while it was being read, so it was \
             reopened rather than moved aside"
        );
    }

    let mut connection = connect(database)?;
    migrate(&mut connection, database)?;

    Ok(connection)
}

/// The first session id the store carries that [`is_uuidv7`] refuses, if any.
///
/// Takes a `&Connection` rather than a transaction so both callers can share
/// it: the unlocked pre-check, which must be as cheap as a read gets, and the
/// decision inside [`QuarantineLock`], where a `Transaction` derefs to one.
fn preuuid_id(connection: &Connection, database: &Path) -> Result<Option<String>, StorageError> {
    names(connection, "SELECT id FROM session")
        .map(|ids| ids.into_iter().find(|id| !is_uuidv7(id)))
        .map_err(|source| StorageError::Sql {
            path: database.to_path_buf(),
            source,
        })
}

/// The exclusive advisory lock a pre-UUIDv7 quarantine is decided and
/// performed under (**D493**).
///
/// A sibling of the database, `<database>.quarantine.lock`, locked with
/// [`fs::File::lock`] — `flock(2)` on every platform this builds for, so the
/// lock belongs to the open file description and two `ganja` processes really
/// do exclude each other. No crate is involved and no lock protocol is
/// invented: this is one call each way.
///
/// **It is never removed.** Unlinking a lock file is how a lock file stops
/// working — the process that removed it and the one still holding a
/// descriptor on it are no longer locking the same inode, so the next pair
/// races exactly as if there were no lock. It is created only when a store
/// that really does predate UUIDv7 is found, which happens at most once in a
/// project's life, so what it leaves behind is one empty file and only where
/// one was needed.
///
/// A lock that cannot be taken is reported, not worked around: see
/// [`QuarantineLock::take`].
struct QuarantineLock(fs::File);

impl QuarantineLock {
    /// Blocks until the lock is this process's, or says it cannot be had.
    ///
    /// [`None`] means the quarantine cannot be *coordinated* — a filesystem
    /// with no advisory locking, a directory that will not take the file — and
    /// an uncoordinated quarantine is the failure this exists to prevent
    /// rather than a lesser version of it: it is what renames a store another
    /// process is writing into, costing that process the session it is in the
    /// middle of. So the caller reads the store as it is, old ids and all, and
    /// the next open on a machine that can lock does the job properly. Waiting
    /// costs nothing that is not already lost; renaming blind costs a session.
    fn take(database: &Path) -> Option<Self> {
        let name = database.file_name().unwrap_or_default().to_string_lossy();
        let path = database.with_file_name(format!("{name}.{QUARANTINE_LOCK}"));

        let refused = |failure: &io::Error| {
            tracing::warn!(
                path = %path.display(),
                %failure,
                "a store that predates UUIDv7 ids could not be set aside safely and was \
                 left as it is: the quarantine lock could not be taken"
            );
        };

        // `write` because a lock wants a writable handle and nothing else;
        // `truncate(false)` said out loud because the file's *contents* are
        // not the lock and never were — truncating one another process is
        // holding would be a write for no reason.
        let file = match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(failure) => {
                refused(&failure);

                return None;
            }
        };
        if let Err(failure) = file.lock() {
            refused(&failure);

            return None;
        }

        Some(Self(file))
    }
}

impl Drop for QuarantineLock {
    /// Lets the next process in.
    ///
    /// Closing the descriptor releases the lock on its own — that is what
    /// makes a killed process let go of one, and it is why this needs no
    /// staleness rule — so the call is not what makes the lock correct. It is
    /// here because a guard's whole job is to be held and never read, and a
    /// `Drop` is where that says so out loud rather than as a silenced
    /// warning.
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Whether `path` still names the file `file` was opened on.
///
/// [`fs::File::metadata`] is an `fstat` on the descriptor and
/// [`fs::metadata`] a `stat` on the path, so this is those two compared by
/// identity — the device and inode pair, which is what a `rename(2)` under a
/// held descriptor moves apart.
#[cfg(unix)]
fn still_named_by(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let (Ok(held), Ok(named)) = (file.metadata(), fs::metadata(path)) else {
        return false;
    };

    held.dev() == named.dev() && held.ino() == named.ino()
}

/// Windows answers "cannot tell", which the caller reads as "do not rename".
///
/// The identity a plain `stat` carries there is not the `st_dev`/`st_ino` pair
/// — it needs a handle opened for it — and this tree's windows support is
/// parked with no compile signal to keep such a path honest. Refusing to
/// quarantine is the harmless direction: the store keeps its old ids rather
/// than losing them.
#[cfg(not(unix))]
fn still_named_by(_file: &fs::File, _path: &Path) -> bool {
    false
}

/// Whether SQLite refused because the file is damaged rather than because the
/// machine is busy.
///
/// Two codes, because a database breaks in two places: `SQLITE_CORRUPT` when a
/// page is damaged and some reads still work, and `SQLITE_NOTADB` when the
/// header is, and nothing works at all — not even reading the schema.
fn damaged(error: &StorageError) -> bool {
    let StorageError::Sql {
        source: rusqlite::Error::SqliteFailure(failure, _),
        ..
    } = error
    else {
        return false;
    };

    matches!(
        failure.code,
        rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
    )
}

/// `path` with `suffix` appended to its file name, which is how SQLite names
/// the write-ahead log and the shared-memory index beside a database.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        return path.to_path_buf();
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    path.with_file_name(format!("{name}{suffix}"))
}

/// Brings the database up to this build's schema, or refuses it.
///
/// Upstream's three-way branch (`migration.ts:21-38`), with its reasons intact:
///
/// 1. **Empty** — apply every migration and stamp all of them complete. A
///    fresh install replays no history, and because the journal lands with the
///    schema, a database can never be found holding tables it has no record of
///    writing.
/// 2. **Ours** — a `session` table and a journal naming only migrations this
///    build has: apply whatever is left, each with its journal row beside it.
/// 3. **Anything else** — refuse. A journal naming a migration this build has
///    never heard of is a newer build's database and is never migrated *down*;
///    a non-empty database with no journal, or one with no `session` table, is
///    somebody else's and is not guessed at.
///
/// Two things about the transaction, both learned the hard way.
///
/// It is **`IMMEDIATE`** rather than the deferred one rusqlite gives by
/// default, because a deferred transaction takes a *read* lock on its first
/// statement and then has to upgrade when it reaches a write — and SQLite
/// refuses to wait for that upgrade. It cannot: two readers both waiting to
/// become writers is a deadlock, so it returns `SQLITE_BUSY` at once and **the
/// busy handler never runs**, which makes `busy_timeout` look as though it
/// were never set. Asking for the write lock up front is what puts the wait
/// back.
///
/// It covers the **whole** branch, probe included, rather than one migration
/// each. Upstream serializes this with a process-wide semaphore
/// (`migration.ts:11`), which is no help against a second `ganja` process; a
/// write lock is. And because the probe is inside it, the loser of a race sees
/// the winner's finished schema instead of acting on its own stale reading of
/// an empty file. The cost is that a multi-step upgrade becomes one
/// transaction rather than several — atomicity gained, resumability given up,
/// and with a single migration nothing yet to tell apart.
fn migrate(connection: &mut Connection, path: &Path) -> Result<(), StorageError> {
    let named = |source: rusqlite::Error| StorageError::Sql {
        path: path.to_path_buf(),
        source,
    };

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(named)?;

    let existing = names(
        &transaction,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .map_err(named)?;

    if existing.is_empty() {
        let completed = stamp(now());
        transaction.execute_batch(JOURNAL).map_err(named)?;
        for migration in MIGRATIONS {
            transaction.execute_batch(migration.up).map_err(named)?;
            transaction
                .execute(
                    "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
                    params![migration.id, completed],
                )
                .map_err(named)?;
        }

        return transaction.commit().map_err(named);
    }

    let ours = existing.iter().any(|name| name == SESSION_TABLE)
        && existing.iter().any(|name| name == JOURNAL_TABLE);
    if !ours {
        // Dropping the transaction rolls it back: a database this build
        // refuses is one it has written nothing to.
        return Err(StorageError::Foreign {
            path: path.to_path_buf(),
        });
    }

    let applied = names(&transaction, "SELECT id FROM migration").map_err(named)?;
    if let Some(unknown) = applied
        .iter()
        .find(|id| !MIGRATIONS.iter().any(|migration| migration.id == **id))
    {
        return Err(StorageError::Newer {
            path: path.to_path_buf(),
            unknown: unknown.clone(),
        });
    }

    let completed = stamp(now());
    for migration in MIGRATIONS {
        if applied.iter().any(|id| id == migration.id) {
            continue;
        }
        transaction.execute_batch(migration.up).map_err(named)?;
        transaction
            .execute(
                "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
                params![migration.id, completed],
            )
            .map_err(named)?;
    }

    transaction.commit().map_err(named)
}

/// Every single-column string a query returns.
fn names(connection: &Connection, sql: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut statement = connection.prepare(sql)?;

    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
}

/// Carries an older store's `storage/` tree into the database, once.
///
/// Upstream has no importer — it had no version field to import *by*, and its
/// answer to data it could not carry was to drop it. This tree has carried a
/// `version` on every record since the format was written down, for exactly
/// this, so the files are read back through the reader that already exists:
/// the version gate, the newer-build skip and the set-aside of a corrupt file
/// all still apply, and a file that fails any of them costs its own record and
/// nothing else.
///
/// One transaction per session, so an interrupted conversion leaves whole
/// sessions rather than half of one. When every session made it, the tree is
/// **renamed**, not deleted — `storage.migrated-<millis>`, the same reversible
/// set-aside a corrupt file gets — so a person who downgrades tomorrow loses
/// only what they wrote today. When one did not, the tree stays exactly where
/// it is, because the only copy of what did not make it is in there.
///
/// A tree whose sessions predate UUIDv7 ids is a third outcome, and it is
/// decided before anything is carried (**D493**): the whole tree is set aside
/// under [`PREUUID`] and nothing is imported. Deciding it afterwards would
/// mean importing those sessions and then having [`set_aside_preuuid`]
/// quarantine the store holding them — the same rows moved twice in one open,
/// for one reason.
fn convert(connection: &mut Connection, root: &Path, path: &Path) -> Result<(), StorageError> {
    let files = stored_files(&root.join(SESSION).join(INFO))?;
    if files.is_empty() {
        return Ok(());
    }

    // Read the roster whole before carrying any of it, because the question
    // below is about the tree rather than about one session.
    let mut infos = Vec::with_capacity(files.len());
    let mut lost = 0_usize;
    for file in files {
        match read_stored::<SessionInfo>(&file)? {
            Some(info) => infos.push(info),
            None => lost += 1,
        }
    }

    if let Some(old) = infos.iter().find(|info| !is_uuidv7(info.id.as_str())) {
        let aside = tree_aside(root, PREUUID);
        match fs::rename(root, &aside) {
            Ok(()) => tracing::warn!(
                session = old.id.as_str(),
                kept = %aside.display(),
                "the stored sessions predate UUIDv7 ids and were set aside rather than \
                 carried across; this project starts with an empty store"
            ),
            Err(failure) => tracing::warn!(
                session = old.id.as_str(),
                root = %root.display(),
                %failure,
                "the stored sessions predate UUIDv7 ids and could not be set aside; \
                 nothing was carried across and the old store is left where it is"
            ),
        }

        return Ok(());
    }

    let mut carried = 0_usize;
    for info in &infos {
        match carry(connection, root, info) {
            Ok(()) => carried += 1,
            Err(error) => {
                lost += 1;
                tracing::warn!(
                    session = info.id.as_str(),
                    database = %path.display(),
                    %error,
                    "a stored session could not be carried into the database"
                );
            }
        }
    }

    if lost > 0 {
        tracing::warn!(
            carried,
            lost,
            root = %root.display(),
            "some sessions did not reach the database; the old store is left where it is"
        );

        return Ok(());
    }

    let aside = tree_aside(root, MIGRATED);
    match fs::rename(root, &aside) {
        Ok(()) => tracing::info!(
            carried,
            kept = %aside.display(),
            "the stored sessions were carried into the database and the old store was set aside"
        ),
        Err(failure) => tracing::warn!(
            root = %root.display(),
            %failure,
            "the stored sessions were carried into the database, \
             but the old store could not be set aside"
        ),
    }

    Ok(())
}

/// The name an older store's tree is set aside under: `storage.<kind>-<millis>`.
///
/// One place rather than two, because the two reasons a tree moves —
/// [`MIGRATED`] once it has been carried across, [`PREUUID`] when it must not
/// be — have to be told apart by suffix and by nothing else.
fn tree_aside(root: &Path, kind: &str) -> PathBuf {
    root.with_file_name(format!(
        "{}.{kind}-{}",
        root.file_name().unwrap_or_default().to_string_lossy(),
        now()
    ))
}

/// Carries one session and its whole transcript, in one transaction.
fn carry(
    connection: &mut Connection,
    root: &Path,
    info: &SessionInfo,
) -> Result<(), rusqlite::Error> {
    let session = info.id.as_str();
    // `IMMEDIATE` for the reason `migrate` gives: a transaction that starts by
    // reading cannot wait for the write lock it then discovers it needs.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

    transaction.execute(
        "INSERT INTO session (id, parent_id, time_created, time_updated, data) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            session,
            info.parent.as_ref().map(SessionId::as_str),
            stamp(info.created),
            stamp(info.updated),
            json(info)?
        ],
    )?;

    let messages = root.join(SESSION).join(MESSAGE).join(session);
    for file in stored_files(&messages).unwrap_or_default() {
        // A part directory is opened only from an envelope that was read,
        // which is what makes a lost envelope cost its parts too — the rule
        // the transcript reader keeps, kept here so a conversion cannot carry
        // across something a read would never have shown.
        let Ok(Some(envelope)) = read_stored::<Envelope<Message>>(&file) else {
            continue;
        };
        let message = envelope.payload;
        transaction.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                message.id.as_str(),
                session,
                stamp(message.time.created),
                stamp(message.time.completed.unwrap_or(message.time.created)),
                json(&Envelope {
                    version: VERSION,
                    payload: &message,
                })?
            ],
        )?;

        let parts = root
            .join(SESSION)
            .join(PART)
            .join(session)
            .join(message.id.as_str());
        for file in stored_files(&parts).unwrap_or_default() {
            let Ok(Some(envelope)) = read_stored::<Envelope<Part>>(&file) else {
                continue;
            };
            transaction.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    envelope.payload.id.as_str(),
                    message.id.as_str(),
                    session,
                    stamp(message.time.created),
                    stamp(message.time.created),
                    json(&envelope)?
                ],
            )?;
        }
    }

    transaction.commit()
}

/// A millisecond stamp as SQLite stores an integer.
///
/// SQLite's `INTEGER` is signed, so a `u64` of milliseconds since the epoch
/// has to be narrowed. It does not reach the top of an `i64` until roughly
/// three hundred million years from now, and saturating rather than wrapping
/// means a value that somehow did would sort last rather than first.
fn stamp(millis: u64) -> i64 {
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Re-encodes a record the conversion just read.
///
/// A value that came out of a file and will not go back in is a bug about this
/// build's types rather than about the file, and it has to stop the session it
/// belongs to rather than be written as something else — so it travels as the
/// error the surrounding transaction already handles.
fn json<T: Serialize>(value: &T) -> Result<String, rusqlite::Error> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

/// Starts the thread every write goes through, on its own connection.
///
/// The queue outlives nothing: when the last [`Storage`] handle is dropped the
/// sender goes with it, the loop below ends, and the connection closes. A
/// connection that will not open is reported once and the thread stops, which
/// every waiting caller sees as an error rather than as a panic on the turn
/// task.
fn spawn_writer(
    path: PathBuf,
) -> Result<(mpsc::Sender<Job>, thread::JoinHandle<()>), StorageError> {
    let (sender, receiver) = mpsc::channel::<Job>();
    let opened = path.clone();

    let thread = thread::Builder::new()
        .name("ganja-storage".to_owned())
        .spawn(move || {
            let connection = match connect(&opened) {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::error!(
                        path = %opened.display(),
                        %error,
                        "the session store could not be opened for writing"
                    );

                    return;
                }
            };

            for job in receiver {
                let outcome = apply(&connection, &opened, &job.work);
                // A caller that stopped waiting is not an error: the answer
                // simply has nowhere to go, and the write already happened.
                let _ = job.reply.send(outcome);
            }
        })
        .map_err(|source| StorageError::Io { path, source })?;

    Ok((sender, thread))
}

/// Applies one write.
///
/// Each is a single statement, and therefore its own transaction: a part
/// reaches the disk when it is written rather than when the turn ends, which
/// is what a resume after a `kill -9` reads back.
fn apply(connection: &Connection, path: &Path, work: &Work) -> Result<(), StorageError> {
    let outcome = match work {
        Work::Session {
            id,
            parent,
            created,
            updated,
            data,
        } => connection.execute(
            "INSERT INTO session (id, parent_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (id) DO UPDATE SET \
               parent_id = excluded.parent_id, \
               time_created = excluded.time_created, \
               time_updated = excluded.time_updated, \
               data = excluded.data",
            params![id, parent, stamp(*created), stamp(*updated), data],
        ),
        Work::Message {
            session,
            id,
            created,
            updated,
            data,
        } => connection.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (session_id, id) DO UPDATE SET \
               time_created = excluded.time_created, \
               time_updated = excluded.time_updated, \
               data = excluded.data",
            params![id, session, stamp(*created), stamp(*updated), data],
        ),
        Work::Part {
            session,
            message,
            id,
            updated,
            data,
        } => connection.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (session_id, message_id, id) DO UPDATE SET \
               time_updated = excluded.time_updated, \
               data = excluded.data",
            params![id, message, session, stamp(*updated), stamp(*updated), data],
        ),
        // The parts go with it, through the cascade rather than through a
        // second statement: an interrupted deletion used to have to leave a
        // message short of content rather than parts with no envelope, and one
        // statement cannot be interrupted in the middle.
        Work::Delete { session, id } => connection.execute(
            "DELETE FROM message WHERE session_id = ?1 AND id = ?2",
            params![session, id],
        ),
    };

    outcome.map(|_| ()).map_err(|source| StorageError::Sql {
        path: path.to_path_buf(),
        source,
    })
}

/// Decides what a stored record is, before anything decodes it.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Decoded<T> {
    match serde_json::from_slice::<Versioned>(bytes) {
        Ok(stored) if stored.version == VERSION => {}
        Ok(stored) => return Decoded::Newer(stored.version),
        Err(error) => return Decoded::Unreadable(error),
    }

    match serde_json::from_slice(bytes) {
        Ok(value) => Decoded::Usable(value),
        Err(error) => Decoded::Unreadable(error),
    }
}

/// Reads one stored file, or [`None`] when there is nothing this build can use
/// there.
///
/// The reader the conversion runs on, and the reason the conversion is safe:
/// three ways to come back with nothing, deliberately not the same thing. A
/// file that is not there was never written, one that does not parse is moved
/// aside and skipped, and one a later build wrote is left exactly where it is.
/// Only the filesystem refusing to answer is an error.
fn read_stored<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StorageError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StorageError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    match decode(&bytes) {
        Decoded::Usable(value) => Ok(Some(value)),
        Decoded::Newer(version) => {
            tracing::warn!(
                path = %path.display(),
                version,
                understands = VERSION,
                "a stored file was written by a newer build and was left alone"
            );

            Ok(None)
        }
        Decoded::Unreadable(error) => {
            quarantine(path, &error);

            Ok(None)
        }
    }
}

/// Moves a file this build cannot read aside, and says where it went.
///
/// The name it lands under carries the moment it was set aside — which keeps
/// two of them apart — and loses the `.json` extension a listing reads, so the
/// file is skipped from then on without anything having to remember it. What
/// was in it is still there for whoever wants to look.
///
/// A rename that fails is warned about and nothing more. The read that
/// prompted it has already come back empty, so the caller is no worse off, and
/// the next read reaches the same conclusion by the same route.
fn quarantine(path: &Path, error: &serde_json::Error) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let aside = path.with_file_name(format!("{name}.{QUARANTINE}-{}", now()));

    match fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            kept = %aside.display(),
            %error,
            "a stored file could not be read and was moved aside"
        ),
        Err(failure) => tracing::warn!(
            path = %path.display(),
            %error,
            %failure,
            "a stored file could not be read, and could not be moved aside either"
        ),
    }
}

/// Every stored file directly under `directory`, in id order.
///
/// Sorting by name is sorting by id, and ids ascend with creation, so this is
/// the order the things in these files happened in. Anything without the
/// `.json` extension is not stored data — a quarantined file has lost it, a
/// temporary one never had it — and a directory that does not exist holds
/// nothing rather than failing: a project that never had a file store is now
/// the ordinary case.
fn stored_files(directory: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(StorageError::Io {
                path: directory.to_path_buf(),
                source,
            });
        }
    };

    let mut paths = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|source| StorageError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == EXTENSION)
        {
            paths.push(path);
        }
    }
    // By stem rather than by whole name, so the extension cannot come between
    // one id and the next.
    paths.sort_by(|left, right| left.file_stem().cmp(&right.file_stem()));

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{
        DATABASE, MIGRATIONS, PRAGMAS, SessionId, SessionInfo, Storage, StorageError, VERSION,
        connect,
    };
    use crate::protocol::{
        Message, MessageId, MessageTime, Part, PartBody, PartId, REASONING_TAG, Role, ToolState,
        Usage,
    };

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// A store under a directory that does not exist yet, which is what every
    /// first run opens.
    fn storage(directory: &TempDir) -> Storage {
        Storage::open(directory.path().join("storage"))
    }

    fn session(id: &str) -> SessionId {
        SessionId::from(id.to_owned())
    }

    /// A session id spelled the way the mint spells one, from a small ordinal.
    ///
    /// Most fixtures below name their sessions `ses_1`, and may: `SessionId`'s
    /// own doc says the prefix is a convention rather than an invariant, and
    /// nothing reads such a store twice. The ones that **reopen** a store, or
    /// that write an older store's tree for the conversion to read, cannot —
    /// a store whose `session` rows carry that spelling is set aside rather
    /// than read (**D493**), which is a different test and has four of its own
    /// binaries. Those name their sessions through here instead, which keeps
    /// the ordinal readable while spelling the id the way
    /// [`crate::protocol::uuidv7`] spells one; the ordinal lands in the
    /// trailing field, so `ORDER BY id` still orders them the way the test
    /// wrote them.
    fn minted(ordinal: u32) -> String {
        format!("0198f2c4-a1b0-7000-8000-{ordinal:012x}")
    }

    /// Info with pinned times, so a test asserts on the order it asked for
    /// rather than on whatever the clock said.
    fn info(id: &str, updated: u64) -> SessionInfo {
        SessionInfo {
            id: session(id),
            version: VERSION,
            title: None,
            created: 1,
            updated,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            effort: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        }
    }

    /// Every persisted shape of the effort field, pinned where the row lives:
    /// a session running Default writes the exact bytes it always wrote, a
    /// selected effort survives the round trip, and a row from before the
    /// field existed parses through the serde default — which is also how a
    /// row written under the field's old name reads, as effort-unselected.
    #[test]
    fn the_session_row_preserves_default_bytes_round_trips_effort_and_reads_older_rows() {
        let mut carried = info("ses_effort", 2);
        carried.model = Some("claude-opus-5".to_owned());
        carried.effort = Some("max".to_owned());

        let encoded = serde_json::to_string(&carried).expect("the row serializes");
        assert!(encoded.contains(r#""effort":"max""#), "got {encoded}");
        assert!(!encoded.contains(r#""variant""#), "got {encoded}");
        let decoded: SessionInfo = serde_json::from_str(&encoded).expect("the row parses back");
        assert_eq!(decoded, carried);

        let bare = serde_json::to_string(&info("ses_default", 2)).expect("the row serializes");
        assert_eq!(
            bare,
            r#"{"id":"ses_default","version":1,"created":1,"updated":2,"usage":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0},"context_tokens":0}"#,
            "Default is the field's absence, so an unselected row keeps its old bytes"
        );

        let older = r#"{"id":"ses_older","version":1,"created":1,"updated":2}"#;
        let decoded: SessionInfo = serde_json::from_str(older)
            .expect("the default reads a row from before the field existed");
        assert_eq!(decoded.effort, None);
    }

    /// A message with pinned ids and times, carrying `parts`.
    fn message(id: &str, parts: Vec<Part>) -> Message {
        Message {
            id: MessageId::from(id.to_owned()),
            role: Role::Assistant,
            parts,
            time: MessageTime {
                created: 7,
                completed: Some(9),
            },
            model: Some("canned".to_owned()),
            usage: Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            }),
        }
    }

    fn text(id: &str, text: &str) -> Part {
        Part {
            id: PartId::from(id.to_owned()),
            body: PartBody::Text {
                text: text.to_owned(),
            },
        }
    }

    /// A completed tool call, the richest shape a part takes.
    fn tool(id: &str) -> Part {
        Part {
            id: PartId::from(id.to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"path": "a.rs"}),
                    output: "fn main() {}".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({"lines": 1}),
                    started: 7,
                    completed: 9,
                },
            },
        }
    }

    /// Everything directly inside `directory`, by name, sorted.
    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("the directory lists")
            .map(|entry| {
                entry
                    .expect("the entry reads")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();

        names
    }

    /// Stores a whole message the way the engine does — envelope first, then
    /// each part — which is the order the part table's foreign key needs.
    fn store_message(storage: &Storage, id: &SessionId, message: &Message) {
        storage
            .save_message(id, message)
            .expect("the envelope stores");
        for part in &message.parts {
            storage
                .save_part(id, &message.id, part)
                .expect("the part stores");
        }
    }

    /// A second connection onto the same database, for the tests that have to
    /// damage a record rather than write one.
    ///
    /// Deliberately not the store's own: what is being simulated is bytes that
    /// rotted underneath it, and a test that could only reach them through the
    /// writer would be testing the writer.
    fn beside(storage: &Storage) -> Connection {
        let connection =
            Connection::open(storage.database()).expect("the database opens a second time");
        for pragma in PRAGMAS {
            connection
                .execute_batch(pragma)
                .expect("the pragmas apply to any connection");
        }

        connection
    }

    #[test]
    fn a_store_that_was_never_written_reads_as_empty_rather_than_failing() {
        let directory = temporary();
        let storage = storage(&directory);

        assert!(
            storage
                .list_sessions()
                .expect("an unwritten store lists")
                .is_empty()
        );
        assert_eq!(
            storage
                .load_info(&session("ses_missing"))
                .expect("an unwritten store reads"),
            None
        );
        assert!(
            storage
                .load_transcript(&session("ses_missing"))
                .expect("an unwritten store loads")
                .is_empty()
        );
    }

    #[test]
    fn an_info_record_round_trips_with_its_optional_fields_set_and_unset() {
        let directory = temporary();
        let storage = storage(&directory);

        let bare = info("ses_bare", 5);
        let full = SessionInfo {
            title: Some("what it was about".to_owned()),
            usage: Usage {
                input_tokens: 11,
                output_tokens: 22,
                ..Usage::default()
            },
            context_tokens: 33,
            summary: Some(MessageId::from("msg_summary".to_owned())),
            agent: Some("plan".to_owned()),
            model: Some("anthropic/claude".to_owned()),
            parent: Some(session("ses_bare")),
            ..info("ses_full", 6)
        };

        storage.save_info(&bare).expect("the bare record writes");
        storage.save_info(&full).expect("the full record writes");

        assert_eq!(
            storage.load_info(&bare.id).expect("the bare record reads"),
            Some(bare)
        );
        assert_eq!(
            storage.load_info(&full.id).expect("the full record reads"),
            Some(full)
        );
    }

    #[test]
    fn a_message_is_stored_without_its_parts_and_the_caller_keeps_them() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let held = message("msg_1", vec![text("prt_1", "kept by the caller")]);
        storage
            .save_message(&id, &held)
            .expect("the envelope stores");

        assert_eq!(
            held.parts.len(),
            1,
            "the caller's message must not be emptied by storing it"
        );
        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded.len(), 1);
        assert!(
            loaded[0].parts.is_empty(),
            "the envelope is stored without its parts, got {:?}",
            loaded[0].parts
        );
    }

    #[test]
    fn a_completed_tool_part_round_trips_whole() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let held = message("msg_1", vec![tool("prt_1")]);
        store_message(&storage, &id, &held);

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded[0].parts, held.parts);
    }

    #[test]
    fn a_transcript_reassembles_its_messages_and_parts_in_id_order() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        // Written out of order on purpose: the store is what puts them back in
        // id order, and id order is creation order.
        let second = message("msg_2", vec![text("prt_3", "c"), text("prt_4", "d")]);
        let first = message("msg_1", vec![text("prt_2", "b"), text("prt_1", "a")]);
        store_message(&storage, &id, &second);
        store_message(&storage, &id, &first);

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        let shape: Vec<(&str, Vec<&str>)> = loaded
            .iter()
            .map(|message| {
                (
                    message.id.as_str(),
                    message
                        .parts
                        .iter()
                        .map(|part| part.id.as_str())
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        assert_eq!(
            shape,
            vec![
                ("msg_1", vec!["prt_1", "prt_2"]),
                ("msg_2", vec!["prt_3", "prt_4"]),
            ]
        );
    }

    #[test]
    fn a_deleted_message_takes_its_parts_and_leaves_the_rest_of_the_transcript() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let doomed = message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]);
        let kept = message("msg_2", vec![text("prt_3", "c")]);
        store_message(&storage, &id, &doomed);
        store_message(&storage, &id, &kept);

        storage
            .delete_message(&id, &doomed.id)
            .expect("the message deletes");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, kept.id);
        assert_eq!(loaded[0].parts, kept.parts);

        // Straight at the table, because the point is the cascade: without
        // `foreign_keys = ON` the delete would leave both parts behind and the
        // transcript above would look exactly the same.
        let orphans: i64 = beside(&storage)
            .query_row(
                "SELECT COUNT(*) FROM part WHERE message_id = ?1",
                rusqlite::params![doomed.id.as_str()],
                |row| row.get(0),
            )
            .expect("the part table counts");
        assert_eq!(
            orphans, 0,
            "the cascade must carry a deleted message's parts away with it"
        );

        storage
            .delete_message(&id, &doomed.id)
            .expect("deleting what is already gone is not an error");
    }

    #[test]
    fn a_corrupt_info_row_is_skipped_and_the_rest_still_lists() {
        let directory = temporary();
        let storage = storage(&directory);
        storage
            .save_info(&info("ses_rotten", 5))
            .expect("the record writes");
        storage
            .save_info(&info("ses_intact", 4))
            .expect("the record writes");

        beside(&storage)
            .execute(
                "UPDATE session SET data = 'not json at all' WHERE id = 'ses_rotten'",
                [],
            )
            .expect("the row is damaged");

        let listed: Vec<String> = storage
            .list_sessions()
            .expect("one unreadable record must not fail the listing")
            .into_iter()
            .map(|info| info.id.as_str().to_owned())
            .collect();
        assert_eq!(listed, vec!["ses_intact".to_owned()]);
        assert_eq!(
            storage
                .load_info(&session("ses_rotten"))
                .expect("the unreadable record reads as absent"),
            None
        );

        // Left where it is: a row has no name to lose, so the reversible
        // set-aside a file gets is simply not deleting it.
        let still_there: i64 = beside(&storage)
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = 'ses_rotten'",
                [],
                |row| row.get(0),
            )
            .expect("the session table counts");
        assert_eq!(still_there, 1, "nothing may be destroyed to skip it");
    }

    #[test]
    fn a_corrupt_envelope_takes_its_message_and_its_parts_out_of_the_transcript() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let rotten = message("msg_1", vec![text("prt_1", "gone with it")]);
        let kept = message("msg_2", vec![text("prt_2", "still here")]);
        store_message(&storage, &id, &rotten);
        store_message(&storage, &id, &kept);

        beside(&storage)
            .execute("UPDATE message SET data = '{' WHERE id = 'msg_1'", [])
            .expect("the row is damaged");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded.len(), 1, "{loaded:#?}");
        assert_eq!(loaded[0].id, kept.id);
        assert_eq!(
            loaded[0].parts.len(),
            1,
            "the surviving message keeps exactly its own parts"
        );
    }

    #[test]
    fn a_corrupt_part_row_is_skipped_and_its_message_keeps_the_rest() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let held = message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]);
        store_message(&storage, &id, &held);

        beside(&storage)
            .execute("UPDATE part SET data = 'nonsense' WHERE id = 'prt_1'", [])
            .expect("the row is damaged");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0]
                .parts
                .iter()
                .map(|part| part.id.as_str())
                .collect::<Vec<_>>(),
            vec!["prt_2"]
        );
    }

    /// The downgrade this build is written for: a session stored by a build
    /// whose reasoning part is shaped differently. The message must survive
    /// whole apart from that one part, the loss must be *in* the transcript
    /// rather than only in a log line, and the record must still be there for
    /// the build that can read it.
    #[test]
    fn a_reasoning_record_this_build_cannot_read_costs_continuity_and_says_so() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");
        store_message(
            &storage,
            &id,
            &message(
                "msg_1",
                vec![
                    text("prt_1", "before"),
                    Part {
                        id: PartId::from("prt_2".to_owned()),
                        body: PartBody::Reasoning {
                            provider: "openai".to_owned(),
                            item: "rs_1".to_owned(),
                            encrypted: Some("sealed".to_owned()),
                        },
                    },
                    tool("prt_3"),
                ],
            ),
        );

        // What a later build's record looks like from here: the reserved tag,
        // and a body whose required shape this one does not have.
        let ahead = serde_json::json!({
            "version": VERSION,
            "payload": {
                "id": "prt_2",
                "type": "reasoning_v2",
                "provider": "openai",
                "item": "rs_1",
                "segments": [{"sealed": "sealed", "scheme": "something-later"}],
            },
        })
        .to_string();
        let connection = beside(&storage);
        connection
            .execute(
                "UPDATE part SET data = ?1 WHERE id = 'prt_2'",
                rusqlite::params![ahead],
            )
            .expect("the row is replaced");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(
            loaded[0]
                .parts
                .iter()
                .map(|part| part.id.as_str())
                .collect::<Vec<_>>(),
            vec!["prt_1", "prt_2", "prt_3"],
            "the rest of the message survives and the lost part keeps its place"
        );
        assert_eq!(
            loaded[0].parts[1].body,
            PartBody::Reasoning {
                provider: "openai".to_owned(),
                item: "rs_1".to_owned(),
                // Nothing is salvaged out of a shape this build did not
                // understand: a wrong blob is a refused request, a missing one
                // is a model that reasons again.
                encrypted: None,
            },
            "the transcript itself has to say the continuity is gone"
        );

        let stored: String = connection
            .query_row("SELECT data FROM part WHERE id = 'prt_2'", [], |row| {
                row.get(0)
            })
            .expect("the row reads");
        assert_eq!(
            stored, ahead,
            "reading a record this build cannot decode must not rewrite it"
        );

        // The other way a record becomes unreadable — a whole format this
        // build predates — has to reach the same answer, or the marker would
        // depend on *how* the future arrived rather than on what was lost.
        connection
            .execute(
                "UPDATE part SET data = ?1 WHERE id = 'prt_2'",
                rusqlite::params![
                    serde_json::json!({
                        "version": VERSION + 1,
                        "payload": {"id": "prt_2", "type": REASONING_TAG},
                    })
                    .to_string()
                ],
            )
            .expect("the row is replaced");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(
            loaded[0].parts[1].body,
            PartBody::Reasoning {
                provider: String::new(),
                item: String::new(),
                encrypted: None,
            },
            "provenance the record does not spell plainly is left unknown \
             rather than guessed"
        );
    }

    /// Readable thinking is a normal versioned row: it is written, it is read
    /// back word for word, and a session resumed tomorrow still shows what the
    /// model was working through — which is the whole of what persisting it
    /// buys, since no wire ever carries it.
    #[test]
    fn readable_thinking_is_stored_and_reads_back_as_itself() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let mut reply = Message::assistant("canned");
        reply
            .parts
            .push(Part::reasoning_text("weighing a greeting"));
        reply.parts.push(Part::text("hello"));
        store_message(&storage, &id, &reply);

        let loaded = storage.load_transcript(&id).expect("the transcript loads");

        assert_eq!(
            loaded[0].parts[0].body,
            PartBody::ReasoningText {
                text: "weighing a greeting".to_owned()
            },
            "a stored thought comes back whole, not as a marker"
        );
        assert_eq!(loaded[0].parts[1].as_text(), Some("hello"));
    }

    /// The other half of the ruling: only request-affecting state earns a
    /// marker. A text row that will not decode is still dropped whole, because
    /// a marker for it would put a reasoning part where the model never
    /// reasoned.
    #[test]
    fn an_unreadable_part_that_is_not_reasoning_is_still_dropped_whole() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");
        store_message(
            &storage,
            &id,
            &message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]),
        );

        beside(&storage)
            .execute(
                "UPDATE part SET data = ?1 WHERE id = 'prt_1'",
                rusqlite::params![
                    serde_json::json!({
                        "version": VERSION,
                        "payload": {"id": "prt_1", "type": "text"},
                    })
                    .to_string()
                ],
            )
            .expect("the row is replaced");

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(
            loaded[0]
                .parts
                .iter()
                .map(|part| part.id.as_str())
                .collect::<Vec<_>>(),
            vec!["prt_2"]
        );
    }

    #[test]
    fn a_record_from_a_newer_build_is_skipped_and_left_exactly_where_it_is() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");
        storage
            .save_info(&info("ses_1", 5))
            .expect("the record writes");
        store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));

        let ahead = serde_json::json!({
            "version": VERSION + 1,
            "id": "ses_1",
            "created": 1,
            "updated": 5,
            "shape_this_build_has_never_seen": {"nested": true},
        })
        .to_string();
        let connection = beside(&storage);
        connection
            .execute(
                "UPDATE session SET data = ?1 WHERE id = 'ses_1'",
                rusqlite::params![ahead],
            )
            .expect("the row is replaced");
        connection
            .execute(
                "UPDATE part SET data = ?1 WHERE id = 'prt_1'",
                rusqlite::params![
                    serde_json::json!({"version": VERSION + 1, "payload": {"whatever": 1}})
                        .to_string()
                ],
            )
            .expect("the row is replaced");

        assert_eq!(
            storage
                .load_info(&id)
                .expect("a newer build's record is not an error"),
            None
        );
        assert!(
            storage
                .list_sessions()
                .expect("a newer build's record is not an error")
                .is_empty()
        );
        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert!(
            loaded[0].parts.is_empty(),
            "a newer build's part is skipped, not decoded"
        );

        // The bytes are still exactly the ones the newer build wrote.
        let stored: String = connection
            .query_row("SELECT data FROM session WHERE id = 'ses_1'", [], |row| {
                row.get(0)
            })
            .expect("the row reads");
        assert_eq!(
            stored, ahead,
            "a newer build's record must not be rewritten"
        );
    }

    #[test]
    fn sessions_list_newest_first_and_ignore_what_could_not_be_read() {
        let directory = temporary();
        let storage = storage(&directory);
        for (id, updated) in [("ses_a", 10), ("ses_b", 30), ("ses_c", 20), ("ses_d", 30)] {
            storage
                .save_info(&info(id, updated))
                .expect("the record writes");
        }
        storage
            .save_info(&info("ses_e", 40))
            .expect("the record writes");
        beside(&storage)
            .execute("UPDATE session SET data = '' WHERE id = 'ses_e'", [])
            .expect("the row is damaged");

        let listed: Vec<String> = storage
            .list_sessions()
            .expect("the listing survives one unreadable record")
            .into_iter()
            .map(|info| info.id.as_str().to_owned())
            .collect();

        // Newest first, and the later id first when two share an instant.
        assert_eq!(listed, vec!["ses_d", "ses_b", "ses_c", "ses_a"]);
    }

    #[test]
    fn rewriting_a_part_replaces_it_rather_than_leaving_the_old_one() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        let mut held = message("msg_1", vec![text("prt_1", "half")]);
        store_message(&storage, &id, &held);

        held.parts = vec![text("prt_1", "half and then the rest")];
        store_message(&storage, &id, &held);

        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded[0].parts, held.parts, "one part, at its latest text");
    }

    #[test]
    fn a_part_whose_message_was_never_stored_is_refused_rather_than_orphaned() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");

        // Nothing ever read such a part back — a transcript reaches parts only
        // through an envelope — so what used to be a file nobody opened is now
        // a write that says so.
        let orphan = storage.save_part(
            &id,
            &MessageId::from("msg_never".to_owned()),
            &text("prt_1", "a"),
        );
        assert!(
            matches!(orphan, Err(StorageError::Sql { .. })),
            "a part with no message must be refused, got {orphan:?}"
        );
        assert!(
            storage
                .load_transcript(&id)
                .expect("the transcript loads")
                .is_empty()
        );
    }

    #[test]
    fn a_write_leaves_nothing_beside_the_database_but_what_sqlite_owns() {
        let directory = temporary();
        let storage = storage(&directory);
        let id = session("ses_1");
        storage
            .save_info(&info("ses_1", 5))
            .expect("the record writes");
        store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));

        for name in names(directory.path()) {
            assert!(
                name == DATABASE
                    || name == format!("{DATABASE}-wal")
                    || name == format!("{DATABASE}-shm"),
                "a write left {name} behind"
            );
        }
        assert!(
            !directory.path().join("storage").exists(),
            "a store that had no files to convert must not create the directory"
        );
    }

    #[test]
    fn a_message_id_reused_in_another_session_does_not_overwrite_it() {
        let directory = temporary();
        let storage = storage(&directory);
        let mine = session("ses_mine");
        let yours = session("ses_yours");

        // Two sessions can carry a message under the same id however it was
        // minted; under a bare `id` primary key the second write would take
        // the first one's row.
        store_message(
            &storage,
            &mine,
            &message("msg_same", vec![text("prt_same", "mine")]),
        );
        store_message(
            &storage,
            &yours,
            &message("msg_same", vec![text("prt_same", "yours")]),
        );

        let mine = storage
            .load_transcript(&mine)
            .expect("the transcript loads");
        let yours = storage
            .load_transcript(&yours)
            .expect("the transcript loads");
        assert_eq!(mine.len(), 1);
        assert_eq!(yours.len(), 1);
        assert_eq!(mine[0].parts[0].as_text(), Some("mine"));
        assert_eq!(yours[0].parts[0].as_text(), Some("yours"));
    }

    #[test]
    fn two_stores_on_one_database_take_turns_rather_than_take_each_others_writes() {
        let directory = temporary();
        let root = directory.path().join("storage");
        let mine = Storage::open(root.clone());
        let yours = Storage::open(root);

        // Two handles is what two `ganja` processes in one project look like
        // from here: two writer threads, two connections, one file. WAL admits
        // one writer at a time and `busy_timeout` is what makes the other wait
        // instead of failing — a claim nothing else in this suite exercises.
        //
        // Both sides deliberately use the *same* message and part ids, because
        // that is the collision the composite keys exist for. UUIDv7 ids make
        // it far less likely than the old per-process counter did (**D493**),
        // but "unlikely" is not what a primary key is for: under a bare `id`
        // the second writer would take the first one's row.
        let rounds = 25;
        let write = |storage: &Storage, owner: &str, what: &str| {
            let id = session(owner);
            storage
                .save_info(&info(owner, 1))
                .expect("the record writes");
            for round in 0..rounds {
                let held = message(
                    "msg_same",
                    vec![text("prt_same", &format!("{what} {round}"))],
                );
                store_message(storage, &id, &held);
            }
        };

        // The two sessions carry this build's own spelling, because the second
        // handle to open finds the first one's rows already there — and a
        // store whose rows are older than UUIDv7 is set aside at that moment
        // rather than written into (**D493**).
        let mine_id = minted(1);
        let yours_id = minted(2);
        std::thread::scope(|scope| {
            scope.spawn(|| write(&mine, &mine_id, "mine"));
            scope.spawn(|| write(&yours, &yours_id, "yours"));
        });

        // Either handle answers for both sessions: one database, two views of
        // it, and neither writer's row was overwritten by the other's.
        let read = |storage: &Storage, owner: &str, what: &str| {
            let loaded = storage
                .load_transcript(&session(owner))
                .expect("the transcript loads");
            assert_eq!(loaded.len(), 1, "{owner}: {loaded:#?}");
            assert_eq!(
                loaded[0].parts.len(),
                1,
                "{owner} rewrote one part rather than accumulating them"
            );
            assert_eq!(
                loaded[0].parts[0].as_text(),
                Some(format!("{what} {}", rounds - 1).as_str()),
                "{owner} must hold its own last write"
            );
        };
        for storage in [&mine, &yours] {
            read(storage, &mine_id, "mine");
            read(storage, &yours_id, "yours");
        }
        assert_eq!(
            mine.list_sessions().expect("the store lists").len(),
            2,
            "both writers' sessions are in the one database"
        );
    }

    #[test]
    fn every_connection_sets_the_pragmas_the_store_depends_on() {
        let directory = temporary();
        let storage = storage(&directory);
        storage
            .save_info(&info("ses_1", 1))
            .expect("the record writes");

        let connection = connect(storage.database()).expect("a connection opens");
        let read = |pragma: &str| -> i64 {
            connection
                .query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
                .expect("the pragma reads")
        };
        let journal: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("the pragma reads");

        assert_eq!(journal, "wal");
        assert_eq!(
            read("synchronous"),
            1,
            "NORMAL, not the build's FULL default"
        );
        assert_eq!(
            read("foreign_keys"),
            1,
            "without this the cascade is a no-op"
        );
        assert_eq!(read("busy_timeout"), 5000);
        assert_eq!(read("cache_size"), -64_000);
    }

    #[test]
    fn a_second_open_finds_the_schema_already_there_and_the_sessions_with_it() {
        let directory = temporary();
        let name = minted(1);
        let id = session(&name);
        {
            let storage = storage(&directory);
            storage
                .save_info(&info(&name, 5))
                .expect("the record writes");
            store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));
        }

        let storage = storage(&directory);
        assert!(storage.load_info(&id).expect("the record reads").is_some());
        assert_eq!(
            storage.load_transcript(&id).expect("the transcript loads")[0].parts[0].as_text(),
            Some("a")
        );

        let applied: i64 = beside(&storage)
            .query_row("SELECT COUNT(*) FROM migration", [], |row| row.get(0))
            .expect("the journal counts");
        assert_eq!(
            applied,
            MIGRATIONS.len() as i64,
            "a second open must not replay what the first one stamped"
        );
    }

    #[test]
    fn an_unreadable_database_is_set_aside_and_the_store_starts_fresh() {
        let directory = temporary();
        {
            let storage = storage(&directory);
            storage
                .save_info(&info("ses_lost", 5))
                .expect("the record writes");
        }

        // The log goes first, and its absence is the point rather than
        // housekeeping: a write-ahead log beside a damaged database is not
        // damage — SQLite recovers the file out of it, header and all, and the
        // store reads perfectly. That is the right outcome, and it is also
        // exactly why `set_aside` has to carry all three files together.
        let database = directory.path().join(DATABASE);
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(directory.path().join(format!("{DATABASE}{suffix}")));
        }

        // Then the header, so nothing works at all — not even reading the
        // schema — which is the one failure `integrity_check` cannot report,
        // because it cannot run: the check itself errors instead of returning
        // a verdict.
        let mut bytes = fs::read(&database).expect("the database reads");
        bytes[..16].copy_from_slice(b"not a database!!");
        fs::write(&database, &bytes).expect("the database is damaged");

        let storage = storage(&directory);
        assert!(
            storage
                .list_sessions()
                .expect("a damaged store opens rather than failing")
                .is_empty(),
            "what replaces a damaged store is empty"
        );
        storage
            .save_info(&info("ses_new", 1))
            .expect("the fresh store writes");
        assert_eq!(
            storage
                .list_sessions()
                .expect("the fresh store lists")
                .len(),
            1,
            "the store that replaces a damaged one is a working one"
        );

        let aside = names(directory.path());
        assert!(
            aside.iter().any(|name| name.contains(".corrupt-")),
            "the damaged database must be kept rather than deleted, got {aside:?}"
        );
    }

    #[test]
    fn a_database_set_aside_takes_its_write_ahead_log_with_it() {
        let directory = temporary();
        let database = directory.path().join(DATABASE);
        for suffix in ["", "-wal", "-shm"] {
            fs::write(
                directory.path().join(format!("{DATABASE}{suffix}")),
                b"whatever was there",
            )
            .expect("the file is writable");
        }

        assert!(super::set_aside(
            &database,
            "for the test",
            super::QUARANTINE
        ));

        // A log left behind is recovered into the *fresh* file that takes the
        // old name, which would pour the damaged store straight back in — so
        // the three files move together or the set-aside is worse than
        // useless.
        let left = names(directory.path());
        assert_eq!(
            left.iter()
                .filter(|name| !name.contains(".corrupt-"))
                .count(),
            0,
            "nothing may be left under the name a fresh database will take, got {left:?}"
        );
        for suffix in ["", "-wal", "-shm"] {
            assert!(
                left.iter()
                    .any(|name| name.contains(".corrupt-") && name.ends_with(suffix)),
                "the {suffix:?} file must travel with its database, got {left:?}"
            );
        }
    }

    /// The quarantine waits for the lock, and then asks again.
    ///
    /// This is the regression drill for the bug the first version of
    /// [`set_aside_preuuid`] shipped: it decided on its own connection and
    /// renamed afterwards, so two processes could both decide "old ids", and
    /// the second one would rename the *fresh* store the first had already put
    /// in place. `ganja-cli`'s `two_processes_racing_the_quarantine…` catches
    /// it with two real processes, roughly one run in six; this catches it
    /// every time, by holding the lock and playing the winner underneath a
    /// store that is already past the point of no return.
    ///
    /// Both halves are asserted, because either alone would pass on a build
    /// with no lock at all: that the waiting store does **not** rename while
    /// the lock is held, and that once it gets in it re-reads the path and
    /// finds nothing to do.
    #[test]
    fn a_quarantine_waits_for_the_lock_and_then_finds_nothing_left_to_do() {
        let directory = temporary();
        let root = directory.path().join("storage");
        let old = "ses_0193b2f0a1c2000000";
        {
            let planted = storage(&directory);
            planted.save_info(&info(old, 5)).expect("the record writes");
        }
        let database = Storage::open(root.clone()).database().to_path_buf();

        // Taken before anybody else can want it, and held across the whole
        // interleaving below.
        let held = super::QuarantineLock::take(&database).expect("the lock is available");

        let waiting = Storage::open(root.clone());
        std::thread::scope(|scope| {
            let parked = scope.spawn(|| waiting.list_sessions());
            // Long enough for that store to have read the old id, asked for
            // the lock, and be waiting on it.
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(
                !names(directory.path())
                    .iter()
                    .any(|name| name.contains(".preuuid-")),
                "nothing may be set aside while another process holds the lock"
            );

            // The winner's move, played by this test: the store renamed aside
            // and a fresh one created at the name it left.
            assert!(super::set_aside(&database, "for the test", super::PREUUID));
            let fresh = minted(1);
            Storage::open(root.clone())
                .save_info(&info(&fresh, 1))
                .expect("the fresh store writes");
            drop(held);

            let listed = parked
                .join()
                .expect("the waiting store does not panic")
                .expect("the waiting store opens rather than failing");
            assert_eq!(
                listed
                    .iter()
                    .map(|info| info.id.as_str())
                    .collect::<Vec<_>>(),
                vec![fresh.as_str()],
                "the store that waited must go on with what the winner left"
            );
        });

        let left = names(directory.path());
        assert_eq!(
            left.iter()
                .filter(|name| name.contains(".preuuid-")
                    && !name.ends_with("-wal")
                    && !name.ends_with("-shm"))
                .count(),
            1,
            "the store that waited must not set the winner's fresh store aside too, got {left:?}"
        );
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_rather_than_migrated_down() {
        let directory = temporary();
        {
            let storage = storage(&directory);
            storage
                .save_info(&info("ses_1", 5))
                .expect("the record writes");
            beside(&storage)
                .execute(
                    "INSERT INTO migration (id, time_completed) VALUES ('29991231235959_from_ahead', 1)",
                    [],
                )
                .expect("a newer build's journal row is written");
        }

        let storage = storage(&directory);
        let refused = storage.list_sessions();
        assert!(
            matches!(
                &refused,
                Err(StorageError::Newer { unknown, .. }) if unknown == "29991231235959_from_ahead"
            ),
            "a newer build's database must be refused by name, got {refused:?}"
        );

        // Refused is not quarantined: the sessions in there belong to the
        // build that can read them.
        assert!(
            !names(directory.path())
                .iter()
                .any(|name| name.contains(".corrupt-")),
            "a database this build merely does not understand must be left alone"
        );
    }

    #[test]
    fn a_database_that_is_not_a_session_store_is_refused_rather_than_guessed_at() {
        let directory = temporary();
        fs::create_dir_all(directory.path()).expect("the directory exists");
        let connection =
            Connection::open(directory.path().join(DATABASE)).expect("a database is creatable");
        connection
            .execute_batch("CREATE TABLE somebody_elses (id TEXT);")
            .expect("the foreign table is creatable");
        drop(connection);

        let storage = storage(&directory);
        let refused = storage.list_sessions();
        assert!(
            matches!(&refused, Err(StorageError::Foreign { .. })),
            "somebody else's database must be refused, got {refused:?}"
        );
    }

    #[test]
    fn an_older_file_store_is_carried_across_on_first_open_and_set_aside_intact() {
        let directory = temporary();
        let root = directory.path().join("storage");
        let name = minted(1);
        let id = session(&name);
        let held = message("msg_1", vec![text("prt_1", "a"), tool("prt_2")]);

        // The file layout, written by hand: this is the store a build before
        // this one left, and nothing in the tree writes it any more. Its ids
        // are this build's, because a tree carrying older ones is set aside
        // rather than carried across (**D493**) — `storage_preuuid_tree.rs`
        // is where that is asserted.
        let info = SessionInfo {
            title: Some("carried".to_owned()),
            ..info(&name, 5)
        };
        write_json(
            &root
                .join("session")
                .join("info")
                .join(format!("{name}.json")),
            &info,
        );
        let mut envelope = held.clone();
        envelope.parts.clear();
        write_json(
            &root
                .join("session")
                .join("message")
                .join(&name)
                .join("msg_1.json"),
            &serde_json::json!({"version": VERSION, "payload": envelope}),
        );
        for part in &held.parts {
            write_json(
                &root
                    .join("session")
                    .join("part")
                    .join(&name)
                    .join("msg_1")
                    .join(format!("{}.json", part.id.as_str())),
                &serde_json::json!({"version": VERSION, "payload": part}),
            );
        }

        let storage = Storage::open(root.clone());
        assert_eq!(
            storage.load_info(&id).expect("the carried record reads"),
            Some(info)
        );
        let loaded = storage.load_transcript(&id).expect("the transcript loads");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].parts, held.parts);

        // Renamed rather than deleted: whoever downgrades tomorrow finds every
        // file exactly where it was.
        assert!(!root.exists(), "the converted tree is not left in place");
        let aside = names(directory.path())
            .into_iter()
            .find(|name| name.starts_with("storage.migrated-"))
            .expect("the converted tree is kept under a new name");
        assert!(
            directory
                .path()
                .join(aside)
                .join("session")
                .join("info")
                .join(format!("{name}.json"))
                .is_file(),
            "the set-aside tree must still hold what it held"
        );
    }

    /// A tree the conversion cannot carry whole keeps the tree.
    ///
    /// Two info files claiming one session id is what a hand-edited or
    /// half-restored store looks like. `carry` inserts plainly where the
    /// running store upserts, so the second one is a UNIQUE violation: that
    /// session is counted lost, the warning says so, and the tree is left
    /// exactly where it is — because the only copy of what did not make it is
    /// in there. An upsert here would silently keep one of the two and then
    /// rename the tree away, which is the one outcome nobody could undo.
    #[test]
    fn a_tree_holding_one_id_twice_is_left_where_it_is() {
        let directory = temporary();
        let root = directory.path().join("storage");

        let name = minted(1);
        for (file, title) in [
            (format!("{name}.json"), "first"),
            (format!("also-{name}.json"), "second"),
        ] {
            write_json(
                &root.join("session").join("info").join(file),
                &SessionInfo {
                    title: Some(title.to_owned()),
                    ..info(&name, 5)
                },
            );
        }

        let storage = Storage::open(root.clone());
        // Whichever of the two got there first is readable; which one it is is
        // the directory's order to decide and not this test's.
        assert!(
            storage
                .load_info(&session(&name))
                .expect("the database opens")
                .is_some(),
            "the session that did carry across is there"
        );
        assert!(
            root.join("session")
                .join("info")
                .join(format!("{name}.json"))
                .is_file(),
            "and the tree that holds the one that did not is untouched"
        );
        assert!(
            !names(directory.path())
                .into_iter()
                .any(|name| name.starts_with("storage.migrated-")),
            "a conversion that lost something sets nothing aside"
        );
    }

    #[test]
    fn a_second_open_does_not_convert_a_tree_that_appeared_after_the_first() {
        let directory = temporary();
        let root = directory.path().join("storage");
        let native = minted(1);
        {
            let storage = Storage::open(root.clone());
            storage
                .save_info(&info(&native, 5))
                .expect("the record writes");
        }

        // A tree that turns up after the database exists is not this build's
        // to import: convert-on-first-open happens exactly once, and once is
        // the open that created the file.
        let late = minted(2);
        write_json(
            &root
                .join("session")
                .join("info")
                .join(format!("{late}.json")),
            &info(&late, 9),
        );

        let storage = Storage::open(root.clone());
        let listed: Vec<String> = storage
            .list_sessions()
            .expect("the store lists")
            .into_iter()
            .map(|info| info.id.as_str().to_owned())
            .collect();
        assert_eq!(listed, vec![native]);
        assert!(
            root.exists(),
            "a tree that was not converted is not set aside"
        );
    }

    /// Writes one file of the layout the conversion reads.
    fn write_json<T: serde::Serialize>(path: &Path, value: &T) {
        fs::create_dir_all(path.parent().expect("a file has a directory"))
            .expect("the directory is creatable");
        fs::write(path, serde_json::to_vec(value).expect("the value encodes"))
            .expect("the file is writable");
    }
}

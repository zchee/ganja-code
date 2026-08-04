//! Where sessions live between runs: versioned JSON files under a project's
//! data directory.
//!
//! Spec: upstream's legacy `packages/opencode/src/storage/` layout. Under the
//! root this module is opened on — `<project data dir>/storage/` — files land
//! as:
//!
//! ```text
//! session/info/<sid>.json                 SessionInfo
//! session/message/<sid>/<mid>.json        Message envelope, parts stripped
//! session/part/<sid>/<mid>/<pid>.json     one Part
//! ```
//!
//! Parts live apart from their envelope so that a streaming turn rewrites one
//! small file per fragment instead of the whole message, which is the same
//! reason upstream splits them. Ascending ids double as ordering: reassembly
//! sorts by filename, and filenames sort by creation.
//!
//! Every file carries a `version` field — [`SessionInfo`] inline, message and
//! part files through an envelope `{"version":1,"payload":…}` — so the P7
//! SQLite migration knows exactly what it is reading. Writes are atomic:
//! create-new temp file beside the target, then rename, the [`crate::auth`]
//! pattern. A file that does not parse is quarantined — renamed to
//! `<name>.corrupt-<millis>` with a warning — and skipped, never a crash and
//! never an error the caller has to fear.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::protocol::{Message, MessageId, Part, Usage, ascending, now};

/// The storage format this build writes.
pub const VERSION: u32 = 1;

/// Prefix session ids carry, matching upstream's `ses_` ids.
const SESSION_PREFIX: &str = "ses";

/// Directory every session artefact hangs under, matching upstream's layout.
const SESSION: &str = "session";

/// Directory holding one info file per session.
const INFO: &str = "info";

/// Directory holding one directory of message envelopes per session.
const MESSAGE: &str = "message";

/// Directory holding one directory of part files per message.
const PART: &str = "part";

/// Extension every stored file carries, and the only one a listing reads.
const EXTENSION: &str = "json";

/// What a file that could not be read is renamed to, ahead of the moment it
/// was set aside: `<name>.corrupt-<millis>`.
const QUARANTINE: &str = "corrupt";

/// Keeps one write's temporary file apart from another's inside this process.
/// The name is derived from the file being replaced, so only two writes to the
/// same file can collide — a debounced text flush landing on the part a status
/// change is rewriting — and that is exactly what a counter settles.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Identifies a stored session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(SESSION_PREFIX))
    }

    /// The id as it appears in paths and listings.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    /// Adopts a stored id, whatever it was written with.
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Everything known about a session apart from its transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Names the session, in paths and in the picker.
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
    /// The session that delegated this one, when a `task` call created it.
    ///
    /// Present on subagent sessions and absent on every other, which is what
    /// lets a listing tell a conversation somebody had from one a tool call
    /// spawned. Absent from the wire when there is none, so an ordinary
    /// session's bytes are what they always were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
}

/// A write the storage could not perform. Reads do not produce errors for
/// content: a missing file is [`None`] and a corrupt one is quarantined and
/// skipped, so the only failures left are the filesystem refusing to act.
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
}

/// What a message or part file holds: the value, and the format it is in.
///
/// [`SessionInfo`] carries its own `version` field, so only these two need
/// wrapping. The order of the fields is the order they reach the file, which
/// puts the version first — where something reading a truncated file, or a
/// person reading it in a pager, meets it before anything it governs.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    /// Format `payload` is written in; see [`VERSION`].
    version: u32,
    /// The stored value.
    payload: T,
}

/// The version field alone, read before the rest of a file is decoded.
///
/// Deciding what a file *is* has to come before decoding it. A file a later
/// build wrote may be shaped in ways this one cannot parse at all, and a build
/// that treated "does not decode" as "is corrupt" would quarantine the newer
/// build's sessions on sight — which is the one outcome this module exists to
/// prevent. So the version is read first, and a file that is not this build's
/// is never touched.
#[derive(Deserialize)]
struct Versioned {
    /// Format the rest of the file is written in. Deliberately not defaulted:
    /// every build stamps it, so a file without one was not written by any of
    /// them.
    version: u32,
}

/// One project's session store, rooted at `<project data dir>/storage/`.
///
/// Opening is free of I/O: directories appear when the first write needs
/// them, so resolving a project never litters the data directory.
#[derive(Clone, Debug)]
pub struct Storage {
    /// The `storage/` directory everything lives under.
    root: PathBuf,
}

impl Storage {
    /// A store rooted at `root`, the `storage/` directory itself.
    #[must_use]
    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory this store reads and writes under.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Writes a session's info file atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses the write.
    pub fn save_info(&self, info: &SessionInfo) -> Result<(), StorageError> {
        write_json(&self.info_path(&info.id), info)
    }

    /// Reads one session's info, or [`None`] when it does not exist or was
    /// quarantined as corrupt.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses the read.
    pub fn load_info(&self, id: &SessionId) -> Result<Option<SessionInfo>, StorageError> {
        read_stored(&self.info_path(id))
    }

    /// Every stored session, newest [`SessionInfo::updated`] first.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses the listing; an
    /// absent store lists as empty.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, StorageError> {
        let mut sessions: Vec<SessionInfo> = Vec::new();
        for path in stored_files(&self.info_dir())? {
            if let Some(info) = read_stored(&path)? {
                sessions.push(info);
            }
        }
        // Newest first, and by id when two carry the same instant: ids ascend,
        // so the one minted later still sorts first and a picker never shows
        // two sessions in an order that changes between listings.
        sessions.sort_by(|left, right| {
            right
                .updated
                .cmp(&left.updated)
                .then_with(|| right.id.cmp(&left.id))
        });

        Ok(sessions)
    }

    /// Writes a message's envelope atomically, its parts stripped: parts have
    /// their own files, and this file says the message exists, who spoke it,
    /// and when.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses the write.
    pub fn save_message(&self, session: &SessionId, message: &Message) -> Result<(), StorageError> {
        // Stripping happens here rather than at the call site because a caller
        // that had to remember would eventually forget, and forgetting writes
        // every part twice — once inline, once in its own file — which is how
        // a transcript comes back doubled.
        let mut stored = message.clone();
        stored.parts.clear();

        write_json(
            &self.message_path(session, &message.id),
            &Envelope {
                version: VERSION,
                payload: &stored,
            },
        )
    }

    /// Writes one part atomically, replacing what was there: a streaming text
    /// part is rewritten as it grows, and its file is small precisely so this
    /// stays cheap.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses the write.
    pub fn save_part(
        &self,
        session: &SessionId,
        message: &MessageId,
        part: &Part,
    ) -> Result<(), StorageError> {
        write_json(
            &self.part_path(session, message, part),
            &Envelope {
                version: VERSION,
                payload: part,
            },
        )
    }

    /// Reads a session's whole transcript back: envelopes in id order, each
    /// carrying its parts in id order. Corrupt files are quarantined and
    /// skipped; a message whose envelope is gone takes its parts with it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the filesystem refuses a read; a session
    /// with no stored messages loads as empty.
    pub fn load_transcript(&self, session: &SessionId) -> Result<Vec<Message>, StorageError> {
        let mut transcript = Vec::new();
        for path in stored_files(&self.message_dir(session))? {
            // The part directory is opened only from an envelope that was
            // read, which is what makes a lost envelope cost its parts too:
            // nothing else ever looks in there.
            let Some(envelope) = read_stored::<Envelope<Message>>(&path)? else {
                continue;
            };
            let mut message = envelope.payload;

            for path in stored_files(&self.part_dir(session, &message.id))? {
                if let Some(envelope) = read_stored::<Envelope<Part>>(&path)? {
                    message.parts.push(envelope.payload);
                }
            }
            transcript.push(message);
        }

        Ok(transcript)
    }

    /// Where every session's info file lives.
    fn info_dir(&self) -> PathBuf {
        self.root.join(SESSION).join(INFO)
    }

    /// Where one session's info lives.
    fn info_path(&self, id: &SessionId) -> PathBuf {
        self.info_dir().join(file_name(id.as_str()))
    }

    /// Where one session's message envelopes live.
    fn message_dir(&self, session: &SessionId) -> PathBuf {
        self.root.join(SESSION).join(MESSAGE).join(session.as_str())
    }

    /// Where one message's envelope lives.
    fn message_path(&self, session: &SessionId, message: &MessageId) -> PathBuf {
        self.message_dir(session).join(file_name(message.as_str()))
    }

    /// Where one message's parts live.
    fn part_dir(&self, session: &SessionId, message: &MessageId) -> PathBuf {
        self.root
            .join(SESSION)
            .join(PART)
            .join(session.as_str())
            .join(message.as_str())
    }

    /// Where one part lives.
    fn part_path(&self, session: &SessionId, message: &MessageId, part: &Part) -> PathBuf {
        self.part_dir(session, message)
            .join(file_name(part.id.as_str()))
    }
}

/// What an id is called on disk.
fn file_name(id: &str) -> String {
    format!("{id}.{EXTENSION}")
}

/// Writes `value` at `path` as JSON, creating the directories it needs.
///
/// The bytes land in a sibling that is renamed into place, so a write that is
/// interrupted — or one that races the read of a resume — can only leave the
/// old file or the new one, never half of either.
///
/// Two things the [`crate::auth`] pattern does are deliberately left out. The
/// file is not created private: a transcript is not a credential, and a mode
/// nobody else in the data directory has would only be surprising. And there
/// is no `sync_all`: a turn writes one of these every few hundred
/// milliseconds, where the cost of a flush is real and what a crash could take
/// is the tail of a stream that the crash ended anyway.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StorageError> {
    let parent = path.parent().ok_or_else(|| StorageError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::NotFound,
            "the file has no directory to be created in",
        ),
    })?;
    fs::create_dir_all(parent).map_err(|source| StorageError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut json = serde_json::to_vec(value).map_err(|source| StorageError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    json.push(b'\n');

    let temporary = temporary_beside(path);
    write_new(&temporary, &json).map_err(|source| StorageError::Io {
        path: temporary.clone(),
        source,
    })?;

    fs::rename(&temporary, path).map_err(|source| {
        // A rename that fails leaves the sibling holding a copy of what was
        // being written, and a listing that later tripped over it would be
        // reading a file nobody meant to keep.
        let _ = fs::remove_file(&temporary);
        StorageError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// The sibling `path` is written through.
///
/// It sits beside the target so the rename stays within one filesystem, and it
/// carries an extension no listing reads, so a write that dies before its
/// rename cannot be mistaken for stored data.
fn temporary_beside(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    path.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Writes `bytes` to a newly created file.
///
/// `create_new` is `O_CREAT | O_EXCL`, which does not follow a symbolic link at
/// the final component: the name is predictable enough for someone sharing the
/// machine to plant one, and an open that followed it would write through to
/// wherever it led and then rename that file over the stored session.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        // Either a write that died before its rename, or something planted to
        // catch this one. Unlinking the name and creating it again exclusively
        // settles both: what is removed is the name, never whatever it pointed
        // at, and a link planted in between fails the retry outright.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
        }
        result => result?,
    };

    file.write_all(bytes)
}

/// Reads one stored file, or [`None`] when there is nothing this build can use
/// there.
///
/// Three ways to come back with nothing, and they are deliberately not the
/// same thing: a file that is not there was never written, one that does not
/// parse is moved aside and skipped, and one a later build wrote is left
/// exactly where it is. Only the filesystem refusing to answer is an error —
/// a corrupt session must cost that session, not the ability to start one.
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

    match serde_json::from_slice::<Versioned>(&bytes) {
        Ok(stored) if stored.version == VERSION => {}
        Ok(stored) => {
            tracing::warn!(
                path = %path.display(),
                version = stored.version,
                understands = VERSION,
                "a stored file was written by a newer build and was left alone"
            );

            return Ok(None);
        }
        Err(error) => {
            quarantine(path, &error);

            return Ok(None);
        }
    }

    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
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
/// nothing rather than failing: a session with no messages is a session that
/// was never answered, not a broken store.
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

    use tempfile::TempDir;

    use super::{SessionId, SessionInfo, Storage, VERSION};
    use crate::protocol::{
        Message, MessageId, MessageTime, Part, PartBody, PartId, Role, ToolState, Usage,
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
            parent: None,
        }
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

    /// The ids of a transcript's messages, each with the ids of its parts.
    fn shape(transcript: &[Message]) -> Vec<(String, Vec<String>)> {
        transcript
            .iter()
            .map(|message| {
                (
                    message.id.as_str().to_owned(),
                    message
                        .parts
                        .iter()
                        .map(|part| part.id.as_str().to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    /// Plants a file that this build did not write, creating the directories
    /// it needs.
    fn plant(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("the fixture has a directory"))
            .expect("the fixture directory is creatable");
        fs::write(path, contents).expect("the fixture writes");
    }

    /// The parsed contents of a stored file.
    fn stored(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).expect("the file exists")).expect("the file is JSON")
    }

    #[test]
    fn a_store_that_was_never_written_reads_as_empty_rather_than_failing() {
        let directory = temporary();
        let storage = storage(&directory);

        assert_eq!(
            storage
                .load_info(&session("ses_1"))
                .expect("a miss is fine"),
            None
        );
        assert!(storage.list_sessions().expect("a miss is fine").is_empty());
        assert!(
            storage
                .load_transcript(&session("ses_1"))
                .expect("a miss is fine")
                .is_empty()
        );
        assert!(
            !storage.root().exists(),
            "reading must not create the store: {}",
            storage.root().display()
        );
    }

    #[test]
    fn an_info_file_round_trips_with_its_optional_fields_set_and_unset() {
        let directory = temporary();
        let storage = storage(&directory);

        let bare = info("ses_1", 10);
        storage.save_info(&bare).expect("the info stores");
        assert_eq!(
            storage.load_info(&bare.id).expect("the info reads back"),
            Some(bare.clone())
        );

        // The absent halves are absent on disk too, rather than stored as
        // nulls a later build would have to interpret.
        let written = stored(&directory.path().join("storage/session/info/ses_1.json"));
        assert_eq!(written["version"], VERSION);
        assert!(written.get("title").is_none(), "{written}");
        assert!(written.get("summary").is_none(), "{written}");
        assert!(written.get("agent").is_none(), "{written}");
        assert!(written.get("model").is_none(), "{written}");

        let filled = SessionInfo {
            title: Some("porting storage".to_owned()),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            },
            context_tokens: 1_234,
            summary: Some(MessageId::from("msg_2".to_owned())),
            agent: Some("plan".to_owned()),
            model: Some("claude-haiku-4.5".to_owned()),
            parent: None,
            ..bare.clone()
        };
        storage.save_info(&filled).expect("the info stores");

        assert_eq!(
            storage.load_info(&bare.id).expect("the info reads back"),
            Some(filled),
            "a second write has to replace the first"
        );
    }

    /// The envelope says a message exists; the parts have their own files. A
    /// caller that handed its live message over must get it back untouched —
    /// the stripping is the store's, not a mutation of the transcript.
    #[test]
    fn a_message_is_stored_without_its_parts_and_the_caller_keeps_them() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let message = message("msg_1", vec![text("prt_1", "hello"), tool("prt_2")]);

        storage
            .save_message(&session, &message)
            .expect("the envelope stores");

        assert_eq!(message.parts.len(), 2, "the caller's message was emptied");

        let written = stored(
            &directory
                .path()
                .join("storage/session/message/ses_1/msg_1.json"),
        );
        assert_eq!(written["version"], VERSION);
        assert_eq!(written["payload"]["parts"], serde_json::json!([]));
        assert_eq!(written["payload"]["id"], "msg_1");
        assert_eq!(written["payload"]["model"], "canned");
    }

    #[test]
    fn a_completed_tool_part_round_trips_whole() {
        let directory = temporary();
        let storage = storage(&directory);
        let part = tool("prt_1");
        let reassembled = message("msg_1", vec![part.clone()]);
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());

        storage
            .save_message(&session, &message)
            .expect("the envelope stores");
        storage
            .save_part(&session, &message.id, &part)
            .expect("the part stores");

        let written = stored(
            &directory
                .path()
                .join("storage/session/part/ses_1/msg_1/prt_1.json"),
        );
        assert_eq!(written["version"], VERSION);
        assert_eq!(written["payload"]["state"]["status"], "completed");

        assert_eq!(
            storage
                .load_transcript(&session)
                .expect("the transcript reads"),
            vec![reassembled],
            "every field of the richest part has to survive the round trip"
        );
    }

    /// Ordering is by id everywhere, and ids ascend with creation — so writing
    /// in the wrong order has to come back in the right one.
    #[test]
    fn a_transcript_reassembles_its_messages_and_parts_in_id_order() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let first = message("msg_1", Vec::new());
        let second = message("msg_2", Vec::new());

        storage
            .save_message(&session, &second)
            .expect("the envelope stores");
        storage
            .save_message(&session, &first)
            .expect("the envelope stores");
        for (message, parts) in [(&second, ["prt_3", "prt_4"]), (&first, ["prt_1", "prt_2"])] {
            for id in parts.iter().rev() {
                storage
                    .save_part(&session, &message.id, &text(id, id))
                    .expect("the part stores");
            }
        }

        let transcript = storage
            .load_transcript(&session)
            .expect("the transcript reads");

        assert_eq!(
            shape(&transcript),
            vec![
                (
                    "msg_1".to_owned(),
                    vec!["prt_1".to_owned(), "prt_2".to_owned()]
                ),
                (
                    "msg_2".to_owned(),
                    vec!["prt_3".to_owned(), "prt_4".to_owned()]
                ),
            ]
        );
    }

    /// A session whose info is unreadable must not take the listing with it,
    /// and the file has to be kept: the next listing skips it because it no
    /// longer looks like stored data, not because anything remembered it.
    #[test]
    fn a_corrupt_info_file_is_quarantined_and_the_rest_still_lists() {
        let directory = temporary();
        let storage = storage(&directory);
        let good = info("ses_2", 20);
        storage.save_info(&good).expect("the info stores");
        let broken = directory.path().join("storage/session/info/ses_1.json");
        plant(&broken, "{ this was never JSON");

        assert_eq!(
            storage
                .load_info(&session("ses_1"))
                .expect("a read is fine"),
            None
        );
        assert!(!broken.exists(), "the unreadable file should be aside");
        assert_eq!(
            names(broken.parent().expect("the directory exists"))
                .iter()
                .filter(|name| name.starts_with("ses_1.json.corrupt-"))
                .count(),
            1,
            "{:?}",
            names(broken.parent().expect("the directory exists"))
        );

        assert_eq!(
            storage.list_sessions().expect("the listing reads"),
            vec![good],
            "one unreadable session must not cost the others"
        );
    }

    /// The envelope is the message: without it there is nothing to attach
    /// parts to, so they are not read at all.
    #[test]
    fn a_corrupt_envelope_takes_its_message_and_its_parts_out_of_the_transcript() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let kept = message("msg_2", Vec::new());

        for message in [&message("msg_1", Vec::new()), &kept] {
            storage
                .save_message(&session, message)
                .expect("the envelope stores");
            storage
                .save_part(&session, &message.id, &text("prt_1", "hello"))
                .expect("the part stores");
        }
        let broken = directory
            .path()
            .join("storage/session/message/ses_1/msg_1.json");
        plant(&broken, "{\"version\":1,\"payload\":");

        let transcript = storage
            .load_transcript(&session)
            .expect("the transcript reads");

        assert_eq!(
            shape(&transcript),
            vec![("msg_2".to_owned(), vec!["prt_1".to_owned()])]
        );
        assert!(!broken.exists(), "the unreadable file should be aside");
        assert!(
            directory
                .path()
                .join("storage/session/part/ses_1/msg_1/prt_1.json")
                .exists(),
            "an unread part is not a corrupt one, and must be left alone"
        );
    }

    #[test]
    fn a_corrupt_part_is_quarantined_and_its_message_keeps_the_rest() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());

        storage
            .save_message(&session, &message)
            .expect("the envelope stores");
        for id in ["prt_1", "prt_3"] {
            storage
                .save_part(&session, &message.id, &text(id, id))
                .expect("the part stores");
        }
        let broken = directory
            .path()
            .join("storage/session/part/ses_1/msg_1/prt_2.json");
        plant(&broken, "{\"version\":1,\"payload\":{\"id\":\"prt_2\"}}");

        let transcript = storage
            .load_transcript(&session)
            .expect("the transcript reads");

        assert_eq!(
            shape(&transcript),
            vec![(
                "msg_1".to_owned(),
                vec!["prt_1".to_owned(), "prt_3".to_owned()]
            )],
            "a lost part costs that part and nothing more"
        );
        assert!(!broken.exists(), "the unreadable file should be aside");
        assert_eq!(
            names(broken.parent().expect("the directory exists"))
                .iter()
                .filter(|name| name.starts_with("prt_2.json.corrupt-"))
                .count(),
            1
        );
    }

    /// A file a later build wrote is skipped, never moved: it does not decode
    /// here, and quarantining on that basis would destroy data the build that
    /// wrote it can still read.
    #[test]
    fn a_file_from_a_newer_build_is_skipped_and_left_exactly_where_it_is() {
        let directory = temporary();
        let storage = storage(&directory);
        let skipped = session("ses_2");
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());
        storage
            .save_message(&session, &message)
            .expect("the envelope stores");
        storage
            .save_part(&session, &message.id, &text("prt_1", "hello"))
            .expect("the part stores");

        // Every one of these is shaped in a way this build cannot decode, on
        // purpose: the version has to be what decides, before the shape does.
        let newer = [
            (
                directory.path().join("storage/session/info/ses_2.json"),
                r#"{"version":2,"id":"ses_2","started":"yesterday"}"#,
            ),
            (
                directory
                    .path()
                    .join("storage/session/message/ses_1/msg_2.json"),
                r#"{"version":2,"payload":{"id":"msg_2","speaker":"tool"}}"#,
            ),
            (
                directory
                    .path()
                    .join("storage/session/part/ses_1/msg_1/prt_2.json"),
                r#"{"version":2,"payload":{"id":"prt_2","kind":"reasoning"}}"#,
            ),
        ];
        for (path, contents) in &newer {
            plant(path, contents);
        }

        assert_eq!(storage.load_info(&skipped).expect("a read is fine"), None);
        assert!(
            storage
                .list_sessions()
                .expect("the listing reads")
                .is_empty()
        );
        assert_eq!(
            shape(
                &storage
                    .load_transcript(&session)
                    .expect("the transcript reads")
            ),
            vec![("msg_1".to_owned(), vec!["prt_1".to_owned()])]
        );

        for (path, contents) in &newer {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some(*contents),
                "a newer build's file must survive this one reading it: {}",
                path.display()
            );
        }
    }

    #[test]
    fn sessions_list_newest_first_and_ignore_what_was_quarantined() {
        let directory = temporary();
        let storage = storage(&directory);
        for (id, updated) in [("ses_1", 30), ("ses_2", 10), ("ses_3", 30), ("ses_4", 20)] {
            storage.save_info(&info(id, updated)).expect("info stores");
        }
        // The shape a quarantined file has, planted rather than provoked: what
        // matters here is that a listing does not read it back.
        plant(
            &directory
                .path()
                .join("storage/session/info/ses_5.json.corrupt-1"),
            r#"{"version":1,"id":"ses_5","created":1,"updated":99}"#,
        );

        assert_eq!(
            storage
                .list_sessions()
                .expect("the listing reads")
                .iter()
                .map(|info| info.id.as_str().to_owned())
                .collect::<Vec<_>>(),
            // Newest first, and the later id first when two share an instant.
            vec!["ses_3", "ses_1", "ses_4", "ses_2"]
        );
    }

    /// The sibling a write goes through is renamed, not copied, so a store
    /// that has been written to holds nothing but stored data.
    #[test]
    fn a_write_leaves_nothing_behind_it() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());

        storage.save_info(&info("ses_1", 10)).expect("info stores");
        storage.save_info(&info("ses_1", 20)).expect("info stores");
        storage
            .save_message(&session, &message)
            .expect("the envelope stores");
        storage
            .save_part(&session, &message.id, &text("prt_1", "hello"))
            .expect("the part stores");

        assert_eq!(
            names(&directory.path().join("storage/session/info")),
            vec!["ses_1.json"]
        );
        assert_eq!(
            names(&directory.path().join("storage/session/message/ses_1")),
            vec!["msg_1.json"]
        );
        assert_eq!(
            names(&directory.path().join("storage/session/part/ses_1/msg_1")),
            vec!["prt_1.json"]
        );
    }

    /// A streaming text part is written again on every fragment, which is only
    /// affordable because each write replaces the last.
    #[test]
    fn rewriting_a_part_replaces_it_rather_than_leaving_the_old_one() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());
        storage
            .save_message(&session, &message)
            .expect("the envelope stores");

        for fragment in ["hel", "hello", "hello world"] {
            storage
                .save_part(&session, &message.id, &text("prt_1", fragment))
                .expect("the part stores");
        }

        assert_eq!(
            names(&directory.path().join("storage/session/part/ses_1/msg_1")),
            vec!["prt_1.json"]
        );
        assert_eq!(
            storage
                .load_transcript(&session)
                .expect("the transcript reads")
                .first()
                .and_then(|message| message.parts.first())
                .and_then(Part::as_text),
            Some("hello world")
        );
    }

    /// Parts are only ever reached through an envelope, so a directory left
    /// behind by a message that was quarantined cannot put it back.
    #[test]
    fn a_part_directory_with_no_envelope_does_not_resurrect_its_message() {
        let directory = temporary();
        let storage = storage(&directory);
        let session = session("ses_1");
        let message = message("msg_1", Vec::new());

        storage
            .save_part(&session, &message.id, &text("prt_1", "hello"))
            .expect("the part stores");

        assert!(
            storage
                .load_transcript(&session)
                .expect("the transcript reads")
                .is_empty(),
            "a part with no message must not invent one"
        );
    }
}

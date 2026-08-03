//! Provider credentials: where they come from, and how they are kept.
//!
//! Two sources, in this order: the environment variable each vendor's own SDK
//! reads, then `auth.json` under the XDG data directory. The environment wins
//! so that `ANTHROPIC_API_KEY=… ganja` is a one-shot override that leaves the
//! stored key alone, which is how every other tool in the terminal behaves.
//!
//! The file mirrors upstream's shape (`packages/opencode/src/auth/index.ts`) so
//! that the two can eventually read each other's storage:
//!
//! ```json
//! { "anthropic": { "type": "api", "key": "sk-…" } }
//! ```
//!
//! Reading is deliberately tolerant. Entries this build cannot interpret —
//! upstream's `oauth` and `wellknown` credentials, providers it has never heard
//! of — are carried through a rewrite untouched instead of being dropped, so
//! `ganja auth login` can never cost someone a credential it did not understand.
//!
//! Secrets never reach a log. [`Credential`] renders as its last four
//! characters through both [`Debug`] and [`Display`], and nothing in this module
//! formats a whole key.

use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::Deserialize;
use serde_json::{Value, error::Category};

/// Directory ganja keeps its state in, under the XDG data home.
const DIRECTORY: &str = "ganja";

/// File credentials live in, named after upstream's.
const FILE: &str = "auth.json";

/// Characters of a secret that any output may show.
const TAIL: usize = 4;

/// Stands in for the part of a secret that stays hidden.
const MASK: &str = "****";

/// Mode the credential file is created with and required to have: readable and
/// writable by its owner, invisible to everyone else.
#[cfg(unix)]
const PRIVATE: u32 = 0o600;

/// Bits that would let someone other than the owner read the file.
#[cfg(unix)]
const SHARED: u32 = 0o077;

/// Environment variables that carry an API key, by provider.
///
/// The names are the ones each vendor's own SDK reads, so a shell already set
/// up for `curl` or an official client needs no further configuration.
pub const KEY_VARS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
];

/// The environment variable an API key for `provider_id` may be passed in.
#[must_use]
pub fn key_var(provider_id: &str) -> Option<&'static str> {
    KEY_VARS
        .iter()
        .find(|(provider, _)| *provider == provider_id)
        .map(|(_, variable)| *variable)
}

/// The last few characters of a secret, which is all any output may show.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedTail(String);

impl RedactedTail {
    /// Renders `secret` as a mask followed by its last [`TAIL`] characters.
    ///
    /// Public so that nothing outside this module has to invent its own idea of
    /// how much of a key may be shown.
    ///
    /// Character counting is deliberate: an API key is ASCII in practice, but
    /// slicing bytes off a value that turned out not to be would panic.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        let characters: Vec<char> = secret.chars().collect();
        let visible: String = characters[characters.len().saturating_sub(TAIL)..]
            .iter()
            .collect();

        Self(format!("{MASK}{visible}"))
    }

    /// The redacted form, for a caller that needs to place it in a table.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedTail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for RedactedTail {
    /// Same as [`Display`]: a redacted value that grows quotes in a debug dump
    /// is still redacted, and one that grows the key back is a leak.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An API key, and the only thing a provider needs to authenticate.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The key itself, as the provider expects it in a header.
    pub api_key: String,
}

impl Credential {
    /// The key as it may be shown.
    #[must_use]
    pub fn tail(&self) -> RedactedTail {
        RedactedTail::of(&self.api_key)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("api_key", &self.tail())
            .finish()
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.tail().fmt(formatter)
    }
}

/// Where a credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The environment variable named here.
    Environment(&'static str),
    /// The stored credential file.
    File,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(variable) => formatter.write_str(variable),
            Self::File => formatter.write_str(FILE),
        }
    }
}

/// One provider that can be authenticated, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Provider the credential belongs to.
    pub provider_id: String,
    /// What may be shown of the key.
    pub tail: RedactedTail,
    /// Where [`credential_for`] would read it.
    pub source: Source,
}

/// A credential could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The store could not be reached.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, naming the path where there is one.
        context: String,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// The store is not the JSON object it has to be.
    ///
    /// The parser's own message is deliberately thrown away, and there is no
    /// `#[source]` to walk to it: `serde_json` quotes the offending value back
    /// — `invalid type: string "sk-…", expected a map` — and in this file every
    /// value is a secret. The position says where to look without saying what
    /// is there.
    #[error(
        "{} is not valid credential storage: {kind} at line {line}, column {column}",
        .path.display()
    )]
    Malformed {
        /// The file that could not be understood.
        path: PathBuf,
        /// How it failed to make sense, in words that quote nothing.
        kind: &'static str,
        /// Line the parser stopped on.
        line: usize,
        /// Column the parser stopped on.
        column: usize,
    },
    /// The store is exposed to other users of the machine.
    #[error(
        "{path} is readable by users other than its owner (mode {mode:04o}); \
         a leaked key cannot be un-leaked, so nothing was read from it - \
         run `chmod 600 {path}`, or rotate the key and store it again",
        path = .path.display()
    )]
    Permissions {
        /// The file with the permissions.
        path: PathBuf,
        /// The mode it was found with.
        mode: u32,
    },
}

impl AuthError {
    /// Reports a parse failure by position and kind, never by content.
    fn malformed(path: &Path, error: &serde_json::Error) -> Self {
        Self::Malformed {
            path: path.to_path_buf(),
            kind: match error.classify() {
                Category::Io => "the file could not be read",
                Category::Syntax => "the JSON is malformed",
                Category::Data => "the JSON is not the shape a credential store has",
                Category::Eof => "the JSON ends early",
            },
            line: error.line(),
            column: error.column(),
        }
    }
}

/// A stored credential, as far as this build understands it.
///
/// Upstream also stores `oauth` and `wellknown` credentials, which P2 cannot
/// use; they decode as [`Stored::Unusable`] so that a rewrite keeps them.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Stored {
    /// A plain API key.
    Api {
        /// The key.
        key: String,
    },
    /// Something this build cannot authenticate with.
    #[serde(other)]
    Unusable,
}

/// The credential file, wherever it turned out to be.
struct Store {
    path: PathBuf,
}

impl Store {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Resolves the store's location from the XDG data directory.
    ///
    /// XDG conventions are used on every platform, macOS included, matching
    /// upstream's own `~/.local/share/opencode` behaviour there.
    fn open() -> Result<Self, AuthError> {
        let base = Xdg::new().map_err(|source| AuthError::Io {
            context: "the home directory holding the credential store could not be located"
                .to_owned(),
            source: io::Error::other(source),
        })?;

        Ok(Self::new(base.data_dir().join(DIRECTORY).join(FILE)))
    }

    fn io(&self, attempt: &str, source: io::Error) -> AuthError {
        AuthError::Io {
            context: format!("{} {attempt}", self.path.display()),
            source,
        }
    }

    /// Reads the file as it stands, entries this build cannot use included.
    ///
    /// A missing file is the first run, not a failure.
    fn read(&self) -> Result<BTreeMap<String, Value>, AuthError> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(source) => return Err(self.io("could not be inspected", source)),
        };
        check_private(&self.path, &metadata)?;

        let bytes = fs::read(&self.path).map_err(|source| self.io("could not be read", source))?;

        serde_json::from_slice(&bytes).map_err(|error| AuthError::malformed(&self.path, &error))
    }

    /// Replaces the file's contents.
    ///
    /// The bytes land in a sibling file that is renamed into place, so an
    /// interrupted write cannot leave a truncated store behind — losing every
    /// stored key to a crash would be a worse bug than any this method has.
    fn write(&self, data: &BTreeMap<String, Value>) -> Result<(), AuthError> {
        let parent = self.path.parent().ok_or_else(|| {
            self.io(
                "has no directory to be created in",
                io::Error::from(io::ErrorKind::NotFound),
            )
        })?;
        fs::create_dir_all(parent).map_err(|source| AuthError::Io {
            context: format!("{} could not be created", parent.display()),
            source,
        })?;

        let mut json = serde_json::to_vec_pretty(data)
            .map_err(|error| AuthError::malformed(&self.path, &error))?;
        json.push(b'\n');

        let temporary = self
            .path
            .with_file_name(format!("{FILE}.{}.tmp", std::process::id()));
        write_private(&temporary, &json).map_err(|source| AuthError::Io {
            context: format!("{} could not be written", temporary.display()),
            source,
        })?;

        fs::rename(&temporary, &self.path).map_err(|source| {
            // A rename that fails leaves the temporary file holding a copy of
            // every key, which is exactly what must not be left lying around.
            let _ = fs::remove_file(&temporary);
            self.io("could not be replaced", source)
        })
    }

    fn get(&self, provider_id: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self
            .read()?
            .get(provider_id)
            .and_then(usable_key)
            .map(|api_key| Credential { api_key }))
    }

    fn set(&self, provider_id: &str, api_key: &str) -> Result<(), AuthError> {
        let mut data = self.read()?;
        data.insert(
            provider_id.to_owned(),
            serde_json::json!({ "type": "api", "key": api_key }),
        );

        self.write(&data)
    }

    fn remove(&self, provider_id: &str) -> Result<bool, AuthError> {
        let mut data = self.read()?;
        if data.remove(provider_id).is_none() {
            return Ok(false);
        }
        self.write(&data)?;

        Ok(true)
    }

    /// Every stored provider this build could authenticate with, sorted.
    fn stored(&self) -> Result<Vec<(String, RedactedTail)>, AuthError> {
        Ok(self
            .read()?
            .iter()
            .filter_map(|(provider_id, value)| {
                usable_key(value).map(|key| (provider_id.clone(), RedactedTail::of(&key)))
            })
            .collect())
    }
}

/// The API key an entry carries, when it carries one this build can use.
///
/// An entry storing an empty key is treated as absent rather than as a
/// credential that will fail at the provider with a confusing message.
fn usable_key(value: &Value) -> Option<String> {
    match serde_json::from_value::<Stored>(value.clone()) {
        // An entry that does not decode at all is somebody else's — upstream
        // filters the same way rather than failing the whole read.
        Ok(Stored::Api { key }) if !key.trim().is_empty() => Some(key),
        _ => None,
    }
}

/// Rejects a file anyone but its owner can read.
#[cfg(unix)]
fn check_private(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    let mode = metadata.permissions().mode() & 0o777;

    if mode & SHARED == 0 {
        return Ok(());
    }

    Err(AuthError::Permissions {
        path: path.to_path_buf(),
        mode,
    })
}

/// Windows has no mode bits to check; its ACLs are a P7 problem.
#[cfg(not(unix))]
fn check_private(_path: &Path, _metadata: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

/// Writes `bytes` to a file only its owner can read, creating or truncating it.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

    // The mode is set at creation rather than afterwards so that the file is
    // never, even briefly, readable by anyone else.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(PRIVATE)
        .open(path)?;
    // An existing temporary file keeps its old mode, so this fixes one up.
    file.set_permissions(fs::Permissions::from_mode(PRIVATE))?;
    file.write_all(bytes)?;

    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

/// The API key `provider_id`'s environment variable carries.
///
/// Surrounding whitespace is trimmed, because a key read out of a file with
/// `$(cat …)` arrives with a newline that would corrupt the request header. An
/// exported-but-empty variable reads as unset: that is how a shell says "not
/// for this command", and it must not shadow a stored key.
fn key_from_env(provider_id: &str) -> Option<String> {
    let value = env::var(key_var(provider_id)?).ok()?;
    let trimmed = value.trim();

    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Where credentials are stored.
///
/// # Errors
///
/// Returns [`AuthError::Io`] when there is no home directory to resolve the
/// path against.
pub fn store_path() -> Result<PathBuf, AuthError> {
    Ok(Store::open()?.path)
}

/// The credential to authenticate `provider_id` with, if there is one.
///
/// The environment is consulted first; only then the stored file.
///
/// # Errors
///
/// Returns [`AuthError`] when the stored file exists but cannot be read,
/// cannot be understood, or is readable by other users. A provider with no
/// credential at all is [`Ok(None)`], not an error: choosing what to say about
/// it belongs to the caller.
pub fn credential_for(provider_id: &str) -> Result<Option<Credential>, AuthError> {
    if let Some(api_key) = key_from_env(provider_id) {
        return Ok(Some(Credential { api_key }));
    }

    Store::open()?.get(provider_id)
}

/// Stores `api_key` as `provider_id`'s credential, replacing any it had.
///
/// Credentials belonging to providers this build does not know are left as they
/// were.
///
/// # Errors
///
/// Returns [`AuthError`] when the existing file cannot be read or the new one
/// cannot be written.
pub fn set_credential(provider_id: &str, api_key: &str) -> Result<(), AuthError> {
    Store::open()?.set(provider_id, api_key)
}

/// Forgets `provider_id`'s stored credential, reporting whether there was one.
///
/// An environment variable is not this function's to clear, so a provider
/// authenticated that way keeps working; [`list_providers`] shows where a
/// credential is coming from.
///
/// # Errors
///
/// Returns [`AuthError`] when the file cannot be read or rewritten.
pub fn remove_credential(provider_id: &str) -> Result<bool, AuthError> {
    Store::open()?.remove(provider_id)
}

/// Every provider that has a credential, and where [`credential_for`] finds it.
///
/// A provider whose environment variable is set appears once, as
/// [`Source::Environment`], because that is the credential that would be used.
///
/// # Errors
///
/// Returns [`AuthError`] when the stored file cannot be read.
pub fn list_providers() -> Result<Vec<Entry>, AuthError> {
    let mut entries: Vec<Entry> = KEY_VARS
        .iter()
        .filter_map(|(provider_id, variable)| {
            key_from_env(provider_id).map(|key| Entry {
                provider_id: (*provider_id).to_owned(),
                tail: RedactedTail::of(&key),
                source: Source::Environment(variable),
            })
        })
        .collect();

    for (provider_id, tail) in Store::open()?.stored()? {
        if entries.iter().any(|entry| entry.provider_id == provider_id) {
            continue;
        }
        entries.push(Entry {
            provider_id,
            tail,
            source: Source::File,
        });
    }
    entries.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        sync::{Mutex, MutexGuard, PoisonError},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::{
        AuthError, Credential, Entry, KEY_VARS, RedactedTail, Source, Store, credential_for,
        key_var, list_providers, set_credential, store_path,
    };

    /// A key that exists only to be hunted for in output. Nothing may print it
    /// whole.
    const CANARY: &str = "sk-canary-8842";

    /// Serializes the tests that read or write process-wide environment
    /// variables. `cargo test` runs a binary's tests on a thread pool, and
    /// `set_var` is a process-wide mutation: without this, two tests setting
    /// `ANTHROPIC_API_KEY` would see each other's values.
    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    fn environment() -> MutexGuard<'static, ()> {
        // A test that panicked while holding the lock has already failed; the
        // ones after it should still run against a known environment.
        ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets or clears `name` for a test that holds [`environment`].
    fn set_env(name: &str, value: Option<&str>) {
        // SAFETY: every caller holds the ENVIRONMENT lock, so no other test
        // thread is reading or writing the environment concurrently.
        unsafe {
            match value {
                Some(value) => env::set_var(name, value),
                None => env::remove_var(name),
            }
        }
    }

    /// Clears every provider's key variable, so a developer's own exported key
    /// cannot make a test pass or fail.
    fn clear_keys() {
        for (_, variable) in KEY_VARS {
            set_env(variable, None);
        }
    }

    fn store(directory: &TempDir) -> Store {
        Store::new(directory.path().join("auth.json"))
    }

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// Everything an error would print, the way `anyhow` renders one: the
    /// message plus every cause it can be walked down to. A secret hiding in a
    /// `#[source]` is as leaked as one in the message itself.
    fn rendered(error: &AuthError) -> Vec<String> {
        let mut chain = vec![error.to_string()];
        let mut cause = std::error::Error::source(error);

        while let Some(next) = cause {
            chain.push(next.to_string());
            cause = next.source();
        }

        chain
    }

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_missing_store_reads_as_no_credentials_rather_than_an_error() {
        let directory = temporary();
        let store = store(&directory);

        assert_eq!(
            store.get("anthropic").expect("a missing file is fine"),
            None
        );
        assert!(store.stored().expect("a missing file is fine").is_empty());
        assert!(!store.remove("anthropic").expect("a missing file is fine"));
    }

    #[test]
    fn a_stored_key_round_trips_and_can_be_forgotten() {
        let directory = temporary();
        let store = store(&directory);

        store.set("anthropic", CANARY).expect("the key stores");

        assert_eq!(
            store.get("anthropic").expect("the key reads back"),
            Some(Credential {
                api_key: CANARY.to_owned()
            })
        );
        assert_eq!(
            store.stored().expect("the listing reads"),
            vec![("anthropic".to_owned(), RedactedTail::of(CANARY))]
        );

        assert!(store.remove("anthropic").expect("the key is removable"));
        assert_eq!(store.get("anthropic").expect("the file still reads"), None);
    }

    /// The file shape is upstream's, so that the two tools can eventually read
    /// each other's storage.
    #[test]
    fn the_file_is_written_in_upstreams_shape() {
        let directory = temporary();
        let store = store(&directory);
        store.set("openai", CANARY).expect("the key stores");

        let written = fs::read_to_string(&store.path).expect("the file exists");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("the file is JSON");

        assert_eq!(parsed["openai"]["type"], "api");
        assert_eq!(parsed["openai"]["key"], CANARY);
    }

    /// Credentials this build cannot use — upstream's OAuth entries, providers
    /// it has never heard of — survive a rewrite. Dropping them would silently
    /// log someone out of a tool that is still using the same file.
    #[test]
    fn foreign_entries_survive_a_rewrite() {
        let directory = temporary();
        let store = store(&directory);
        let original = serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "refresh-token",
                "access": "access-token",
                "expires": 1,
            },
            "some-future-provider": { "type": "wellknown", "key": "k", "token": "t" },
            "openai": { "type": "api", "key": "sk-old-0001", "metadata": { "label": "work" } },
        });
        fs::write(
            &store.path,
            serde_json::to_vec_pretty(&original).expect("the fixture serializes"),
        )
        .expect("the fixture writes");
        #[cfg(unix)]
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");

        // The OAuth entry is not a usable API key, so it is not offered.
        assert_eq!(store.get("anthropic").expect("the file reads"), None);
        assert_eq!(
            store.stored().expect("the listing reads"),
            vec![("openai".to_owned(), RedactedTail::of("sk-old-0001"))]
        );

        store
            .set("anthropic", CANARY)
            .expect("a new key stores beside them");

        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).expect("the file exists"))
                .expect("the file is still JSON");

        assert_eq!(
            rewritten["some-future-provider"],
            original["some-future-provider"]
        );
        assert_eq!(rewritten["openai"], original["openai"]);
        assert_eq!(rewritten["anthropic"]["type"], "api");
        assert_eq!(
            store.get("anthropic").expect("the new key reads back"),
            Some(Credential {
                api_key: CANARY.to_owned()
            })
        );
    }

    #[test]
    fn an_entry_storing_an_empty_key_is_not_a_credential() {
        let directory = temporary();
        let store = store(&directory);
        store.set("openai", "   ").expect("the entry stores");

        assert_eq!(store.get("openai").expect("the file reads"), None);
        assert!(store.stored().expect("the listing reads").is_empty());
    }

    /// Corruption is reported rather than read as "no credentials", which
    /// would send someone hunting for a key that is sitting right there. The
    /// report itself must not quote the file back, since the file is full of
    /// secrets.
    #[test]
    fn a_file_that_is_not_a_json_object_is_reported_without_quoting_it_back() {
        let corrupt: [&[u8]; 3] = [
            b"{ this is not json",
            // A document whose shape is wrong, carrying a key: the parser sees
            // the secret, and still must not put it in the message.
            br#"["sk-canary-8842"]"#,
            br#""sk-canary-8842""#,
        ];

        for fixture in corrupt {
            let directory = temporary();
            let store = store(&directory);
            fs::write(&store.path, fixture).expect("the fixture writes");
            #[cfg(unix)]
            fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
                .expect("the fixture is made private");

            let error = store.get("anthropic").expect_err("corruption is reported");

            assert!(
                matches!(error, AuthError::Malformed { .. }),
                "got {error:?}"
            );
            for line in rendered(&error) {
                assert!(
                    !line.contains(CANARY),
                    "an error must not carry the file's contents: {line}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_written_store_is_private_to_its_owner() {
        let directory = temporary();
        let store = store(&directory);

        store.set("anthropic", CANARY).expect("the key stores");
        assert_eq!(mode(&store.path), 0o600);

        // A second write goes through the same rename dance and must not
        // loosen anything.
        store.set("openai", CANARY).expect("a second key stores");
        assert_eq!(mode(&store.path), 0o600);
        assert!(
            fs::read_dir(directory.path())
                .expect("the directory lists")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() == "auth.json"),
            "the temporary file should not outlive the write"
        );
    }

    /// A key readable by other users of the machine is already compromised;
    /// reading it anyway would hide that.
    #[cfg(unix)]
    #[test]
    fn a_group_or_world_readable_store_is_refused_with_a_way_out() {
        for exposed in [0o640, 0o604, 0o660, 0o666] {
            let directory = temporary();
            let store = store(&directory);
            store.set("anthropic", CANARY).expect("the key stores");
            fs::set_permissions(&store.path, fs::Permissions::from_mode(exposed))
                .expect("the mode is loosened");

            let error = store.get("anthropic").expect_err("exposure is refused");
            let explanation = error.to_string();

            assert!(
                matches!(error, AuthError::Permissions { mode, .. } if mode == exposed),
                "{exposed:04o} should be refused, got {error:?}"
            );
            assert!(
                explanation.contains("chmod 600"),
                "the way out should be spelled out: {explanation}"
            );
            for line in rendered(&error) {
                assert!(
                    !line.contains(CANARY),
                    "an error must not carry the key: {line}"
                );
            }
        }
    }

    #[test]
    fn nothing_renders_a_whole_key() {
        let credential = Credential {
            api_key: CANARY.to_owned(),
        };
        let tail = credential.tail();
        let entry = Entry {
            provider_id: "anthropic".to_owned(),
            tail: tail.clone(),
            source: Source::Environment("ANTHROPIC_API_KEY"),
        };

        let renderings = [
            format!("{credential:?}"),
            format!("{credential}"),
            format!("{tail:?}"),
            format!("{tail}"),
            format!("{entry:?}"),
            tail.as_str().to_owned(),
        ];

        for rendering in &renderings {
            assert!(
                !rendering.contains(CANARY) && !rendering.contains("sk-canary"),
                "a whole key reached output: {rendering}"
            );
            assert!(
                rendering.contains("8842"),
                "the tail is what identifies a key: {rendering}"
            );
        }

        assert_eq!(tail.as_str(), "****8842");
        // A key too short to have a tail still shows nothing of itself.
        assert_eq!(RedactedTail::of("ab").as_str(), "****ab");
        assert_eq!(RedactedTail::of("").as_str(), "****");
    }

    #[test]
    fn every_shipped_provider_has_a_key_variable_and_others_do_not() {
        assert_eq!(key_var("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(key_var("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(key_var("fake"), None);
    }

    /// The whole precedence chain, against the real XDG resolution: nothing,
    /// then a stored key, then an environment variable that outranks it, then
    /// an empty variable that does not.
    #[test]
    fn the_environment_outranks_the_file_and_an_empty_variable_outranks_nothing() {
        let _guard = environment();
        let directory = temporary();

        clear_keys();
        set_env("XDG_DATA_HOME", Some(&directory.path().to_string_lossy()));

        let expected = directory.path().join("ganja").join("auth.json");
        assert_eq!(store_path().expect("the path resolves"), expected);

        assert_eq!(
            credential_for("anthropic").expect("an empty environment is fine"),
            None
        );
        assert!(list_providers().expect("the listing reads").is_empty());

        set_credential("anthropic", "sk-stored-0001").expect("the key stores");
        assert!(expected.is_file(), "the parent directories are created");
        assert_eq!(
            credential_for("anthropic")
                .expect("the stored key reads")
                .map(|credential| credential.api_key),
            Some("sk-stored-0001".to_owned())
        );
        assert_eq!(
            list_providers().expect("the listing reads"),
            vec![Entry {
                provider_id: "anthropic".to_owned(),
                tail: RedactedTail::of("sk-stored-0001"),
                source: Source::File,
            }]
        );

        set_env("ANTHROPIC_API_KEY", Some(CANARY));
        assert_eq!(
            credential_for("anthropic")
                .expect("the environment reads")
                .map(|credential| credential.api_key),
            Some(CANARY.to_owned()),
            "the environment has to win"
        );
        assert_eq!(
            list_providers()
                .expect("the listing reads")
                .first()
                .map(|entry| entry.source),
            Some(Source::Environment("ANTHROPIC_API_KEY")),
            "the listing has to show the credential actually in use"
        );

        // Whitespace around a key pasted out of a file is trimmed, and an
        // exported-but-empty variable falls through to the stored key.
        set_env("ANTHROPIC_API_KEY", Some("  sk-padded-0002\n"));
        assert_eq!(
            credential_for("anthropic")
                .expect("the environment reads")
                .map(|credential| credential.api_key),
            Some("sk-padded-0002".to_owned())
        );

        set_env("ANTHROPIC_API_KEY", Some("   "));
        assert_eq!(
            credential_for("anthropic")
                .expect("the stored key reads")
                .map(|credential| credential.api_key),
            Some("sk-stored-0001".to_owned()),
            "an empty variable must not shadow a stored key"
        );

        clear_keys();
        set_env("XDG_DATA_HOME", None);
    }
}

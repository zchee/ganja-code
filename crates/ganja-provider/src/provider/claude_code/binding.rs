//! What one ganja conversation remembers about the CLI records it has
//! opened, and the lock that keeps two ganjas from sharing it.
//!
//! # It is never a resume target
//!
//! The binding predates posture C and survived it with one field demoted.
//! `cli_session_id` is kept **for logs** — a person can find the CLI's own
//! `.jsonl` for a record this wire opened — and is read by no argv builder;
//! `--resume` is on `NEVER_ANYWHERE`, so there is no code path that could
//! spend it.
//!
//! What it is *for* is three things the in-memory table cannot do:
//!
//! - `sent` is what `honest(request)` is checked against when no entry is
//!   live, which is what tells a `rewind` (an id in `sent` is gone from the
//!   request) from an `exited`, an `idle-evicted` or an `unsent-history`;
//! - `sent` is also what the exactly-once opening frame is computed from;
//! - `refused` and `refused_streak` persist a reason and a bound across ganja
//!   processes, where the `dropped` ring is one process's memory.
//!
//! # The binding may lag the record by one write, and the direction is chosen
//!
//! The frame is written **before** the binding. A crash between the two
//! leaves the record one message *ahead* of `sent`: the next request finds
//! that id owed and writes it again, a duplicate the model reads twice. The
//! other order turns the same crash into a **loss** — a message the model
//! never saw and that nothing will ever send again — which is why nobody
//! should "fix" this back.
//!
//! # The lock
//!
//! A `<key>.lock` beside the file, held non-blocking for the life of the CLI
//! process by whichever ganja spawned it — `fs::File::lock_shared`'s
//! exclusive sibling, which is `flock(2)` on every platform this builds for,
//! no crate and no invented protocol (`ganja_storage`'s own precedent). A
//! ganja whose `try_lock` fails reads no binding, writes none, and opens a
//! fresh record logged `locked-elsewhere`. The lock file is **never
//! removed**: unlinking one is how a lock file stops working, since the
//! remover and a holder are then no longer locking the same inode.

use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

/// What one conversation's key remembers between processes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The id of the record this key last opened — **for logs only**.
    #[serde(default)]
    pub cli_session_id: String,
    /// The ids of the user messages the wire has written, in order.
    #[serde(default)]
    pub sent: Vec<String>,
    /// Whether the record this describes saw a vendor-safeguard refusal.
    ///
    /// A file written before this field existed reads it as `false`, which is
    /// the right answer: a record nobody recorded a refusal for is one no
    /// refusal was seen on.
    #[serde(default)]
    pub refused: bool,
    /// How many records on this key were refused **in a row**.
    ///
    /// Incremented by the refusal, carried across the fresh record that
    /// replaces it — a record that has not yet been served has not yet shown
    /// the streak is over — and reset to zero only by a served `result`. At
    /// two the next request on the key spawns nothing.
    #[serde(default)]
    pub refused_streak: u8,
}

/// The number of consecutive refused records after which the wire stops
/// spending turns on this key.
///
/// Two: one with the transcript rendered, one with the prompts alone. Past
/// that the wire has tried both shapes it has and a third spend would be the
/// same request on the same classifier, so the turn is failed locally naming
/// both attempts and the two doors — `/compact`, or a new session.
///
/// The bound is **per ganja process on a key, and consecutive**. The
/// `locked-elsewhere` arm reads no binding by design and so sits outside it:
/// a second ganja on a locked conversation spends its own two.
pub const REFUSED_STREAK_BOUND: u8 = 2;

/// Where a key's binding and its lock live.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `<data home>/ganja/claude-code/`.
    root: PathBuf,
}

impl Paths {
    /// The tree under `data_home`.
    #[must_use]
    pub fn under(data_home: &Path) -> Self {
        Self { root: data_home.join("ganja").join("claude-code") }
    }

    /// This build's data home, the way `auth` and `catalog` resolve theirs —
    /// so `XDG_DATA_HOME` redirects it and a test never touches a real one.
    ///
    /// # Errors
    ///
    /// Returns the reason no home could be resolved, which on a machine with
    /// no `HOME` is the honest answer rather than a guess at `/`.
    pub fn resolved() -> Result<Self, String> {
        use etcetera::BaseStrategy as _;

        let strategy = etcetera::choose_base_strategy()
            .map_err(|error| format!("no data home for the claude-code binding: {error}"))?;

        Ok(Self::under(&strategy.data_dir()))
    }

    /// The binding file for `key`.
    #[must_use]
    pub fn binding(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    /// The lock beside it.
    #[must_use]
    pub fn lock(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.lock"))
    }

    /// The empty scratch directory a conversation's child runs in.
    #[must_use]
    pub fn cwd(&self, key: &str) -> PathBuf {
        self.root.join("cwd").join(key)
    }

    /// The one a one-shot's child runs in, shared and left behind empty.
    #[must_use]
    pub fn one_shot_cwd(&self) -> PathBuf {
        self.root.join("cwd").join("one-shot")
    }

    /// The one the `--version` probe runs in: a sibling of `cwd/`, sealed the
    /// same way and left behind empty, so that no child this wire spawns runs
    /// in a directory the wire does not own (CC-10).
    ///
    /// Beside `cwd/` rather than inside it, because what lives under `cwd/` is
    /// named by a conversation's key and removed when the entry closes, and
    /// the probe belongs to no conversation.
    #[must_use]
    pub fn probe_cwd(&self) -> PathBuf {
        self.root.join("probe")
    }

    /// `mkdir -p` at `0700` for `directory` **and for every directory of this
    /// tree above it**.
    ///
    /// Sealing the leaf alone left the intermediates at the process umask —
    /// `0755` under the usual `022` — so whether `claude-code/`, the directory
    /// holding every binding, was private before the first binding landed
    /// depended on which caller happened to run first: a one-shot reaches only
    /// its own scratch leaf and would have left the tree above it readable
    /// until a conversation's binding write sealed it (CC-9). Each component
    /// is made and sealed on the way **down**, so a directory this wire owns
    /// is at the umask for no longer than the moment between its own `mkdir`
    /// and its own `chmod`.
    ///
    /// The data home itself is not this wire's to seal: `<data home>/ganja` is
    /// shared with everything else ganja keeps there.
    ///
    /// # Errors
    ///
    /// Returns the reason a component could not be made or sealed, or
    /// [`io::ErrorKind::InvalidInput`] for a directory outside this tree —
    /// which is a caller bug rather than a filesystem one.
    pub fn create_private(&self, directory: &Path) -> io::Result<()> {
        let under = directory.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not inside {}", directory.display(), self.root.display()),
            )
        })?;

        let mut at = self.root.clone();
        create_private(&at)?;
        for component in under.components() {
            at.push(component);
            create_private(&at)?;
        }

        Ok(())
    }
}

/// The advisory exclusive lock one ganja holds on one key.
///
/// Held for the life of the CLI process and released by dropping — the file
/// handle *is* the lock, so there is nothing to unlock and nothing to unlink.
#[derive(Debug)]
pub struct Lock {
    /// Kept for its lock; never read or written.
    _file: fs::File,
}

impl Lock {
    /// Claims `path`, or reports that somebody else holds it.
    ///
    /// # Errors
    ///
    /// Returns the reason the lock could not be claimed — held elsewhere, or
    /// a directory that could not be made. Both are the same answer to the
    /// caller: read no binding, write none, open a fresh record.
    pub fn claim(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            create_private(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }

        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

/// Reads `path`, treating a missing or unreadable file as "nothing recorded".
///
/// A malformed file is a `warn!` and a [`None`], never a failed turn: what it
/// costs to lose one is a fresh record, and what it would cost to fail on one
/// is a conversation that cannot start.
#[must_use]
pub fn load(path: &Path) -> Option<Binding> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(provider = super::ID, path = %path.display(), %error, "binding unreadable");

            return None;
        }
    };

    match serde_json::from_slice(&bytes) {
        Ok(binding) => Some(binding),
        Err(error) => {
            tracing::warn!(provider = super::ID, path = %path.display(), %error, "binding malformed");

            None
        }
    }
}

/// Writes `binding` to `path`, atomically and owner-only.
///
/// # Errors
///
/// Returns the reason the write failed. A caller logs it and continues: a
/// binding that could not be written costs the next turn a fresh record,
/// which is a cost, not a failure.
pub fn store(path: &Path, binding: &Binding) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_private(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }

    let bytes = serde_json::to_vec(binding).map_err(|error| error.to_string())?;
    let temporary = crate::atomic::temporary_beside(path);

    crate::atomic::write_new(&temporary, &bytes)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    owner_only(&temporary).map_err(|error| format!("{}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);

        format!("{}: {error}", path.display())
    })
}

/// `mkdir -p` at `0700`: a binding names a conversation, and the directory
/// listing alone would say how many a person is holding.
fn create_private(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    private(directory)
}

#[cfg(unix)]
fn private(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private(_directory: &Path) -> io::Result<()> {
    // Windows has no mode bit to set here, and this build ships no windows
    // lane; the directory inherits whatever the data home grants.
    Ok(())
}

#[cfg(unix)]
fn owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn owner_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "binding_tests.rs"]
mod tests;

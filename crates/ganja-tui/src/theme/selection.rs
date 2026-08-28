//! Where a runtime theme pick is remembered.
//!
//! Spec: upstream `packages/tui/src/context/kv.tsx` — a small JSON file under
//! the user's state directory, written atomically, holding what the `/themes`
//! dialog last confirmed. Upstream keeps three keys in it (`theme`,
//! `theme_mode`, `theme_mode_lock`); the two mode keys belong to the terminal
//! detection ganja defers (deviation D3), so only the pick is stored.
//!
//! The file is versioned the way the session store is, and read with the same
//! rule: a version this build does not know is left alone rather than
//! overwritten, so downgrading does not destroy what a newer build wrote.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

use etcetera::BaseStrategy as _;
use etcetera::base_strategy::Xdg;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The shape this build writes and reads.
pub const VERSION: u32 = 1;

/// Directory under the XDG data directory, shared with the session store and
/// the credential file.
const DIRECTORY: &str = "ganja";

/// The file itself. Upstream's name for the same thing.
const FILE: &str = "tui.json";

/// Distinguishes the temporary files two writes in one process create.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// What a pick can fail at.
#[derive(Debug, Error)]
pub enum SelectionError {
    #[error("the directory holding the interface's own state could not be located")]
    Unlocatable,
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}: the pick could not be encoded: {source}", path.display())]
    Encode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// The stored pick.
#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    version: u32,
    theme: String,
}

/// `<XDG data>/ganja/tui.json`, or [`None`] when there is no home directory to
/// resolve it against.
pub fn path() -> Option<PathBuf> {
    let base = Xdg::new().ok()?;

    Some(base.data_dir().join(DIRECTORY).join(FILE))
}

/// The theme name stored at `path`, or [`None`] when there is none to read.
///
/// Every failure answers [`None`]: a pick that cannot be read is a preference
/// lost, not a reason to refuse to start.
pub fn read(path: &Path) -> Option<String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the stored theme could not be read");
            return None;
        }
    };

    let stored: Stored = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the stored theme was not readable");
            return None;
        }
    };

    if stored.version != VERSION {
        tracing::warn!(
            path = %path.display(),
            version = stored.version,
            "the stored theme was written by a newer build and was left alone"
        );
        return None;
    }

    Some(stored.theme)
}

/// Writes `theme` to `path`, replacing whatever was there.
///
/// The write goes to a sibling and is renamed over the target, so a run that
/// dies mid-write leaves the previous pick intact rather than a truncated file
/// the next run has to guess at.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the file cannot be
/// written.
pub fn write(path: &Path, theme: &str) -> Result<(), SelectionError> {
    let parent = path.parent().ok_or_else(|| SelectionError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::NotFound,
            "the file has no directory to be created in",
        ),
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| SelectionError::Io { path: parent.to_path_buf(), source })?;

    let mut json = serde_json::to_vec(&Stored { version: VERSION, theme: theme.to_owned() })
        .map_err(|source| SelectionError::Encode { path: path.to_path_buf(), source })?;
    json.push(b'\n');

    let temporary = temporary_beside(path);
    ganja_permission::write_new(&temporary, &json)
        .map_err(|source| SelectionError::Io { path: temporary.clone(), source })?;

    fs::rename(&temporary, path).map_err(|source| {
        // A rename that failed leaves the sibling holding a copy of a pick
        // nobody asked to keep.
        let _ = fs::remove_file(&temporary);
        SelectionError::Io { path: path.to_path_buf(), source }
    })
}

/// The sibling `path` is written through.
///
/// Beside the target so the rename stays on one filesystem, and carrying an
/// extension nothing reads so a dead write cannot be mistaken for a pick.
fn temporary_beside(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    path.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;

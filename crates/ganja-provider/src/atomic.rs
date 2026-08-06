//! Replacing a file by writing a sibling and renaming it into place.
//!
//! **A recorded duplication, and the house pattern for it.** These two
//! functions exist a third time in `ganja_permission::permission` and a fourth
//! in `ganja_tui::theme::selection`, because a crate that writes exactly one
//! kind of file keeps its own thirty lines rather than reaching sideways for
//! them. They arrived here from `ganja_core::storage`, whose copy went away
//! with its only caller: the catalog was the last thing in the engine that
//! wrote a file through them, and it moved.
//!
//! A `ganja-provider` → `ganja-core` edge would invert the boundary this crate
//! exists to draw, for thirty lines. A crate holding only these would earn less
//! than it costs at four callers; `tempfile` was considered and rejected on the
//! same arithmetic — admitting a dependency to replace two trivial functions is
//! the larger of the two prices. If a fifth copy is ever wanted, that is the
//! moment the shared home starts paying, and all of them should collapse into
//! it at once.
//!
//! Until then: a change to one of these copies is a change to all of them.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Keeps one write's temporary file apart from another's inside this process.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// The sibling `path` is written through.
///
/// It sits beside the target so the rename stays within one filesystem, and it
/// carries an extension no listing reads, so a write that dies before its
/// rename cannot be mistaken for stored data.
pub(crate) fn temporary_beside(path: &Path) -> PathBuf {
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
/// wherever it led and then rename that file over the stored data.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

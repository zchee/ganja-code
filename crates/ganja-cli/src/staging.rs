//! The one staging step three config writers share.
//!
//! A module of its own rather than a corner of one of them: `migrate.rs` and
//! `mcp.rs` each held a copy, and the copy is the half neither file owns — the
//! *rename* that publishes the bytes stays with each writer, because what they
//! publish differs (a printed [`toml_edit::DocumentMut`] against a rendered
//! string, with and without a trailing newline, over `persist` against
//! `persist_noclobber`), while staging a file beside a path differs in nothing
//! at all.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use tempfile::NamedTempFile;

/// Writes `bytes` to a fresh file beside `path`, and hands back the file
/// itself — unnamed by anything the caller has to remember to clean up.
///
/// Staged *beside* `path` rather than at a fixed name because two processes
/// writing one directory would otherwise write the same staging file and one
/// would rename the other's half-written bytes into place. The uniqueness used
/// to be a `create_new` loop over a pid-stamped name, which was correct about
/// collisions and silent about failure: a write that failed returned early and
/// left the staged file where it fell, since only the caller's *rename* arm
/// ever cleaned up. Tying the temporary's life to a value fixes both halves at
/// once — the file is removed when this is dropped, on every path out of every
/// caller, including the one nobody wrote.
pub(crate) fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));

    // Every sentence names `path` and not the staged file: the temporary's
    // name is this function's business and never something somebody typed, so
    // a person reading the failure is told about the config file they asked
    // to write.
    let mut staged = NamedTempFile::new_in(directory)
        .with_context(|| format!("{} could not be written", path.display()))?;
    staged.write_all(bytes).with_context(|| format!("{} could not be written", path.display()))?;
    // The rename publishes the file as complete, so its bytes must reach the
    // backing store before that atomic namespace change makes them current.
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("{} could not be written", path.display()))?;

    // A temporary is created `0600`, and a rename carries that mode onto the
    // target — so an existing config would quietly lose whatever mode its
    // owner gave it. Copying the mode across keeps an edit an edit; a file
    // this *creates* keeps the `0600`, which is the safer default for a
    // document whose remote entries carry `Authorization` headers.
    if let Ok(existing) = fs::symlink_metadata(path)
        && existing.file_type().is_file()
    {
        staged
            .as_file()
            .set_permissions(existing.permissions())
            .with_context(|| format!("{} could not be written", path.display()))?;
    }

    Ok(staged)
}

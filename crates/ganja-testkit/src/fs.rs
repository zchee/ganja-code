//! Filesystem fixtures every suite that seeds a real directory tree reaches
//! for: a scratch directory, and the `XDG_DATA_HOME` redirect a project's
//! stored state resolves beneath.

use tempfile::TempDir;

/// A fresh temporary directory, deleted when the handle drops.
///
/// ```
/// let dir = ganja_testkit::temp_dir();
/// assert!(dir.path().is_dir());
/// ```
pub fn temp_dir() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// Points `XDG_DATA_HOME` at a fresh temporary directory and returns its
/// handle, so a project's resolved data directory — and anything a test
/// stores there — lives under it instead of the real user's.
///
/// # Safety
///
/// Mutates process-wide environment. The caller must uphold the same
/// invariant the inlined version at every call site did: call this before
/// any other thread in the process has started, which in practice means it
/// belongs in a test binary that holds exactly one test. `nextest` gives
/// every test its own process, but a plain `cargo test` runs a binary's
/// tests on parallel threads, and this crosses them all.
pub unsafe fn redirect_xdg_data_home() -> TempDir {
    let home = temp_dir();
    // SAFETY: the caller upholds the invariant documented above.
    unsafe { std::env::set_var("XDG_DATA_HOME", home.path()) };

    home
}

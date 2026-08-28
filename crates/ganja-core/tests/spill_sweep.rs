//! The spill sweep over the directories it really resolves.
//!
//! One test, in its own binary, because it redirects `XDG_DATA_HOME` and
//! `TMPDIR` for the whole process — which is the only way to watch a sweep
//! visit *both* of the candidates a clamp would have spilled to without
//! reaching into the real user's data directory.
//!
//! The module's own tests cover which files a sweep picks. This one covers
//! where it goes looking, since that is resolved from the environment and
//! cannot be asserted from inside the module.

use std::time::{Duration, SystemTime};

use ganja_core::tool::truncate;

/// Comfortably past the week a spill is kept.
const STALE: Duration = Duration::from_secs(8 * 24 * 60 * 60);

#[test]
fn a_sweep_visits_both_of_the_directories_a_clamp_would_have_spilled_to() {
    let data = tempfile::tempdir().expect("a temporary data home");
    let temp = tempfile::tempdir().expect("a temporary temp directory");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
        std::env::set_var("TMPDIR", temp.path());
    }

    // Spelled the way `truncate` spells them, which is what makes a divergence
    // between the two visible here rather than in a user's home directory.
    let dirs = [
        data.path().join("ganja").join("tool-output"),
        std::env::temp_dir().join("ganja").join("tool-output"),
    ];
    assert_ne!(dirs[0], dirs[1], "the two candidates must be two places");
    for dir in &dirs {
        std::fs::create_dir_all(dir).expect("the fixture makes a spill directory");
        plant(&dir.join("tool_stale"), STALE);
        plant(&dir.join("tool_fresh"), Duration::ZERO);
        plant(&dir.join("notes.txt"), STALE);
    }

    assert_eq!(truncate::sweep(), 2, "one stale spill in each of the two candidate directories");

    for dir in &dirs {
        assert!(
            !dir.join("tool_stale").exists(),
            "{}: a week-old spill is nobody's context any more",
            dir.display()
        );
        assert!(
            dir.join("tool_fresh").exists(),
            "{}: a fresh spill may still be the file a model was told to read",
            dir.display()
        );
        assert!(
            dir.join("notes.txt").exists(),
            "{}: a sweep only ever deletes what this crate wrote",
            dir.display()
        );
    }
}

/// Writes a file and backdates it by `age`.
fn plant(path: &std::path::Path, age: Duration) {
    std::fs::write(path, "spilled").expect("the fixture writes");
    let when = SystemTime::now().checked_sub(age).expect("a representable stamp");
    // Opened for writing because a stamp is metadata a handle must be allowed
    // to write: unix grants that with the file's own permissions, Windows only
    // through a handle that asked for write access.
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(when))
        .expect("the fixture can move the stamp");
}

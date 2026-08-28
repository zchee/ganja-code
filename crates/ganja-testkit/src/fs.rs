//! Filesystem fixtures every suite that seeds a real directory tree reaches
//! for: a scratch directory, the `XDG_DATA_HOME` redirect a project's
//! stored state resolves beneath, and the project/data-home pair the suites
//! that drive the shipped binary build first.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Writes `text` to `root/relative`, creating whatever directories it
/// needs — the seeding stroke every fixture-tree test makes.
pub fn plant(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
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

/// The project a run of the shipped binary works in and the data home it
/// stores under, both gone with the test.
///
/// The pair is one fixture because it only means anything together: the
/// project's checkout marker pins which store a run opens, and the data home
/// is where that store then lands. What varies per suite — the script the
/// fake provider plays, the environment a child is given — stays in the
/// suite, over [`Homes::script`] and [`Homes::pin`].
pub struct Homes {
    project: TempDir,
    data: TempDir,
}

impl Homes {
    /// A project (with its checkout marker) and a data home.
    #[must_use]
    pub fn new() -> Self {
        let project = temp_dir();
        // The checkout marker pins the project — and so the one store every
        // process opens — to this directory rather than to whatever the
        // temporary directory happens to sit inside.
        fs::create_dir(project.path().join(".git")).expect("the checkout marker is creatable");

        Self { project, data: temp_dir() }
    }

    /// The project directory a run works in.
    #[must_use]
    pub fn project(&self) -> &Path {
        self.project.path()
    }

    /// The data home a run stores under.
    #[must_use]
    pub fn data(&self) -> &Path {
        self.data.path()
    }

    /// The config home a run pinned by [`Homes::pin`] resolves —
    /// `$XDG_CONFIG_HOME/ganja` under this fixture's data home — and so
    /// where a lead's team is kept.
    #[must_use]
    pub fn config_home(&self) -> PathBuf {
        self.data.path().join("config").join("ganja")
    }

    /// Writes a fake-provider script under the project and answers its path.
    pub fn script(&self, name: &str, turns: serde_json::Value) -> PathBuf {
        let path = self.project.path().join(name);
        fs::write(&path, serde_json::json!({"cadence_ms": 1, "turns": turns}).to_string())
            .expect("the script is writable");

        path
    }

    /// The store the pinned runs wrote into.
    ///
    /// Found rather than computed: the project directory's name is
    /// `ganja-permission`'s to decide, and that there is exactly one of them
    /// is itself worth asserting — a run that stored under a second project
    /// stored somewhere the binary will never look.
    #[must_use]
    pub fn store(&self) -> ganja_core::Storage {
        let mut roots: Vec<PathBuf> = fs::read_dir(self.data.path().join("ganja").join("project"))
            .expect("a run created a project directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        roots.sort();
        assert_eq!(roots.len(), 1, "one working directory is one project, got {roots:?}");

        ganja_core::Storage::open(roots.remove(0).join("storage"))
    }

    /// Pins everything that could decide what a run of the shipped binary
    /// does onto this fixture's own directories: a developer's global config
    /// can choose a provider, and their cached catalog can decide what a
    /// model is sized at, so all of it moves or none of it has moved.
    pub fn pin(&self, command: &mut Command, script: &Path) {
        command
            .current_dir(self.project.path())
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_FAKE_SCRIPT", script)
            .env("XDG_DATA_HOME", self.data.path())
            // The three variables that decide where ganja's *global* home
            // lands, moved together — an empty pinned `XDG_CONFIG_HOME`
            // falls through to `~/.ganja` via `HOME`.
            .env("HOME", self.data.path())
            .env("XDG_CONFIG_HOME", self.data.path().join("config"))
            .env("XDG_CACHE_HOME", self.data.path().join("cache"))
            // A pinned run must not become a catalog fetch: the compiled-in
            // snapshot answers, and nothing measures the network.
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env_remove("GANJA_CONFIG_HOME")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY");
    }
}

impl Default for Homes {
    fn default() -> Self {
        Self::new()
    }
}

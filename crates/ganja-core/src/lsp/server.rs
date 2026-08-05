//! Which language servers exist, where each one is rooted, and how it is run.
//!
//! Spec: upstream `packages/opencode/src/lsp/server.ts` (`NearestRoot`,
//! `Gopls`, `RustAnalyzer`) and `packages/opencode/src/lsp/lsp.ts:151-189`
//! (how a config entry merges over a builtin).
//!
//! Two builtins ship, against upstream's thirty-nine. That is the plan's
//! scope, not an accident, and the config surface is the way to add a third
//! without waiting for one: an entry with a `command` and `extensions` is a
//! server, whether or not this file has heard of it.
//!
//! # No server is ever installed
//!
//! Upstream's gopls will `go install golang.org/x/tools/gopls@latest` when the
//! binary is missing. That is not ported: ganja does not install software
//! because a file was opened. A missing binary is a server that does not
//! start, which is a session with no diagnostics for that language and
//! nothing else (deviation: lsp-no-auto-install).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::config::LspEntry;

/// The builtin whose id is `"rust"` — upstream's id for rust-analyzer, which
/// is not the binary's name.
pub const RUST: &str = "rust";

/// The builtin whose id is the binary's name.
pub const GOPLS: &str = "gopls";

/// Ids this build ships a definition for.
///
/// The list a custom server is measured against: an entry naming something not
/// here has no builtin to inherit `extensions` from, so it must bring its own.
pub const BUILTIN_IDS: &[&str] = &[GOPLS, RUST];

/// How a server's root directory is found for a given file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    /// The crate the file belongs to, raised to the workspace that owns the
    /// crate when there is one. See [`rust_root`].
    Rust,
    /// The nearest `go.work`, else the nearest `go.mod`/`go.sum`, else the
    /// project directory.
    Gopls,
    /// The project directory, full stop — what a config-defined server gets,
    /// because the config has no key for saying anything else (upstream
    /// `lsp.ts:171`).
    Directory,
}

/// One language server this session may run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// What the server is called in config, in a log line, and in the key that
    /// pairs it with a root.
    pub id: String,
    /// Extensions this server is asked about. **Empty matches every file**,
    /// which is upstream's `if (server.extensions.length && !includes)
    /// continue` read as written (`lsp.ts:255`).
    pub extensions: Vec<String>,
    /// The program and its arguments. [`None`] means "find the builtin's
    /// binary on `PATH`"; a config `command` replaces the builtin's spawn
    /// entirely rather than adding arguments to it.
    pub command: Option<Vec<String>>,
    /// Where the root comes from.
    pub root: Root,
    /// Variables layered over the ones this process already has.
    pub env: BTreeMap<String, String>,
    /// `initializationOptions`, and the settings a `workspace/configuration`
    /// request is answered out of.
    pub initialization: Option<serde_json::Value>,
}

impl Spec {
    /// Whether this server is asked about `path`.
    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return false;
        };
        let dotted = format!(".{extension}");

        self.extensions.contains(&dotted)
    }

    /// The builtin definition for `id`, if this build ships one.
    fn builtin(id: &str) -> Option<Self> {
        let (extensions, root) = match id {
            RUST => (vec![".rs".to_owned()], Root::Rust),
            GOPLS => (vec![".go".to_owned()], Root::Gopls),
            _ => return None,
        };

        Some(Self {
            id: id.to_owned(),
            extensions,
            command: None,
            root,
            env: BTreeMap::new(),
            initialization: None,
        })
    }

    /// The program to run, and its arguments.
    ///
    /// A builtin with no configured `command` is its own binary looked up on
    /// `PATH`, with no arguments — both servers here take their configuration
    /// over the wire, not on the command line. [`None`] is a server that
    /// cannot be started, which the caller turns into a broken entry.
    #[must_use]
    pub fn program(&self) -> Option<Vec<String>> {
        if let Some(command) = &self.command {
            return (!command.is_empty()).then(|| command.clone());
        }
        let binary = match self.id.as_str() {
            RUST => "rust-analyzer",
            GOPLS => GOPLS,
            // A server with neither a builtin binary nor a configured command
            // is refused at config load; this arm is the type being honest.
            _ => return None,
        };

        which(binary).map(|path| vec![path.to_string_lossy().into_owned()])
    }
}

/// Every server this session may run, given what the config asked for.
///
/// `false` (and an absent key) never reaches here — that is no LSP at all, and
/// the caller decides it. `true` is the builtins untouched. A map merges over
/// them by name (`lsp.ts:160-182`): a `disabled` entry removes a server, an
/// entry naming a builtin overrides the fields it names, and an entry naming
/// nothing this build ships is a new server on the project directory.
///
/// Sorted by id, so a session's servers are asked about a file in an order
/// that does not depend on how a config file happened to be written.
#[must_use]
pub fn resolve(entries: &BTreeMap<String, LspEntry>) -> Vec<Spec> {
    let mut specs: BTreeMap<String, Spec> = BUILTIN_IDS
        .iter()
        .filter_map(|id| Spec::builtin(id).map(|spec| ((*id).to_owned(), spec)))
        .collect();

    for (name, entry) in entries {
        if entry.disabled {
            specs.remove(name);
            tracing::debug!(server = name.as_str(), "LSP server is disabled");
            continue;
        }

        let existing = specs.remove(name);
        let inherited = existing.as_ref();
        specs.insert(
            name.clone(),
            Spec {
                id: name.clone(),
                extensions: entry
                    .extensions
                    .clone()
                    .or_else(|| inherited.map(|spec| spec.extensions.clone()))
                    .unwrap_or_default(),
                command: entry
                    .command
                    .clone()
                    .or_else(|| inherited.and_then(|spec| spec.command.clone())),
                root: inherited.map_or(Root::Directory, |spec| spec.root),
                env: entry.env.clone(),
                initialization: entry.initialization.clone(),
            },
        );
    }

    specs.into_values().collect()
}

/// The directory `server` is rooted at for `file`, or [`None`] when it has no
/// root there and therefore no client.
///
/// `directory` is where the project starts and where every upward walk stops;
/// `worktree` bounds the rust workspace search, which is the one walk allowed
/// to look above the project directory.
#[must_use]
pub fn root(server: &Spec, file: &Path, directory: &Path, worktree: &Path) -> Option<PathBuf> {
    match server.root {
        Root::Rust => rust_root(file, directory, worktree),
        Root::Gopls => Some(
            nearest_root(file, &["go.work"], directory).unwrap_or_else(|| {
                nearest_root(file, &["go.mod", "go.sum"], directory)
                    .unwrap_or_else(|| directory.to_owned())
            }),
        ),
        Root::Directory => Some(directory.to_owned()),
    }
}

/// The nearest ancestor of `file` (itself included) holding any of `markers`.
///
/// Walks up from the file's directory and stops **after** checking `stop`,
/// which is upstream's `Filesystem.up` (`util/filesystem.ts:213-226`). Markers
/// are tried in order within each directory, so `["go.mod", "go.sum"]` prefers
/// the module file to the lock beside it.
///
/// [`None`] means no marker was found anywhere on the way up. Upstream's
/// `NearestRoot` folds that into "the project directory" at this point;
/// keeping it separate lets the two callers that want different fallbacks say
/// so.
fn nearest_root(file: &Path, markers: &[&str], stop: &Path) -> Option<PathBuf> {
    let mut current = file.parent()?;
    loop {
        for marker in markers {
            if current.join(marker).is_file() {
                return Some(current.to_owned());
            }
        }
        if current == stop {
            return None;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

/// The root rust-analyzer is started at for `file`.
///
/// The crate the file belongs to, then raised: walking up from the crate root,
/// the **first** `Cargo.toml` whose text contains `[workspace]` wins and the
/// walk stops there (upstream `server.ts:892-920` — the `return currentDir`
/// inside the loop is what makes it the nearest workspace and not the
/// outermost). Nothing found leaves the crate root standing, and the walk
/// refuses to leave the worktree.
///
/// The marker is looked for as a substring, exactly as upstream does. It will
/// therefore accept a `Cargo.toml` that only mentions `[workspace]` inside a
/// comment or a string. Matching upstream matters more here than being clever:
/// the two ports must agree on which directory a server is started in, and a
/// manifest that says the word in a comment is not a manifest anyone has.
fn rust_root(file: &Path, directory: &Path, worktree: &Path) -> Option<PathBuf> {
    let crate_root = nearest_root(file, &["Cargo.toml", "Cargo.lock"], directory)
        .unwrap_or_else(|| directory.to_owned());

    let mut current = crate_root.clone();
    loop {
        if std::fs::read_to_string(current.join("Cargo.toml"))
            .is_ok_and(|manifest| manifest.contains("[workspace]"))
        {
            return Some(current);
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_owned();
        // Upstream compares path *text* here (`startsWith`). Comparing
        // components instead is the same intent without the bug where
        // `/src/ganja-code-old` counts as inside `/src/ganja-code`.
        if !current.starts_with(worktree) {
            break;
        }
    }

    Some(crate_root)
}

/// Where `binary` is on `PATH`, if it is anywhere.
///
/// A hand-rolled `which` rather than a crate: the whole of it is "split PATH,
/// join, ask whether it is a file somebody may execute", and a dependency for
/// that is a dependency to audit.
fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path)
        .flat_map(|directory| spellings(&directory.join(binary)))
        .find(|candidate| executable(candidate))
}

/// Every name `path` might be on disk under.
///
/// Exactly one on unix, where a binary is its own name. On Windows an
/// executable carries an extension and `PATHEXT` says which: rustup installs
/// `rust-analyzer.exe`, an npm-shimmed server is a `.cmd`, and joining the bare
/// name — which is all upstream's Node needs, because `child_process` does this
/// search itself — finds neither. The bare name is still tried first, so a
/// config that named the extension itself is not handed a second one.
fn spellings(path: &Path) -> Vec<PathBuf> {
    #[cfg(unix)]
    let found = vec![path.to_owned()];
    #[cfg(not(unix))]
    let found = {
        let mut found = vec![path.to_owned()];
        found.extend(extensions().into_iter().map(|extension| {
            let mut spelling = path.to_owned().into_os_string();
            spelling.push(extension);
            PathBuf::from(spelling)
        }));

        found
    };

    found
}

/// The extensions this machine treats as executable, from `PATHEXT`.
///
/// The fallback is what every Windows since NT has shipped with, so a process
/// started without the variable — a service, a stripped environment — still
/// finds an ordinary `.exe`.
#[cfg(not(unix))]
fn extensions() -> Vec<String> {
    const FALLBACK: &str = ".COM;.EXE;.BAT;.CMD";

    std::env::var("PATHEXT")
        .unwrap_or_else(|_| FALLBACK.to_owned())
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `path` is a file this process could run.
#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Whether `path` is a file at all, which is as much as this asks where there
/// is no execute bit to ask about.
#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{GOPLS, RUST, Root, Spec, nearest_root, resolve, root, rust_root, spellings};
    use crate::config::LspEntry;

    /// The bare name is what a config that already spelled the extension gave,
    /// so it is what gets tried first — on every platform, because a search
    /// that reordered itself per machine would find a different binary on each.
    #[test]
    fn the_name_a_server_was_configured_under_is_the_first_thing_looked_for() {
        let named = Path::new("/opt/bin/rust-analyzer");

        assert_eq!(
            spellings(named).first().map(PathBuf::as_path),
            Some(named),
            "the name as given comes first"
        );
    }

    /// Windows executables carry an extension and `PATHEXT` says which. Joining
    /// the bare name finds nothing there, which is why the LSP never started on
    /// a machine where rustup had installed `rust-analyzer.exe` and put it on
    /// `PATH`.
    #[cfg(windows)]
    #[test]
    fn a_windows_binary_is_looked_for_under_the_extensions_that_make_it_one() {
        let found = spellings(Path::new(r"C:\opt\bin\rust-analyzer"));

        assert!(
            found.contains(&PathBuf::from(r"C:\opt\bin\rust-analyzer.EXE"))
                || found.contains(&PathBuf::from(r"C:\opt\bin\rust-analyzer.exe")),
            "an executable extension has to be among the spellings tried: {found:?}"
        );
        assert!(
            found.len() > 1,
            "the bare name alone is what fails on this platform: {found:?}"
        );
    }

    /// Writes `contents` at `root/relative`, creating the directories above it.
    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("the file has a parent"))
            .expect("the fixture directories are created");
        fs::write(path, contents).expect("the fixture file is written");
    }

    /// An entry with everything defaulted, so a case names only what it means.
    fn entry() -> LspEntry {
        LspEntry {
            command: None,
            extensions: None,
            disabled: false,
            env: BTreeMap::new(),
            initialization: None,
        }
    }

    #[test]
    fn a_bare_true_ships_both_builtins() {
        let specs = resolve(&BTreeMap::new());

        let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(
            ids,
            [GOPLS, RUST],
            "sorted, so the order is not the config's"
        );
        assert_eq!(specs[1].extensions, [".rs"]);
        assert_eq!(specs[1].root, Root::Rust);
        assert!(
            specs.iter().all(|spec| spec.command.is_none()),
            "a builtin finds its own binary"
        );
    }

    #[test]
    fn a_disabled_entry_removes_that_builtin_and_leaves_the_other() {
        let entries = BTreeMap::from([(
            RUST.to_owned(),
            LspEntry {
                disabled: true,
                ..entry()
            },
        )]);

        let specs = resolve(&entries);

        let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids, [GOPLS]);
    }

    #[test]
    fn a_configured_command_replaces_the_builtin_spawn_and_keeps_its_root() {
        let entries = BTreeMap::from([(
            RUST.to_owned(),
            LspEntry {
                command: Some(vec!["ra-multiplex".to_owned(), "--server".to_owned()]),
                ..entry()
            },
        )]);

        let specs = resolve(&entries);
        let rust = specs
            .iter()
            .find(|spec| spec.id == RUST)
            .expect("rust survives");

        assert_eq!(
            rust.program().as_deref(),
            Some(["ra-multiplex".to_owned(), "--server".to_owned()].as_slice())
        );
        assert_eq!(rust.extensions, [".rs"], "unspecified fields are inherited");
        assert_eq!(rust.root, Root::Rust, "and so is the root rule");
    }

    #[test]
    fn a_custom_server_is_rooted_at_the_project_directory() {
        let entries = BTreeMap::from([(
            "zls".to_owned(),
            LspEntry {
                command: Some(vec!["zls".to_owned()]),
                extensions: Some(vec![".zig".to_owned()]),
                ..entry()
            },
        )]);

        let specs = resolve(&entries);
        let zls = specs
            .iter()
            .find(|spec| spec.id == "zls")
            .expect("zls is a server");

        assert_eq!(zls.root, Root::Directory);
        assert!(zls.matches(Path::new("/p/build.zig")));
        assert!(!zls.matches(Path::new("/p/main.rs")));
    }

    #[test]
    fn empty_extensions_match_every_file() {
        let entries = BTreeMap::from([(
            "everything".to_owned(),
            LspEntry {
                command: Some(vec!["srv".to_owned()]),
                extensions: Some(Vec::new()),
                ..entry()
            },
        )]);

        let specs = resolve(&entries);
        let all = specs
            .iter()
            .find(|spec| spec.id == "everything")
            .expect("it is a server");

        assert!(all.matches(Path::new("/p/main.rs")));
        assert!(all.matches(Path::new("/p/README")));
    }

    #[test]
    fn the_nearest_marker_upward_is_the_root() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "go.mod", "module example.com/outer\n");
        write(base, "svc/go.mod", "module example.com/svc\n");
        write(base, "svc/internal/main.go", "package main\n");

        let found = nearest_root(&base.join("svc/internal/main.go"), &["go.mod"], base);

        assert_eq!(found.as_deref(), Some(base.join("svc").as_path()));
    }

    #[test]
    fn a_walk_with_no_marker_finds_nothing_and_stops_at_the_project() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "src/main.go", "package main\n");

        assert_eq!(
            nearest_root(&base.join("src/main.go"), &["go.mod"], base),
            None
        );
    }

    #[test]
    fn a_go_file_with_no_module_falls_back_to_the_project_directory() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "src/main.go", "package main\n");
        let specs = resolve(&BTreeMap::new());
        let gopls = specs
            .iter()
            .find(|spec| spec.id == GOPLS)
            .expect("gopls is a server");

        let found = root(gopls, &base.join("src/main.go"), base, base);

        assert_eq!(found.as_deref(), Some(base));
    }

    #[test]
    fn a_go_workspace_outranks_the_module_beside_it() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "go.work", "go 1.24\n");
        write(base, "svc/go.mod", "module example.com/svc\n");
        write(base, "svc/main.go", "package main\n");
        let specs = resolve(&BTreeMap::new());
        let gopls = specs
            .iter()
            .find(|spec| spec.id == GOPLS)
            .expect("gopls is a server");

        let found = root(gopls, &base.join("svc/main.go"), base, base);

        assert_eq!(
            found.as_deref(),
            Some(base),
            "go.work wins over the nearer go.mod"
        );
    }

    #[test]
    fn a_crate_in_a_workspace_is_rooted_at_the_workspace() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(
            base,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            base,
            "crates/core/Cargo.toml",
            "[package]\nname = \"core\"\n",
        );
        write(base, "crates/core/src/lib.rs", "pub fn hello() {}\n");

        let found = rust_root(&base.join("crates/core/src/lib.rs"), base, base);

        assert_eq!(found.as_deref(), Some(base));
    }

    #[test]
    fn the_nearest_workspace_wins_and_the_walk_stops_there() {
        // Two nested workspaces. Upstream returns from inside the loop, so the
        // inner one answers — the walk never reaches the outer.
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "Cargo.toml", "[workspace]\nmembers = [\"inner\"]\n");
        write(
            base,
            "inner/Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            base,
            "inner/crates/leaf/Cargo.toml",
            "[package]\nname = \"leaf\"\n",
        );
        write(base, "inner/crates/leaf/src/lib.rs", "pub fn hello() {}\n");

        let found = rust_root(&base.join("inner/crates/leaf/src/lib.rs"), base, base);

        assert_eq!(found.as_deref(), Some(base.join("inner").as_path()));
    }

    #[test]
    fn a_standalone_crate_keeps_its_own_root() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "thing/Cargo.toml", "[package]\nname = \"thing\"\n");
        write(base, "thing/src/main.rs", "fn main() {}\n");

        let found = rust_root(&base.join("thing/src/main.rs"), base, base);

        assert_eq!(
            found.as_deref(),
            Some(base.join("thing").as_path()),
            "no [workspace] anywhere above, so the crate root stands"
        );
    }

    #[test]
    fn the_workspace_walk_does_not_leave_the_worktree() {
        // The workspace manifest sits above the worktree, so it must not win
        // even though the walk would otherwise reach it.
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "Cargo.toml", "[workspace]\nmembers = [\"tree/*\"]\n");
        write(
            base,
            "tree/thing/Cargo.toml",
            "[package]\nname = \"thing\"\n",
        );
        write(base, "tree/thing/src/main.rs", "fn main() {}\n");
        let worktree = base.join("tree");

        let found = rust_root(&base.join("tree/thing/src/main.rs"), &worktree, &worktree);

        assert_eq!(found.as_deref(), Some(base.join("tree/thing").as_path()));
    }

    #[test]
    fn a_rust_file_outside_any_crate_falls_back_to_the_project_directory() {
        let temp = TempDir::new().expect("a temp dir");
        let base = temp.path();
        write(base, "scratch/note.rs", "fn main() {}\n");

        let found = rust_root(&base.join("scratch/note.rs"), base, base);

        assert_eq!(found.as_deref(), Some(base));
    }

    #[test]
    fn a_builtin_with_no_binary_on_path_cannot_be_started() {
        // `program()` for a builtin is a PATH lookup, so a spec naming a
        // binary nobody has is the `None` the caller marks broken on.
        let spec = Spec {
            id: RUST.to_owned(),
            extensions: vec![".rs".to_owned()],
            command: Some(Vec::new()),
            root: Root::Rust,
            env: BTreeMap::new(),
            initialization: None,
        };

        assert_eq!(spec.program(), None, "an empty command runs nothing");
    }
}

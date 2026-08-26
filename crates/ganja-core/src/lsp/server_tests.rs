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

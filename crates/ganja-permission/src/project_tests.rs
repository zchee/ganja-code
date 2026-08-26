use std::{fs, path::Path};

use tempfile::TempDir;

use super::{MAX, Project, base36, digest32, slug_for};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

#[test]
fn a_directory_inside_a_checkout_resolves_to_the_checkout() {
    let directory = temporary();
    let root = directory.path().join("api");
    let nested = root.join("crates").join("core").join("src");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");

    let outer = Project::resolve(&root);
    let inner = Project::resolve(&nested);

    assert_eq!(inner.root(), outer.root());
    assert_eq!(inner.slug(), outer.slug());
    assert!(inner.slug().ends_with("-api"), "{}", inner.slug());
}

/// A linked worktree and a submodule both mark their root with a `.git`
/// file rather than a directory, and both are working trees.
#[test]
fn a_git_file_marks_a_root_just_as_a_git_directory_does() {
    let directory = temporary();
    let root = directory.path().join("worktree");
    let nested = root.join("src");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/w")
        .expect("the fixture marker is writable");

    assert_eq!(
        Project::resolve(&nested).root(),
        Project::resolve(&root).root()
    );
}

#[test]
fn a_directory_outside_any_checkout_is_its_own_project() {
    let directory = temporary();
    let loose = directory.path().join("loose");
    fs::create_dir(&loose).expect("the fixture directory is creatable");

    assert_eq!(
        Project::resolve(&loose).root(),
        fs::canonicalize(&loose).expect("the fixture exists")
    );
}

#[test]
fn the_same_path_always_slugs_the_same_and_different_paths_do_not() {
    let directory = temporary();
    let left = directory.path().join("work").join("api");
    let right = directory.path().join("play").join("api");
    fs::create_dir_all(&left).expect("the fixture tree is creatable");
    fs::create_dir_all(&right).expect("the fixture tree is creatable");

    assert_eq!(
        Project::resolve(&left).slug(),
        Project::resolve(&left).slug(),
        "the same path has to keep its stored state"
    );
    assert_ne!(
        Project::resolve(&left).slug(),
        Project::resolve(&right).slug(),
        "projects that share a name must not share their state"
    );

    // And the slug is the path, which is the point of the scheme: the
    // separators are dashes and every other character is where it was.
    let slug = Project::resolve(&left).slug().to_owned();
    assert!(slug.ends_with("-work-api"), "{slug}");
    assert!(
        slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "{slug}"
    );
}

/// A path that reaches the same directory by a different route is the same
/// project, or a rule remembered through one route would not apply through
/// the other.
#[test]
fn an_untidy_path_resolves_to_the_same_project() {
    let directory = temporary();
    let root = directory.path().join("api");
    fs::create_dir(&root).expect("the fixture directory is creatable");
    let untidy = directory
        .path()
        .join("api")
        .join(".")
        .join("..")
        .join("api");

    assert_eq!(
        Project::resolve(&untidy).slug(),
        Project::resolve(&root).slug()
    );
}

#[test]
fn a_name_that_is_not_path_safe_is_reduced_rather_than_refused() {
    let directory = temporary();
    let awkward = directory.path().join("My Project (v2)!");
    fs::create_dir(&awkward).expect("the fixture directory is creatable");

    let slug = Project::resolve(&awkward).slug().to_owned();

    // Case survives where it can, and nothing else does: one dash per
    // character rather than one per run, so the trailing `)!` leaves two.
    assert!(slug.ends_with("-My-Project--v2--"), "{slug}");
}

#[test]
fn the_filesystem_root_still_gets_a_name() {
    assert_eq!(Project::resolve(Path::new("/")).slug(), "-");
}

/// The expected value is a directory name Claude Code really wrote, so
/// this is the pin that says the two schemes are one scheme. `slug_for`
/// rather than `Project::resolve` because resolving canonicalises first,
/// and what is under test is the reduction rather than the walk feeding
/// it.
#[test]
fn a_path_reduces_to_the_name_claude_code_gives_it() {
    assert_eq!(
        slug_for(Path::new(
            "/Users/zchee/rust/src/github.com/zchee/ganja-code"
        )),
        "-Users-zchee-rust-src-github-com-zchee-ganja-code"
    );
}

/// A path with no room left in a filename is cut at the cap and given a
/// hash, which is the only thing keeping two long paths that share a
/// prefix apart.
#[test]
fn a_path_too_long_for_a_filename_is_cut_and_hashed() {
    let deep = format!("/{}", "a".repeat(MAX));
    let deeper = format!("/{}", "a".repeat(MAX + 1));

    let slug = slug_for(Path::new(&deep));
    let (head, tail) = slug.split_at(MAX);

    assert_eq!(head, format!("-{}", "a".repeat(MAX - 1)));
    assert!(
        tail.starts_with('-') && tail.len() > 1,
        "a cut slug has to carry a hash: {slug}"
    );

    let sibling = slug_for(Path::new(&deeper));
    assert_eq!(sibling[..MAX], slug[..MAX], "the fixtures share their cut");
    assert_ne!(sibling, slug, "the hash is what tells them apart");
}

/// Both halves of the suffix are JavaScript's, and both are pinned because
/// a directory name depends on them meaning the same thing forever.
#[test]
fn the_hash_and_its_digits_are_the_ones_javascript_renders() {
    // Java's `String.hashCode`, which is what the JavaScript spells out.
    assert_eq!(digest32(""), 0);
    assert_eq!(digest32("a"), 97);
    assert_eq!(digest32("abc"), 96354);

    // The one input that hashes to `i32::MIN`, where `Math.abs` widens and
    // Rust's `abs` would overflow.
    assert_eq!(digest32("polygenelubricants"), 2_147_483_648);

    assert_eq!(base36(0), "0");
    assert_eq!(base36(35), "z");
    assert_eq!(base36(36), "10");
    assert_eq!(base36(u32::MAX), "1z141z3");
}

/// Only the layout is asserted here. Which data home it hangs off is
/// `tests/permissions.rs`'s to check, because deciding that means setting
/// `XDG_DATA_HOME`, and a unit test that did would be setting it for every
/// other test in the binary at the same time.
#[test]
fn a_projects_data_hangs_off_the_data_home_and_is_not_created_by_asking() {
    let scratch = temporary();
    let project = Project::resolve(scratch.path());
    let directory = project.data_dir().expect("the path resolves");

    assert!(
        directory.ends_with(Path::new("ganja").join("project").join(project.slug())),
        "{}",
        directory.display()
    );
    assert!(directory.is_absolute(), "{}", directory.display());
    assert!(
        !directory.exists(),
        "resolving a project must not create anything: {}",
        directory.display()
    );
}

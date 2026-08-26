use std::path::Path;
#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
};

use super::{Anchor, AnchorError};

/// A project directory whose root is pinned by a `.git` marker.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::create_dir(dir.path().join(".git")).expect("the marker is creatable");

    dir
}

#[test]
fn an_anchor_addresses_the_file_it_was_given() {
    let dir = project();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "hello").expect("the fixture writes");

    let anchor = Anchor::open(&path, false).expect("the parent exists");

    assert_eq!(
        anchor.path(),
        std::fs::canonicalize(&path).expect("the file exists")
    );
}

/// Unix paths are byte strings, so anchoring must not make UTF-8 a hidden
/// precondition for the file name.
#[cfg(unix)]
#[test]
fn a_non_utf8_path_survives_the_anchor_byte_for_byte() {
    let dir = project();
    let name = OsString::from_vec(b"notes-\xfe.txt".to_vec());
    let path = dir.path().join(&name);

    let anchor = Anchor::open(&path, false).expect("the parent is an ordinary directory");
    assert_eq!(
        anchor
            .path()
            .file_name()
            .expect("the file has a name")
            .as_bytes(),
        name.as_os_str().as_bytes(),
    );
}

/// A model-supplied NUL must be refused rather than turning the bytes after
/// it into an invisible suffix and opening a different file.
#[cfg(unix)]
#[test]
fn a_nul_byte_in_a_name_is_refused_instead_of_truncated() {
    let dir = project();
    std::fs::write(dir.path().join("notes"), "different file").expect("the truncated name exists");
    let name = OsString::from_vec(b"notes\0-hidden".to_vec());
    let path = dir.path().join(name);
    let anchor = Anchor::open(&path, false).expect("the parent is an ordinary directory");

    let refused = anchor
        .read()
        .expect_err("a NUL-bearing name is never opened");

    let AnchorError::Io(refused_path, error) = refused else {
        panic!("the refusal must remain an I/O argument error: {refused:?}");
    };
    assert_eq!(refused_path, anchor.path());
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "a path component holds a NUL byte");
}

#[test]
fn missing_parents_are_created_only_when_the_caller_asks() {
    let dir = project();
    let path = dir.path().join("a").join("b").join("deep.txt");

    let refused = Anchor::open(&path, false).expect_err("nothing may be created unasked");
    assert!(refused.is_missing(), "got {refused:?}");
    assert!(!dir.path().join("a").exists());

    let anchor = Anchor::open(&path, true).expect("the parents are created on request");
    assert!(dir.path().join("a").join("b").is_dir());
    let (mut file, existed) = anchor.write().expect("a fresh file is created");
    assert!(!existed, "the file did not exist before this call");
    std::io::Write::write_all(&mut file, b"x").expect("the file is writable");
    assert_eq!(std::fs::read_to_string(&path).expect("it is there"), "x");
}

/// The refusal this module exists for, at the level it is implemented: a
/// link at the final component is not followed, wherever it points.
#[cfg(unix)]
#[test]
fn a_link_at_the_name_is_refused_by_the_open_itself() {
    let dir = project();
    let target = dir.path().join("real.txt");
    std::fs::write(&target, "before").expect("the fixture writes");
    let planted = dir.path().join("notes.txt");
    std::os::unix::fs::symlink(&target, &planted).expect("the link is creatable");

    let anchor = Anchor::open(&planted, false).expect("the parent is an ordinary directory");

    assert!(
        matches!(anchor.read(), Err(AnchorError::Link(_))),
        "a read through a planted link must be refused by the open"
    );
    assert!(matches!(anchor.write(), Err(AnchorError::Link(_))));
    assert_eq!(
        std::fs::read_to_string(&target).expect("the target still exists"),
        "before"
    );
}

/// The same refusal, recovered on the platform that has no `O_NOFOLLOW`:
/// the open asks for the reparse point itself, so a link planted at the
/// answered name is judged on the handle rather than followed to wherever
/// it leads.
///
/// Planting an NTFS symbolic link needs `SeCreateSymbolicLinkPrivilege`,
/// which an elevated session and a GitHub windows runner have and an
/// ordinary desktop session does not. The fixture says so outright rather
/// than skipping: a security test that quietly passes because it could not
/// build its own attack is worse than no test at all.
#[cfg(windows)]
#[test]
fn a_link_at_the_name_is_refused_by_the_open_itself() {
    let dir = project();
    let target = dir.path().join("real.txt");
    std::fs::write(&target, "before").expect("the fixture writes");
    let planted = dir.path().join("notes.txt");
    std::os::windows::fs::symlink_file(&target, &planted).expect(
        "this test has to plant the link it is about to refuse, and that needs \
             SeCreateSymbolicLinkPrivilege: run it elevated, or turn Developer Mode on",
    );

    let anchor = Anchor::open(&planted, false).expect("the parent is an ordinary directory");

    assert!(
        matches!(anchor.read(), Err(AnchorError::Link(_))),
        "a read through a planted link must be refused by the open"
    );
    assert!(matches!(anchor.write(), Err(AnchorError::Link(_))));
    assert_eq!(
        std::fs::read_to_string(&target).expect("the target still exists"),
        "before",
        "and the link's target is untouched"
    );
}

/// A directory refuses in words that say it was a directory.
///
/// Left to the system the two platforms answer differently and only one of
/// them answers usefully: unix opens the directory and fails at the first
/// read with `EISDIR`, Windows refuses the open with "Access is denied" —
/// which reads as a permissions problem the caller does not have and cannot
/// fix.
#[test]
fn a_directory_at_the_name_is_refused_as_a_directory() {
    let dir = project();
    let path = dir.path().join("adir");
    std::fs::create_dir(&path).expect("the fixture makes a directory");

    let anchor = Anchor::open(&path, false).expect("the parent is an ordinary directory");
    let refused = anchor.read().expect_err("a directory is not a file");

    assert!(
        matches!(refused, AnchorError::Directory(_)),
        "got {refused:?}"
    );
    assert!(
        refused.to_string().contains("directory"),
        "the refusal has to name what was wrong: {refused}"
    );
}

/// A linked *directory* is a perfectly ordinary way to arrange a checkout:
/// the anchor resolves it once, up front, and then works relative to where
/// it really led.
#[cfg(unix)]
#[test]
fn a_linked_directory_is_resolved_once_and_then_held() {
    let dir = project();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("the fixture makes a directory");
    std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("the link is creatable");

    let anchor = Anchor::open(&dir.path().join("link").join("notes.txt"), false)
        .expect("a linked directory resolves");

    assert_eq!(
        anchor.path(),
        std::fs::canonicalize(&real)
            .expect("the directory exists")
            .join("notes.txt"),
        "the anchor names where the link really led"
    );
}

#[test]
fn a_path_with_no_final_name_is_refused() {
    assert!(matches!(
        Anchor::open(Path::new("/"), false),
        Err(AnchorError::Nameless(_))
    ));
}

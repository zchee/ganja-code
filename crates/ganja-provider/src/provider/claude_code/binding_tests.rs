use std::path::Path;

use super::{Binding, Lock, Paths, REFUSED_STREAK_BOUND, load, store};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn paths(home: &Path) -> Paths {
    Paths::under(home)
}

#[test]
fn a_key_that_has_never_been_written_reads_as_nothing_recorded() {
    let home = temp();

    assert_eq!(load(&paths(home.path()).binding("k")), None);
}

#[test]
fn what_is_written_is_what_is_read_back() {
    let home = temp();
    let path = paths(home.path()).binding("k");
    let binding = Binding {
        cli_session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        sent: vec!["m1".to_owned(), "m2".to_owned()],
        refused: true,
        refused_streak: 1,
    };

    store(&path, &binding).expect("the binding is written");

    assert_eq!(load(&path), Some(binding));
}

/// A binding names a conversation, so the directory listing alone would say
/// how many a person is holding.
#[cfg(unix)]
#[test]
fn the_binding_is_owner_only_and_so_is_the_directory_it_lives_in() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = temp();
    let path = paths(home.path()).binding("k");
    store(&path, &Binding::default()).expect("the binding is written");

    let file = std::fs::metadata(&path).expect("the file exists");
    assert_eq!(file.permissions().mode() & 0o777, 0o600);

    let directory = std::fs::metadata(path.parent().expect("a parent")).expect("the directory");
    assert_eq!(directory.permissions().mode() & 0o777, 0o700);
}

/// Written through a sibling and renamed, so a reader never sees half a
/// binding — and the sibling does not survive the write.
#[test]
fn the_write_leaves_no_temporary_behind() {
    let home = temp();
    let paths = paths(home.path());
    store(&paths.binding("k"), &Binding::default()).expect("the binding is written");

    let left: Vec<String> = std::fs::read_dir(home.path().join("ganja").join("claude-code"))
        .expect("the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(left, ["k.json"]);
}

/// A malformed file costs the next turn a fresh record, which is a cost. A
/// failed turn would be a conversation that cannot start.
#[test]
fn a_malformed_binding_reads_as_nothing_rather_than_failing_a_turn() {
    let home = temp();
    let path = paths(home.path()).binding("k");
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(&path, b"{ not json at all").expect("the file is planted");

    assert_eq!(load(&path), None);
}

/// A file from before a field existed reads that field as its default, which
/// for both refusal fields is the right answer: a record nobody recorded a
/// refusal for is one no refusal was seen on.
#[test]
fn a_binding_from_before_the_refusal_fields_reads_them_as_never_refused() {
    let home = temp();
    let path = paths(home.path()).binding("k");
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(&path, br#"{"cli_session_id":"old","sent":["m1"]}"#).expect("the file");

    let binding = load(&path).expect("an old binding still loads");

    assert_eq!(binding.sent, ["m1"]);
    assert!(!binding.refused);
    assert_eq!(binding.refused_streak, 0);
}

#[test]
fn the_binding_lock_and_the_scratch_cwd_all_live_under_one_root() {
    let home = temp();
    let paths = paths(home.path());
    let root = home.path().join("ganja").join("claude-code");

    assert_eq!(paths.binding("k"), root.join("k.json"));
    assert_eq!(paths.lock("k"), root.join("k.lock"));
    assert_eq!(paths.cwd("k"), root.join("cwd").join("k"));
    assert_eq!(paths.one_shot_cwd(), root.join("cwd").join("one-shot"));
}

/// Two ganjas on one conversation never share a binding, and which one holds
/// it is whichever spawned first.
#[test]
fn a_second_holder_is_refused_the_lock_while_the_first_holds_it() {
    let home = temp();
    let path = paths(home.path()).lock("k");

    let first = Lock::claim(&path).expect("the first claim succeeds");
    let second = Lock::claim(&path);
    assert!(second.is_err(), "a second ganja must not share the binding");
    assert!(
        second.unwrap_err().contains("k.lock"),
        "the refusal names the lock, so a person can see which conversation"
    );

    drop(first);
    Lock::claim(&path).expect("the lock is released by dropping it");
}

/// Unlinking a lock file is how a lock file stops working: the remover and a
/// holder are then no longer locking the same inode.
#[test]
fn the_lock_file_survives_the_lock_being_released() {
    let home = temp();
    let path = paths(home.path()).lock("k");

    drop(Lock::claim(&path).expect("the claim succeeds"));

    assert!(path.exists(), "a lock file is never removed");
}

/// One with the transcript rendered, one with the prompts alone, and then the
/// wire stops spending rather than trying a third.
#[test]
fn the_refusal_bound_is_two_consecutive_records() {
    assert_eq!(REFUSED_STREAK_BOUND, 2);
}

/// The direction of the one-write lag is **chosen**: a duplicate the model
/// reads twice, never a message it never saw.
#[test]
fn a_binding_one_write_behind_leaves_the_last_message_owed_rather_than_lost() {
    let home = temp();
    let path = paths(home.path()).binding("k");
    // The frame carrying `m2` was written and the crash landed before the
    // binding was.
    store(&path, &Binding { sent: vec!["m1".to_owned()], ..Binding::default() })
        .expect("the binding is written");

    let recorded = load(&path).expect("a binding").sent;

    assert_eq!(recorded, ["m1"]);
    assert!(
        !recorded.contains(&"m2".to_owned()),
        "so the next request finds m2 owed and writes it again — a duplicate, never a loss"
    );
}

/// Sealing the leaf alone left the tree above it at the process umask, so
/// whether `claude-code/` — the directory holding every binding — was private
/// before the first binding landed depended on which caller ran first: a
/// one-shot reaches only its own scratch leaf (CC-9).
#[cfg(unix)]
#[test]
fn every_directory_this_wire_makes_is_owner_only_including_the_intermediates() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = temp();
    let paths = paths(home.path());
    let cwd = paths.cwd("k");

    // The one-shot's own path first, which is what a title spawns before any
    // binding is ever written.
    paths.create_private(&paths.one_shot_cwd()).expect("the scratch tree is made");
    paths.create_private(&cwd).expect("the scratch tree is made");

    let root = home.path().join("ganja").join("claude-code");
    for directory in [&root, &root.join("cwd"), &cwd, &paths.one_shot_cwd()] {
        let mode = std::fs::metadata(directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{} is not owner-only", directory.display());
    }
}

/// A directory outside this tree is a caller bug, and the refusal says so
/// rather than sealing whatever it was handed.
#[test]
fn a_directory_outside_the_tree_is_refused_rather_than_created() {
    let home = temp();
    let elsewhere = home.path().join("not-ours");

    let refused = paths(home.path()).create_private(&elsewhere).expect_err("outside the tree");

    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!elsewhere.exists(), "and nothing was made");
}

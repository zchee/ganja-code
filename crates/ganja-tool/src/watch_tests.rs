use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use notify::{
    EventKind,
    event::{DataChange, ModifyKind},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Registrar, Roots, bridge, register_reads};
use crate::{FileTimes, ToolError};

/// An event of the shape a backend reports for a saved file.
fn changed(paths: Vec<PathBuf>) -> notify::Event {
    notify::Event {
        kind: EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        paths,
        attrs: notify::event::EventAttributes::default(),
    }
}

/// Moves `path`'s modification stamp somewhere it provably was not, so a
/// test does not race a filesystem whose stamps have one-second
/// resolution.
///
/// Opened for writing because a stamp is metadata a handle must be allowed
/// to write: unix grants that with the file's own permissions, Windows only
/// through a handle that asked for write access.
fn age(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::UNIX_EPOCH))
        .expect("the fixture can move the stamp");
}

/// A registrar over `root` and the set it fills, for the tests that assert
/// on which directories a session ends up watching.
///
/// On the stub watcher, deliberately: these drills assert bookkeeping,
/// and the platform backend is where the timing lives — registering with
/// the real windows one blocks on an ack its server thread may take
/// arbitrarily long to send. What the real registration asks of its
/// backend is pinned by `the_watch_taken_is_never_recursive` instead.
fn registrar(
    root: &Path,
) -> (
    Registrar<notify::NullWatcher>,
    Arc<Mutex<BTreeSet<PathBuf>>>,
) {
    let watched = Arc::new(Mutex::new(BTreeSet::new()));

    (
        Registrar {
            watcher: notify::NullWatcher,
            root: root.to_owned(),
            watched: Arc::clone(&watched),
        },
        watched,
    )
}

fn set<const N: usize>(paths: [PathBuf; N]) -> BTreeSet<PathBuf> {
    paths.into_iter().collect()
}

#[tokio::test]
async fn the_bridge_condemns_a_read_file_that_changed_and_ignores_one_nobody_read() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let read = dir.path().join("read.txt");
    let unread = dir.path().join("unread.txt");
    std::fs::write(&read, "one").expect("the fixture writes");
    std::fs::write(&unread, "one").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    files.record(&read);
    age(&read);
    age(&unread);

    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send(changed(vec![read.clone(), unread.clone()]))
        .expect("the bridge is listening");
    // The loop ends when the last sender goes, so awaiting it is awaiting
    // every event already queued — no sleeping, no polling.
    drop(sender);
    bridge(receiver, Roots::new(dir.path()), Arc::clone(&files)).await;

    let refused = files
        .check_fresh(&read)
        .expect_err("the file changed under the session");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
        "got {refused:?}"
    );
    assert_eq!(
        files.take_stale(),
        vec![read],
        "only a file this session read is any of the watcher's business"
    );
    let never_read = files
        .check_fresh(&unread)
        .expect_err("an unread file is still unread");
    assert!(
        matches!(&never_read, ToolError::Failed(message) if message.contains("read it first")),
        "got {never_read:?}"
    );
}

#[tokio::test]
async fn a_file_that_did_not_move_is_left_alone_however_many_events_name_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    files.record(&path);

    let (sender, receiver) = mpsc::unbounded_channel();
    for _ in 0..3 {
        sender
            .send(changed(vec![path.clone()]))
            .expect("the bridge is listening");
    }
    drop(sender);
    bridge(receiver, Roots::new(dir.path()), Arc::clone(&files)).await;

    files
        .check_fresh(&path)
        .expect("an event is not a change; the stamp decides");
    assert!(files.take_stale().is_empty());
}

#[tokio::test]
async fn a_file_that_went_away_is_stale() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    files.record(&path);
    std::fs::remove_file(&path).expect("the fixture can delete it");

    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send(changed(vec![path.clone()]))
        .expect("the bridge is listening");
    drop(sender);
    bridge(receiver, Roots::new(dir.path()), Arc::clone(&files)).await;

    assert_eq!(files.take_stale(), vec![path]);
}

/// **The structural claim, with no timing in it**: registering a read
/// takes one watch, on the one directory holding it. The subtree beside it
/// is what a root-recursive registration would have walked, and it is
/// never named here however large it grows.
#[test]
fn a_read_watches_the_directory_holding_it_and_nothing_else() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let source = dir.path().join("src");
    let heavy = dir.path().join("target/debug/deps");
    std::fs::create_dir_all(&source).expect("the fixture nests");
    std::fs::create_dir_all(&heavy).expect("the fixture nests");
    let read = source.join("main.rs");
    std::fs::write(&read, "fn main() {}").expect("the fixture writes");

    let files = FileTimes::default();
    files.record(&read);
    let (mut registrar, watched) = registrar(dir.path());

    registrar.register(&read, &files);

    assert_eq!(
        *watched.lock().expect("the watched set is never poisoned"),
        set([source.clone()]),
        "one read is one watch, on the directory that holds it"
    );

    // Read a second file in the same directory: still one watch.
    let sibling = source.join("lib.rs");
    std::fs::write(&sibling, "").expect("the fixture writes");
    files.record(&sibling);
    registrar.register(&sibling, &files);

    assert_eq!(
        *watched.lock().expect("the watched set is never poisoned"),
        set([source]),
        "a directory already watched is not watched twice"
    );
}

#[test]
fn a_file_read_outside_the_project_is_not_watched() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let project = dir.path().join("project");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&project).expect("the fixture nests");
    std::fs::create_dir_all(&elsewhere).expect("the fixture nests");
    let outside = elsewhere.join("hosts");
    std::fs::write(&outside, "127.0.0.1").expect("the fixture writes");

    let files = FileTimes::default();
    files.record(&outside);
    let (mut registrar, watched) = registrar(&project);

    registrar.register(&outside, &files);

    assert!(
        watched
            .lock()
            .expect("the watched set is never poisoned")
            .is_empty(),
        "the session answers for its project, not for wherever else it read"
    );
}

/// The window lazy registration opens: a file changed between the read and
/// the watch produced no event, so the registration stats it once.
#[tokio::test]
async fn a_change_between_the_read_and_the_watch_is_caught_by_the_registration() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    files.record(&path);
    // Somebody else's editor, in the moment before the watch was taken.
    std::fs::write(&path, "somebody else").expect("the fixture writes");
    age(&path);

    let (mut registrar, _watched) = registrar(dir.path());
    registrar.register(&path, &files);

    assert_eq!(
        files.take_stale(),
        vec![path],
        "the registration's own stat is what covers the gap it opened"
    );
}

#[tokio::test]
async fn the_registration_loop_ends_when_the_session_stops_announcing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let source = dir.path().join("src");
    std::fs::create_dir_all(&source).expect("the fixture nests");
    let read = source.join("main.rs");
    std::fs::write(&read, "").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    let (sender, receiver) = mpsc::unbounded_channel();
    let (registrar, watched) = registrar(dir.path());

    sender.send(read.clone()).expect("the loop is listening");
    drop(sender);
    register_reads(
        receiver,
        registrar,
        Arc::clone(&files),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(
        *watched.lock().expect("the watched set is never poisoned"),
        set([source]),
        "everything queued is registered before the loop returns"
    );
}

#[tokio::test]
async fn a_stopped_session_stops_registering() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let files = Arc::new(FileTimes::default());
    let (sender, receiver) = mpsc::unbounded_channel();
    let (registrar, watched) = registrar(dir.path());
    let stop = CancellationToken::new();
    stop.cancel();

    // Queued before the loop runs, and never registered: the stop wins.
    sender
        .send(dir.path().join("src/main.rs"))
        .expect("the loop is listening");
    register_reads(receiver, registrar, Arc::clone(&files), stop).await;

    assert!(
        watched
            .lock()
            .expect("the watched set is never poisoned")
            .is_empty()
    );
}

#[test]
fn an_event_path_is_rebased_onto_the_root_the_session_named() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let real = dir.path().join("project");
    std::fs::create_dir(&real).expect("the fixture nests");
    #[cfg(unix)]
    let named = {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("the link plants");
        link
    };
    #[cfg(not(unix))]
    let named = real.clone();

    let roots = Roots::new(&named);
    let resolved = real.canonicalize().expect("the fixture resolves");

    assert_eq!(
        roots.rebase(&resolved.join("src/main.rs")),
        named.join("src/main.rs"),
        "the log holds the path the session named, and that is what has to be looked up"
    );
    assert_eq!(
        roots.rebase(Path::new("/elsewhere/a.txt")),
        Path::new("/elsewhere/a.txt"),
        "a path outside the root is nothing this can translate"
    );
}

#[test]
fn a_root_that_is_already_resolved_translates_nothing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().canonicalize().expect("the fixture resolves");
    let roots = Roots::new(&root);

    assert_eq!(roots.rebase(&root.join("a.txt")), root.join("a.txt"));
}

/// The whole-tree recursive watch was tried once and reverted: on Linux,
/// `notify`'s inotify backend registers a recursive watch by walking the
/// entire directory synchronously, which is what timed out the pty
/// drills in CI once a checkout grew a `node_modules` or a warm
/// `target/`. Non-recursive versus recursive has no other footprint any
/// caller can observe — no return value changes, no event shape changes
/// — so no behavioral test can pin it; reading the argument straight out
/// of the source is the same trick the workspace's own purity check uses
/// to keep this crate free of `ratatui`.
#[test]
fn the_watch_taken_is_never_recursive() {
    let source = include_str!("watch.rs");

    assert!(
        source.contains("RecursiveMode::NonRecursive"),
        "the watch registration should still ask for the non-recursive mode"
    );

    // Assembled from two pieces rather than written as one literal, so
    // this test's own source is never itself an occurrence of the thing
    // it is checking for.
    let banned = ["Recursive", "Mode::Recursive"].concat();
    assert!(
        !source.contains(&banned),
        "the watch must never switch back to the mode that walks the \
             whole tree synchronously"
    );
}

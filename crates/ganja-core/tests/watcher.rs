//! The stale-read watcher against the real filesystem.
//!
//! The unit tests in `watch.rs` hand the bridge events they built themselves,
//! which proves the rule and proves nothing about the platform: whether the
//! backend reports a save at all, and whether it spells the path the way the
//! session did, are questions only a real watch over a real directory answers.
//! That is what this binary is for — and it is why the assertions here poll
//! instead of asserting once: a notification is asynchronous by nature, and the
//! only bound on it is a timeout generous enough not to be a flake.
//!
//! Every test here waits for the watch to be *registered* before it changes
//! anything. Registration follows the read log and happens on the watcher's own
//! task, and it stats the file it registers — so a change made before the watch
//! was taken would be caught by that stat rather than by an event, and a test
//! that did not wait could pass on a platform where watching does nothing.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use ganja_core::{
    ToolCtx, ToolError,
    tool::{Credentials, FileTimes, Tool as _, write::WriteTool},
    watch::Watcher,
};
use tokio_util::sync::CancellationToken;

/// How long a platform gets to report a change before the test calls it a
/// failure. FSEvents coalesces on a latency of its own and inotify is prompt;
/// this is far above both, so a timeout means the event never came.
const REPORTED: Duration = Duration::from_secs(10);

/// Waits until `directory` is really under watch.
///
/// The fence every test below opens with: until the watch exists, a change
/// makes no event, and what condemned the file would be the registration's own
/// stat instead of the platform.
async fn wait_for_watch(watcher: &Watcher, directory: &Path) -> BTreeSet<PathBuf> {
    let deadline = Instant::now() + REPORTED;

    loop {
        let watched = watcher.watched();
        if watched.contains(directory) || Instant::now() >= deadline {
            return watched;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for the watcher to name something, and hands over what it named.
///
/// The queue is the one signal that belongs to the watcher alone: a stamp
/// comparison would answer "changed" whether or not an event ever arrived, and
/// a test that polled *that* would pass on a platform where watching does
/// nothing at all.
async fn wait_for_notice(files: &FileTimes) -> Vec<PathBuf> {
    let deadline = Instant::now() + REPORTED;

    loop {
        let named = files.take_stale();
        if !named.is_empty() || Instant::now() >= deadline {
            return named;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn stamp(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .expect("the fixture's filesystem stamps")
}

#[tokio::test]
async fn a_file_edited_outside_the_session_is_refused_and_named_to_the_model() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let watched = dir.path().join("notes.md");
    std::fs::write(&watched, "as the session read it").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    let watcher = Watcher::new(dir.path(), Arc::clone(&files));
    files.record(&watched);
    let as_read = stamp(&watched);
    assert!(
        wait_for_watch(&watcher, dir.path())
            .await
            .contains(dir.path()),
        "the directory holding a read file is what gets watched, and nothing happens until it is"
    );
    files
        .check_fresh(&watched)
        .expect("a file just read is fresh");
    assert!(
        files.take_stale().is_empty(),
        "registering a file nobody has touched condemns nothing"
    );

    // Somebody else's editor, writing the file the session is holding — after
    // the watch is live, so only an event can report it.
    std::fs::write(&watched, "and as somebody else left it").expect("the fixture writes");

    assert_eq!(
        wait_for_notice(&files).await,
        vec![watched.clone()],
        "the platform never reported a change to a file the session had read"
    );

    // Put the stamp back where the read found it, so that what refuses the
    // write below can only be the watcher's verdict: a comparison would say
    // this file never moved.
    std::fs::File::open(&watched)
        .and_then(|file| file.set_modified(as_read))
        .expect("the fixture can move the stamp");
    let refused = files
        .check_fresh(&watched)
        .expect_err("a condemned file is refused");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
        "got {refused:?}"
    );

    files.record(&watched);
    files
        .check_fresh(&watched)
        .expect("reading it again is what repairs it");
}

/// The self-write invariant the ordering comments in `write.rs` and `edit.rs`
/// exist for: a tool's own write records the file's new stamp inside the call
/// that caused the event, so the event compares clean and the session never
/// condemns its own work.
///
/// The wait is a fence rather than a sleep. A second file is changed from
/// outside *after* the tool wrote, and the assertion waits for that one to be
/// reported; the two changes share one watch and one channel, so by the time
/// the later event has been applied the earlier one has been too. What is left
/// to assert is that applying it did nothing.
#[tokio::test]
async fn a_files_own_write_does_not_condemn_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let written = dir.path().join("written.txt");
    let fence = dir.path().join("fence.txt");
    std::fs::write(&fence, "one").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    let watcher = Watcher::new(dir.path(), Arc::clone(&files));
    files.record(&fence);
    wait_for_watch(&watcher, dir.path()).await;

    let ctx = ToolCtx {
        cwd: dir.path().to_owned(),
        cancel: CancellationToken::new(),
        call_id: "call-1".to_owned(),
        files: Arc::clone(&files),
        credentials: Credentials::Unguarded,
        spawn: None,
    };
    for content in ["what the agent wrote", "and then wrote again"] {
        WriteTool
            .run(
                serde_json::json!({ "filePath": "written.txt", "content": content }),
                &ctx,
            )
            .await
            .expect("the tool writes");
    }

    std::fs::write(&fence, "somebody else").expect("the fixture writes");

    assert_eq!(
        wait_for_notice(&files).await,
        vec![fence],
        "only the change from outside is worth telling the model about, and the fence \
         arriving alone is what says the writes before it were applied and did nothing"
    );
    files
        .check_fresh(&written)
        .expect("a session must not condemn the file it just wrote");
}

/// **The structural guard on a real `Watcher`.** Registering follows the read
/// log, so a project's bulk — the `target/` a build filled, the `node_modules`
/// an install unpacked — is never registered at all. This is the claim the
/// root-recursive version could not make: it walked every one of these
/// directories and spent a watch descriptor on each, which is what blocked
/// startup on Linux.
#[tokio::test]
async fn a_subtree_the_session_never_reads_is_never_watched() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let source = dir.path().join("src");
    std::fs::create_dir_all(&source).expect("the fixture nests");

    // The bulk a real checkout carries, in miniature: deep, wide, and none of
    // it ever read.
    let mut heavy = Vec::new();
    for package in 0..40 {
        let nested = dir
            .path()
            .join("node_modules")
            .join(format!("package-{package}"))
            .join("dist");
        std::fs::create_dir_all(&nested).expect("the fixture nests");
        std::fs::write(nested.join("index.js"), "module.exports = {}").expect("the fixture writes");
        heavy.push(nested);
    }

    let read = source.join("main.rs");
    std::fs::write(&read, "fn main() {}").expect("the fixture writes");

    let files = Arc::new(FileTimes::default());
    let watcher = Watcher::new(dir.path(), Arc::clone(&files));
    assert!(
        watcher.watched().is_empty(),
        "construction registers nothing — there is no read to follow yet, which is why it \
         can return without touching the filesystem"
    );

    files.record(&read);
    let watched = wait_for_watch(&watcher, &source).await;

    assert_eq!(
        watched,
        [source].into_iter().collect::<BTreeSet<_>>(),
        "one file read is one directory watched — not the root, and not the tree beneath it"
    );
    for nested in &heavy {
        assert!(
            !watched.contains(nested),
            "{} was never read and must never be watched",
            nested.display()
        );
    }
}

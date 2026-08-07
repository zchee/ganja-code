//! Filesystem events, narrowed to the files this session has read.
//!
//! Spec: upstream `packages/core/src/filesystem/watcher.ts` — for the shape of
//! the thing only. **Deviation (watcher-staleness-to-model):** what this does
//! with the events is ganja's, not a port. Upstream watches the project
//! directory behind an experimental flag that defaults to *off*, watches `.git`
//! unconditionally to notice a branch change, and surfaces nothing to the
//! model: its one in-repo consumer republishes `HEAD` changes. It has no
//! read-before-write gate at all — its `edit` re-reads the file at execute
//! time, so nothing it holds can go stale. Ganja does have that gate, which is
//! what makes an external change worth reporting: a model that read a file ten
//! minutes ago is otherwise reasoning about bytes that are no longer there, and
//! finds out only when a write is refused.
//!
//! **What is watched, and why it is not the project root** (deviation D171,
//! amending R11). A recursive watch on the root was landed first, in `ace58ba`,
//! and reverted for cause. On Linux, registering one is a
//! *synchronous walk of the whole tree*: `notify`'s inotify backend blocks the
//! caller (`inotify.rs`, `watch_inner` → `rx.recv()`) while its event loop runs
//! `WalkDir` over every directory and spends an `inotify_add_watch` on each.
//! A checkout with a `node_modules` or a warm `target/` costs seconds of
//! startup and thousands of watch descriptors against a per-user limit — all
//! to watch directories a session will never open. macOS hides it, because
//! registering an FSEvents stream is O(1) whatever the tree holds. Upstream
//! carries an ignore set for the same reason, and still leaves the whole thing
//! off by default.
//!
//! So registration follows the read log instead: a **non-recursive** watch on
//! the directory holding each file as it is read, taken on this module's own
//! task and never on the caller's. The set only grows within a session and is
//! bounded by the number of distinct directories the model actually opened —
//! typically a handful. Everything downstream is unchanged: events are still
//! filtered to paths already in the read log, the file's own stamp still
//! decides, an agent's own write still compares clean, and staleness is still
//! a state that survives until the file is read again.
//!
//! Nothing here can fail a turn, and nothing here can fail the engine: a
//! backend that will not start, or a directory that will not register, is a
//! warning and no more. A subagent is untouched — it runs on a [`FileTimes`]
//! of its own, and nobody watches on its behalf.

use std::{
    borrow::Cow,
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use notify::{RecursiveMode, Watcher as _};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::FileTimes;

/// Watches the directories holding the files a session has read.
///
/// Holding it is what keeps the watch alive: dropping it ends the task that
/// owns the platform watcher, which drops the watcher, which closes the event
/// channel and ends the bridge. There is no shutdown to call and none to
/// forget.
pub struct Watcher {
    /// Cancels the task on drop. Never read — that is the whole contract.
    _stop: DropGuard,
    /// Which directories are under watch, shared with the task that registers
    /// them.
    watched: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl Watcher {
    /// Starts watching for changes to the files `files` records under `root`.
    ///
    /// **Returns without touching the filesystem.** The platform watcher is
    /// built, and every directory registered, on the task this spawns — a
    /// caller on a startup path must not wait for either, because on Linux the
    /// registration cost is a function of the tree rather than a constant.
    /// Must therefore be called from inside a tokio runtime.
    ///
    /// A backend this platform cannot provide is one warning from that task,
    /// after which the session behaves exactly as it did before watching
    /// existed: a read-before-write gate that notices a change when a write
    /// asks about it.
    #[must_use]
    pub fn new(root: &Path, files: Arc<FileTimes>) -> Self {
        let stop = CancellationToken::new();
        let watched = Arc::new(Mutex::new(BTreeSet::new()));

        tokio::spawn(run(
            root.to_owned(),
            files.announce_reads(),
            files,
            Arc::clone(&watched),
            stop.clone(),
        ));

        Self {
            _stop: stop.drop_guard(),
            watched,
        }
    }

    /// The directories under watch, in the spelling the read log uses.
    ///
    /// What proves the registration strategy structurally: a session that read
    /// one file has one directory here, whatever else the project contains.
    #[must_use]
    pub fn watched(&self) -> BTreeSet<PathBuf> {
        self.watched
            .lock()
            .expect("the watched set is never poisoned")
            .clone()
    }
}

/// Owns the platform watcher, registers directories as reads arrive, and lets
/// [`bridge`] apply what comes back.
///
/// Wiring only (**D78**): every decision it makes lives in [`Registrar`] or
/// [`register_reads`], which are functions a test drives without a terminal, a
/// project or a platform backend.
async fn run(
    root: PathBuf,
    reads: mpsc::UnboundedReceiver<PathBuf>,
    files: Arc<FileTimes>,
    watched: Arc<Mutex<BTreeSet<PathBuf>>>,
    stop: CancellationToken,
) {
    let (sender, events) = mpsc::unbounded_channel();
    let watcher = match notify::recommended_watcher(move |event| match event {
        Ok(event) => {
            // The receiver going away means the session is gone; there is no
            // one left to tell.
            let _ = sender.send(event);
        }
        // The backend complaining about one path is not a reason to stop
        // watching the rest, and it is not something the model can act on.
        Err(error) => tracing::debug!(%error, "the filesystem watcher reported an error"),
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(
                %error,
                "no filesystem watcher; a file changed outside the session will be noticed \
                 when something writes to it"
            );

            return;
        }
    };

    let roots = Roots::new(&root);
    tokio::spawn(bridge(events, roots.clone(), Arc::clone(&files)));

    // Returning drops the registrar, and with it the platform watcher, which
    // closes the channel the bridge above is draining and ends that task too.
    register_reads(
        reads,
        Registrar {
            watcher,
            root,
            watched,
        },
        files,
        stop,
    )
    .await;
}

/// Registers the directory of every path on `reads`, until the session ends.
///
/// A function rather than a loop inside [`run`] so a test can hold the sender
/// and decide exactly what is read and when; dropping that sender ends it,
/// which is what makes the assertions below deterministic.
pub(crate) async fn register_reads(
    mut reads: mpsc::UnboundedReceiver<PathBuf>,
    mut registrar: Registrar,
    files: Arc<FileTimes>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            path = reads.recv() => match path {
                Some(path) => registrar.register(&path, &files),
                None => return,
            },
        }
    }
}

/// Takes the watches, and remembers which it has.
pub(crate) struct Registrar {
    /// The platform watcher every registration goes through.
    watcher: notify::RecommendedWatcher,
    /// The project. A file read outside it is left unwatched — watching
    /// `/etc` because the model read `/etc/hosts` would be a surprise, and the
    /// session is answerable for its project.
    root: PathBuf,
    /// Directories already registered, so a project read a thousand times
    /// costs one watch.
    watched: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl Registrar {
    /// Watches the directory holding `path`, if it is in the project and is
    /// not watched already.
    ///
    /// Non-recursive, deliberately: the point is the files this session reads,
    /// and a recursive watch here would drag in every directory beneath a read
    /// file for the same cost the root-recursive version was reverted for.
    pub(crate) fn register(&mut self, path: &Path, files: &FileTimes) {
        let Some(parent) = path.parent() else {
            return;
        };
        if !parent.starts_with(&self.root) {
            return;
        }
        if self
            .watched
            .lock()
            .expect("the watched set is never poisoned")
            .contains(parent)
        {
            return;
        }

        if let Err(error) = self.watcher.watch(parent, RecursiveMode::NonRecursive) {
            // One directory nobody can watch, not a watcher that has died: the
            // rest of the session keeps its own, and this file falls back to
            // the freshness check a write makes for itself.
            tracing::warn!(
                %error,
                directory = %parent.display(),
                "this directory cannot be watched; a change to it will be noticed when \
                 something writes there"
            );

            return;
        }
        self.watched
            .lock()
            .expect("the watched set is never poisoned")
            .insert(parent.to_owned());

        // The window lazy registration opens, closed on the spot: between the
        // read and the watch above, a change would have produced no event
        // anyone was listening for. One stat says whether it did — and it is
        // the same stat an event would have caused.
        files.note_change(path);
    }
}

/// Applies every event on `events` to `files`, until the watcher that feeds it
/// is dropped.
///
/// The bridge between the notification thread and the session, as a function
/// with no terminal, no runtime assumptions of its own and no watcher: a test
/// holds the sender and decides exactly what arrives and when.
pub(crate) async fn bridge(
    mut events: mpsc::UnboundedReceiver<notify::Event>,
    roots: Roots,
    files: Arc<FileTimes>,
) {
    while let Some(event) = events.recv().await {
        apply(&event, &roots, &files);
    }
}

/// Offers each of `event`'s paths to the read log.
///
/// Deliberately blind to `event.kind` (deviation:
/// watcher-events-not-filtered-by-kind): what the kinds mean differs by
/// backend — FSEvents coalesces, inotify splits one save into several, and a
/// rename arrives as its own shape on each — while the stamp comparison the
/// log makes is the same question on every platform and answers it from the
/// file rather than from the report. An `Access` event costs one `stat` of a
/// file the session already read, which is cheaper than being wrong about
/// which kinds a save produces here.
fn apply(event: &notify::Event, roots: &Roots, files: &FileTimes) {
    for path in &event.paths {
        files.note_change(&roots.rebase(path));
    }
}

/// Where the watch is rooted, in both spellings the two sides use.
#[derive(Clone)]
pub(crate) struct Roots {
    /// The path the session resolves its files under.
    named: PathBuf,
    /// What that path resolves to, which is the spelling the platform reports
    /// events in — macOS answers for `/private/var/…` where the session said
    /// `/var/…`, and any project reached through a symlink has the same two
    /// names.
    resolved: PathBuf,
}

impl Roots {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            named: root.to_owned(),
            resolved: root.canonicalize().unwrap_or_else(|_| root.to_owned()),
        }
    }

    /// `path` as the read log would have spelled it (deviation:
    /// watcher-paths-rebased-onto-the-named-root).
    ///
    /// Only the root is translated, and only when the two spellings differ:
    /// resolving every event path would be a `stat` per event to answer a
    /// question the prefix already answers, and would resolve links *inside*
    /// the project that the session named as it found them.
    pub(crate) fn rebase<'a>(&self, path: &'a Path) -> Cow<'a, Path> {
        if self.named == self.resolved {
            return Cow::Borrowed(path);
        }

        match path.strip_prefix(&self.resolved) {
            Ok(rest) => Cow::Owned(self.named.join(rest)),
            Err(_) => Cow::Borrowed(path),
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn registrar(root: &Path) -> (Registrar, Arc<Mutex<BTreeSet<PathBuf>>>) {
        let watched = Arc::new(Mutex::new(BTreeSet::new()));
        let watcher = notify::recommended_watcher(|_| {})
            .expect("this platform provides a filesystem watcher");

        (
            Registrar {
                watcher,
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
}

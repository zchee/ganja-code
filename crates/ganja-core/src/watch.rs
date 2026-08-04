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
//! What is watched: the project root, recursively. What is *acted on*: only
//! paths already in the read log, which is why there is no ignore set here —
//! upstream needs one because it watches everything for everyone, and a rule
//! that only fires for files the model has read cannot fire for `target/` or
//! `node_modules` unless the model read something in them, in which case it
//! wanted to know. What a recursive watch still costs is the platform's own
//! bookkeeping — one inotify descriptor per directory, against a per-user
//! limit — so a checkout large enough to exhaust it gets the degraded path
//! below rather than an ignore set that would have to guess which directories
//! the model will never open.
//!
//! Nothing here can fail a turn, and nothing here can fail the engine: a
//! watcher that will not start is one warning, and the session behaves exactly
//! as it did before this module existed. A subagent is untouched — it runs on
//! a [`FileTimes`] of its own, and nobody watches on its behalf.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use notify::{RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use crate::tool::FileTimes;

/// Watches a project for changes to the files a session has read.
///
/// Holding it is what keeps the watch alive: dropping it stops the platform's
/// notifications, which closes the channel, which ends the bridge task. There
/// is no shutdown to call and none to forget.
pub struct Watcher {
    /// The platform watcher. Never read — its `Drop` is the whole contract.
    _watcher: notify::RecommendedWatcher,
}

impl Watcher {
    /// Starts watching `root`, condemning any file in `files` that changes
    /// under it.
    ///
    /// Must be called from inside a tokio runtime: the bridge between the
    /// notification thread and the log runs as a task, so that a burst of
    /// events costs the thread the platform gave us a channel send and nothing
    /// more.
    ///
    /// # Errors
    ///
    /// Returns whatever `notify` refused with — no backend for this platform,
    /// a root that cannot be watched, a limit the OS imposes on watches. Every
    /// one of them is a reason to go without a watcher, never a reason to fail.
    pub fn new(root: &Path, files: Arc<FileTimes>) -> Result<Self, notify::Error> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |event| match event {
            Ok(event) => {
                // The receiver going away means the session is gone; there is
                // no one left to tell.
                let _ = sender.send(event);
            }
            // The backend complaining about one path is not a reason to stop
            // watching the rest, and it is not something the model can act on.
            Err(error) => tracing::debug!(%error, "the filesystem watcher reported an error"),
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;

        tokio::spawn(bridge(receiver, Roots::new(root), files));

        Ok(Self { _watcher: watcher })
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
    use std::{path::PathBuf, sync::Arc, time::SystemTime};

    use notify::{
        EventKind,
        event::{DataChange, ModifyKind},
    };
    use tokio::sync::mpsc;

    use super::{Roots, bridge};
    use crate::tool::{FileTimes, ToolError};

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
    fn age(path: &std::path::Path) {
        std::fs::File::open(path)
            .and_then(|file| file.set_modified(SystemTime::UNIX_EPOCH))
            .expect("the fixture can move the stamp");
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

    #[test]
    fn an_event_path_is_rebased_onto_the_root_the_session_named() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let real = dir.path().join("project");
        std::fs::create_dir(&real).expect("the fixture nests");
        let named = dir.path().join("link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &named).expect("the link plants");
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
            roots.rebase(std::path::Path::new("/elsewhere/a.txt")),
            std::path::Path::new("/elsewhere/a.txt"),
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
}

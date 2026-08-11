//! Git snapshots of the working tree, and the revert they make possible.
//!
//! Spec: upstream `packages/opencode/src/snapshot/index.ts` for the mechanism,
//! `packages/opencode/src/session/revert.ts` for the walk over a transcript.
//!
//! A snapshot is a **tree object in a git repository of ganja's own**. The git
//! directory lives under the data home, apart from the project's, and every
//! command is `git --git-dir <ours> --work-tree <the project>` — so nothing
//! here can touch the checkout's index, its HEAD or its reflog. The project's
//! own `.git` is read twice and written never: once for its ignore rules, once
//! for the exclude file those rules are seeded from.
//!
//! There are no commits. `git write-tree` names the staged tree and that hash
//! *is* the snapshot; `git checkout <hash> -- <file>` is how a file comes back.
//! Nothing references those trees, which is why a `gc --prune=7.days` runs
//! hourly rather than never.
//!
//! **Nothing here may fail a turn.** A git that will not spawn answers as a git
//! that exited non-zero and said why; a missing binary, a project that is not a
//! checkout, or a data directory that cannot be resolved disables the subsystem
//! at construction; every entry point below then does nothing at all. The
//! engine never branches on whether a snapshot succeeded, and the worst a
//! broken one costs is an undo with less to restore.

use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt as _, sync::Mutex};

use crate::{
    project::{self, Project},
    protocol::{Message, MessageId, PartBody, RevertInfo, Role},
};

/// Directory the per-project snapshot repositories hang under, beside the
/// `project/` tree the rest of a project's state lives in:
/// `<data home>/ganja/snapshot/<project slug>/<hash of the worktree>`.
///
/// The worktree hash is upstream's, and it is not redundant beside the slug
/// even though ganja's slug already names a path: upstream keys a project on
/// its git remote, so one project can have several worktrees, and keeping the
/// layout means a later build that adopts remote-keyed projects finds what
/// this one wrote where it expects it.
const SNAPSHOTS: &str = "snapshot";

/// How long an unreferenced object survives a collection, upstream's `prune`.
const PRUNE: &str = "--prune=7.days";

/// Largest an **untracked** file may be and still be snapshotted, upstream's
/// `limit`. A build artefact nobody asked git to track is not worth copying,
/// and the files that blow this up are exactly the ones an ignore rule missed.
const LIMIT: u64 = 2 * 1024 * 1024;

/// How often the unreferenced trees are collected. Upstream runs its first
/// collection a minute after init and hourly after that; here the clock starts
/// at construction, so the first one falls an hour into a session rather than
/// during its opening minute.
const GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Flags every command carries, upstream's `core`: long paths, and symlinks
/// written as symlinks, so a restore puts back what was there rather than a
/// platform's idea of it.
const CORE: &[&str] = &["-c", "core.longpaths=true", "-c", "core.symlinks=true"];

/// [`CORE`] plus the line-ending translation git would otherwise apply,
/// switched off: a snapshot is a copy of the bytes on disk. Upstream's `cfg`.
const CFG: &[&str] = &[
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
];

/// [`CFG`] plus verbatim path output, for the commands whose stdout is parsed
/// as paths: git quotes and octal-escapes a non-ASCII name otherwise, and a
/// snapshot that skipped those files would silently not restore them.
/// Upstream's `quote`.
const QUOTE: &[&str] = &[
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];

/// What one step of a turn changed, and the tree it changed it from.
///
/// **The hash is the state *before* the step**, which is what makes it useful:
/// checking those files out of that tree is exactly undoing the step. The
/// files are the ones that differ from it, project-relative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    /// Tree the files are restored from.
    pub hash: String,
    /// What changed, relative to the project root.
    pub files: Vec<String>,
}

/// Where a reverted session stands.
///
/// Persisted on [`SessionInfo`](crate::storage::SessionInfo) rather than held
/// in memory, because the messages a revert hid are still in the transcript:
/// a session reopened after an undo has to know that it is one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertState {
    /// The user message the revert stopped at. It and everything after it are
    /// still stored, and are deleted only when the next prompt makes the
    /// revert permanent.
    pub message_id: MessageId,
    /// The working tree as it was **before** the first undo of this chain,
    /// which is what a redo restores. Absent when the snapshot could not be
    /// taken, in which case a redo has nothing to put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Files the revert put back, relative to the project root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

impl RevertState {
    /// What a frontend is told about this state.
    #[must_use]
    pub fn info(&self) -> RevertInfo {
        RevertInfo {
            message_id: self.message_id.clone(),
            files: self.files.clone(),
        }
    }
}

/// Why a session takes no snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unavailable {
    /// The config asked for none.
    Disabled,
    /// The project is not a git checkout, so there is no working tree to
    /// snapshot and no ignore rules to snapshot it by.
    NotAProject,
    /// There is no `git` on `PATH`.
    NoGit,
    /// There is no data directory to keep the repository in.
    NoDataDirectory,
}

/// The snapshots one session takes, and the reverts they allow.
#[derive(Debug)]
pub struct Snapshots {
    /// Where the snapshot repository lives. Empty when unavailable.
    gitdir: PathBuf,
    /// The working tree every command operates on: the project root.
    worktree: PathBuf,
    /// [`None`] when snapshots are on; the reason otherwise.
    unavailable: Option<Unavailable>,
    /// Serializes mutations of the repository.
    ///
    /// Upstream keys a semaphore on the git directory because one process
    /// serves several instances; here one of these owns one directory, so the
    /// instance is the key. Behind an [`Arc`] so the detached collection can
    /// hold it too.
    lock: Arc<Mutex<()>>,
    /// When the last collection ran, so the next one is an hour later.
    collected: std::sync::Mutex<Instant>,
}

impl Snapshots {
    /// The snapshots `project` takes, on when `enabled` and the project can be
    /// snapshotted at all.
    ///
    /// Probing for `git` runs the binary, synchronously, once: the answer
    /// decides what [`Snapshots::notice`] says, and a frontend asks for that
    /// before it draws its first frame.
    #[must_use]
    pub fn new(project: &Project, enabled: bool) -> Self {
        let worktree = project.root().to_owned();
        let (gitdir, unavailable) = Self::resolve(project, &worktree, enabled);

        Self {
            gitdir,
            worktree,
            unavailable,
            lock: Arc::default(),
            collected: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Where this project's snapshot repository belongs, and why it will not
    /// be used.
    fn resolve(
        project: &Project,
        worktree: &Path,
        enabled: bool,
    ) -> (PathBuf, Option<Unavailable>) {
        if !enabled {
            return (PathBuf::new(), Some(Unavailable::Disabled));
        }
        // A `.git` entry is what `Project` itself walked up looking for, so
        // asking again here is asking whether it found one or fell back to the
        // directory it was handed.
        if !worktree.join(".git").exists() {
            return (PathBuf::new(), Some(Unavailable::NotAProject));
        }
        if !git_exists() {
            return (PathBuf::new(), Some(Unavailable::NoGit));
        }
        let Ok(home) = project::data_home() else {
            return (PathBuf::new(), Some(Unavailable::NoDataDirectory));
        };

        let gitdir = home
            .join(SNAPSHOTS)
            .join(project.slug())
            .join(project::digest(worktree));

        (gitdir, None)
    }

    /// Whether this session takes snapshots at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.unavailable.is_none()
    }

    /// One line saying why this session cannot undo, for a status bar to show
    /// once at startup.
    ///
    /// [`None`] when snapshots are on — and when the config switched them off,
    /// because somebody who wrote `"snapshot": false` does not need telling.
    #[must_use]
    pub fn notice(&self) -> Option<&'static str> {
        match self.unavailable? {
            Unavailable::Disabled => None,
            Unavailable::NotAProject => {
                Some("this directory is not a git checkout, so /undo has nothing to restore")
            }
            Unavailable::NoGit => Some("git is not on PATH, so /undo has nothing to restore"),
            Unavailable::NoDataDirectory => Some(
                "the data directory holding snapshots could not be located, so /undo has \
                 nothing to restore",
            ),
        }
    }

    /// Stages the working tree and names it, returning the tree's hash.
    ///
    /// [`None`] when snapshots are off, and when git refused — a caller that
    /// has no hash simply has nothing to attribute the next step's changes to.
    pub async fn track(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }

        let hash = {
            let guard = self.lock.lock().await;
            self.initialize().await;
            self.add().await;

            let named = self.git(&arguments(CORE, self.args(["write-tree"]))).await;
            drop(guard);

            if named.code != 0 {
                tracing::warn!(
                    code = named.code,
                    stderr = named.stderr.trim(),
                    "the working tree could not be snapshotted; this step will not be undoable"
                );
                return None;
            }

            named.text.trim().to_owned()
        };
        self.collect_later();

        (!hash.is_empty()).then_some(hash)
    }

    /// What changed since `hash`, as the patch a step is recorded with.
    ///
    /// The result always names `hash`, so a caller that only wants to know
    /// whether anything moved can read [`Patch::files`] and ignore the rest.
    /// A git that refuses answers "nothing changed", which costs the step its
    /// undo and nothing else.
    pub async fn patch(&self, hash: &str) -> Patch {
        let mut patch = Patch {
            hash: hash.to_owned(),
            files: Vec::new(),
        };
        if !self.enabled() {
            return patch;
        }

        let guard = self.lock.lock().await;
        self.add().await;
        let changed = self
            .git(&arguments(
                QUOTE,
                self.args([
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--name-only",
                    hash,
                    "--",
                    ".",
                ]),
            ))
            .await;
        if changed.code != 0 {
            drop(guard);
            tracing::warn!(
                hash,
                code = changed.code,
                stderr = changed.stderr.trim(),
                "the step's changes could not be listed; it will not be undoable"
            );
            return patch;
        }

        let files: Vec<String> = changed
            .text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        // A file the project ignores was never the agent's doing to show, and
        // upstream hides these for the same reason.
        let ignored = self.ignored(&files).await;
        drop(guard);

        patch.files = files
            .into_iter()
            .filter(|file| !ignored.contains(file))
            .collect();

        patch
    }

    /// Puts the whole working tree back to `snapshot`.
    ///
    /// What a redo does: the tree is read into the index and checked out
    /// wholesale, so every file it holds is restored whether or not anything
    /// changed it.
    pub async fn restore(&self, snapshot: &str) {
        if !self.enabled() {
            return;
        }

        let guard = self.lock.lock().await;
        let read = self
            .git(&arguments(CORE, self.args(["read-tree", snapshot])))
            .await;
        if read.code != 0 {
            drop(guard);
            tracing::error!(
                snapshot,
                code = read.code,
                stderr = read.stderr.trim(),
                "the snapshot could not be read back; the working tree is unchanged"
            );
            return;
        }

        let checkout = self
            .git(&arguments(CORE, self.args(["checkout-index", "-a", "-f"])))
            .await;
        drop(guard);
        if checkout.code != 0 {
            tracing::error!(
                snapshot,
                code = checkout.code,
                stderr = checkout.stderr.trim(),
                "the snapshot could not be checked out; the working tree is part-way restored"
            );
        }
    }

    /// Puts every file in `patches` back to the tree its patch names.
    ///
    /// The first patch naming a file wins, so walking the transcript newest
    /// first would undo the wrong step: callers pass them oldest first, which
    /// is the order a message holds them in.
    ///
    /// A file that is not in the tree it is being restored from was created by
    /// the step, so undoing the step deletes it. A checkout that fails for a
    /// file that *was* there keeps what is on disk — refusing to restore is
    /// never a reason to destroy.
    ///
    /// Answers the files that really came back, in the order the patches named
    /// them: the ones whose checkout succeeded, and the ones the revert wanted
    /// gone and that are gone. A file this could not move — a checkout that
    /// failed on a path something else now occupies, a removal the filesystem
    /// refused — is left out, so a caller reporting this list is reporting what
    /// happened rather than what was intended.
    pub async fn revert(&self, patches: &[Patch]) -> Vec<String> {
        if !self.enabled() {
            return Vec::new();
        }

        let mut achieved = Vec::new();
        let guard = self.lock.lock().await;
        for (hash, file) in dedupe(patches) {
            let restored = self
                .git(&arguments(
                    CORE,
                    self.args(["checkout", &hash, "--", &file]),
                ))
                .await;
            if restored.code == 0 {
                achieved.push(file);
                continue;
            }

            // Upstream batches up to a hundred of these per `checkout` and
            // falls back to this path when a batch fails; ganja takes the
            // fallback path always (deviation:
            // revert-checks-out-one-file-at-a-time). The outcome is identical
            // — the batch exists to amortize process spawns on a revert that
            // spans hundreds of files.
            let known = self
                .git(&arguments(CORE, self.args(["ls-tree", &hash, "--", &file])))
                .await;
            if known.code == 0 && !known.text.trim().is_empty() {
                tracing::info!(
                    file,
                    hash,
                    stderr = restored.stderr.trim(),
                    "the file is in the snapshot but could not be checked out; keeping it as it is"
                );
                continue;
            }

            let absolute = self.worktree.join(&file);
            match std::fs::remove_file(&absolute) {
                // Absent is the state a file the step invented is meant to end
                // in, so one that is already gone came back as much as it ever
                // will.
                Ok(()) => achieved.push(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => achieved.push(file),
                Err(error) => tracing::info!(
                    file,
                    %error,
                    "the file was created by the step being undone but could not be removed"
                ),
            }
        }
        drop(guard);

        achieved
    }

    /// Creates the repository when it is not there yet, with the settings a
    /// very large worktree needs to stage in bounded time.
    ///
    /// Assumes the mutation lock is held.
    async fn initialize(&self) {
        if self.gitdir.exists() {
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&self.gitdir) {
            tracing::warn!(
                path = %self.gitdir.display(),
                %error,
                "the snapshot repository could not be created; this session will not undo"
            );
            return;
        }

        // `init` is the one command that goes through the environment rather
        // than through `--git-dir`: it is creating the directory the flag
        // would have to name.
        self.git_with_env(
            &["init".to_owned()],
            &[
                ("GIT_DIR", self.gitdir.as_os_str()),
                ("GIT_WORK_TREE", self.worktree.as_os_str()),
            ],
        )
        .await;

        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.longpaths", "true"),
            ("core.symlinks", "true"),
            ("core.fsmonitor", "false"),
            // Tuning for very large worktrees, so the first staging stays
            // bounded rather than rehashing a checkout the size of chromium.
            ("feature.manyFiles", "true"),
            ("index.version", "4"),
            ("index.threads", "true"),
            ("core.untrackedCache", "true"),
        ] {
            self.git(&[
                "--git-dir".to_owned(),
                self.gitdir.to_string_lossy().into_owned(),
                "config".to_owned(),
                key.to_owned(),
                value.to_owned(),
            ])
            .await;
        }

        self.seed().await;
        tracing::info!(path = %self.gitdir.display(), "snapshot repository initialized");
    }

    /// Points the new repository at the project's own object database and
    /// index, so the first staging reuses hashes instead of recomputing them.
    ///
    /// Upstream's comment is the whole justification: on a checkout the size of
    /// chromium, rebuilding those hashes takes minutes. Best-effort in every
    /// step — a missing or incompatible index costs a full rehash and nothing
    /// else.
    async fn seed(&self) {
        let common = self
            .git(&[
                "rev-parse".to_owned(),
                "--path-format=absolute".to_owned(),
                "--git-common-dir".to_owned(),
            ])
            .await;
        if common.code != 0 {
            return;
        }
        let source = PathBuf::from(common.text.trim());
        if source.as_os_str().is_empty() || !source.exists() {
            return;
        }

        // The source's own alternates travel too, or a blob it borrows from a
        // third repository would not resolve here. One that has gone away is
        // dropped rather than written through.
        let objects = source.join("objects");
        let chained =
            std::fs::read_to_string(objects.join("info").join("alternates")).unwrap_or_default();
        let mut alternates: Vec<String> = Vec::new();
        for candidate in std::iter::once(objects.to_string_lossy().into_owned())
            .chain(chained.lines().map(str::trim).map(ToOwned::to_owned))
        {
            if !candidate.is_empty() && Path::new(&candidate).exists() {
                alternates.push(candidate);
            }
        }
        if alternates.is_empty() {
            return;
        }

        let info = self.gitdir.join("objects").join("info");
        if std::fs::create_dir_all(&info).is_err() {
            return;
        }
        if std::fs::write(
            info.join("alternates"),
            format!("{}\n", alternates.join("\n")),
        )
        .is_err()
        {
            return;
        }

        let index = source.join("index");
        if index.exists() {
            let _ = std::fs::copy(index, self.gitdir.join("index"));
        }
    }

    /// Stages everything the project changed since git last saw it.
    ///
    /// Candidates are the tracked files that differ from the project's index
    /// and every untracked file the project does not ignore. Assumes the
    /// mutation lock is held.
    ///
    /// Scoped to the **whole worktree**, where upstream lists from the
    /// directory its instance was started in (deviation:
    /// snapshot-scope-is-the-project-root). Upstream can have several
    /// instances under one checkout and narrows each to its own; ganja has one
    /// project per session, and its `!` passthrough and `@` mentions already
    /// resolve against the root — a snapshot that stopped at whichever
    /// subdirectory the terminal was opened in would leave an agent's edits
    /// elsewhere in the project unundoable.
    async fn add(&self) {
        self.exclude(&[]).await;

        let tracked = self
            .git(&arguments(
                QUOTE,
                self.args(["diff-files", "--name-only", "-z", "--", "."]),
            ))
            .await;
        let untracked = self
            .git(&arguments(
                QUOTE,
                self.args([
                    "ls-files",
                    "--full-name",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                ]),
            ))
            .await;
        if tracked.code != 0 || untracked.code != 0 {
            tracing::warn!(
                tracked = tracked.code,
                untracked = untracked.code,
                "the working tree could not be listed; this snapshot will be incomplete"
            );
            return;
        }

        let untracked: Vec<String> = split_nul(&untracked.text);
        let mut seen: HashSet<String> = HashSet::new();
        let mut candidates: Vec<String> = Vec::new();
        for file in split_nul(&tracked.text)
            .into_iter()
            .chain(untracked.clone())
        {
            if seen.insert(file.clone()) {
                candidates.push(file);
            }
        }
        if candidates.is_empty() {
            return;
        }

        // Resolved against the **project's** rules rather than the snapshot
        // repository's, which has none: what the agent may not commit is what
        // the snapshot may not keep.
        let ignored = self.ignored(&candidates).await;
        if !ignored.is_empty() {
            // A file that has only just become ignored is already staged from
            // an earlier snapshot, and would go on being staged forever.
            self.drop_staged(&ignored).await;
        }

        let allowed: Vec<String> = candidates
            .into_iter()
            .filter(|file| !ignored.contains(file))
            .collect();
        if allowed.is_empty() {
            return;
        }

        // A large *untracked* file is a build artefact an ignore rule missed;
        // a large tracked one is somebody's data and is staged regardless.
        let untracked_set: HashSet<&String> = untracked.iter().collect();
        let blocked: Vec<String> = allowed
            .iter()
            .filter(|file| untracked_set.contains(file) && self.oversized(file))
            .cloned()
            .collect();
        if !blocked.is_empty() {
            self.exclude(&blocked).await;
        }

        let blocked_set: HashSet<&String> = blocked.iter().collect();
        let staged: Vec<String> = allowed
            .into_iter()
            .filter(|file| !blocked_set.contains(file))
            .collect();
        if staged.is_empty() {
            return;
        }

        let added = self
            .git_with_stdin(
                &arguments(
                    CFG,
                    self.args([
                        "add",
                        "--all",
                        "--sparse",
                        "--pathspec-from-file=-",
                        "--pathspec-file-nul",
                    ]),
                ),
                pathspecs(&staged).into_bytes(),
            )
            .await;
        if added.code != 0 {
            tracing::warn!(
                code = added.code,
                stderr = added.stderr.trim(),
                "some files could not be staged; this snapshot will be incomplete"
            );
        }
    }

    /// Which of `files` the project's own ignore rules cover.
    ///
    /// Asked of the project's repository, not the snapshot's, and with
    /// `--no-index` so the answer stays a question about the patterns even for
    /// a file git already tracks.
    async fn ignored(&self, files: &[String]) -> BTreeSet<String> {
        if files.is_empty() {
            return BTreeSet::new();
        }

        // `check-ignore` reads a leading colon as pathspec magic, and accepts
        // — and echoes back — a `./` prefix that stops it.
        let guarded: Vec<String> = files
            .iter()
            .map(|file| {
                if file.starts_with(':') {
                    format!("./{file}")
                } else {
                    file.clone()
                }
            })
            .collect();

        // The project's repository is discovered from the working directory
        // rather than named with `--git-dir`, which upstream does: in a linked
        // worktree or a submodule the `.git` entry is a *file*, and a
        // `--git-dir` pointing at it fails outright (deviation:
        // check-ignore-discovers-the-repo).
        let mut command: Vec<String> = QUOTE.iter().map(ToString::to_string).collect();
        command.extend(
            ["check-ignore", "--no-index", "--stdin", "-z"]
                .into_iter()
                .map(ToOwned::to_owned),
        );

        let checked = self
            .git_with_stdin(&command, nul_terminated(&guarded).into_bytes())
            .await;
        // 0 is "some are ignored", 1 is "none are"; anything else is git
        // refusing to answer, and the safe reading of that is to filter
        // nothing out.
        if checked.code != 0 && checked.code != 1 {
            return BTreeSet::new();
        }

        split_nul(&checked.text)
            .into_iter()
            .map(|file| match file.strip_prefix("./") {
                Some(rest) if rest.starts_with(':') => rest.to_owned(),
                _ => file,
            })
            .collect()
    }

    /// Removes files from the snapshot index without touching the disk.
    async fn drop_staged(&self, files: &BTreeSet<String>) {
        let files: Vec<String> = files.iter().cloned().collect();
        tracing::info!(
            count = files.len(),
            "dropping newly ignored files from the snapshot"
        );

        self.git_with_stdin(
            &arguments(
                CFG,
                self.args([
                    "rm",
                    "--cached",
                    "-f",
                    "--ignore-unmatch",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ]),
            ),
            pathspecs(&files).into_bytes(),
        )
        .await;
    }

    /// Rewrites the snapshot repository's exclude file: what the project
    /// excludes, plus whatever `blocked` names.
    async fn exclude(&self, blocked: &[String]) {
        let mut text = String::new();
        if let Some(source) = self.project_excludes().await
            && let Ok(contents) = std::fs::read_to_string(source)
        {
            text.push_str(contents.trim_end());
        }
        for file in blocked {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push('/');
            text.push_str(&file.replace('\\', "/"));
        }
        if !text.is_empty() {
            text.push('\n');
        }

        let info = self.gitdir.join("info");
        if std::fs::create_dir_all(&info).is_err() {
            return;
        }
        let _ = std::fs::write(info.join("exclude"), text);
    }

    /// Where the project keeps its own exclude file, when it has one.
    async fn project_excludes(&self) -> Option<PathBuf> {
        let resolved = self
            .git(&[
                "rev-parse".to_owned(),
                "--path-format=absolute".to_owned(),
                "--git-path".to_owned(),
                "info/exclude".to_owned(),
            ])
            .await;
        let path = PathBuf::from(resolved.text.trim());

        (!path.as_os_str().is_empty() && path.exists()).then_some(path)
    }

    /// Whether a file is past [`LIMIT`]. A file that cannot be stat'd is not:
    /// refusing to snapshot something because its size is unknown would lose
    /// more than it saves.
    fn oversized(&self, file: &str) -> bool {
        std::fs::metadata(self.worktree.join(file))
            .is_ok_and(|stat| stat.is_file() && stat.len() > LIMIT)
    }

    /// Collects the unreferenced trees, in the background, at most hourly.
    ///
    /// Detached because a session should never wait on it: the snapshot it
    /// just took is already named, and the collection takes the same lock the
    /// next one will.
    fn collect_later(&self) {
        {
            let mut collected = self
                .collected
                .lock()
                .expect("the collection clock is never poisoned");
            if collected.elapsed() < GC_INTERVAL {
                return;
            }
            *collected = Instant::now();
        }

        let gitdir = self.gitdir.clone();
        let worktree = self.worktree.clone();
        let lock = Arc::clone(&self.lock);
        tokio::spawn(async move {
            let guard = lock.lock().await;
            let collected = run(
                &arguments(
                    CORE,
                    named(
                        &gitdir,
                        &worktree,
                        ["gc", PRUNE].into_iter().map(str::to_owned),
                    ),
                ),
                &worktree,
                None,
                &[],
            )
            .await;
            drop(guard);

            if collected.code == 0 {
                tracing::info!(prune = PRUNE, "snapshot repository collected");
            } else {
                tracing::warn!(
                    code = collected.code,
                    stderr = collected.stderr.trim(),
                    "the snapshot repository could not be collected"
                );
            }
        });
    }

    /// `cmd` addressed at this session's repository and working tree.
    fn args<'a>(&self, cmd: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        named(
            &self.gitdir,
            &self.worktree,
            cmd.into_iter().map(str::to_owned),
        )
    }

    async fn git(&self, args: &[String]) -> Output {
        run(args, &self.worktree, None, &[]).await
    }

    async fn git_with_stdin(&self, args: &[String], stdin: Vec<u8>) -> Output {
        run(args, &self.worktree, Some(stdin), &[]).await
    }

    async fn git_with_env(&self, args: &[String], env: &[(&str, &OsStr)]) -> Output {
        run(args, &self.worktree, None, env).await
    }
}

/// The user message a fresh undo reverts to: the last one before `anchor`, or
/// the last one of all when nothing is reverted yet.
///
/// Upstream's TUI computes the same message before it asks the server to
/// revert (`routes/session/index.tsx`, `session.undo`); the engine owns it
/// here so that a frontend does not have to hold the transcript to undo.
///
/// `history` is the engine's **live window**, where upstream walks the whole
/// stored transcript (deviation: undo-walks-the-live-window). The two differ
/// only after a compaction, and there the window is the honest bound: the
/// turns a summary replaced took their patch parts with them, so an undo
/// anchored before it would hide messages while restoring nothing. Stopping at
/// the summary keeps what is hidden and what is restored the same range.
#[must_use]
pub fn undo_anchor(history: &[Message], anchor: Option<&MessageId>) -> Option<MessageId> {
    history
        .iter()
        .rev()
        .find(|message| {
            message.role == Role::User && anchor.is_none_or(|anchor| message.id < *anchor)
        })
        .map(|message| message.id.clone())
}

/// The user message a redo steps forward to: the first one after `anchor`.
///
/// [`None`] means the redo has walked back to where the session was before any
/// of this, which is a full restore rather than a shallower revert.
#[must_use]
pub fn redo_anchor(history: &[Message], anchor: &MessageId) -> Option<MessageId> {
    history
        .iter()
        .find(|message| message.role == Role::User && message.id > *anchor)
        .map(|message| message.id.clone())
}

/// Every patch recorded at or after `anchor`, oldest first.
///
/// Order is what decides a file's fate when several steps touched it: the
/// oldest patch names the tree the file had before any of them, and
/// [`Snapshots::revert`] lets the first one win.
#[must_use]
pub fn patches_from(history: &[Message], anchor: &MessageId) -> Vec<Patch> {
    history
        .iter()
        .filter(|message| message.id >= *anchor)
        .flat_map(|message| &message.parts)
        .filter_map(|part| match &part.body {
            PartBody::Patch { hash, files } => Some(Patch {
                hash: hash.clone(),
                files: files.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The text of the message `anchor` names, for the editor an undo hands it
/// back to.
#[must_use]
pub fn prompt_at(history: &[Message], anchor: &MessageId) -> Option<String> {
    history
        .iter()
        .find(|message| message.id == *anchor)?
        .parts
        .iter()
        .find_map(|part| part.as_text())
        .map(ToOwned::to_owned)
}

/// Every file in `patches` paired with the tree it is restored from, first
/// mention winning.
fn dedupe(patches: &[Patch]) -> Vec<(String, String)> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut ops = Vec::new();
    for patch in patches {
        for file in &patch.files {
            if seen.insert(file.as_str()) {
                ops.push((patch.hash.clone(), file.clone()));
            }
        }
    }

    ops
}

/// Whether there is a `git` on `PATH` at all.
///
/// Synchronous, and run once per session at construction: the answer is what a
/// startup notice is written from, and blocking a few milliseconds before the
/// first frame is cheaper than a subsystem that only reveals itself broken at
/// the moment somebody tries to undo.
fn git_exists() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `cmd` addressed at `gitdir` over `worktree`.
fn named(gitdir: &Path, worktree: &Path, cmd: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut args = vec![
        "--git-dir".to_owned(),
        gitdir.to_string_lossy().into_owned(),
        "--work-tree".to_owned(),
        worktree.to_string_lossy().into_owned(),
    ];
    args.extend(cmd);

    args
}

/// `settings` in front of `cmd`, which is where git wants its `-c` flags.
fn arguments(settings: &[&str], cmd: Vec<String>) -> Vec<String> {
    let mut args: Vec<String> = settings.iter().map(ToString::to_string).collect();
    args.extend(cmd);

    args
}

/// `files` as the NUL-terminated list git's `--stdin -z` reads.
fn nul_terminated(files: &[String]) -> String {
    files.iter().fold(String::new(), |mut list, file| {
        list.push_str(file);
        list.push('\0');
        list
    })
}

/// `files` as NUL-terminated pathspecs that mean the literal path from the top
/// of the worktree, so a name containing a glob character stages itself and
/// nothing else.
fn pathspecs(files: &[String]) -> String {
    let literal: Vec<String> = files
        .iter()
        .map(|file| format!(":(top,literal){file}"))
        .collect();

    nul_terminated(&literal)
}

/// Splits git's `-z` output, which is NUL-**terminated** rather than
/// -separated, so the last field is followed by one.
fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|field| !field.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// What one git invocation said.
struct Output {
    /// Its exit status. A git that could not be spawned, or that a signal
    /// ended, reports 1 — the same shape upstream catches a spawn failure
    /// into, so every caller here has one failure to handle rather than two.
    code: i32,
    /// Standard output, decoded lossily: git speaks the filesystem's bytes,
    /// and one name that is not UTF-8 must not cost the whole snapshot.
    text: String,
    /// Standard error, for the line that explains a failure.
    stderr: String,
}

/// Runs git and reports what it said.
///
/// Never fails. Every caller in this module treats a snapshot that did not
/// happen as a snapshot it does not have, and a `Result` here would only push
/// that decision up into the turn.
async fn run(
    args: &[String],
    cwd: &Path,
    stdin: Option<Vec<u8>>,
    env: &[(&str, &OsStr)],
) -> Output {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }

    let failed = |error: &dyn std::fmt::Display| Output {
        code: 1,
        text: String::new(),
        stderr: error.to_string(),
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed(&error),
    };
    // The stdin write and the stdout/stderr drain must run concurrently:
    // `check-ignore --stdin -z` starts echoing answers while it is still
    // reading paths, so once the reply outgrows the OS pipe buffer (~64KB)
    // a git that is blocked writing output meets a caller still blocked
    // writing input — a permanent deadlock, taken under the snapshot
    // mutation lock, that no cancellation can reach. Taking stdin out of
    // the child (rather than leaving it for `wait_with_output`) is what
    // lets the write run beside the drain instead of before it; dropping
    // the pipe when the write finishes is still what tells the child it
    // has seen end of input.
    let pipe = stdin.is_some().then(|| child.stdin.take()).flatten();
    let write = async move {
        if let (Some(bytes), Some(mut pipe)) = (stdin, pipe) {
            let written = pipe.write_all(&bytes).await;
            drop(pipe);
            written
        } else {
            Ok(())
        }
    };

    let (written, output) = tokio::join!(write, child.wait_with_output());
    if let Err(error) = written {
        return failed(&error);
    }
    match output {
        Ok(output) => Output {
            code: output.status.code().unwrap_or(1),
            text: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => failed(&error),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use tempfile::TempDir;

    use super::{
        Patch, Snapshots, dedupe, patches_from, pathspecs, redo_anchor, split_nul, undo_anchor,
    };
    use crate::{
        project::Project,
        protocol::{Message, MessageId, Part, PartBody, Role},
    };

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// A message with a pinned id, so the ordering the walk depends on is the
    /// test's to state rather than the clock's.
    fn message(id: &str, role: Role, parts: Vec<Part>) -> Message {
        let mut message = match role {
            Role::User => Message::user(String::new()),
            Role::Assistant => Message::assistant("model"),
        };
        message.id = MessageId::from(id.to_owned());
        message.parts = parts;

        message
    }

    fn patch(hash: &str, files: &[&str]) -> Part {
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::Patch {
                hash: hash.to_owned(),
                files: files.iter().map(|file| (*file).to_owned()).collect(),
            },
        }
    }

    /// The golden differential and every scripted run works in a temporary
    /// directory that is not a checkout. `snapshot` defaults to *on*, so what
    /// actually keeps snapshots out of those runs is this — and a build where
    /// it stopped being true would have every golden run spawning git.
    #[test]
    fn a_directory_that_is_not_a_checkout_takes_no_snapshots() {
        let directory = temporary();
        let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);

        assert!(!snapshots.enabled());
        assert!(
            snapshots.notice().is_some(),
            "a session that cannot undo has to say so"
        );
    }

    #[test]
    fn a_configuration_that_switched_snapshots_off_says_nothing_about_it() {
        let directory = temporary();
        std::fs::create_dir(directory.path().join(".git")).expect("the marker is creatable");
        let snapshots = Snapshots::new(&Project::resolve(directory.path()), false);

        assert!(!snapshots.enabled());
        assert_eq!(
            snapshots.notice(),
            None,
            "somebody who asked for no snapshots does not need telling they have none"
        );
    }

    #[test]
    fn a_checkout_keeps_its_snapshots_beside_the_project_state_and_creates_nothing() {
        let directory = temporary();
        let root = directory.path().join("api");
        std::fs::create_dir_all(root.join(".git")).expect("the fixture repository is creatable");
        let project = Project::resolve(&root);
        let snapshots = Snapshots::new(&project, true);

        assert!(snapshots.enabled(), "git is a test prerequisite");
        assert!(
            snapshots
                .gitdir
                .starts_with(crate::project::data_home().expect("the data home resolves")),
            "{}",
            snapshots.gitdir.display()
        );
        assert!(
            snapshots.gitdir.to_string_lossy().contains(&format!(
                "snapshot{}{}",
                std::path::MAIN_SEPARATOR,
                project.slug()
            )),
            "{}",
            snapshots.gitdir.display()
        );
        assert!(
            !snapshots.gitdir.exists(),
            "asking where snapshots go must not create anything"
        );
    }

    #[test]
    fn a_walk_backwards_stops_at_each_user_message_in_turn() {
        let history = vec![
            message("msg_1", Role::User, Vec::new()),
            message("msg_2", Role::Assistant, Vec::new()),
            message("msg_3", Role::User, Vec::new()),
            message("msg_4", Role::Assistant, Vec::new()),
        ];

        let first = undo_anchor(&history, None).expect("there is a prompt to undo");
        assert_eq!(first.as_str(), "msg_3");

        let second = undo_anchor(&history, Some(&first)).expect("there is one before it");
        assert_eq!(second.as_str(), "msg_1");

        assert_eq!(
            undo_anchor(&history, Some(&second)),
            None,
            "the first prompt of a session has nothing behind it"
        );
    }

    #[test]
    fn a_walk_forwards_steps_one_prompt_at_a_time_and_then_runs_out() {
        let history = vec![
            message("msg_1", Role::User, Vec::new()),
            message("msg_2", Role::Assistant, Vec::new()),
            message("msg_3", Role::User, Vec::new()),
        ];

        let next = redo_anchor(&history, &MessageId::from("msg_1".to_owned()))
            .expect("there is a prompt after the first");
        assert_eq!(next.as_str(), "msg_3");
        assert_eq!(redo_anchor(&history, &next), None);
    }

    #[test]
    fn a_revert_collects_the_patches_from_the_anchor_on_and_leaves_the_earlier_ones() {
        let history = vec![
            message("msg_1", Role::User, Vec::new()),
            message(
                "msg_2",
                Role::Assistant,
                vec![patch("older", &["kept.txt"])],
            ),
            message("msg_3", Role::User, Vec::new()),
            message(
                "msg_4",
                Role::Assistant,
                vec![patch("newer", &["changed.txt"])],
            ),
        ];

        let patches = patches_from(&history, &MessageId::from("msg_3".to_owned()));

        assert_eq!(
            patches,
            vec![Patch {
                hash: "newer".to_owned(),
                files: vec!["changed.txt".to_owned()],
            }]
        );
    }

    /// Two steps that both touched a file must restore it to what it was
    /// before the *first* of them, or an undo of a turn would leave the file
    /// half-way through it.
    #[test]
    fn the_oldest_patch_naming_a_file_is_the_one_that_restores_it() {
        let patches = vec![
            Patch {
                hash: "before".to_owned(),
                files: vec!["a.txt".to_owned(), "b.txt".to_owned()],
            },
            Patch {
                hash: "midway".to_owned(),
                files: vec!["a.txt".to_owned(), "c.txt".to_owned()],
            },
        ];

        assert_eq!(
            dedupe(&patches),
            vec![
                ("before".to_owned(), "a.txt".to_owned()),
                ("before".to_owned(), "b.txt".to_owned()),
                ("midway".to_owned(), "c.txt".to_owned()),
            ]
        );
    }

    #[test]
    fn a_pathspec_is_literal_from_the_top_and_terminated_rather_than_separated() {
        assert_eq!(
            pathspecs(&["src/[a].rs".to_owned(), "b.rs".to_owned()]),
            ":(top,literal)src/[a].rs\0:(top,literal)b.rs\0"
        );
    }

    #[test]
    fn a_terminated_listing_does_not_end_in_an_empty_name() {
        assert_eq!(
            split_nul("one\0two\0"),
            vec!["one".to_owned(), "two".to_owned()]
        );
        assert!(split_nul("").is_empty());
    }

    /// Nothing in this module may reach the project's own git directory to
    /// write: the whole point of a separate repository is that a snapshot
    /// cannot cost somebody their index.
    #[test]
    fn the_snapshot_repository_is_never_the_project_one() {
        let directory = temporary();
        std::fs::create_dir(directory.path().join(".git")).expect("the marker is creatable");
        let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);

        assert!(
            !snapshots.gitdir.starts_with(directory.path()),
            "{}",
            snapshots.gitdir.display()
        );
        assert_ne!(snapshots.gitdir, Path::new(".git"));
    }

    /// A real, empty repository whose `.gitignore` matches everything — so
    /// `check-ignore`'s reply to a stdin request is exactly as large as the
    /// request, which is what this drill needs from it.
    fn ignore_everything(root: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(root)
            .status()
            .expect("git is a test prerequisite");
        assert!(status.success(), "git init failed");
        std::fs::write(root.join(".gitignore"), "*\n").expect("the ignore file is writable");
    }

    /// **Regression, pipe deadlock.** `run` used to write the whole stdin
    /// payload to completion before it read a single byte back — correct
    /// for a reply that fits in the OS pipe buffer, and a permanent hang for
    /// one that does not. `check-ignore --stdin -z` answers while it is
    /// still reading, so a large enough request makes it fill its own
    /// stdout pipe before this call has finished writing stdin: git blocks
    /// writing a reply nobody is draining, this call blocks writing a
    /// request nobody is reading, and neither is ever going to move again.
    ///
    /// Several hundred KB of candidates, every one matched by the blanket
    /// `.gitignore`, is comfortably past the ~64KB a pipe buffer usually
    /// holds — enough that the old, sequential `run` deadlocked on this
    /// exact call every time it was tried. **Non-vacuity, checked by hand
    /// while landing the fix:** reverting `run` to write-then-drain made
    /// this test hang and fail on the timeout below instead of passing.
    #[tokio::test]
    async fn a_huge_check_ignore_reply_does_not_deadlock_the_stdin_write_that_asked_for_it() {
        let directory = temporary();
        ignore_everything(directory.path());
        let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);
        assert!(snapshots.enabled(), "git is a test prerequisite");

        let candidates: Vec<String> = (0..4000)
            .map(|i| {
                format!(
                    "some/moderately/deeply/nested/directory/tree/padding/out/the/path/\
                     length/file-{i:06}.txt"
                )
            })
            .collect();

        let ignored = tokio::time::timeout(Duration::from_secs(30), snapshots.ignored(&candidates))
            .await
            .expect(
                "check-ignore answers well within this drill's patience; a hang here is the \
                 deadlock this test exists to catch",
            );

        assert_eq!(
            ignored.len(),
            candidates.len(),
            "every candidate matches the blanket .gitignore"
        );
    }
}

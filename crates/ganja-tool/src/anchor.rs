//! Opening a file through a directory this process is already holding open.
//!
//! No upstream counterpart: upstream's `write.ts` and `edit.ts` write by path,
//! through `Bun.write`, and inherit the race this module exists to close.
//!
//! Every write in this crate used to be two steps — decide the path is
//! allowed, then open it by name — and between those two steps the name is
//! somebody else's to redefine. `write` and `edit` are permission-gated, so a
//! person answered a question about a path; a link planted at that name after
//! the answer and before the open sends the bytes somewhere they were never
//! allowed to go. The window is small and it is real, and a tool that asks
//! before it acts is precisely the shape that makes it exploitable: the pause
//! for the answer *is* the window.
//!
//! What closes it is anchoring. The parent directory is canonicalised, then
//! walked one component at a time from the root with `O_NOFOLLOW`, so every
//! step is the directory it was checked to be or the walk refuses. The
//! descriptor that comes out of that walk is the only thing the rest of the
//! call speaks to: containment is verified against it, `rustix::fs::openat`
//! opens the final name relative to it — again `O_NOFOLLOW`, so the name itself
//! may not be a link — and the freshness stamp is an `fstat` on that same
//! descriptor rather than a fresh look at the path. Nothing between the check
//! and the write is reachable by renaming anything, because after the anchor
//! is taken no name is resolved again.
//!
//! Two consequences worth stating plainly:
//!
//! - **A link at the final component is refused outright**, wherever it
//!   points. That is what the port gains over the lexical guard below, which
//!   could only refuse a link that *escaped the project*. Editing a checkout's
//!   own symlinked file (a `CLAUDE.md` pointing at `AGENTS.md`, say) now means
//!   naming the file it points at, and the refusal says so.
//! - **Missing parents are created with `rustix::fs::mkdirat`** under the same
//!   anchor, so `create_dir_all`'s own walk — which resolves names, and
//!   therefore links — is not a second way in.
//!
//! Windows keeps the first of those two and not the second. It has no `openat`
//! to anchor to, but it does have `FILE_FLAG_OPEN_REPARSE_POINT`, which opens a
//! link rather than following it — so the handle a write gets back is the
//! planted link's own, the refusal is made on that handle, and nothing is ever
//! written through it. What is given up there, and only there, is:
//!
//! - **the mid-path directory swap**. The parent is resolved by path instead of
//!   being walked a component at a time, so a directory replaced *above* the
//!   final name between the check and the open goes unnoticed. The final
//!   component — the one somebody actually answered a question about — does
//!   not.
//! - **`mkdirat`-style parent creation**. Missing parents are made with
//!   `create_dir_all`, which resolves those names itself.
//!
//! What ACLs a created file lands with is a separate question and still an open
//! one, the same way the credential store's is (`tool/truncate.rs`).

use std::{
    ffi::OsString,
    fs::File,
    io,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use ganja_permission::project::Project;

use crate::ToolError;

/// Refuses `file` when the name it was opened by turned out to be a directory.
///
/// Asked of the open descriptor rather than of the path, for the reason the
/// whole module exists: a second look at the name is a second chance for
/// somebody to change what it means.
///
/// Only the unix descriptor walk asks; the windows open refuses a directory
/// by itself, so the question is gated with its one asker.
#[cfg(unix)]
fn refuse_directory(file: &File, at: &Path) -> Result<(), AnchorError> {
    // A descriptor whose metadata cannot be read is not a directory anybody can
    // prove, and failing the call on that would refuse ordinary files on any
    // filesystem with a thin `stat`. The read that follows will say so instead.
    if file.metadata().is_ok_and(|meta| meta.is_dir()) {
        return Err(AnchorError::Directory(at.to_owned()));
    }

    Ok(())
}

/// The modification stamp of a file that is already open, read from the
/// descriptor rather than from the path it was opened by.
///
/// What [`crate::FileTimes::record_stat`] and
/// [`crate::FileTimes::check_fresh_stat`] are documented to want: a
/// second resolution of the name would be a second chance for somebody to
/// point it somewhere else. [`None`] where the filesystem offers no stamp, on
/// which the read log deliberately fails open.
pub(crate) fn stamp(file: &File) -> Option<SystemTime> {
    file.metadata().and_then(|meta| meta.modified()).ok()
}

/// A file, addressed as a name relative to a directory this process holds
/// open, rather than as a path something else could redefine.
#[derive(Debug)]
pub(crate) struct Anchor {
    /// The opened parent directory. Every syscall the anchor makes is relative
    /// to this, which is what makes the name below unambiguous.
    #[cfg(unix)]
    dir: std::os::fd::OwnedFd,
    /// The parent's canonical path — where the descriptor above was verified
    /// to point when it was opened.
    parent: PathBuf,
    /// The final component, the only part of the path still resolved by name.
    name: OsString,
}

/// A file that could not be addressed.
#[derive(Debug)]
pub(crate) enum AnchorError {
    /// Something on the path is a symbolic link and the open refused to follow
    /// it. The path named is the component that refused.
    Link(PathBuf),
    /// The name is a directory, and no tool here writes or reads one as a file.
    ///
    /// Refused explicitly rather than left to the open, because the two
    /// platforms fail this differently and only one of them says anything
    /// useful: unix opens a directory happily and fails at the first read with
    /// `EISDIR`, while Windows refuses the open outright with "Access is
    /// denied" — a message that sends the model looking for a permission
    /// problem it does not have.
    Directory(PathBuf),
    /// The path names no file — a bare root, or one ending in `..`.
    Nameless(PathBuf),
    /// Whatever the filesystem said, and about which path it said it.
    Io(PathBuf, io::Error),
}

impl AnchorError {
    /// Whether this is "there is nothing there", which callers read as an
    /// absent file rather than as a failure — `edit` tells a file it may create
    /// from one it may not touch on exactly this.
    pub(crate) fn is_missing(&self) -> bool {
        matches!(
            self,
            Self::Io(_, error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                )
        )
    }
}

impl std::fmt::Display for AnchorError {
    /// Model-facing, like every other [`ToolError::Failed`] message: it says
    /// what was refused and what to do instead, because that text is the next
    /// thing the model reads.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(path) => write!(
                formatter,
                "{} is a symbolic link. Refusing to open it — a link is the one \
                 thing on a path that can change between the moment a call is \
                 allowed and the moment it runs. Name the file it points at if \
                 that is what you meant.",
                path.display()
            ),
            Self::Directory(path) => write!(
                formatter,
                "{} is a directory, not a file. Name a file inside it if that \
                 is what you meant.",
                path.display()
            ),
            Self::Nameless(path) => write!(formatter, "{} does not name a file", path.display()),
            Self::Io(path, error) => {
                write!(formatter, "{} could not be opened: {error}", path.display())
            }
        }
    }
}

impl From<AnchorError> for ToolError {
    fn from(error: AnchorError) -> Self {
        Self::Failed(error.to_string())
    }
}

impl Anchor {
    /// Where the anchored file actually is: the parent as it was verified, plus
    /// the name that will be opened relative to it.
    ///
    /// Deliberately not the whole path canonicalised — the final component is
    /// never followed, so this is the file's own location and not a link
    /// target's.
    pub(crate) fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }
}

/// Refuses a path that is inside the project by its text but lands outside it
/// once the filesystem has its say — that is, one a symbolic link redirects.
///
/// The permission gate resolves the same path and asks about it
/// (`permission.rs`, `outside`), but it answers *before* the call runs, and a
/// [`ganja_permission::Permissions::default`] set has no project to compare
/// anything against at all. This is the check the writing tools make for
/// themselves, before they open anything.
///
/// Only a link that *escapes* is refused. A call that openly names somewhere
/// outside the project is the gate's business and not this function's — the
/// user was asked about that directory and may well have allowed it — so the
/// rule is precisely the one the two writing tools state in their contracts:
/// do not follow a link out of the project. Text inside plus reality outside is
/// that link and nothing else.
///
/// Resolution mirrors `permission::resolve` deliberately, so the gate and the
/// tools cannot disagree about where a path goes: canonicalise what exists, so
/// every link along it and every `..` is already applied, then apply what does
/// not exist yet by text — a `..` there cannot cross a link, because nothing it
/// names is on the disk to be one.
///
/// This runs *before* the anchor is taken, and answers a different question.
/// [`Anchor::open`] closes the race; this says which side of the project line
/// the call is on, and produces the message that names the escape. Both are
/// wanted: without this, a link out of the project would be refused as a bare
/// link with no mention of where it led.
///
/// Shared by `write` and `edit`, which held a copy each until the anchoring
/// above needed a module to live in anyway.
pub(crate) fn refuse_link_escape(cwd: &Path, path: &Path) -> Result<(), ToolError> {
    escaped(cwd, path, &real_path(path))
}

/// The same rule, asked of the directory the call is actually holding open.
///
/// [`refuse_link_escape`] judges a path; this judges a descriptor, and the
/// difference is the whole point of the module: what it approves cannot be
/// moved afterwards, because the approval is of something already open.
pub(crate) fn refuse_anchor_escape(
    cwd: &Path,
    path: &Path,
    anchor: &Anchor,
) -> Result<(), ToolError> {
    escaped(cwd, path, &anchor.path())
}

/// Whether `path`, which really lands at `real`, is an escape from the project
/// `cwd` belongs to.
fn escaped(cwd: &Path, path: &Path, real: &Path) -> Result<(), ToolError> {
    let root = Project::resolve(cwd).root().to_owned();
    if real.starts_with(&root) {
        return Ok(());
    }
    if !claimed(cwd, path).starts_with(&root) {
        return Ok(());
    }

    Err(ToolError::Failed(format!(
        "{} is inside the project but resolves to {}, which is not: a symbolic \
         link on that path leads out of {}. Refusing to write through it — name \
         the real path if that is what you meant.",
        path.display(),
        real.display(),
        root.display()
    )))
}

/// Where `path` really lands: canonical for as much of it as exists, lexical
/// for whatever does not exist yet.
fn real_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut ancestors: Vec<Component> = path.components().collect();
    let mut rest: Vec<Component> = Vec::new();
    while let Some(component) = ancestors.pop() {
        rest.push(component);
        let existing: PathBuf = ancestors.iter().collect();
        if existing.as_os_str().is_empty() {
            continue;
        }
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            return apply(canonical, rest.iter().rev().copied());
        }
    }

    lexical(path)
}

/// What the call claimed the path was, before any link was followed: the
/// session directory in canonical form, with everything the call added applied
/// by text.
///
/// Only the anchor is canonicalised, and that asymmetry is the whole point —
/// the anchor is the one part of the path the model did not choose. Comparing
/// raw text against a canonical root would be wrong in the other direction on
/// any machine where the session directory is reached through a link, which on
/// macOS is every temporary directory there is (`/var` -> `/private/var`):
/// every path under it would read as "outside the project" and every write
/// would be refused.
fn claimed(cwd: &Path, path: &Path) -> PathBuf {
    let anchor = std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical(cwd));

    match path.strip_prefix(cwd) {
        Ok(rest) => apply(anchor, rest.components()),
        Err(_) => lexical(path),
    }
}

/// `path` made absolute with its `.` and `..` applied by text alone, which is
/// what it claims to be before the filesystem is consulted.
fn lexical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());

    apply(PathBuf::new(), absolute.components())
}

/// `base` with `rest` applied by text: `.` ignored, `..` popped, anything else
/// pushed.
fn apply<'a>(mut base: PathBuf, rest: impl Iterator<Item = Component<'a>>) -> PathBuf {
    for component in rest {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            other => base.push(other.as_os_str()),
        }
    }

    base
}

/// Splits a directory path into the deepest part of it that exists —
/// canonicalised, so the walk that follows has nothing left to resolve — and
/// the components that do not exist yet, outermost first.
///
/// Only the unix `mkdirat` walk needs the split; gated with its one caller
/// rather than left dead where the windows open creates parents itself.
#[cfg(unix)]
fn split_existing(dir: &Path) -> Result<(PathBuf, Vec<OsString>), AnchorError> {
    let mut head = dir.to_owned();
    let mut missing: Vec<OsString> = Vec::new();

    loop {
        if let Ok(canonical) = std::fs::canonicalize(&head) {
            missing.reverse();
            return Ok((canonical, missing));
        }
        let name = head
            .file_name()
            .ok_or_else(|| AnchorError::Nameless(dir.to_owned()))?
            .to_owned();
        missing.push(name);
        if !head.pop() {
            return Err(AnchorError::Nameless(dir.to_owned()));
        }
    }
}

/// The two halves an [`Anchor`] is built from: the directory to open, with
/// every `.` and `..` already applied against what is on disk, and the final
/// component, which is the one name the anchor still resolves.
///
/// Only the *parent* is normalised. Normalising the whole path would follow a
/// link at the final component, which is the one thing this module refuses to
/// do.
fn split_path(path: &Path) -> Result<(PathBuf, OsString), AnchorError> {
    let name = match path.components().next_back() {
        Some(Component::Normal(name)) => name.to_owned(),
        _ => return Err(AnchorError::Nameless(path.to_owned())),
    };
    let parent = path
        .parent()
        .ok_or_else(|| AnchorError::Nameless(path.to_owned()))?;

    Ok((real_path(parent), name))
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsStr,
        fs::File,
        io,
        os::{fd::OwnedFd, unix::ffi::OsStrExt as _},
        path::{Component, Path, PathBuf},
    };

    use rustix::{
        fs::{self, Mode, OFlags},
        io::Errno,
    };

    use super::{Anchor, AnchorError, split_existing, split_path};

    /// Mode a created file is asked for, which the process umask then narrows.
    /// `std::fs::write`'s own default, kept so anchoring a write does not
    /// quietly change the bits a file lands with.
    const CREATE_MODE: Mode = Mode::from_raw_mode(0o666);

    /// Mode a created directory is asked for. `std::fs::create_dir_all`'s
    /// default, for the same reason.
    const CREATE_DIR_MODE: Mode = Mode::from_raw_mode(0o777);

    impl Anchor {
        /// Opens the directory holding `path` and keeps it open, so everything
        /// the caller does afterwards happens relative to a directory that was
        /// verified rather than to a name that can be redefined.
        ///
        /// `create_parents` makes the missing part of the path with `mkdirat`
        /// under the same anchor — `create_dir_all` would resolve those names
        /// itself, which is the hole this is closing.
        pub(crate) fn open(path: &Path, create_parents: bool) -> Result<Self, AnchorError> {
            let (parent, name) = split_path(path)?;
            let (existing, missing) = split_existing(&parent)?;
            if !missing.is_empty() && !create_parents {
                return Err(AnchorError::Io(
                    parent,
                    io::Error::from(io::ErrorKind::NotFound),
                ));
            }

            let mut walked = existing.clone();
            let mut dir = open_root(&existing)?;
            for component in missing {
                walked.push(&component);
                mkdirat(&dir, &component, &walked)?;
                dir = openat_dir(&dir, &component, &walked)?;
            }

            Ok(Self {
                dir,
                parent: walked,
                name,
            })
        }

        /// Opens the anchored file for reading, refusing to follow a link
        /// planted at its name.
        ///
        /// A directory opens perfectly well here and fails at the first read,
        /// so the refusal is made on the descriptor already in hand — no name
        /// is resolved a second time to ask the question.
        pub(crate) fn read(&self) -> Result<File, AnchorError> {
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let file = openat(&self.dir, &self.name, flags, Mode::empty(), &self.path())
                .map(File::from)?;
            super::refuse_directory(&file, &self.path())?;

            Ok(file)
        }

        /// Opens the anchored file for writing, saying whether it was already
        /// there. Nothing is truncated: the caller has a freshness rule to
        /// apply first, and applying it to a file already emptied would be a
        /// check that cannot fail usefully.
        ///
        /// A file that is not there yet is created with `O_EXCL`, which never
        /// follows a link at the name — the same reason `truncate.rs` creates
        /// its spill files that way.
        pub(crate) fn write(&self) -> Result<(File, bool), AnchorError> {
            let existing = OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fresh = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
            let path = self.path();

            match openat(&self.dir, &self.name, existing, Mode::empty(), &path) {
                Ok(file) => Ok((File::from(file), true)),
                Err(error) if error.is_missing() => {
                    match openat(&self.dir, &self.name, fresh, CREATE_MODE, &path) {
                        Ok(file) => Ok((File::from(file), false)),
                        // Somebody created the name in between. Whatever they
                        // put there is opened by the rule above, which refuses
                        // a link — so the race is answered, not retried blind.
                        Err(AnchorError::Io(_, raced))
                            if raced.kind() == io::ErrorKind::AlreadyExists =>
                        {
                            openat(&self.dir, &self.name, existing, Mode::empty(), &path)
                                .map(|file| (File::from(file), true))
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
    }

    /// Walks `canonical` from the root, one component at a time, refusing to
    /// follow a link at any of them.
    ///
    /// `canonical` came out of `canonicalize`, so every component *was* a real
    /// directory a moment ago; this is what notices when one has been replaced
    /// since. Opening the whole path in one call would only ever check the
    /// last.
    fn open_root(canonical: &Path) -> Result<OwnedFd, AnchorError> {
        let mut walked = PathBuf::from("/");
        let mut dir = open_dir(Path::new("/"), &walked)?;

        for component in canonical.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    walked.push(name);
                    dir = openat_dir(&dir, name, &walked)?;
                }
                // A canonical path holds neither a `.` nor a `..`, and neither
                // does one this module built. Refusing beats assuming.
                _ => return Err(AnchorError::Nameless(canonical.to_owned())),
            }
        }

        Ok(dir)
    }

    /// Opens a directory by absolute path, refusing a link at its last
    /// component. Only ever called for the root itself.
    fn open_dir(path: &Path, at: &Path) -> Result<OwnedFd, AnchorError> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let result = retry(|| fs::open(path, flags, Mode::empty()));

        decode(result, at)
    }

    /// Opens `name` inside `dir` as a directory, refusing to follow a link.
    fn openat_dir(dir: &OwnedFd, name: &OsStr, at: &Path) -> Result<OwnedFd, AnchorError> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;

        openat(dir, name, flags, Mode::empty(), at)
    }

    /// Opens `name` inside `dir` with `flags`, owning the descriptor before an
    /// early return can leak it.
    fn openat(
        dir: &OwnedFd,
        name: &OsStr,
        flags: OFlags,
        mode: Mode,
        at: &Path,
    ) -> Result<OwnedFd, AnchorError> {
        let name = checked_name(name, at)?;
        let result = retry(|| fs::openat(dir, name, flags, mode));

        decode(result, at)
    }

    /// Creates `name` inside `dir`, treating "it is already there" as success —
    /// which is what `create_dir_all` does, and what two calls racing to make
    /// the same directory need.
    fn mkdirat(dir: &OwnedFd, name: &OsStr, at: &Path) -> Result<(), AnchorError> {
        let name = checked_name(name, at)?;

        match retry(|| fs::mkdirat(dir, name, CREATE_DIR_MODE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(AnchorError::Io(at.to_owned(), error.into())),
        }
    }

    /// Runs a syscall again when a signal interrupted it, which is a thing that
    /// happens to a process whose tools spawn children.
    fn retry<T>(mut call: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
        loop {
            match call() {
                Err(Errno::INTR) => {}
                result => return result,
            }
        }
    }

    /// Decodes an anchored operation's failure without losing its owned value.
    ///
    /// A refused link is the error this module exists to produce, and which
    /// errno says so is not agreed on: Linux and macOS answer `ELOOP`, the BSDs
    /// answer `EMLINK`. Both are read as the same refusal.
    fn decode<T>(result: rustix::io::Result<T>, at: &Path) -> Result<T, AnchorError> {
        match result {
            Ok(value) => Ok(value),
            Err(Errno::LOOP | Errno::MLINK) => Err(AnchorError::Link(at.to_owned())),
            Err(error) => Err(AnchorError::Io(at.to_owned(), error.into())),
        }
    }

    /// Refuses a NUL-bearing component before rustix converts it to a C string.
    ///
    /// Such a component cannot come from a real path, but it can come from an
    /// argument the model invented. Rustix would return `EINVAL`; keeping the
    /// explicit check preserves the actionable refusal this module already
    /// exposed instead of replacing it with the platform's generic wording.
    fn checked_name<'a>(name: &'a OsStr, at: &Path) -> Result<&'a OsStr, AnchorError> {
        if name.as_bytes().contains(&b'\0') {
            return Err(AnchorError::Io(
                at.to_owned(),
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a path component holds a NUL byte",
                ),
            ));
        }

        Ok(name)
    }
}

/// Windows, which has no `openat` to anchor to but can still refuse a link at
/// the name it was asked about.
///
/// The parent is resolved by path and the missing part of it created with
/// `create_dir_all`, which is where the two divergences in the module docs come
/// from. What is *not* given up is the refusal that matters most: every open
/// below asks for the reparse point itself rather than what it points at, and
/// then refuses it. A link planted at the answered name between the dialog and
/// the write is therefore caught here exactly as it is on unix — the handle
/// that comes back is the link's own, and nothing is ever written through it.
#[cfg(windows)]
mod windows {
    use std::{
        fs::{File, OpenOptions},
        io,
        os::windows::fs::OpenOptionsExt as _,
        path::Path,
    };

    use super::{Anchor, AnchorError, split_path};

    /// `FILE_FLAG_OPEN_REPARSE_POINT` from `winbase.h`. Opens a reparse point —
    /// which is what a symbolic link, a junction and a mount point all are —
    /// instead of following it, so the handle names the link and the check
    /// below can see one.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    /// `FILE_FLAG_BACKUP_SEMANTICS` from `winbase.h`. Permits a handle to a
    /// directory at all. Without it a directory refuses the open outright and
    /// the refusal a caller gets is "Access is denied", which says nothing
    /// about what was actually wrong.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    impl Anchor {
        pub(crate) fn open(path: &Path, create_parents: bool) -> Result<Self, AnchorError> {
            let (parent, name) = split_path(path)?;

            if create_parents {
                std::fs::create_dir_all(&parent)
                    .map_err(|error| AnchorError::Io(parent.clone(), error))?;
            } else if !parent.is_dir() {
                return Err(AnchorError::Io(
                    parent,
                    io::Error::from(io::ErrorKind::NotFound),
                ));
            }

            Ok(Self { parent, name })
        }

        pub(crate) fn read(&self) -> Result<File, AnchorError> {
            let path = self.path();
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(&path)
                .map_err(|error| AnchorError::Io(path.clone(), error))?;
            judge(&file, &path)?;

            Ok(file)
        }

        /// Opens the anchored file for writing, saying whether it was already
        /// there — and refusing a link at the name before a byte is written.
        ///
        /// The order is what makes that true. Nothing is truncated (the
        /// caller has a freshness rule to apply first), the handle is the
        /// reparse point's own rather than its target's, and the refusal is
        /// made on that handle: a link planted at the name gets a handle
        /// nobody writes to and an error naming it. A file that is not there
        /// yet is created with `create_new`, which fails rather than follow a
        /// link somebody planted in the meantime.
        pub(crate) fn write(&self) -> Result<(File, bool), AnchorError> {
            let path = self.path();
            let flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS;

            let opened = OpenOptions::new()
                .write(true)
                .custom_flags(flags)
                .open(&path);
            let (file, existed) = match opened {
                Ok(file) => (file, true),
                Err(error) if missing(&error) => {
                    match OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .custom_flags(flags)
                        .open(&path)
                    {
                        Ok(file) => (file, false),
                        // Somebody created the name in between. Whatever they
                        // put there is opened by the rule above, which refuses
                        // a link — so the race is answered, not retried blind.
                        Err(raced) if raced.kind() == io::ErrorKind::AlreadyExists => {
                            let file = OpenOptions::new()
                                .write(true)
                                .custom_flags(flags)
                                .open(&path)
                                .map_err(|error| AnchorError::Io(path.clone(), error))?;
                            (file, true)
                        }
                        Err(error) => return Err(AnchorError::Io(path, error)),
                    }
                }
                Err(error) => return Err(AnchorError::Io(path, error)),
            };
            judge(&file, &path)?;

            Ok((file, existed))
        }
    }

    /// Refuses a handle that turned out to name a link or a directory rather
    /// than a file.
    ///
    /// Both questions are asked of the open handle, never of the path again,
    /// which is the one piece of the unix discipline this platform can still
    /// keep: whatever the name meant when it was opened is what is judged.
    fn judge(file: &File, at: &Path) -> Result<(), AnchorError> {
        let Ok(meta) = file.metadata() else {
            return Ok(());
        };
        if meta.file_type().is_symlink() {
            return Err(AnchorError::Link(at.to_owned()));
        }
        if meta.is_dir() {
            return Err(AnchorError::Directory(at.to_owned()));
        }

        Ok(())
    }

    /// Whether `error` is "there is nothing at that name".
    ///
    /// Two kinds, because a name whose parent is a file rather than a
    /// directory reports the second and means the same thing to a caller that
    /// is about to create it.
    fn missing(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    }
}

#[cfg(test)]
#[path = "anchor_tests.rs"]
mod tests;

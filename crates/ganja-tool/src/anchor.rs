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
//! call speaks to: containment is verified against it, `openat` opens the
//! final name relative to it — again `O_NOFOLLOW`, so the name itself may not
//! be a link — and the freshness stamp is an `fstat` on that same descriptor
//! rather than a fresh look at the path. Nothing between the check and the
//! write is reachable by renaming anything, because after the anchor is taken
//! no name is resolved again.
//!
//! Two consequences worth stating plainly:
//!
//! - **A link at the final component is refused outright**, wherever it
//!   points. That is what the port gains over the lexical guard below, which
//!   could only refuse a link that *escaped the project*. Editing a checkout's
//!   own symlinked file (a `CLAUDE.md` pointing at `AGENTS.md`, say) now means
//!   naming the file it points at, and the refusal says so.
//! - **Missing parents are created with `mkdirat`** under the same anchor, so
//!   `create_dir_all`'s own walk — which resolves names, and therefore links —
//!   is not a second way in.
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
        ffi::{CStr, CString, OsStr},
        fs::File,
        io,
        os::{
            fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
            unix::ffi::OsStrExt as _,
        },
        path::{Component, Path, PathBuf},
    };

    use super::{Anchor, AnchorError, split_existing, split_path};

    /// Mode a created file is asked for, which the process umask then narrows.
    /// `std::fs::write`'s own default, kept so anchoring a write does not
    /// quietly change the bits a file lands with.
    const CREATE_MODE: libc::mode_t = 0o666;

    /// Mode a created directory is asked for. `std::fs::create_dir_all`'s
    /// default, for the same reason.
    const CREATE_DIR_MODE: libc::mode_t = 0o777;

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
            let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let file = openat(&self.dir, &self.name, flags, 0, &self.path()).map(File::from)?;
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
            let existing = libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let fresh = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC;
            let path = self.path();

            match openat(&self.dir, &self.name, existing, 0, &path) {
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
                            openat(&self.dir, &self.name, existing, 0, &path)
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
        let mut dir = open_dir(c"/", &walked)?;

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
    fn open_dir(path: &CStr, at: &Path) -> Result<OwnedFd, AnchorError> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: the path is a valid C string and the variadic mode argument
        // is unread without `O_CREAT`. The descriptor is owned immediately.
        let raw = retry(|| unsafe { libc::open(path.as_ptr(), flags) });

        own(raw, at)
    }

    /// Opens `name` inside `dir` as a directory, refusing to follow a link.
    fn openat_dir(dir: &OwnedFd, name: &OsStr, at: &Path) -> Result<OwnedFd, AnchorError> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

        openat(dir, name, flags, 0, at)
    }

    /// Opens `name` inside `dir` with `flags`, owning the descriptor before an
    /// early return can leak it.
    fn openat(
        dir: &OwnedFd,
        name: &OsStr,
        flags: libc::c_int,
        mode: libc::mode_t,
        at: &Path,
    ) -> Result<OwnedFd, AnchorError> {
        let name = cstring(name, at)?;
        // SAFETY: the descriptor is open for the duration of the call, the name
        // is a valid C string, and `mode` is read only when `flags` carries
        // `O_CREAT`. The descriptor is owned immediately.
        let raw = retry(|| unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags,
                libc::c_uint::from(mode),
            )
        });

        own(raw, at)
    }

    /// Creates `name` inside `dir`, treating "it is already there" as success —
    /// which is what `create_dir_all` does, and what two calls racing to make
    /// the same directory need.
    fn mkdirat(dir: &OwnedFd, name: &OsStr, at: &Path) -> Result<(), AnchorError> {
        let name = cstring(name, at)?;
        // SAFETY: the descriptor is open for the duration of the call and the
        // name is a valid C string.
        let result =
            retry(|| unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), CREATE_DIR_MODE) });
        if result >= 0 {
            return Ok(());
        }

        match io::Error::last_os_error() {
            error if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            error => Err(AnchorError::Io(at.to_owned(), error)),
        }
    }

    /// Runs a syscall again when a signal interrupted it, which is a thing that
    /// happens to a process whose tools spawn children.
    fn retry(mut call: impl FnMut() -> libc::c_int) -> libc::c_int {
        loop {
            let result = call();
            if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return result;
            }
        }
    }

    /// Takes ownership of a raw descriptor, or reads why there is not one.
    ///
    /// A refused link is the error this module exists to produce, and which
    /// errno says so is not agreed on: Linux and macOS answer `ELOOP`, the BSDs
    /// answer `EMLINK`. Both are read as the same refusal.
    fn own(raw: libc::c_int, at: &Path) -> Result<OwnedFd, AnchorError> {
        if raw >= 0 {
            // SAFETY: `raw` is a fresh descriptor this call owns, handed
            // straight to the type that will close it.
            return Ok(unsafe { OwnedFd::from_raw_fd(raw) });
        }

        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ELOOP | libc::EMLINK) => Err(AnchorError::Link(at.to_owned())),
            _ => Err(AnchorError::Io(at.to_owned(), error)),
        }
    }

    /// A path component as the C interface needs it. A name holding a NUL byte
    /// cannot come from a real path, but it can come from an argument the model
    /// invented, and it is refused rather than truncated.
    fn cstring(name: &OsStr, at: &Path) -> Result<CString, AnchorError> {
        CString::new(name.as_bytes()).map_err(|_| {
            AnchorError::Io(
                at.to_owned(),
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a path component holds a NUL byte",
                ),
            )
        })
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
mod tests {
    use std::path::Path;

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
}

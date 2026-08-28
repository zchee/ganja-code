//! Which project a working directory belongs to, and where its state lives.
//!
//! Spec: upstream `packages/core/src/project.ts`. A directory inside a
//! checkout belongs to the checkout, not to itself, so the whole repository
//! shares one set of permission rules, one session history and one set of
//! everything else a later phase stores: opening `src/` and opening the
//! repository root are the same project.
//!
//! Upstream discovers the working tree by shelling out to `git rev-parse`, then
//! names the project after a SHA-1 of its origin remote URL, falling back to
//! the hash of the root commit and finally to a shared `global` project. This
//! port walks up for a `.git` entry instead of spawning git — resolution
//! happens on the way to reading a small file, and a subprocess per session
//! start is a poor trade — and names the project after the resolved root path.
//! Keying on the path rather than on the remote is a deliberate difference:
//! two worktrees of one repository get their own rules here, where upstream
//! would have them share, and a checkout with no remote is still a project
//! rather than falling into a global bucket shared with every other one.
//!
//! The slug is Claude Code's: the whole absolute path with every character
//! that is not ASCII alphanumeric replaced by `-`, so `/Users/me/work/api`
//! becomes `-Users-me-work-api` and a person scanning the data directory reads
//! the path back off the listing. A path too long to be a filename is cut and
//! given a hash of the original.
//!
//! Two roots that differ only where that reduction flattens them — `/a/b` and
//! `/a-b` — share a slug, where the `<directory name>-<hash>` scheme this
//! replaced told them apart. Claude accepts that collision and so does this: a
//! slug somebody can read back to a path is worth more than the pair of
//! directories nobody has.
//!
//! Nothing here creates a directory. Resolution is a pure question about a
//! path, and answering it should not litter the data directory with folders
//! for projects that never store anything; whoever writes creates the parents
//! it needs on the way.

use std::path::{Path, PathBuf};
use std::{fs, io};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};

/// Directory ganja keeps its state in, under the XDG data home. Matches
/// `ganja-core`'s `auth`, which resolves the credential store the same way.
const DIRECTORY: &str = "ganja";

/// Directory per-project state is grouped under, mirroring upstream's
/// `~/.local/share/opencode/project/`.
const PROJECTS: &str = "project";

/// What git leaves at the root of a working tree: a directory in a plain
/// checkout, a file pointing elsewhere in a linked worktree or a submodule.
/// Either answers "the tree starts here".
const GIT: &str = ".git";

/// Longest a slug may be before it is cut short and given a hash, in
/// characters. Claude Code's own 200, which leaves room for the hash inside
/// the 255 bytes filesystems still cap a path component at.
const MAX: usize = 200;

/// The digits [`base36`] renders into, in JavaScript's order and case.
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Offset basis and prime of FNV-1a, 64 bit.
///
/// The hash is spelled out here rather than pulled from a crate because the
/// value ends up in a directory name that has to keep meaning the same thing
/// across upgrades: `DefaultHasher` is explicitly allowed to change between
/// releases, which would silently orphan every stored ruleset. FNV is fixed
/// forever, and nothing here needs a hash to resist an adversary — the worst a
/// collision does is make two projects share a directory.
const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// See [`FNV_BASIS`].
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The project a directory belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// Top of the working tree, or the directory itself when it is not in one.
    root: PathBuf,
    /// Stable name for [`Project::root`], usable as a path component.
    slug: String,
}

/// A project's data directory could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// There is no home directory to resolve the XDG data home against.
    #[error("the home directory holding project state could not be located: {source}")]
    Home {
        /// What the lookup said.
        #[source]
        source: io::Error,
    },
}

impl Project {
    /// The project `cwd` belongs to.
    ///
    /// Infallible by construction: a path that cannot be canonicalised is used
    /// as it stands rather than failing, because refusing to name a project is
    /// never a better outcome than naming it from a slightly less tidy path.
    #[must_use]
    pub fn resolve(cwd: &Path) -> Self {
        let root = root_of(cwd);
        let slug = slug_for(&root);

        Self { root, slug }
    }

    /// Top of the working tree this project lives in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable name for the project, safe to use as a path component.
    ///
    /// The same root always produces the same slug, which is what lets a later
    /// run find what an earlier one stored. Two roots that differ only in the
    /// characters the slug flattens can share one; the module docs argue that
    /// trade.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// Where this project's state belongs: `<data home>/ganja/project/<slug>`.
    ///
    /// The directory is not created here; the first writer creates it.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::Home`] when there is no home directory to
    /// resolve the path against.
    pub fn data_dir(&self) -> Result<PathBuf, ProjectError> {
        Ok(data_home()?.join(PROJECTS).join(&self.slug))
    }
}

/// Where ganja keeps everything it stores: `<data home>/ganja`.
///
/// Per-project state hangs off [`Project::data_dir`] under here; what does not
/// belong to one project — the model catalog's cache, the snapshot
/// repositories — sits beside it. Nothing is created by asking.
///
/// # Errors
///
/// Returns [`ProjectError::Home`] when there is no home directory to resolve
/// the path against.
pub fn data_home() -> Result<PathBuf, ProjectError> {
    let base =
        Xdg::new().map_err(|source| ProjectError::Home { source: io::Error::other(source) })?;

    Ok(base.data_dir().join(DIRECTORY))
}

/// Writes `bytes` to a newly created file, and does not return until they are
/// on the disk.
///
/// `create_new` is `O_CREAT | O_EXCL`, which does not follow a symbolic link
/// at the final component: these names are predictable enough for somebody
/// sharing the machine to plant one, and an open that followed it would write
/// through to wherever it led — and then have that file renamed over the real
/// one by the caller's own rename-into-place.
///
/// Here rather than beside either caller because both write the same shape of
/// small file into a directory this module already owns the idea of — the
/// stored permission answers and the stored theme choice — and the two had a
/// copy each. The copies had already drifted on the question this one settles:
/// [`std::fs::File::sync_all`] before the rename, so a machine that loses
/// power between the write and the rename finds either the old file or the new
/// one and never an empty one. That costs a flush on a file somebody's
/// keystroke produced, which is the direction to be wrong in.
///
/// # Errors
///
/// Returns whatever the create, the write or the flush returned. A name that
/// already exists is not one of them: that is either a write that died before
/// its rename or something planted to catch this one, and unlinking the *name*
/// — never whatever it pointed at — and creating it again exclusively settles
/// both, with a link planted in between failing the retry outright.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::OpenOptions::new().write(true).create_new(true).open(path)?
        }
        result => result?,
    };
    file.write_all(bytes)?;

    file.sync_all()
}

/// The working tree `cwd` sits in, or `cwd` itself when it sits in none.
fn root_of(cwd: &Path) -> PathBuf {
    let start = absolute(cwd);

    start
        .ancestors()
        .find(|ancestor| ancestor.join(GIT).exists())
        .map_or(start.clone(), Path::to_path_buf)
}

/// `path` as an absolute path, with symbolic links and `..` resolved where the
/// filesystem can do it.
///
/// Canonicalising matters for the slug: `~/work/api`, `~/work/./api` and a
/// symbolic link to either have to be one project, or a rule remembered
/// through one of them would not apply through the others. A path that does
/// not exist yet cannot be canonicalised, so it is only made absolute.
fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// A name for `root`, readable as the path it came from.
///
/// Claude Code's scheme, reproduced: every character that is not ASCII
/// alphanumeric becomes `-`, so `/Users/me/work/api` is `-Users-me-work-api`,
/// case and all. A reduced path longer than [`MAX`] is cut there and given
/// [`digest32`] of the original, because a filesystem component still has to
/// fit in 255 bytes.
///
/// What this replaced was a `<directory name>-<FNV of the path>` slug of
/// ganja's own; the module docs carry what that trade costs. What is kept is
/// the property that matters — the same root always reduces the same way, so a
/// later run finds what an earlier one stored.
///
/// Nothing migrates on the way in. A slug is only ever where to look for what
/// an earlier run wrote, so a machine upgrading past this change leaves its old
/// directories where they are and starts new ones beside them.
///
/// One divergence stands, and it is in the input rather than the reduction:
/// Claude normalises the resolved path to NFC before reducing it, where
/// [`absolute`] hands over whatever the filesystem returned. It costs nothing
/// here — every resolution of one directory goes through the same call and so
/// reduces the same way — and buys nothing either, since these directories are
/// ganja's own and are never read by the tool whose scheme this is.
fn slug_for(root: &Path) -> String {
    let path = root.to_string_lossy();
    // Claude's regex and its hash both step through UTF-16 code units, and
    // that is what decides how many dashes a character outside ASCII leaves:
    // one for `é`, two for anything above the basic plane. Stepping the same
    // units is what makes the two answers agree.
    let reduced: String = path
        .encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect();

    if reduced.len() <= MAX {
        return reduced;
    }

    // Every character of `reduced` is ASCII, so cutting by bytes cuts by
    // characters and lands on a boundary.
    format!("{}-{}", &reduced[..MAX], base36(digest32(&path)))
}

/// Claude Code's string hash of `path`, as the magnitude it renders.
///
/// `hash = hash * 31 + unit` over UTF-16 code units, wrapped to 32 bits
/// signed — Java's `String.hashCode`, which is what the JavaScript spells as
/// `(h << 5) - h + c | 0` and what Claude uses to tell apart the paths its
/// slug had to cut short. Written out here for the reason [`FNV_BASIS`] gives:
/// the value ends up in a directory name, which has to keep meaning the same
/// thing across upgrades.
///
/// The magnitude rather than the value because Claude takes `Math.abs` of it,
/// and JavaScript's arithmetic is wide enough to negate [`i32::MIN`] where
/// Rust's is not: [`i32::unsigned_abs`] is the operation that agrees.
fn digest32(path: &str) -> u32 {
    let mut hash: i32 = 0;
    for unit in path.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
    }

    hash.unsigned_abs()
}

/// `value` in base 36, JavaScript's `Number.prototype.toString(36)`.
///
/// Written out rather than taken from a formatting crate for [`FNV_BASIS`]'s
/// reason once more: the alphabet and the case are part of a directory name,
/// and a crate is free to change its mind about either.
fn base36(value: u32) -> String {
    if value == 0 {
        return "0".to_owned();
    }

    let mut digits = Vec::new();
    let mut left = value;
    while left > 0 {
        digits.push(BASE36[(left % 36) as usize]);
        left /= 36;
    }
    digits.reverse();

    String::from_utf8(digits).expect("base 36 digits are ASCII")
}

/// A stable 64-bit hash of `root`, as 16 hexadecimal characters.
///
/// The path is hashed as its lossy UTF-8 form so the answer does not depend on
/// the platform's path representation. Two paths that differ only in bytes no
/// encoding can express would collide, which costs them a shared directory and
/// nothing else.
///
/// Shared with `ganja-core`'s `snapshot`, which names a worktree the same way
/// and for the same reason: the value ends up in a directory name that has to
/// keep meaning the same thing across upgrades. That sharing is what makes it
/// public rather than crate-private.
pub fn digest(root: &Path) -> String {
    let mut hash = FNV_BASIS;
    for byte in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    format!("{hash:016x}")
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;

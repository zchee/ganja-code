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
//! The slug is the root's directory name, reduced to characters that are safe
//! in a path, followed by a hash of the whole absolute path. The name is there
//! so a human can tell which directory is which; the hash is what makes it
//! unambiguous, since `~/work/api` and `~/play/api` share a name.
//!
//! Nothing here creates a directory. Resolution is a pure question about a
//! path, and answering it should not litter the data directory with folders
//! for projects that never store anything; whoever writes creates the parents
//! it needs on the way.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

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

/// Longest the readable half of a slug may be, in characters. Long enough to
/// recognise a project by, short enough to leave room for the hash on the
/// filesystems that still cap a component at 255 bytes.
const NAME: usize = 48;

/// What a slug is called when the root has no usable name of its own — the
/// filesystem root itself, in practice.
const UNNAMED: &str = "root";

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
    /// The same root always produces the same slug, and two different roots
    /// produce different ones, which is what lets a later run find what an
    /// earlier one stored.
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
    let base = Xdg::new().map_err(|source| ProjectError::Home {
        source: io::Error::other(source),
    })?;

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

    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
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

/// A name for `root` that is unique to it and readable by a person.
fn slug_for(root: &Path) -> String {
    format!("{}-{}", readable(root), digest(root))
}

/// `root`'s own name, reduced to characters that mean the same thing on every
/// filesystem.
fn readable(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut reduced = String::with_capacity(name.len());
    for character in name.chars().take(NAME) {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            reduced.push(character.to_ascii_lowercase());
        } else if !reduced.ends_with('-') {
            reduced.push('-');
        }
    }

    // A leading dot would make the project directory hidden, and a name that
    // reduced to nothing but separators says nothing at all.
    let trimmed = reduced.trim_matches(['-', '.'].as_slice());
    if trimmed.is_empty() {
        UNNAMED.to_owned()
    } else {
        trimmed.to_owned()
    }
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
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{Project, UNNAMED};

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    #[test]
    fn a_directory_inside_a_checkout_resolves_to_the_checkout() {
        let directory = temporary();
        let root = directory.path().join("api");
        let nested = root.join("crates").join("core").join("src");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");

        let outer = Project::resolve(&root);
        let inner = Project::resolve(&nested);

        assert_eq!(inner.root(), outer.root());
        assert_eq!(inner.slug(), outer.slug());
        assert!(inner.slug().starts_with("api-"), "{}", inner.slug());
    }

    /// A linked worktree and a submodule both mark their root with a `.git`
    /// file rather than a directory, and both are working trees.
    #[test]
    fn a_git_file_marks_a_root_just_as_a_git_directory_does() {
        let directory = temporary();
        let root = directory.path().join("worktree");
        let nested = root.join("src");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/w")
            .expect("the fixture marker is writable");

        assert_eq!(
            Project::resolve(&nested).root(),
            Project::resolve(&root).root()
        );
    }

    #[test]
    fn a_directory_outside_any_checkout_is_its_own_project() {
        let directory = temporary();
        let loose = directory.path().join("loose");
        fs::create_dir(&loose).expect("the fixture directory is creatable");

        assert_eq!(
            Project::resolve(&loose).root(),
            fs::canonicalize(&loose).expect("the fixture exists")
        );
    }

    #[test]
    fn the_same_path_always_slugs_the_same_and_different_paths_do_not() {
        let directory = temporary();
        let left = directory.path().join("work").join("api");
        let right = directory.path().join("play").join("api");
        fs::create_dir_all(&left).expect("the fixture tree is creatable");
        fs::create_dir_all(&right).expect("the fixture tree is creatable");

        assert_eq!(
            Project::resolve(&left).slug(),
            Project::resolve(&left).slug(),
            "the same path has to keep its stored state"
        );
        assert_ne!(
            Project::resolve(&left).slug(),
            Project::resolve(&right).slug(),
            "projects that share a name must not share their state"
        );

        // Both halves of the slug carry their weight: the name is readable,
        // the hash is what makes it unambiguous.
        let slug = Project::resolve(&left).slug().to_owned();
        let (name, hash) = slug.rsplit_once('-').expect("a slug has both halves");
        assert_eq!(name, "api");
        assert_eq!(hash.len(), 16, "{slug}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{slug}");
    }

    /// A path that reaches the same directory by a different route is the same
    /// project, or a rule remembered through one route would not apply through
    /// the other.
    #[test]
    fn an_untidy_path_resolves_to_the_same_project() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir(&root).expect("the fixture directory is creatable");
        let untidy = directory
            .path()
            .join("api")
            .join(".")
            .join("..")
            .join("api");

        assert_eq!(
            Project::resolve(&untidy).slug(),
            Project::resolve(&root).slug()
        );
    }

    #[test]
    fn a_name_that_is_not_path_safe_is_reduced_rather_than_refused() {
        let directory = temporary();
        let awkward = directory.path().join("My Project (v2)!");
        fs::create_dir(&awkward).expect("the fixture directory is creatable");

        let slug = Project::resolve(&awkward).slug().to_owned();
        let (name, _) = slug.rsplit_once('-').expect("a slug has both halves");

        assert_eq!(name, "my-project-v2");
    }

    #[test]
    fn the_filesystem_root_still_gets_a_name() {
        let slug = Project::resolve(Path::new("/")).slug().to_owned();

        assert!(slug.starts_with(&format!("{UNNAMED}-")), "{slug}");
    }

    /// Only the layout is asserted here. Which data home it hangs off is
    /// `tests/permissions.rs`'s to check, because deciding that means setting
    /// `XDG_DATA_HOME`, and a unit test that did would be setting it for every
    /// other test in the binary at the same time.
    #[test]
    fn a_projects_data_hangs_off_the_data_home_and_is_not_created_by_asking() {
        let scratch = temporary();
        let project = Project::resolve(scratch.path());
        let directory = project.data_dir().expect("the path resolves");

        assert!(
            directory.ends_with(Path::new("ganja").join("project").join(project.slug())),
            "{}",
            directory.display()
        );
        assert!(directory.is_absolute(), "{}", directory.display());
        assert!(
            !directory.exists(),
            "resolving a project must not create anything: {}",
            directory.display()
        );
    }
}

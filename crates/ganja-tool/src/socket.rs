//! Where a session's socket lives, and what a `uds:` address may name
//! (**D505**).
//!
//! No upstream counterpart: opencode serves TCP only, and Claude Code's §5.6
//! names the `uds:` scheme without tracing its wire. The scheme is tmux's
//! (`/tmp/tmux-<uid>/`; plan Resolution 5): a **literal** `/tmp/ganja-<uid>/`
//! — never `std::env::temp_dir()`, whose macOS value is long enough to
//! threaten `sun_path`'s 104 bytes, and never `$XDG_RUNTIME_DIR`, which macOS
//! does not have — owned by the calling uid at mode `0700`, and inside it one
//! socket per session at mode `0600`, named by the first eight hex digits of
//! the session's UUIDv7 and extended a digit at a time past a name a live
//! peer already holds. By construction the path is some thirty bytes.
//!
//! # Why the scheme lives here
//!
//! Four things read it, and they sit at four heights of the tree: the
//! `send_message` tool judges a `uds:` address at §5.2's rung 3
//! ([`vet_address`]); the engine's deliver arm judges the same address once
//! more before it connects; `ganja-serve` binds under it; and `ganja
//! sessions --live` lists and reaps by it. This crate is the lowest of the
//! four — its internal dependency list is exactly `ganja-permission` — so
//! the scheme is spelled once here and re-exported upward, and the binder,
//! the lister, the tool and the deliverer cannot come to disagree about
//! what a session socket is. Making the directory is here too, for the same
//! reason and since P27: [`prepare_directory`] was `ganja-serve`'s until a
//! fifth reader appeared — the shim orphan records, which write a `.shims`
//! sibling into this same directory from a crate that cannot name the
//! server. What is *not* here is anything that binds: the name walk over a
//! session's candidates, the lock that says a name is live, and the
//! peer-uid check on accept are the server's, in
//! `ganja-serve/src/socket.rs`.
//!
//! # A `uds:` address is a session socket of ours, or it is refused
//!
//! `send_message` runs unasked (D498), so what a `uds:` address may name is
//! the whole of what a prompt-injected call could reach: an address that
//! any absolute path satisfied would let a model connect this process to
//! `/var/run/docker.sock`, an ssh agent, or anything else that listens on
//! this machine, and read the answer back into its own context.
//! [`vet_address`] is the gate, and it is the binder's own discipline turned
//! toward the address: the path is plain and absolute, its file name is a
//! session socket's (`<hex, eight digits or more>.sock`), its directory is a
//! private one of ours by [`vet_directory`] — a real directory, not a link,
//! owned by this uid, at exactly `0700` — and what sits at the path is a
//! socket owned by this uid, not a link to one. Every clause is refused by
//! name. The directory clause is the binder's *predicate* rather than the
//! binder's *literal* path on purpose — and never an environment override,
//! which would be the bypass itself: the hidden `--socket-dir` door binds
//! sessions in a private directory a test owns, and an address gate keyed to
//! one literal directory would leave every such session unreachable while
//! adding nothing the predicate does not already refuse. **The residual,
//! stated:** what stays reachable is a socket this same uid deliberately
//! made in a session socket's shape — hex-named, owned by it, in a `0700`
//! directory of its own — which is inside the trust line the socket's own
//! route table draws (a same-uid peer may reach **any lead of this user's,
//! in any project on this machine — not only this team's members** — and
//! nothing else), and no listener anybody else set up. Four rules bound
//! the crossing and each covers its own edge, none the others': this gate
//! bounds *where* a message may go; the deliverer bounds *what a peer may
//! answer*; the receiver's admission gate bounds *what an inbox admits* —
//! policy, guards and a queue cap in front of every out-of-team delivery
//! (**D523**–**D525**, `ganja-core`'s `teammate/inbound.rs`); and the send
//! side is bounded by **nothing yet** — no per-inbox byte ceiling at the
//! mailbox write, no batch cap on delivery (bead `ganja-code-qfk`).
//!
//! The directory clause is checked, not trusted: a symlink, a foreign owner,
//! or a mode looser than `0700` is refused by name. The check→bind window,
//! the sticky-bit assumption it rests on and the peer-uid check on accept are
//! the binder's — `ganja-serve/src/socket.rs` — and are argued there.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

/// The extension every session socket carries, so a listing can tell a
/// socket from anything else somebody left in the directory.
pub const EXTENSION: &str = "sock";

/// The extension of the lock file beside every socket — the liveness token
/// the binder holds. Created once per name and never removed.
pub const LOCK_EXTENSION: &str = "lock";

/// The lock file that holds `socket`'s name: the same stem, [`LOCK_EXTENSION`]
/// in place of [`EXTENSION`].
#[must_use]
pub fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension(LOCK_EXTENSION)
}

/// The fewest hex digits of a session id a socket is named by. Eight is
/// tmux's own visual weight for a short id, and enough that a listing reads
/// as ids rather than noise; a collision extends past it, one digit at a
/// time.
pub const SHORTEST_NAME: usize = 8;

/// The most hex digits a session id has: a UUIDv7's thirty-two, dashes
/// dropped. A name longer than the whole id names nothing this build minted.
pub const LONGEST_NAME: usize = 32;

/// The mode the socket directory is created with and must be found at.
pub const DIRECTORY_MODE: u32 = 0o700;

/// The mode a bound socket, and the lock file beside it, are left at.
pub const SOCKET_MODE: u32 = 0o600;

/// The directory this user's session sockets live in: the literal
/// `/tmp/ganja-<uid>/`, exactly as tmux keeps `/tmp/tmux-<uid>/`.
///
/// Literal on purpose (Resolution 5): `std::env::temp_dir()` honors `$TMPDIR`,
/// which macOS sets to a `/var/folders/…/T/` path long enough to press on
/// `sun_path`, and a socket path that varies with the environment is a socket
/// two processes can fail to agree on.
#[cfg(unix)]
#[must_use]
pub fn directory() -> PathBuf {
    PathBuf::from(format!("/tmp/ganja-{}", uid()))
}

/// The calling process's effective uid: what owns the directory, and what
/// every peer is measured against.
#[cfg(unix)]
#[must_use]
pub fn uid() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches nothing, and cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether `stem` is a session socket's: [`SHORTEST_NAME`] to
/// [`LONGEST_NAME`] hex digits, which is every name the binder's walk can
/// give a session and nothing else. Case is not judged — the binder folds
/// to lowercase, and a file that does not exist under an uppercase spelling
/// is refused by the clause that looks for it.
#[must_use]
pub fn is_session_stem(stem: &str) -> bool {
    (SHORTEST_NAME..=LONGEST_NAME).contains(&stem.len())
        && stem.chars().all(|digit| digit.is_ascii_hexdigit())
}

/// Whether `path`'s file name is a session socket's: a session stem carrying
/// exactly [`EXTENSION`].
#[must_use]
pub fn is_session_socket_name(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some(EXTENSION)
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(is_session_stem)
}

/// The verdict on a directory found at the socket directory's path, from
/// what `stat` said about it: ours (`owner == own`) at exactly `0700`, or
/// refused by name. Pure, so the three refusals — the /tmp-squat check among
/// them, which no test can raise without a second uid — are pinned as unit
/// tests.
pub const fn vet(owner: u32, mode: u32, own: u32) -> Result<(), DirectoryRefusal> {
    if owner != own {
        return Err(DirectoryRefusal::ForeignOwner { owner, uid: own });
    }
    let mode = mode & 0o777;
    if mode != DIRECTORY_MODE {
        return Err(DirectoryRefusal::Permissions { mode });
    }
    Ok(())
}

/// Why a socket directory was refused rather than used.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryRefusal {
    /// Something is at the path, and it is not a directory — a plain file, or
    /// a symlink, which is refused even when it points at a good directory:
    /// `/tmp` is world-writable, and a link somebody planted there is the one
    /// way a socket meant to be private ends up somewhere it is not.
    #[error("it is not a directory")]
    NotADirectory,
    /// Somebody else made it. Nothing inside it can be trusted to be ours.
    #[error("it is owned by uid {owner}, not by this process's uid {uid}")]
    ForeignOwner {
        /// Who owns the directory.
        owner: u32,
        /// Who is asking.
        uid: u32,
    },
    /// Its mode lets a group or the world in, or keeps the owner out.
    #[error("its mode is {mode:04o}, not 0700")]
    Permissions {
        /// The permission bits as found.
        mode: u32,
    },
    /// Its parent is world-writable without the sticky bit, so anybody could
    /// rename or unlink the directory between the binder's check and its
    /// bind — the one assumption the check→bind window rests on, refused
    /// when it does not hold rather than leaned on.
    #[error(
        "its parent {parent} is world-writable without the sticky bit, so the directory could \
         be swapped out from under a bind"
    )]
    ParentNotSticky {
        /// The parent that lacks the bit.
        parent: PathBuf,
    },
    /// Creating or inspecting it failed for a reason the OS named.
    #[error("it could not be prepared: {0}")]
    Io(io::Error),
}

/// Why a `uds:` address was refused as not a session socket of ours — one
/// clause of [`vet_address`], each with the sentence a model reads.
///
/// `Copy` with no payload, deliberately: `send_message`'s refusal kinds are
/// compared as kinds and carry only what a test can spell, so what varies —
/// which uid, which mode — is dropped here and kept in
/// [`DirectoryRefusal`] for the binder and the lister, who act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddressRefusal {
    /// Not an absolute path made of plain components — a relative one, or
    /// one that steps through `.` or `..`, neither of which the binder ever
    /// spells and either of which could be read two ways.
    #[error("it is not a plain absolute path")]
    NotPlainAbsolute,
    /// The file name is not `<eight to thirty-two hex digits>.sock`.
    #[error(
        "its file name is not a session socket's, which is eight to thirty-two hex digits \
         and then .sock"
    )]
    NotASessionName,
    /// The directory could not be inspected at all.
    #[error("its directory could not be inspected")]
    DirectoryUnreadable,
    /// The directory is there and is not a private directory of ours: not a
    /// directory, somebody else's, or looser than `0700`.
    #[error(
        "its directory is not a private socket directory of ours (a real directory, owned by this user, at mode 0700)"
    )]
    DirectoryNotOurs,
    /// Nothing is at the path.
    #[error("nothing is at that path")]
    Absent,
    /// The path could not be inspected.
    #[error("it could not be inspected")]
    Unreadable,
    /// Something is at the path and it is not a socket — a file, or a link,
    /// which is refused even when it points at a socket.
    #[error("it is not a socket")]
    NotASocket,
    /// A socket, but somebody else's.
    #[error("it is a socket owned by another user")]
    ForeignSocket,
}

/// Every clause of the address gate over `path`, in the order the sentences
/// above list them: the string first, then the directory, then the file.
///
/// Inspects, and never touches: nothing here creates, opens, or connects.
///
/// # Errors
///
/// The first [`AddressRefusal`] the path earns.
#[cfg(unix)]
pub fn vet_address(path: &Path) -> Result<(), AddressRefusal> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let mut components = path.components();
    if components.next() != Some(Component::RootDir)
        || !components.all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(AddressRefusal::NotPlainAbsolute);
    }
    if !is_session_socket_name(path) {
        return Err(AddressRefusal::NotASessionName);
    }
    let directory = path.parent().ok_or(AddressRefusal::NotPlainAbsolute)?;
    match vet_directory(directory) {
        Ok(()) => {}
        Err(DirectoryRefusal::Io(_)) => return Err(AddressRefusal::DirectoryUnreadable),
        Err(_) => return Err(AddressRefusal::DirectoryNotOurs),
    }

    // `symlink_metadata`, not `metadata`: a link to a socket is refused as a
    // link — the same rule the binder applies to the directory.
    let found = match std::fs::symlink_metadata(path) {
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AddressRefusal::Absent);
        }
        Err(_) => return Err(AddressRefusal::Unreadable),
    };
    if !found.file_type().is_socket() {
        return Err(AddressRefusal::NotASocket);
    }
    if found.uid() != uid() {
        return Err(AddressRefusal::ForeignSocket);
    }

    Ok(())
}

/// The verdict on what sits at `directory`, as the binder forms it before it
/// binds and the address gate forms it before it connects: a real directory
/// of ours at exactly `0700`, or the first thing wrong with it — the same
/// [`vet`] over what `stat` said, with the not-a-directory arm in front.
/// One spelling for every reader — the binder, `ganja sessions --live`,
/// `send_message`'s rung 3 and the deliverer — because a second spelling of
/// this predicate would be the day two of them disagree. Inspects only —
/// creating the directory is the binder's own step, and a caller that finds
/// nothing at the path is told so through the [`DirectoryRefusal::Io`] arm
/// and decides for itself.
///
/// # Errors
///
/// [`DirectoryRefusal::NotADirectory`] for a file or a link — a link to a
/// perfectly good directory included, since a link is what somebody plants
/// in a world-writable `/tmp` — then [`vet`]'s owner and mode refusals;
/// [`DirectoryRefusal::Io`] when the path could not be inspected at all, an
/// absent one included.
#[cfg(unix)]
pub fn vet_directory(directory: &Path) -> Result<(), DirectoryRefusal> {
    use std::os::unix::fs::MetadataExt as _;

    // `symlink_metadata`, not `metadata`: a link is refused as a link.
    let found = std::fs::symlink_metadata(directory).map_err(DirectoryRefusal::Io)?;
    if !found.file_type().is_dir() {
        return Err(DirectoryRefusal::NotADirectory);
    }

    vet(found.uid(), found.mode(), uid())
}

/// Makes the socket directory at [`DIRECTORY_MODE`] when nothing is there,
/// and refuses by name when what is there is not a private directory of
/// ours — [`vet_directory`]'s verdict, after the creation attempt rather
/// than instead of it.
///
/// Lives beside the predicate it ends in, and that placement is
/// **D505's scheme/binder split, amended once**: the *scheme* crate now
/// owns "make the directory correctly" as well as "recognize one", while
/// everything that actually binds — the name walk, the flock'd `.lock`
/// sibling, the peer-uid check on accept — stays the server's. Two readers
/// need it and only one of them binds: `ganja-serve` before it binds a
/// session socket, and the shim orphan records
/// ([`crate`]'s consumer in `ganja-core`) before they write a `.shims`
/// sibling into the same directory. A second copy of this sequence is the
/// day the two disagree about what `0700` means.
///
/// The refusal type is [`DirectoryRefusal`] rather than any caller's own
/// error, so nothing above has to move down: `ganja-serve` wraps it in the
/// `ServeError` its routes answer in, exactly as it did when it owned the
/// body.
///
/// # Errors
///
/// [`DirectoryRefusal::ParentNotSticky`] when the parent is world-writable
/// without the sticky bit — the one assumption the check-then-bind window
/// rests on, asserted rather than leaned on; [`DirectoryRefusal::Io`] when
/// the directory could not be made or inspected; then [`vet_directory`]'s
/// own refusals for whatever is already there.
#[cfg(unix)]
pub fn prepare_directory(directory: &Path) -> Result<(), DirectoryRefusal> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    // The window between vetting the directory and binding inside it is safe
    // only because a foreign uid cannot rename or unlink an entry it does not
    // own in the parent — which is what the sticky bit on a world-writable
    // parent (`/tmp`) guarantees, and nothing else does. Asserted rather than
    // assumed: a `/tmp` that has lost the bit is refused by name, before
    // anything is made in it. A parent that is not world-writable needs no
    // bit — nobody else can write there.
    if let Some(parent) = directory.parent() {
        let found = std::fs::symlink_metadata(parent).map_err(DirectoryRefusal::Io)?;
        let mode = found.mode();
        let world_writable = mode & 0o002 != 0;
        let sticky = mode & 0o1000 != 0;
        if world_writable && !sticky {
            return Err(DirectoryRefusal::ParentNotSticky {
                parent: parent.to_path_buf(),
            });
        }
    }

    match std::fs::DirBuilder::new()
        .mode(DIRECTORY_MODE)
        .create(directory)
    {
        // Ours, this instant; the umask can only have removed bits, so put
        // the mode where the check below expects it.
        Ok(()) => {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(DIRECTORY_MODE))
                .map_err(DirectoryRefusal::Io)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(DirectoryRefusal::Io(error)),
    }

    vet_directory(directory)
}

/// A session socket of ours, as a test builds one: a real socket bound at a
/// session's name inside a `0700` directory of this uid's — what
/// [`vet_address`] admits and nothing else. The listener is held so the
/// socket stays a socket; nothing connects to it.
#[cfg(all(test, unix))]
pub(crate) struct SessionSocket {
    _directory: tempfile::TempDir,
    _listener: std::os::unix::net::UnixListener,
    pub(crate) path: PathBuf,
}

#[cfg(all(test, unix))]
impl SessionSocket {
    pub(crate) fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("a private directory");
        let path = directory.path().join("0198c1a2.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("a socket binds");

        Self {
            _directory: directory,
            _listener: listener,
            path,
        }
    }

    /// The address as a model writes it.
    pub(crate) fn address(&self) -> String {
        format!("uds:{}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        DirectoryRefusal, LOCK_EXTENSION, is_session_socket_name, is_session_stem, lock_path, vet,
    };

    #[test]
    fn a_session_stem_is_eight_to_thirty_two_hex_digits() {
        assert!(is_session_stem("0198c1a2"));
        assert!(is_session_stem("0198C1A2"), "case is the file's business");
        assert!(is_session_stem("0198c1a23b4c7d5e8f60718293a4b5c6"));
        assert!(!is_session_stem("0198c1a"), "seven is too short");
        assert!(
            !is_session_stem("0198c1a23b4c7d5e8f60718293a4b5c67"),
            "thirty-three is longer than any id"
        );
        assert!(
            !is_session_stem("agent.12"),
            "an ssh agent's name is not hex"
        );
        assert!(
            !is_session_stem("docker00"),
            "nor is a word padded to eight"
        );
        assert!(!is_session_stem(""));
    }

    #[test]
    fn a_session_socket_name_is_a_session_stem_with_the_extension() {
        assert!(is_session_socket_name(Path::new(
            "/tmp/ganja-501/0198c1a2.sock"
        )));
        assert!(!is_session_socket_name(Path::new(
            "/tmp/ganja-501/0198c1a2.lock"
        )));
        assert!(!is_session_socket_name(Path::new(
            "/tmp/ganja-501/0198c1a2"
        )));
        assert!(!is_session_socket_name(Path::new("/var/run/docker.sock")));
        assert!(!is_session_socket_name(Path::new("/tmp/tmux-501/default")));
        assert!(!is_session_socket_name(Path::new("/tmp/ssh-abc/agent.123")));
    }

    #[test]
    fn a_directory_is_ours_at_0700_or_refused_by_the_first_thing_wrong_with_it() {
        assert!(
            vet(501, 0o040_700, 501).is_ok(),
            "the type bits are not the mode"
        );

        assert!(
            matches!(
                vet(0, 0o700, 501),
                Err(DirectoryRefusal::ForeignOwner { owner: 0, uid: 501 })
            ),
            "root's directory is somebody else's — the /tmp squat"
        );
        assert!(
            matches!(
                vet(502, 0o700, 501),
                Err(DirectoryRefusal::ForeignOwner {
                    owner: 502,
                    uid: 501
                })
            ),
            "so is another user's, however private they made it"
        );
        assert!(
            matches!(
                vet(501, 0o755, 501),
                Err(DirectoryRefusal::Permissions { mode: 0o755 })
            ),
            "world-readable"
        );
        assert!(
            matches!(
                vet(501, 0o770, 501),
                Err(DirectoryRefusal::Permissions { mode: 0o770 })
            ),
            "group-readable"
        );
        assert!(
            matches!(
                vet(501, 0o600, 501),
                Err(DirectoryRefusal::Permissions { mode: 0o600 })
            ),
            "tighter than 0700 is refused too: the owner could not enter it"
        );
        assert!(
            matches!(
                vet(0, 0o755, 501),
                Err(DirectoryRefusal::ForeignOwner { .. })
            ),
            "ownership is judged before mode: whose it is comes first"
        );

        // Every refusal is one sentence, single-spaced: a continuation
        // that forgot its backslash reads as a run of blanks in the middle
        // of what a person is told.
        for refusal in [
            DirectoryRefusal::NotADirectory,
            DirectoryRefusal::ForeignOwner { owner: 0, uid: 501 },
            DirectoryRefusal::Permissions { mode: 0o755 },
            DirectoryRefusal::ParentNotSticky {
                parent: "/tmp".into(),
            },
        ] {
            let sentence = refusal.to_string();
            assert!(!sentence.contains("  "), "single-spaced: {sentence:?}");
        }
    }

    #[test]
    fn a_socket_name_is_locked_by_its_sibling_lock_file() {
        assert_eq!(
            lock_path(Path::new("/tmp/ganja-501/0198c1a2.sock")),
            Path::new(&format!("/tmp/ganja-501/0198c1a2.{LOCK_EXTENSION}"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_is_the_literal_tmp_ganja_uid() {
        let directory = super::directory();

        assert_eq!(
            directory,
            Path::new(&format!("/tmp/ganja-{}", super::uid())),
            "not temp_dir(), not XDG_RUNTIME_DIR: tmux's own scheme"
        );
    }

    /// The address gate, clause by clause, on a real filesystem: only a
    /// socket of ours, named as a session's, inside a private directory of
    /// ours passes — and each of the well-known local sockets a
    /// prompt-injected call might name is refused by the clause it fails.
    #[cfg(unix)]
    #[test]
    fn a_uds_address_is_a_session_socket_of_ours_or_refused_by_the_clause_it_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        use super::{AddressRefusal, vet_address};

        let socket = super::SessionSocket::new();
        let private = socket
            .path
            .parent()
            .expect("the socket sits in its directory")
            .to_path_buf();

        assert_eq!(
            vet_address(&socket.path),
            Ok(()),
            "a session socket of ours"
        );

        // The string clauses.
        assert_eq!(
            vet_address(Path::new("tmp/ganja-501/0198c1a2.sock")),
            Err(AddressRefusal::NotPlainAbsolute)
        );
        assert_eq!(
            vet_address(&private.join("..").join("0198c1a2.sock")),
            Err(AddressRefusal::NotPlainAbsolute),
            "a step through .. is refused before anything is inspected"
        );
        assert_eq!(
            vet_address(&private.join("agent.123")),
            Err(AddressRefusal::NotASessionName)
        );
        assert_eq!(
            vet_address(Path::new("/var/run/docker.sock")),
            Err(AddressRefusal::NotASessionName),
            "the docker socket fails on its name before its directory"
        );

        // The directory clauses: a hex-named socket in a directory that is
        // not a private one of ours.
        let loose = tempfile::tempdir().expect("a directory");
        std::fs::set_permissions(loose.path(), std::fs::Permissions::from_mode(0o755))
            .expect("the test may loosen its own directory");
        let in_loose = loose.path().join("0198c1a2.sock");
        let _loose_listener =
            std::os::unix::net::UnixListener::bind(&in_loose).expect("a socket binds");
        assert_eq!(
            vet_address(&in_loose),
            Err(AddressRefusal::DirectoryNotOurs),
            "a world-readable directory is not ours to trust, whatever is in it"
        );
        assert_eq!(
            vet_address(Path::new("/nonexistent-ganja/0198c1a2.sock")),
            Err(AddressRefusal::DirectoryUnreadable)
        );

        // The file clauses, inside a good directory.
        assert_eq!(
            vet_address(&private.join("deadbeef.sock")),
            Err(AddressRefusal::Absent)
        );
        let plain = private.join("cafebabe.sock");
        std::fs::write(&plain, b"").expect("a plain file writes");
        assert_eq!(vet_address(&plain), Err(AddressRefusal::NotASocket));
        let link = private.join("feedface.sock");
        std::os::unix::fs::symlink(&socket.path, &link).expect("a link is made");
        assert_eq!(
            vet_address(&link),
            Err(AddressRefusal::NotASocket),
            "a link to a good socket is refused as a link"
        );
    }
}

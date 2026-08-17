//! Where a session's socket lives and how it is bound. D505's cross-session
//! transport is this crate's own HTTP over a Unix domain socket, and this
//! module is the socket half: the directory, the name a session earns inside
//! it, and the bind that keeps both private to the user who owns them.
//!
//! No upstream counterpart: opencode serves TCP only. The scheme is tmux's
//! (`/tmp/tmux-<uid>/`; plan Resolution 5): a **literal** `/tmp/ganja-<uid>/`
//! — never `std::env::temp_dir()`, whose macOS value is long enough to
//! threaten `sun_path`'s 104 bytes, and never `$XDG_RUNTIME_DIR`, which macOS
//! does not have — owned by the calling uid at mode `0700`, and inside it one
//! socket per session at mode `0600`, named by the first eight hex digits of
//! the session's UUIDv7 and extended a digit at a time past a name a live
//! peer already holds. By construction the path is some thirty bytes, which
//! is why the length refusal the plan first drafted is not here.
//!
//! Extension is routine rather than rare: a UUIDv7's first eight hex digits
//! are the top thirty-two bits of a millisecond clock, so every session minted
//! inside the same 65.536-second window shares them. Two sessions started
//! together land at eight and nine digits; the rule is simply "the shortest
//! prefix, eight digits or longer, whose file nobody live is answering".
//!
//! Authorization is the filesystem, twice over. The directory's mode keeps
//! other users from reaching a socket at all, and it is *checked* rather than
//! trusted — a directory that exists but is a symlink, is owned by somebody
//! else, or is looser than `0700` is refused by name (AC-22 as replaced by
//! Resolution 5). Then every accepted connection is held against the peer's
//! uid, so a socket that somehow leaked serves nobody but the user who bound
//! it. One assumption underwrites the check→bind window, and it is named
//! rather than leaned on: `/tmp` carries the **sticky bit**, so a foreign uid
//! cannot rename or unlink an entry it does not own and the vetted directory
//! cannot be swapped out from under the bind. The same path-based checks in a
//! world-writable directory *without* the sticky bit would be open to exactly
//! that TOCTOU, which is why the literal `/tmp` is the only parent this
//! module is asked to trust.
//!
//! **Liveness is a lock, not a probe.** Beside every socket sits a sibling
//! `<stem>.lock` — in the same vetted `0700` directory, created at `0600`, so
//! the world-writable-parent story above covers it without a word more — and
//! a binder holds an advisory `flock(LOCK_EX | LOCK_NB)` on it for as long as
//! it serves; the lock is the one token that says "this name is live". A name
//! whose lock is held is **never** touched — its file is not unlinked and its
//! bind is not attempted, whatever a connection to it would or would not do —
//! and one whose lock is free is stale by definition. `flock` is the right
//! primitive for exactly one reason: the kernel drops it with the last
//! descriptor, so a holder that exits, crashes, or is `SIGKILL`ed releases
//! the name without running a line of cleanup, and a stale lock can no more
//! wedge a name than a stale socket file can — the file left behind is
//! unlinked and the name reused. Two edges of that definition, named: "live"
//! means *holds our lock* — a socket bound by something that never took the
//! lock is stale by definition, which is fine while every binder is this
//! module and would matter only if the lock ever shipped incrementally; and
//! a same-uid process is inside the trust boundary, not the threat model — a
//! hostile holder of a `.lock` costs a colliding session one digit, never a
//! failure, and even all of a session's names held is answered as
//! [`crate::ServeError::SocketInUse`] rather than waited on. Connecting was the first
//! draft's probe and is gone on purpose: a live listener refuses a connection
//! whenever its accept backlog is full — the window between a bind and the
//! first accept included — so "refused" never meant "nobody home", and two
//! binders racing one name could both read it as free and one die on
//! `EADDRINUSE` in the case the paragraph above calls routine. The lock
//! serializes the walk instead: the loser fails `LOCK_NB`, reads the name as
//! held, and extends by a digit.
//!
//! Lock files are created and never removed. Unlinking one at shutdown would
//! reopen the classic race — a peer that opened the old inode before the
//! unlink locks a file nobody else can see, and a third opens a fresh one, so
//! two binders each believe they hold the name. A zero-byte `.lock` per name
//! ever bound is the price, and a listing that wants the sockets filters by
//! [`EXTENSION`] and never sees them.

use std::{
    io,
    path::{Path, PathBuf},
};

use ganja_protocol::SessionId;

/// The extension every session socket carries, so a listing can tell a
/// socket from anything else somebody left in the directory.
pub const EXTENSION: &str = "sock";

/// The extension of the lock file beside every socket — the liveness token
/// the module doc describes. Created once per name and never removed.
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

/// Every name `id`'s socket may take under `directory`, shortest first: the
/// first [`SHORTEST_NAME`] hex digits of the id, then one digit more per
/// step, to the whole id. The bind walks this list and takes the first name
/// nobody live is holding.
///
/// Dashes are dropped and case is folded, so the name is a prefix of the
/// id's compact hex spelling — which is what a listing that wants to map a
/// socket back to a session compares against.
pub fn candidates(directory: &Path, id: &SessionId) -> impl Iterator<Item = PathBuf> + use<> {
    let compact: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|digit| digit.to_ascii_lowercase())
        .collect();
    let directory = directory.to_path_buf();
    // An id shorter than the shortest name — nothing this build mints, every
    // id being a UUIDv7 (D493) — yields exactly one candidate: itself.
    let longest = compact.len().max(SHORTEST_NAME);

    (SHORTEST_NAME..=longest).map(move |digits| {
        let stem: String = compact.chars().take(digits).collect();
        directory.join(format!("{stem}.{EXTENSION}"))
    })
}

/// Whether a peer with uid `peer` may speak to a socket bound by uid `own`:
/// the same user and nobody else — not even root, who can reach the socket
/// past any mode and is refused here for exactly that reason.
#[must_use]
pub(crate) const fn peer_allowed(peer: u32, own: u32) -> bool {
    peer == own
}

/// The verdict on a directory found at the socket directory's path, from
/// what `stat` said about it: ours (`owner == own`) at exactly `0700`, or
/// refused by name. Pure, so the three refusals — the /tmp-squat check among
/// them, which no test can raise without a second uid — are pinned as unit
/// tests the way [`peer_allowed`] is.
pub(crate) const fn vet(owner: u32, mode: u32, own: u32) -> Result<(), DirectoryRefusal> {
    if owner != own {
        return Err(DirectoryRefusal::ForeignOwner { owner, uid: own });
    }
    let mode = mode & 0o777;
    if mode != DIRECTORY_MODE {
        return Err(DirectoryRefusal::Permissions { mode });
    }
    Ok(())
}

/// The mode the socket directory is created with and must be found at.
pub(crate) const DIRECTORY_MODE: u32 = 0o700;

/// The calling process's effective uid: what owns the directory, and what
/// every peer is measured against.
#[cfg(unix)]
pub(crate) fn uid() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches nothing, and cannot fail.
    unsafe { libc::geteuid() }
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
    /// Creating or inspecting it failed for a reason the OS named.
    #[error("it could not be prepared: {0}")]
    Io(io::Error),
}

#[cfg(unix)]
pub use unix::vet_directory;
#[cfg(unix)]
pub(crate) use unix::{PeerChecked, bind_path, bind_session};

#[cfg(unix)]
mod unix {
    use std::{
        fs, io,
        os::{
            fd::AsRawFd as _,
            unix::fs::{
                DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
                PermissionsExt as _,
            },
        },
        path::{Path, PathBuf},
    };

    use axum::serve::Listener;
    use ganja_protocol::SessionId;
    use tokio::net::{UnixListener, UnixStream, unix::SocketAddr};

    use super::{DIRECTORY_MODE, DirectoryRefusal, candidates, lock_path, peer_allowed, uid, vet};
    use crate::{Address, ServeError};

    /// The mode a bound socket, and the lock file beside it, are left at.
    const SOCKET_MODE: u32 = 0o600;

    /// A listener that answers only its own user: every accepted connection
    /// is checked against the peer's uid, and one from anybody else is closed
    /// unread and logged. Built on axum's own [`Listener`] for
    /// [`UnixListener`] — its accept-error handling included — rather than an
    /// accept loop of its own. It carries the name's lock: the two go into
    /// axum's serve future together and drop together, so the name reads as
    /// live for exactly as long as something accepts behind it.
    pub(crate) struct PeerChecked {
        listener: UnixListener,
        uid: u32,
        _lock: NameLock,
    }

    /// The advisory `flock` on a socket's `.lock` sibling, held open for the
    /// server's life and released by the kernel however that life ends. The
    /// descriptor is the lock: nothing reads it, and dropping it is the
    /// release.
    struct NameLock {
        _file: fs::File,
    }

    impl NameLock {
        /// Claims the lock for `socket`'s name without waiting: [`None`] when
        /// a live binder holds it, an error when the lock file itself cannot
        /// be opened.
        fn claim(socket: &Path) -> io::Result<Option<Self>> {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(SOCKET_MODE)
                .open(lock_path(socket))?;
            // SAFETY: `flock` takes an open descriptor and two flags, and the
            // descriptor is owned by `file` for the whole call.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(Self { _file: file }));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            Err(error)
        }
    }

    impl Listener for PeerChecked {
        type Io = UnixStream;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                let (stream, peer) = <UnixListener as Listener>::accept(&mut self.listener).await;
                match stream.peer_cred() {
                    Ok(credentials) if peer_allowed(credentials.uid(), self.uid) => {
                        return (stream, peer);
                    }
                    Ok(credentials) => tracing::warn!(
                        peer = credentials.uid(),
                        uid = self.uid,
                        "refusing a socket connection from another user"
                    ),
                    Err(error) => tracing::warn!(
                        %error,
                        "refusing a socket connection whose peer could not be identified"
                    ),
                }
                // Dropping the stream closes it: the peer reads a hang-up and
                // never a byte of the engine.
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            self.listener.local_addr()
        }
    }

    /// Binds `id`'s socket under `directory` at the first of its
    /// [`candidates`] nobody live is holding, and says which.
    pub(crate) async fn bind_session(
        directory: &Path,
        id: &SessionId,
    ) -> Result<(PeerChecked, PathBuf), ServeError> {
        let mut last = None;
        for path in candidates(directory, id) {
            match bind_path(&path).await {
                Ok(listener) => return Ok((listener, path)),
                Err(ServeError::SocketInUse { .. }) => last = Some(path),
                Err(other) => return Err(other),
            }
        }

        // Every prefix down to the whole id is answering — the id itself is
        // already served. Unreachable short of one session bound at every one
        // of its names, and answered rather than looped past.
        Err(ServeError::SocketInUse {
            path: last.unwrap_or_else(|| directory.to_path_buf()),
        })
    }

    /// Binds exactly `path`: its directory prepared and checked, its name's
    /// lock claimed — held by a live binder, the name is refused untouched —
    /// a stale socket file there unlinked, and the bound socket left at mode
    /// `0600`. `EADDRINUSE` from the bind itself is unreachable under the
    /// lock and is reported as the plain bind failure it would be.
    pub(crate) async fn bind_path(path: &Path) -> Result<PeerChecked, ServeError> {
        let bind_error = |source| ServeError::Bind {
            address: Address::Unix(path.to_path_buf()),
            source,
        };
        let directory = path.parent().ok_or_else(|| {
            bind_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a socket path needs a directory",
            ))
        })?;
        prepare_directory(directory)?;
        let lock =
            NameLock::claim(path)
                .map_err(bind_error)?
                .ok_or_else(|| ServeError::SocketInUse {
                    path: path.to_path_buf(),
                })?;
        clear_stale(path)?;

        let listener = UnixListener::bind(path).map_err(bind_error)?;
        // The directory is what keeps other users out; the socket's own mode
        // is the second lock, and it is set after the bind rather than through
        // the umask because the umask is process-wide.
        if let Err(source) = fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)) {
            drop(listener);
            let _ = fs::remove_file(path);
            return Err(ServeError::Bind {
                address: Address::Unix(path.to_path_buf()),
                source,
            });
        }

        Ok(PeerChecked {
            listener,
            uid: uid(),
            _lock: lock,
        })
    }

    /// Creates `directory` at `0700` when it is absent, and refuses it by
    /// name when what is there is not a real directory of ours at exactly
    /// that mode. The refusal is the point: `/tmp` is world-writable, so
    /// whatever sits at `/tmp/ganja-<uid>` before we get there was put there
    /// by somebody, and only a private directory we own is somewhere a
    /// private socket can live.
    fn prepare_directory(directory: &Path) -> Result<(), ServeError> {
        let refuse = |reason| ServeError::UnsafeSocketDirectory {
            path: directory.to_path_buf(),
            reason,
        };

        match fs::DirBuilder::new().mode(DIRECTORY_MODE).create(directory) {
            // Ours, this instant; the umask can only have removed bits, so put
            // the mode where the check below expects it.
            Ok(()) => {
                fs::set_permissions(directory, fs::Permissions::from_mode(DIRECTORY_MODE))
                    .map_err(|error| refuse(DirectoryRefusal::Io(error)))?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(refuse(DirectoryRefusal::Io(error))),
        }

        vet_directory(directory).map_err(refuse)
    }

    /// The verdict on what sits at `directory`, as the binder forms it
    /// before it binds: a real directory of ours at exactly `0700`, or the
    /// first thing wrong with it — the same `vet` over what `stat` said,
    /// with the not-a-directory arm in front. Public because a listing that
    /// unlinks dead sockets (`ganja sessions --live`) has to ask the very
    /// same question before it reads the directory, and a second spelling of
    /// this predicate would be the day the two disagree. Inspects only —
    /// creating the directory is the binder's own step, and a caller that
    /// finds nothing at the path is told so through the
    /// [`DirectoryRefusal::Io`] arm and decides for itself.
    ///
    /// # Errors
    ///
    /// [`DirectoryRefusal::NotADirectory`] for a file or a link — a link to a
    /// perfectly good directory included, since a link is what somebody
    /// plants in a world-writable `/tmp` — then `vet`'s owner and mode
    /// refusals; [`DirectoryRefusal::Io`] when the path could not be
    /// inspected at all, an absent one included.
    pub fn vet_directory(directory: &Path) -> Result<(), DirectoryRefusal> {
        // `symlink_metadata`, not `metadata`: a link is refused as a link.
        let found = fs::symlink_metadata(directory).map_err(DirectoryRefusal::Io)?;
        if !found.file_type().is_dir() {
            return Err(DirectoryRefusal::NotADirectory);
        }

        vet(found.uid(), found.mode(), uid())
    }

    /// Unlinks the socket file a dead holder left at `path`, the name's lock
    /// being ours by the time this runs. Anything at the path that is not a
    /// socket is left alone and named — this module removes only what it
    /// would itself have made.
    fn clear_stale(path: &Path) -> Result<(), ServeError> {
        let bind_error = |source| ServeError::Bind {
            address: Address::Unix(path.to_path_buf()),
            source,
        };

        let found = match fs::symlink_metadata(path) {
            Ok(found) => found,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(bind_error(error)),
        };
        if !found.file_type().is_socket() {
            return Err(bind_error(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "something that is not a socket is at the socket path; refusing to remove it",
            )));
        }

        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            // Gone between the stat and the unlink; the bind makes a new one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(bind_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ganja_protocol::SessionId;

    use super::{
        DirectoryRefusal, EXTENSION, LOCK_EXTENSION, SHORTEST_NAME, candidates, lock_path,
        peer_allowed, vet,
    };

    #[test]
    fn a_session_is_named_by_its_first_eight_hex_digits_then_one_more_per_step() {
        let id = SessionId::from("0198C1A2-3B4C-7D5E-8F60-718293A4B5C6".to_owned());
        let names: Vec<String> = candidates(Path::new("/tmp/ganja-501"), &id)
            .map(|path| path.display().to_string())
            .collect();

        assert_eq!(
            names.len(),
            32 - SHORTEST_NAME + 1,
            "eight digits through the whole id"
        );
        assert_eq!(names[0], format!("/tmp/ganja-501/0198c1a2.{EXTENSION}"));
        assert_eq!(names[1], format!("/tmp/ganja-501/0198c1a23.{EXTENSION}"));
        assert_eq!(
            names.last().expect("the whole id"),
            &format!("/tmp/ganja-501/0198c1a23b4c7d5e8f60718293a4b5c6.{EXTENSION}"),
            "dashes dropped, case folded"
        );
    }

    #[test]
    fn an_id_shorter_than_the_shortest_name_is_its_own_one_candidate() {
        let id = SessionId::from("abc".to_owned());
        let names: Vec<_> = candidates(Path::new("/d"), &id).collect();

        assert_eq!(names, vec![Path::new("/d/abc.sock").to_path_buf()]);
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
    }

    #[test]
    fn a_socket_name_is_locked_by_its_sibling_lock_file() {
        assert_eq!(
            lock_path(Path::new("/tmp/ganja-501/0198c1a2.sock")),
            Path::new(&format!("/tmp/ganja-501/0198c1a2.{LOCK_EXTENSION}"))
        );
    }

    #[test]
    fn a_peer_is_allowed_exactly_when_it_is_the_same_user() {
        assert!(peer_allowed(501, 501));
        assert!(!peer_allowed(502, 501));
        assert!(!peer_allowed(0, 501), "root is another user too");
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
}

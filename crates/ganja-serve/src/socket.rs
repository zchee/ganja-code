//! How a session's socket is bound. D505's cross-session transport is this
//! crate's own HTTP over a Unix domain socket, and this module is the
//! binder's half: the name walk over a session's candidates, the lock that
//! says a name is live, the directory prepared and vetted, and the peer-uid
//! check on every accepted connection.
//!
//! No upstream counterpart: opencode serves TCP only. **The scheme itself
//! lives one crate lower** — `ganja_tool::socket`, reached here as
//! [`ganja_core::tool::socket`] — because four readers at four heights of
//! the tree spell it: the `send_message` tool judging a `uds:` address at
//! rung 3, the engine's deliver arm judging it again before it connects, this
//! binder, and `ganja sessions --live`. The literal `/tmp/ganja-<uid>/`, the
//! `<hex>.sock` name, the `0700`/`0600` modes and the directory predicate
//! are that module's; what is here is what only a server can do with them,
//! and what is re-exported below is what the binder's callers read of it.
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

use std::path::{Path, PathBuf};

pub use ganja_core::tool::socket::{
    DirectoryRefusal, EXTENSION, LOCK_EXTENSION, SHORTEST_NAME, lock_path,
};
#[cfg(unix)]
pub use ganja_core::tool::socket::{directory, vet_directory};
use ganja_protocol::SessionId;

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

#[cfg(unix)]
pub use unix::NameLock;
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
    use ganja_core::tool::socket::{DIRECTORY_MODE, SOCKET_MODE, uid};
    use ganja_protocol::SessionId;
    use tokio::net::{UnixListener, UnixStream, unix::SocketAddr};

    use super::{DirectoryRefusal, candidates, lock_path, peer_allowed, vet_directory};
    use crate::{Address, ServeError};

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
    ///
    /// Public because the lister (`ganja sessions --live`) needs the same
    /// token the binder holds, for the same window: a stale socket file may
    /// be unlinked **only under this lock**, or a binder that claims the name
    /// between the lister's probe and its unlink binds a live socket at the
    /// very file the lister then removes — the server left holding an
    /// unnamed inode and a lock nobody can see past. [`NameLock::claim`]
    /// then [`NameLock::unlink_stale`], with the guard alive across both, is
    /// the whole discipline; the binder's own `bind_path` is its first user.
    pub struct NameLock {
        _file: fs::File,
        socket: PathBuf,
    }

    impl NameLock {
        /// Claims the lock for `socket`'s name without waiting: [`None`] when
        /// a live binder holds it, an error when the lock file itself cannot
        /// be opened. Creates the lock file when it is absent — a lister
        /// claiming a name leaves the same zero-byte `.lock` a binder would,
        /// which is the price of closing the probe→unlink window for a
        /// socket no binder ever locked, and lock files are never removed
        /// anyway.
        ///
        /// # Errors
        ///
        /// What opening the lock file said.
        pub fn claim(socket: &Path) -> io::Result<Option<Self>> {
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
                return Ok(Some(Self {
                    _file: file,
                    socket: socket.to_path_buf(),
                }));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            Err(error)
        }

        /// The socket whose name this holds.
        #[must_use]
        pub fn socket(&self) -> &Path {
            &self.socket
        }

        /// Unlinks the socket file a dead holder left at this name, the lock
        /// being ours for the whole of it — so nothing can bind a live socket
        /// there between the check and the unlink. Anything at the path that
        /// is not a socket is left alone and named: this removes only what
        /// the binder would itself have made. Nothing at the path is nothing
        /// to do.
        ///
        /// # Errors
        ///
        /// What the OS said, and [`io::ErrorKind::AlreadyExists`] for a
        /// non-socket at the path.
        pub fn unlink_stale(&self) -> io::Result<()> {
            let found = match fs::symlink_metadata(&self.socket) {
                Ok(found) => found,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if !found.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "something that is not a socket is at the socket path; refusing to remove it",
                ));
            }

            match fs::remove_file(&self.socket) {
                Ok(()) => Ok(()),
                // Gone between the stat and the unlink; nothing left to do.
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
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
        lock.unlink_stale().map_err(bind_error)?;

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

        // The window between vetting the directory and binding inside it is
        // safe only because a foreign uid cannot rename or unlink an entry
        // it does not own in the parent — which is what the sticky bit on a
        // world-writable parent (`/tmp`) guarantees, and nothing else does.
        // Asserted rather than assumed: a `/tmp` that has lost the bit is
        // refused by name, before anything is made in it. A parent that is
        // not world-writable needs no bit — nobody else can write there.
        if let Some(parent) = directory.parent() {
            let found = fs::symlink_metadata(parent)
                .map_err(|error| refuse(DirectoryRefusal::Io(error)))?;
            let mode = found.mode();
            let world_writable = mode & 0o002 != 0;
            let sticky = mode & 0o1000 != 0;
            if world_writable && !sticky {
                return Err(refuse(DirectoryRefusal::ParentNotSticky {
                    parent: parent.to_path_buf(),
                }));
            }
        }

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

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::PermissionsExt as _;

        use axum::serve::Listener as _;
        use tokio::{
            io::{AsyncReadExt as _, AsyncWriteExt as _},
            net::{UnixListener, UnixStream},
        };

        use super::{NameLock, PeerChecked, uid};

        /// **L2 of the W7 boundary review**: the peer-uid refusal, on a real
        /// accept. A `PeerChecked` measuring peers against a uid that is not
        /// this process's — the one override no other test can arrange
        /// without a second user — closes a same-uid connection unread: the
        /// peer's write is swallowed and its read is a hang-up, and the
        /// accept future never yields it. Measured against the real uid, the
        /// same connection is handed over.
        #[tokio::test]
        async fn a_connection_from_another_uid_is_closed_unread() {
            let directory = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .expect("a private directory");
            let path = directory.path().join("0198c1a2.sock");
            let lock = NameLock::claim(&path)
                .expect("the lock file opens")
                .expect("a fresh name is free");
            let mut checked = PeerChecked {
                listener: UnixListener::bind(&path).expect("a socket binds"),
                // Every peer this test can produce is refused by this.
                uid: uid().wrapping_add(1),
                _lock: lock,
            };

            let accepting = tokio::spawn(async move {
                let accepted =
                    tokio::time::timeout(std::time::Duration::from_millis(500), checked.accept())
                        .await;
                (checked, accepted.is_ok())
            });
            let mut peer = UnixStream::connect(&path)
                .await
                .expect("a same-uid connect");
            let _ = peer.write_all(b"GET /global/health HTTP/1.1\r\n\r\n").await;
            let mut answer = Vec::new();
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                peer.read_to_end(&mut answer),
            )
            .await
            .expect("the refused peer is hung up on within the deadline");
            assert!(
                matches!(read, Ok(0)) || read.is_err(),
                "closed unread: not a byte came back, got {read:?} / {answer:?}"
            );
            let (mut checked, accepted) = accepting.await.expect("the accept task ends");
            assert!(!accepted, "the accept never yielded the foreign connection");

            // Measured against the real uid, the same connection is accepted.
            checked.uid = uid();
            let accepting = tokio::spawn(async move { checked.accept().await });
            let _peer = UnixStream::connect(&path)
                .await
                .expect("a same-uid connect");
            let (_stream, _) = tokio::time::timeout(std::time::Duration::from_secs(5), accepting)
                .await
                .expect("the accept yields within the deadline")
                .expect("the accept task ends");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ganja_protocol::SessionId;

    use super::{EXTENSION, SHORTEST_NAME, candidates, peer_allowed};

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
    fn a_peer_is_allowed_exactly_when_it_is_the_same_user() {
        assert!(peer_allowed(501, 501));
        assert!(!peer_allowed(502, 501));
        assert!(!peer_allowed(0, 501), "root is another user too");
    }
}

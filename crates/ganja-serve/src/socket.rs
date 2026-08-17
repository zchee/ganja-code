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
//! it.
//!
//! A stale file — a socket left by a process that died — is unlinked at bind
//! and the name reused. Stale is decided by connecting: a socket whose peer
//! answers is live and is **never** stolen; one that refuses the connection
//! has nobody behind it. That probe is the whole difference between reusing a
//! crashed session's name and knocking a running one off the air.

use std::{
    io,
    path::{Path, PathBuf},
};

use ganja_protocol::SessionId;

/// The extension every session socket carries, so a listing can tell a
/// socket from anything else somebody left in the directory.
pub const EXTENSION: &str = "sock";

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
pub(crate) use unix::{PeerChecked, bind_path, bind_session};

#[cfg(unix)]
mod unix {
    use std::{
        fs, io,
        os::unix::fs::{
            DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _,
        },
        path::{Path, PathBuf},
    };

    use axum::serve::Listener;
    use ganja_protocol::SessionId;
    use tokio::net::{UnixListener, UnixStream, unix::SocketAddr};

    use super::{DirectoryRefusal, candidates, peer_allowed, uid};
    use crate::{Address, ServeError};

    /// The mode the socket directory is created with and must be found at.
    const DIRECTORY_MODE: u32 = 0o700;

    /// The mode a bound socket is left at.
    const SOCKET_MODE: u32 = 0o600;

    /// A listener that answers only its own user: every accepted connection
    /// is checked against the peer's uid, and one from anybody else is closed
    /// unread and logged. Built on axum's own [`Listener`] for
    /// [`UnixListener`] — its accept-error handling included — rather than an
    /// accept loop of its own.
    pub(crate) struct PeerChecked {
        listener: UnixListener,
        uid: u32,
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

    /// Binds exactly `path`: its directory prepared and checked, a stale
    /// socket file there unlinked, a live one refused, and the bound socket
    /// left at mode `0600`.
    pub(crate) async fn bind_path(path: &Path) -> Result<PeerChecked, ServeError> {
        let directory = path.parent().ok_or_else(|| ServeError::Bind {
            address: Address::Unix(path.to_path_buf()),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "a socket path needs a directory",
            ),
        })?;
        prepare_directory(directory)?;
        clear_stale(path).await?;

        let listener = UnixListener::bind(path).map_err(|source| ServeError::Bind {
            address: Address::Unix(path.to_path_buf()),
            source,
        })?;
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

        // `symlink_metadata`, not `metadata`: a link to a perfectly good
        // directory is still a link somebody planted in `/tmp`.
        let found =
            fs::symlink_metadata(directory).map_err(|error| refuse(DirectoryRefusal::Io(error)))?;
        if !found.file_type().is_dir() {
            return Err(refuse(DirectoryRefusal::NotADirectory));
        }
        let own = uid();
        if found.uid() != own {
            return Err(refuse(DirectoryRefusal::ForeignOwner {
                owner: found.uid(),
                uid: own,
            }));
        }
        let mode = found.mode() & 0o777;
        if mode != DIRECTORY_MODE {
            return Err(refuse(DirectoryRefusal::Permissions { mode }));
        }

        Ok(())
    }

    /// Unlinks a socket file at `path` that nobody is listening behind, and
    /// refuses one somebody is. Decided by connecting: a live server accepts,
    /// a dead one's file refuses the connection. Anything at the path that
    /// is not a socket is left alone and named — this module removes only
    /// what it would itself have made.
    async fn clear_stale(path: &Path) -> Result<(), ServeError> {
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

        match UnixStream::connect(path).await {
            // Live: the stream drops closed and the name stays theirs.
            Ok(_live) => Err(ServeError::SocketInUse {
                path: path.to_path_buf(),
            }),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                fs::remove_file(path).map_err(bind_error)
            }
            // The file went away between the stat and the connect — fine, the
            // bind will make a new one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(bind_error(error)),
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

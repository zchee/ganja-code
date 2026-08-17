//! The Unix socket transport (D505), on real sockets in a private directory
//! of the test's own: a session's socket comes up under the scheme and
//! answers `GET /global/health`; a directory that is not ours at `0700` is
//! refused by name (AC-22 as Resolution 5 replaced it); a stale socket file
//! is reused and a live one is never stolen — a second session sharing the
//! first eight hex digits extends its name instead.
//!
//! What this binary cannot hold, both for the same reason — everything a
//! test does here carries the test's own uid, and nothing fakes another:
//!
//! * the peer-uid **refusal** — the same-uid *acceptance* is what every
//!   health check proves; the refusal leg, a peer whose uid is not ours
//!   closed unread, is pinned as the pure predicate's unit test in
//!   `socket.rs`;
//! * the **foreign-owner** directory refusal, the /tmp-squat check — a
//!   directory owned by somebody else cannot be made without privilege, so
//!   the verdict is pinned where it is decided: `socket.rs`'s pure `vet`
//!   over `(owner, mode, own uid)`, its three arms each a unit test. What the
//!   integration tests hold is the not-a-directory and mode arms on real
//!   directories, and that the refusal reaches `serve()`'s caller by name;
//! * a **full accept backlog** behind a live name — Linux blocks a connect
//!   into a full Unix backlog where macOS refuses it, so a flood would hang
//!   the CI lane rather than prove anything. The design makes the backlog's
//!   state irrelevant (liveness is the lock, and no connect is ever made),
//!   and `a_held_name_is_never_unlinked_even_when_nothing_accepts_behind_it`
//!   asserts that with a held lock over a socket file nothing listens behind
//!   — the state a probe cannot tell from a dead name.
//!
//! Every directory is a fresh `tempfile` one rather than the real
//! `/tmp/ganja-<uid>/`: the refusal cases have to chmod and plant files where
//! the socket lives, and that is not something to do to the user's own.

#![cfg(unix)]

mod support;

use std::{
    fs,
    os::{fd::AsRawFd as _, unix::fs::PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use ganja_protocol::SessionId;
use ganja_serve::{
    Address, DirectoryRefusal, Handle, Listen, ServeError,
    socket::{self, EXTENSION, LOCK_EXTENSION, SHORTEST_NAME},
};
use support::{engine, loopback_config};

/// A fresh directory at `0700` — what the socket directory has to be, where
/// `tempdir()`'s default is whatever the umask leaves (`0755`, usually).
fn private_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("a private directory")
}

/// A UUIDv7-shaped id whose first eight hex digits are `0198c1a2`; the
/// suffix keeps two of them apart.
fn session(suffix: &str) -> SessionId {
    SessionId::from(format!("0198c1a2-{suffix}-7d5e-8f60-718293a4b5c6"))
}

/// The config every test binds: the loopback fixture with its listen swapped
/// for `listen`.
fn config(listen: Listen) -> ganja_serve::ServeConfig {
    let mut config = loopback_config();
    config.listen = listen;
    config
}

/// `GET /global/health` over the socket at `path`, through a client bound
/// to that path alone — the one-client-per-socket rule the plan states.
async fn health(path: &Path) -> serde_json::Value {
    reqwest::Client::builder()
        .unix_socket(path.to_path_buf())
        .build()
        .expect("a socket client builds")
        .get("http://ganja/global/health")
        .send()
        .await
        .expect("the socket answers")
        .json()
        .await
        .expect("health is JSON")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("the path exists")
        .permissions()
        .mode()
        & 0o777
}

/// The socket path a handle is bound on — these tests bind sockets, so there
/// is one.
fn bound(handle: &Handle) -> PathBuf {
    match handle.address() {
        Address::Unix(path) => path.clone(),
        Address::Tcp(address) => panic!("a socket was asked for, and tcp {address} came up"),
    }
}

#[tokio::test]
async fn a_session_socket_binds_in_a_private_directory_and_answers_health() {
    let directory = private_dir();
    let id = session("3b4c");

    let handle = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: id.clone(),
            directory: directory.path().to_path_buf(),
        }),
    )
    .await
    .expect("a session socket comes up with no password: the filesystem is the credential");

    let path = bound(&handle);
    let shortest = socket::candidates(directory.path(), &id)
        .next()
        .expect("a session has a name");
    assert_eq!(path, shortest, "an uncontested name is the shortest one");
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(&*format!("0198c1a2.{EXTENSION}")),
        "the first {SHORTEST_NAME} hex digits of the id"
    );
    assert_eq!(mode(directory.path()), 0o700, "the directory is private");
    assert_eq!(mode(&path), 0o600, "and so is the socket");
    assert_eq!(handle.address().to_string(), path.display().to_string());
    assert!(handle.address().tcp().is_none());
    assert_eq!(handle.address().path(), Some(path.as_path()));

    let health = health(&path).await;
    assert_eq!(health["healthy"], true, "the same routes, over the socket");

    handle.shutdown().await.expect("a clean stop");
    assert!(
        !path.exists(),
        "a stopped server gives its socket file back: {}",
        path.display()
    );
}

#[tokio::test]
async fn an_absent_socket_directory_is_created_private() {
    let parent = private_dir();
    let directory = parent.path().join("run");
    assert!(!directory.exists());

    let handle = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: session("0001"),
            directory: directory.clone(),
        }),
    )
    .await
    .expect("the directory is made on the way to the bind");

    assert_eq!(mode(&directory), 0o700, "made at 0700, whatever the umask");
    assert_eq!(health(&bound(&handle)).await["healthy"], true);
    handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_socket_directory_that_is_not_private_is_refused_naming_its_mode() {
    let directory = tempfile::tempdir().expect("a directory");
    // Set rather than assumed: `tempdir()`'s default is the umask's business.
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
        .expect("the test may loosen its own directory");

    let refused = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: session("0002"),
            directory: directory.path().to_path_buf(),
        }),
    )
    .await;

    let error = match refused {
        Err(error) => error,
        Ok(handle) => panic!(
            "a world-readable socket directory must not be used; it was, at {}",
            handle.address()
        ),
    };
    assert!(
        matches!(
            error,
            ServeError::UnsafeSocketDirectory {
                ref path,
                reason: DirectoryRefusal::Permissions { mode: 0o755 },
            } if path == directory.path()
        ),
        "the refusal names the directory and its mode: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains(&directory.path().display().to_string())
            && message.contains("0755")
            && message.contains("0700"),
        "the sentence says which directory, what it is, and what it must be: {message}"
    );
    assert!(
        fs::read_dir(directory.path())
            .expect("the directory is still there")
            .next()
            .is_none(),
        "nothing was bound into a directory that was refused"
    );
}

#[tokio::test]
async fn a_symlink_or_plain_file_where_the_directory_should_be_is_refused() {
    let parent = tempfile::tempdir().expect("a directory");

    // A plain file at the directory's path.
    let file = parent.path().join("file");
    fs::write(&file, b"not a directory").expect("the decoy writes");
    let refused = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: session("0003"),
            directory: file.clone(),
        }),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ServeError::UnsafeSocketDirectory {
                ref path,
                reason: DirectoryRefusal::NotADirectory,
            }) if *path == file
        ),
        "a file is not a directory: {refused:?}"
    );

    // A symlink to a directory that would itself pass — refused all the
    // same, because a link is what somebody plants in a world-writable /tmp.
    let real = private_dir();
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(real.path(), &link).expect("the link is made");
    let refused = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: session("0004"),
            directory: link.clone(),
        }),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ServeError::UnsafeSocketDirectory {
                ref path,
                reason: DirectoryRefusal::NotADirectory,
            }) if *path == link
        ),
        "a symlink is not a directory either, however good its target: {refused:?}"
    );
    assert!(
        fs::read_dir(real.path())
            .expect("the target is still there")
            .next()
            .is_none(),
        "nothing was bound through the link"
    );
}

#[tokio::test]
async fn a_stale_socket_file_is_unlinked_and_the_name_reused() {
    let directory = private_dir();
    let id = session("0005");
    let shortest = socket::candidates(directory.path(), &id)
        .next()
        .expect("a session has a name");

    // A socket somebody bound and then died holding: the file stays, nobody
    // answers behind it.
    let dead = std::os::unix::net::UnixListener::bind(&shortest).expect("the stale socket binds");
    drop(dead);
    assert!(
        shortest.exists(),
        "dropping a listener leaves the file behind"
    );

    let handle = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id,
            directory: directory.path().to_path_buf(),
        }),
    )
    .await
    .expect("a dead socket's name is free to take");

    assert_eq!(bound(&handle), shortest, "the same name, not the next one");
    assert_eq!(health(&shortest).await["healthy"], true);
    let lock = socket::lock_path(&shortest);
    assert_eq!(
        lock.extension().and_then(|extension| extension.to_str()),
        Some(LOCK_EXTENSION)
    );
    assert_eq!(
        mode(&lock),
        0o600,
        "the lock file is as private as the socket"
    );
    handle.shutdown().await.expect("a clean stop");
    assert!(!shortest.exists(), "the socket file goes");
    assert!(
        lock.exists(),
        "the lock file stays: unlinking it would reopen the race"
    );
}

/// The liveness token is the lock, not a connection: a name whose lock is
/// held is walked past untouched even when there is **nothing** behind its
/// socket file — the one state a connect probe cannot tell from a dead name
/// (it is refused either way), and exactly the claim→bind window a live
/// binder is in. The setup is a held lock and a socket file whose listener
/// has been dropped; the connect-probe design unlinks it and binds at the
/// shortest name, the lock design walks. A full accept backlog would be a
/// second such state, and is not staged here on purpose: Linux blocks a
/// connect into a full Unix backlog rather than refusing it, and a test that
/// hangs on one platform proves nothing on the other. The design makes the
/// backlog's state irrelevant, and the dropped-listener state asserts that
/// with no flood at all.
#[tokio::test]
async fn a_held_name_is_never_unlinked_even_when_nothing_accepts_behind_it() {
    let directory = private_dir();
    let id = session("0007");
    let shortest = socket::candidates(directory.path(), &id)
        .next()
        .expect("a session has a name");

    // Somebody live, from this test's point of view: the lock held, and a
    // socket file at the name that nothing is listening behind — a binder
    // between its claim and its bind, or one whose backlog is full, look
    // exactly like this to anything that connects.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(socket::lock_path(&shortest))
        .expect("the lock file opens");
    // SAFETY: an open descriptor and two flags; the test holds `lock` open
    // for as long as the flock must stand.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test takes the name's lock first");
    let bound_then_dropped =
        std::os::unix::net::UnixListener::bind(&shortest).expect("the socket binds");
    drop(bound_then_dropped);
    assert!(
        std::os::unix::net::UnixStream::connect(&shortest).is_err(),
        "nothing answers at the held name — the state a probe misreads as dead"
    );

    let handle = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id,
            directory: directory.path().to_path_buf(),
        }),
    )
    .await
    .expect("a held name is walked past, not fought over");

    let landed = bound(&handle);
    assert_ne!(landed, shortest, "the held name was not taken");
    assert_eq!(
        landed.file_name().and_then(|name| name.to_str()),
        Some(&*format!("0198c1a20.{EXTENSION}")),
        "one digit longer"
    );
    assert!(
        shortest.exists(),
        "and the held name's file was not unlinked"
    );
    assert_eq!(health(&landed).await["healthy"], true);

    handle.shutdown().await.expect("a clean stop");
    assert!(shortest.exists(), "nor was it on the way out");
    drop(lock);
}

/// Eight binders racing one bucket all come up, every one at a name of its
/// own: the lock serializes the walk, so a loser reads the name as held and
/// extends by a digit instead of dying on `EADDRINUSE`.
///
/// A genuine race, not eight binds in a row: the runtime is multi-threaded
/// with a worker per racer, a barrier releases them together, and the whole
/// claim→stat→unlink→bind section is synchronous, so on the current-thread
/// flavor the racers could never interleave inside it and the test would
/// prove nothing. Three rounds, because one overlap is luck and thirty are
/// a pattern. Reverting `socket.rs` to the connect-probe design under this
/// test fails it — the evidence that it is load-bearing lives in the lane
/// report.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn binders_racing_one_bucket_all_come_up_at_different_names() {
    let directory = private_dir();
    // Eight ids that agree on their first eleven hex digits and differ in the
    // twelfth, so the walk has to go three digits deep before it can spread.
    let ids: Vec<SessionId> = (0..8).map(|n| session(&format!("3b4{n:x}"))).collect();
    let dir: Arc<Path> = Arc::from(directory.path());

    for round in 0..3 {
        let go = Arc::new(tokio::sync::Barrier::new(ids.len()));
        let racing: Vec<_> = ids
            .iter()
            .cloned()
            .map(|id| {
                let dir = Arc::clone(&dir);
                let go = Arc::clone(&go);
                tokio::spawn(async move {
                    go.wait().await;
                    ganja_serve::serve(
                        engine(),
                        config(Listen::Session {
                            id,
                            directory: dir.to_path_buf(),
                        }),
                    )
                    .await
                })
            })
            .collect();
        let mut handles = Vec::new();
        for task in racing {
            let handle = task
                .await
                .expect("the task ran")
                .expect("every racer comes up: the lock hands out names, it does not fail binds");
            handles.push(handle);
        }

        let mut names: Vec<PathBuf> = handles.iter().map(bound).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            handles.len(),
            "round {round}: no two racers share a name: {names:?}"
        );
        for handle in &handles {
            assert_eq!(health(&bound(handle)).await["healthy"], true);
        }
        for handle in handles {
            handle.shutdown().await.expect("a clean stop");
        }
        assert!(
            names.iter().all(|name| !name.exists()),
            "round {round}: every socket file was given back"
        );
    }
}

#[tokio::test]
async fn a_live_socket_is_never_stolen_and_a_colliding_session_extends_its_name() {
    let directory = private_dir();
    let first = session("aaaa");
    let second = session("bbbb");

    let holder = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: first.clone(),
            directory: directory.path().to_path_buf(),
        }),
    )
    .await
    .expect("the first session comes up");
    let held = bound(&holder);

    // The exact-path ask names the live socket and is refused, not served.
    let refused = ganja_serve::serve(engine(), config(Listen::Unix { path: held.clone() })).await;
    assert!(
        matches!(refused, Err(ServeError::SocketInUse { ref path }) if *path == held),
        "a live socket is somebody's: {refused:?}"
    );
    assert_eq!(
        health(&held).await["healthy"],
        true,
        "and the somebody is still answering"
    );

    // The session ask walks past it: the same eight digits, one more digit.
    let extended = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id: second.clone(),
            directory: directory.path().to_path_buf(),
        }),
    )
    .await
    .expect("a colliding session extends its name rather than failing");
    let extended_path = bound(&extended);
    let mut candidates = socket::candidates(directory.path(), &second);
    assert_eq!(
        candidates.next().as_ref(),
        Some(&held),
        "the two ids share their first eight digits, so the shortest name is the held one"
    );
    assert_eq!(
        Some(extended_path.clone()),
        candidates.next(),
        "and the second session landed one digit longer"
    );
    assert_eq!(
        extended_path.file_name().and_then(|name| name.to_str()),
        Some(&*format!("0198c1a2b.{EXTENSION}"))
    );

    assert_eq!(health(&held).await["healthy"], true);
    assert_eq!(health(&extended_path).await["healthy"], true);

    holder.shutdown().await.expect("a clean stop");
    extended.shutdown().await.expect("a clean stop");
    assert!(!held.exists() && !extended_path.exists());
}

#[tokio::test]
async fn something_that_is_not_a_socket_at_the_path_is_left_alone() {
    let directory = private_dir();
    let id = session("0006");
    let shortest = socket::candidates(directory.path(), &id)
        .next()
        .expect("a session has a name");
    fs::write(&shortest, b"somebody's file").expect("the decoy writes");

    let refused = ganja_serve::serve(
        engine(),
        config(Listen::Session {
            id,
            directory: directory.path().to_path_buf(),
        }),
    )
    .await;

    assert!(
        matches!(
            refused,
            Err(ServeError::Bind { ref address, ref source })
                if address.path() == Some(shortest.as_path())
                    && source.kind() == std::io::ErrorKind::AlreadyExists
        ),
        "only what this build would have made is ever removed: {refused:?}"
    );
    assert_eq!(
        fs::read(&shortest).expect("the file is still there"),
        b"somebody's file"
    );
}

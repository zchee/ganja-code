use std::io::{Error, ErrorKind};
use std::time::Duration;

use tokio::time::Instant;

use super::spawn_written_executable;

#[test]
fn a_written_executable_spawn_retries_only_busy_errors() {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attempts = 0;
    let result = spawn_written_executable(
        || {
            attempts += 1;
            if attempts < 3 { Err(ErrorKind::ExecutableFileBusy.into()) } else { Ok(17) }
        },
        deadline,
    );
    assert_eq!(result.expect("a transient busy executable becomes spawnable"), 17);
    assert_eq!(attempts, 3);
}

#[test]
fn a_non_busy_spawn_error_is_returned_without_retrying() {
    for kind in [ErrorKind::NotFound, ErrorKind::PermissionDenied, ErrorKind::Interrupted] {
        let mut attempts = 0;
        let result: Result<(), Error> = spawn_written_executable(
            || {
                attempts += 1;
                Err(Error::from(kind))
            },
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(result.expect_err("non-busy spawn error").kind(), kind);
        assert_eq!(attempts, 1, "never retry {kind:?}");
    }
}

#[test]
fn an_expired_spawn_deadline_returns_the_busy_error_without_retrying() {
    let mut attempts = 0;
    let result: Result<(), Error> = spawn_written_executable(
        || {
            attempts += 1;
            Err(Error::from_raw_os_error(libc::ETXTBSY))
        },
        Instant::now(),
    );
    let error = result.expect_err("expired caller deadline");
    assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
    assert_eq!(attempts, 1);
}

#[cfg(target_os = "linux")]
#[test]
fn a_written_executable_spawns_after_another_thread_releases_its_writer() {
    let home = tempfile::tempdir().expect("private executable directory");
    let binary = home.path().join("true");
    std::fs::copy("/bin/true", &binary).expect("private executable copy");
    let writer = std::fs::OpenOptions::new().write(true).open(&binary).expect("held writer");
    let (release, released) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        released.recv().expect("release writer after first spawn attempt");
        drop(writer);
    });
    let mut release = Some(release);
    let mut first_kind = None;
    let mut command = std::process::Command::new(&binary);
    let result = spawn_written_executable(
        || {
            let result = command.spawn();
            if let Some(release) = release.take() {
                first_kind = result.as_ref().err().map(Error::kind);
                release.send(()).expect("release writer thread");
            }
            result
        },
        Instant::now() + Duration::from_secs(5),
    );
    let status = result.and_then(|mut child| child.wait());
    writer.join().expect("writer release thread joined");
    assert_eq!(first_kind, Some(ErrorKind::ExecutableFileBusy));
    assert!(status.expect("spawn succeeds after the writer closes").success());
}

#[cfg(target_os = "linux")]
#[test]
fn a_written_executable_keeps_its_busy_error_while_the_writer_stays_open() {
    let home = tempfile::tempdir().expect("private executable directory");
    let binary = home.path().join("true");
    std::fs::copy("/bin/true", &binary).expect("private executable copy");
    let writer = std::fs::OpenOptions::new().write(true).open(&binary).expect("held writer");
    let mut command = std::process::Command::new(&binary);
    let result =
        spawn_written_executable(|| command.spawn(), Instant::now() + Duration::from_millis(20));
    drop(writer);
    let error = match result {
        Ok(mut child) => {
            let status = child.wait();
            panic!("a held writer must prevent execution: {status:?}");
        }
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
}

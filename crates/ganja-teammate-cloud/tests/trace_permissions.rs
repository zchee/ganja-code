//! Trace permissions are checked in a subprocess so its umask cannot affect other tests.

#![cfg(unix)]

#[path = "../src/trace.rs"]
mod trace;

use std::os::unix::fs::PermissionsExt as _;

#[test]
fn traces_are_private_even_with_permissive_umask() {
    if std::env::var_os("GANJA_TRACE_PERMISSIONS_CHILD").is_none() {
        let home = tempfile::tempdir().expect("trace home");
        for mask in ["000", "777"] {
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "umask \"$1\"; exec \"$2\" --exact traces_are_private_even_with_permissive_umask",
                    "trace-permissions",
                    mask,
                ])
                .arg(std::env::current_exe().expect("test binary"))
                .env("GANJA_TRACE_PERMISSIONS_CHILD", "1")
                .env("GANJA_CLOUD_TRACE", home.path().join(mask))
                .output()
                .expect("permission test subprocess");
            assert!(
                output.status.success(),
                "umask {mask}; stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }

    let directory = std::path::PathBuf::from(std::env::var_os("GANJA_CLOUD_TRACE").expect("path"));
    let mut log = trace::TraceLog::from_env("worker").expect("trace opens").expect("trace enabled");
    assert_eq!(
        std::fs::metadata(&directory).expect("directory").permissions().mode() & 0o777,
        0o700
    );
    let path = directory.join("worker.jsonl");
    assert_eq!(std::fs::metadata(&path).expect("file").permissions().mode() & 0o777, 0o600);
    let id = |seq| format!("01998ad0-0000-7000-8000-000000000000-{seq}").parse().expect("cursor");
    log.frame(&id(0), "connected", None).expect("initial frame");
    log.event(
        &id(1),
        &ganja_protocol::Event::PartDelta {
            session_id: "session".to_owned().into(),
            message_id: "message".to_owned().into(),
            part_id: "part".to_owned().into(),
            delta: "private text".to_owned(),
        },
    )
    .expect("event frame");
    log.frame(&id(2), "message_finished", Some("message".to_owned())).expect("terminal frame");
    drop(log);
    let contents = std::fs::read(&path).expect("trace contents");
    let delivered = trace::read(contents.as_slice()).expect("trace decodes");
    trace::check(
        &delivered,
        &delivered.frames,
        &["part_delta".to_owned(), "message_finished".to_owned()],
    )
    .expect("trace remains usable");

    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777))
        .expect("make existing directory public");
    assert!(trace::TraceLog::from_env("second").is_err(), "public existing directory is refused");
    assert!(!directory.join("second.jsonl").exists());
}

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
    assert!(!is_session_stem("agent.12"), "an ssh agent's name is not hex");
    assert!(!is_session_stem("docker00"), "nor is a word padded to eight");
    assert!(!is_session_stem(""));
}

#[test]
fn a_session_socket_name_is_a_session_stem_with_the_extension() {
    assert!(is_session_socket_name(Path::new("/tmp/ganja-501/0198c1a2.sock")));
    assert!(!is_session_socket_name(Path::new("/tmp/ganja-501/0198c1a2.lock")));
    assert!(!is_session_socket_name(Path::new("/tmp/ganja-501/0198c1a2")));
    assert!(!is_session_socket_name(Path::new("/var/run/docker.sock")));
    assert!(!is_session_socket_name(Path::new("/tmp/tmux-501/default")));
    assert!(!is_session_socket_name(Path::new("/tmp/ssh-abc/agent.123")));
}

#[test]
fn a_directory_is_ours_at_0700_or_refused_by_the_first_thing_wrong_with_it() {
    assert!(vet(501, 0o040_700, 501).is_ok(), "the type bits are not the mode");

    assert!(
        matches!(vet(0, 0o700, 501), Err(DirectoryRefusal::ForeignOwner { owner: 0, uid: 501 })),
        "root's directory is somebody else's — the /tmp squat"
    );
    assert!(
        matches!(
            vet(502, 0o700, 501),
            Err(DirectoryRefusal::ForeignOwner { owner: 502, uid: 501 })
        ),
        "so is another user's, however private they made it"
    );
    assert!(
        matches!(vet(501, 0o755, 501), Err(DirectoryRefusal::Permissions { mode: 0o755 })),
        "world-readable"
    );
    assert!(
        matches!(vet(501, 0o770, 501), Err(DirectoryRefusal::Permissions { mode: 0o770 })),
        "group-readable"
    );
    assert!(
        matches!(vet(501, 0o600, 501), Err(DirectoryRefusal::Permissions { mode: 0o600 })),
        "tighter than 0700 is refused too: the owner could not enter it"
    );
    assert!(
        matches!(vet(0, 0o755, 501), Err(DirectoryRefusal::ForeignOwner { .. })),
        "ownership is judged before mode: whose it is comes first"
    );

    // Every refusal is one sentence, single-spaced: a continuation
    // that forgot its backslash reads as a run of blanks in the middle
    // of what a person is told.
    for refusal in [
        DirectoryRefusal::NotADirectory,
        DirectoryRefusal::ForeignOwner { owner: 0, uid: 501 },
        DirectoryRefusal::Permissions { mode: 0o755 },
        DirectoryRefusal::ParentNotSticky { parent: "/tmp".into() },
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
    let private = socket.path.parent().expect("the socket sits in its directory").to_path_buf();

    assert_eq!(vet_address(&socket.path), Ok(()), "a session socket of ours");

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
    assert_eq!(vet_address(&private.join("agent.123")), Err(AddressRefusal::NotASessionName));
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
    assert_eq!(vet_address(&private.join("deadbeef.sock")), Err(AddressRefusal::Absent));
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

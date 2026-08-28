use super::*;

fn parts(tmux: &str, pane: &str) -> Result<Server, Error> {
    Server::from_parts(Some(OsString::from(tmux)), Some(OsString::from(pane)))
}

fn server() -> Server {
    Server::at("/tmp/private.sock", None)
}

#[test]
fn the_socket_is_the_value_up_to_the_first_comma() {
    let server = parts("/private/tmp/tmux-501/default,4242,0", "%3")
        .expect("a well-formed $TMUX addresses a server");
    assert_eq!(
        server.socket(),
        Path::new("/private/tmp/tmux-501/default"),
        "the pid and session index are not part of the socket"
    );
    assert_eq!(server.pane(), Some("%3"));
}

#[test]
fn a_value_with_no_comma_is_all_socket() {
    let server = parts("/tmp/sock", "%0").expect("a bare socket addresses a server");
    assert_eq!(server.socket(), Path::new("/tmp/sock"));
}

#[test]
fn an_empty_socket_field_addresses_nothing() {
    assert!(
        matches!(parts(",1,2", "%0"), Err(Error::NotInTmux)),
        "a value whose first field is empty names no socket, however many fields follow"
    );
}

#[test]
fn an_unset_or_empty_tmux_is_the_same_refusal() {
    assert!(matches!(Server::from_parts(None, None), Err(Error::NotInTmux)));
    assert!(matches!(Server::from_parts(Some(OsString::new()), None), Err(Error::NotInTmux)));
}

#[test]
fn a_pane_that_is_unset_or_empty_is_absent_rather_than_blank() {
    let unset = Server::from_parts(Some(OsString::from("/tmp/sock")), None)
        .expect("a socket without a pane still addresses a server");
    assert_eq!(unset.pane(), None);

    let empty = parts("/tmp/sock", "").expect("an empty pane is not a failed address");
    assert_eq!(
        empty.pane(),
        None,
        "an empty variable would otherwise be sent to tmux as an empty -t target"
    );
}

#[cfg(unix)]
#[test]
fn a_socket_path_outside_utf8_survives_as_itself() {
    use std::os::unix::ffi::OsStrExt as _;

    let raw = OsStr::from_bytes(b"/tmp/a\x80b,4242,0");
    let server =
        Server::from_parts(Some(raw.to_owned()), None).expect("a path is not obliged to be UTF-8");
    assert_eq!(
        server.socket().as_os_str().as_bytes(),
        b"/tmp/a\x80b",
        "a lossy read would address a socket that does not exist"
    );
}

#[cfg(unix)]
#[test]
fn a_pane_outside_utf8_is_treated_as_absent() {
    use std::os::unix::ffi::OsStrExt as _;

    let server = Server::from_parts(
        Some(OsString::from("/tmp/sock")),
        Some(OsStr::from_bytes(b"%\x80").to_owned()),
    )
    .expect("a damaged pane does not damage the address");
    assert_eq!(server.pane(), None);
}

#[test]
fn a_server_can_be_named_outright() {
    let server = Server::at("/tmp/private.sock", Some("%7".to_string()));
    assert_eq!(server.socket(), Path::new("/tmp/private.sock"));
    assert_eq!(server.pane(), Some("%7"));
}

#[test]
fn every_call_is_pinned_to_its_own_socket() {
    assert_eq!(
        server().argv(["list-panes", "-a"]),
        [
            OsString::from("tmux"),
            OsString::from("-S"),
            OsString::from("/tmp/private.sock"),
            OsString::from("list-panes"),
            OsString::from("-a"),
        ],
        "the socket pin comes before the caller's words, and the caller's words keep order"
    );
}

#[test]
fn a_call_with_no_words_of_its_own_is_still_addressed() {
    assert_eq!(server().argv(Vec::<OsString>::new()).len(), 3);
}

#[cfg(unix)]
#[test]
fn a_word_outside_utf8_is_passed_through_byte_for_byte() {
    use std::os::unix::ffi::OsStrExt as _;

    let word = OsStr::from_bytes(b"a\x80b");
    let argv = server().argv([OsStr::new("rename-window"), word]);
    assert_eq!(
        argv.last().map(|last| last.as_bytes()),
        Some(&b"a\x80b"[..]),
        "argv is execve's, not a rendered line: nothing here may re-encode a word"
    );
}

#[test]
fn a_capture_reads_as_bytes_and_as_text() {
    let captured = Captured { bytes: b"%0 %1\n".to_vec() };
    assert_eq!(captured.bytes(), b"%0 %1\n");
    assert_eq!(captured.text().expect("this capture is text"), "%0 %1\n");
    assert_eq!(captured.text_lossy(), "%0 %1\n");
    assert_eq!(captured.clone().into_bytes(), b"%0 %1\n".to_vec());
}

#[test]
fn a_capture_that_is_not_text_is_refused_strictly_and_repaired_lossily() {
    let captured = Captured { bytes: b"pane \xff title".to_vec() };
    assert!(
        captured.text().is_err(),
        "a strict read must not invent a character tmux did not print"
    );
    assert_eq!(captured.text_lossy(), "pane \u{fffd} title");
    assert_eq!(captured.bytes()[5], 0xff, "the bytes themselves are unharmed");
}

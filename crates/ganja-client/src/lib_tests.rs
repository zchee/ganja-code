#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

use super::{Client, ClientError, Credentials};

/// Nothing may render a password — the canary every credential-carrying
/// type in this workspace is held to.
#[test]
fn no_rendering_of_a_client_or_its_credential_shows_the_password() {
    let credentials = Credentials::new("ganja", "hunter2");
    let client = Client::new("http://127.0.0.1:4096", Some(credentials.clone()))
        .expect("a loopback address is usable");

    for rendered in [format!("{credentials:?}"), format!("{client:?}")] {
        assert!(!rendered.contains("hunter2"), "a password reached a formatter: {rendered}");
        assert!(rendered.contains("redacted"), "and the redaction is visible: {rendered}");
    }
}

#[test]
fn an_address_without_a_scheme_is_refused_rather_than_guessed_at() {
    let error = Client::new("127.0.0.1:4096", None).expect_err("a bare host is not an address");
    let said = error.to_string();
    assert!(said.contains("127.0.0.1:4096"), "{said}");
    assert!(said.contains("http://127.0.0.1:4096"), "{said}");

    // A URL that parses but is not HTTP is refused for a different reason,
    // and says which.
    let error = Client::new("ftp://example.invalid", None).expect_err("not a scheme we speak");
    assert!(error.to_string().contains("ftp"), "{error}");
}

#[test]
fn a_trailing_slash_does_not_double_up_in_a_route() {
    let client = Client::new("http://127.0.0.1:4096/", None).expect("an address with a slash");
    assert_eq!(client.address(), "http://127.0.0.1:4096");
}

/// A socket-bound client is shown under §5.6's own `uds:` spelling, so
/// an error about it reads as the address a `send_message` call would
/// have written, while its requests are spelled under the one `http://`
/// base the socket needs and never resolves.
#[cfg(unix)]
#[test]
fn a_socket_client_is_shown_as_uds_and_spells_its_requests_under_the_socket_base() {
    let client = Client::on_socket("/tmp/ganja/abcd1234.sock").expect("a socket path is usable");
    assert_eq!(client.address(), "uds:/tmp/ganja/abcd1234.sock");
    assert_eq!(client.base, super::SOCKET_URL);
    assert!(client.credentials.is_none(), "a same-uid socket presents no credential");
    assert!(
        format!("{client:?}").contains("uds:/tmp/ganja/abcd1234.sock"),
        "and Debug shows the socket, not the label"
    );
}

/// The two paths no socket can be bound at are refused here, in words,
/// rather than at the first request as an OS error about a name.
#[cfg(unix)]
#[test]
fn an_empty_or_nul_bearing_socket_path_is_refused_in_words() {
    let empty = Client::on_socket("").expect_err("nothing listens at nowhere");
    assert!(
        matches!(empty, ClientError::SocketPath { ref reason, .. } if reason.contains("empty")),
        "{empty}"
    );

    let path = std::ffi::OsStr::from_bytes(b"/tmp/ganja/bad\0name.sock");
    let nul = Client::on_socket(path).expect_err("a NUL cannot travel in a socket path");
    assert!(
        matches!(nul, ClientError::SocketPath { ref reason, .. } if reason.contains("NUL")),
        "{nul}"
    );
}

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
use std::time::Duration;

use super::{Bounds, Client, ClientError, Credentials};

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
    assert!(said.contains("http://127.0.0.1:4096"), "and shows what one looks like: {said}");

    // A URL that parses but is not HTTP is refused for a different reason,
    // and says which.
    let error = Client::new("ftp://example.invalid", None).expect_err("not a scheme we speak");
    assert!(error.to_string().contains("ftp"), "{error}");
}

/// Every error names the address, so an address may not carry a credential:
/// `reqwest` would send its user name and password as one, and every
/// transport failure after that would print them. Refused at construction,
/// before any client exists whose later errors could repeat it, and the
/// refusal itself repeats none of it.
///
/// Most spellings here are ones where the text and the URL parser disagree:
/// a password holding `#`, `?` or `/`, where the parser ends the host and
/// then fails on the port; slashes it skips, reads from `\` or finds a tab
/// inside; no `//` or no scheme at all. The secret on both sides of the
/// character that splits it is checked, so a refusal that repeated half of
/// it would fail too.
#[test]
fn an_address_carrying_a_password_is_refused_without_repeating_it() {
    for address in [
        "http://alice:hunter2@127.0.0.1:4096",
        "https://:hunter2@example.invalid/",
        "http://alice@127.0.0.1:4096",
        "http://alice:hunter2@127.0.0.1:99999",
        "http://alice:hunter2#hunter3@127.0.0.1:4096",
        "http://alice:hunter2?hunter3@127.0.0.1:4096",
        "http://alice:hunter2/hunter3@127.0.0.1:4096",
        "http:/alice:hunter2@127.0.0.1",
        "http:///alice:hunter2@127.0.0.1",
        "http:/\t/alice:hunter2@127.0.0.1",
        "http:\\/alice:hunter2@127.0.0.1",
        "https:///alice:hunter2@127.0.0.1/",
        "http:/alice:hunter2@127.0.0.1:99999",
        "alice:hunter2@127.0.0.1:4096",
        "//alice:hunter2@127.0.0.1:4096",
    ] {
        let error = Client::new(address, None).expect_err("a credential in an address is refused");
        assert!(matches!(error, ClientError::Address { .. }), "{address:?}: {error:?}");
        for rendered in [error.to_string(), format!("{error:?}")] {
            for secret in ["hunter2", "hunter3", "alice"] {
                assert!(
                    !rendered.contains(secret),
                    "{address:?} put {secret} in its refusal: {rendered}"
                );
            }
        }
    }

    let said =
        Client::new("http://alice:hunter2@127.0.0.1:4096", None).expect_err("refused").to_string();
    assert!(said.contains("http://127.0.0.1:4096"), "the refusal still names the server: {said}");
    assert!(said.contains("GANJA_SERVER_PASSWORD"), "and says where a credential goes: {said}");

    // Text the parser cannot read is not repeated even in part.
    let unread = Client::new("http://alice:hunter2#hunter3@127.0.0.1:4096", None)
        .expect_err("an address that does not parse is refused");
    assert!(matches!(unread, ClientError::Address { address: None, .. }), "{unread:?}");
    assert!(unread.to_string().starts_with("the address given is not"), "{unread}");
}

/// A bound of zero would end every call before it began; refused, naming
/// which bound it was.
#[test]
fn a_bound_of_zero_is_refused_rather_than_failing_every_call() {
    for (bounds, which) in [
        (Bounds { connect: Duration::ZERO, ..Bounds::default() }, "connect"),
        (Bounds { read: Duration::ZERO, ..Bounds::default() }, "read"),
    ] {
        let error = Client::with_bounds("http://127.0.0.1:4096", None, bounds)
            .expect_err("a zero bound is refused");
        assert!(
            matches!(error, ClientError::Bound { which: said, given } if said == which && given.is_zero()),
            "{error:?}"
        );
        assert!(error.to_string().contains(&format!("a {which} bound of 0ns")), "{error}");
    }
}

/// A bound past an hour is no bound anybody waits on, and far likelier a
/// mistaken unit or `Duration::MAX` meant as never; refused. An hour itself
/// is kept.
#[test]
fn a_bound_longer_than_an_hour_is_refused_as_no_bound_at_all() {
    let hour = Duration::from_secs(60 * 60);
    assert_eq!(super::LONGEST_BOUND, hour);

    for (bounds, which, given) in [
        (
            Bounds { connect: hour + Duration::from_nanos(1), ..Bounds::default() },
            "connect",
            hour + Duration::from_nanos(1),
        ),
        (Bounds { read: Duration::MAX, ..Bounds::default() }, "read", Duration::MAX),
    ] {
        let error = Client::with_bounds("http://127.0.0.1:4096", None, bounds)
            .expect_err("a bound past an hour is refused");
        assert!(
            matches!(error, ClientError::Bound { which: said, given: carried } if said == which && carried == given),
            "{error:?}"
        );
        assert!(error.to_string().contains("at most 3600s"), "and says the most it keeps: {error}");
    }

    let kept =
        Client::with_bounds("http://127.0.0.1:4096", None, Bounds { connect: hour, read: hour })
            .expect("an hour is the longest bound kept, and kept");
    assert_eq!(kept.bounds, Bounds { connect: hour, read: hour });
}

/// The wire tests run under shorter bounds, so the ones an attached run gets
/// are pinned here: ten seconds to connect, and thirty — three of serve's
/// heartbeats — for one read.
#[test]
fn a_client_for_an_address_waits_ten_seconds_to_connect_and_thirty_for_a_read() {
    let expected = Bounds { connect: Duration::from_secs(10), read: Duration::from_secs(30) };

    assert_eq!(super::READ_DEADLINE, expected.read);
    assert_eq!(Bounds::default(), expected);
    let client = Client::new("http://127.0.0.1:4096", None).expect("a loopback address is usable");
    assert_eq!(client.bounds, expected);
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

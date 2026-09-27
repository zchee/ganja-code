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

/// Every route and every error is spelled from the address as given. A
/// query or a fragment would sit in front of each route, so no request
/// would reach one. The URL parser does not read a control character
/// anywhere, or a space at either end, as written, so the text routes and
/// errors are spelled from would differ from what it read, and an error
/// could print a raw control character. Each is refused at construction.
/// The refusal shows the address only as the parser read it, with query
/// and fragment cleared, so what was refused is never repeated: not a
/// token, not a session id, not text after `#` or `?` that reads like a
/// credential, not a raw control character. The last columns are the reason
/// the refusal gives and the text it must not show; `?`, `#` and every
/// control character are checked on every row.
#[test]
fn a_query_a_fragment_a_control_character_or_padding_is_refused_and_never_repeated() {
    for (address, shown, reason, hidden) in [
        (
            "http://127.0.0.1:4096/?session=ses_hidden",
            "http://127.0.0.1:4096/",
            "query",
            &["session", "ses_hidden"][..],
        ),
        (
            "http://127.0.0.1:4096?token=quiet-lantern",
            "http://127.0.0.1:4096/",
            "query",
            &["token", "quiet-lantern"],
        ),
        ("https://example.invalid/ganja?", "https://example.invalid/ganja", "query", &[]),
        (
            "http://127.0.0.1:4096/?alice:hunter2@example.invalid",
            "http://127.0.0.1:4096/",
            "query",
            &["alice", "hunter2", "example.invalid"],
        ),
        ("http://127.0.0.1:4096/#pane-left", "http://127.0.0.1:4096/", "fragment", &["pane-left"]),
        ("http://127.0.0.1:4096#", "http://127.0.0.1:4096/", "fragment", &[]),
        (
            "http://127.0.0.1:4096/#alice:hunter2@example.invalid",
            "http://127.0.0.1:4096/",
            "fragment",
            &["alice", "hunter2", "example.invalid"],
        ),
        (
            "https://example.invalid#:hunter2@10.0.0.9:4096",
            "https://example.invalid/",
            "fragment",
            &["hunter2", "10.0.0.9"],
        ),
        (
            "http://127.0.0.1:4096/ganja?view=amber#footer-tide",
            "http://127.0.0.1:4096/ganja",
            "query",
            &["view", "amber", "footer-tide"],
        ),
        ("http://127.0.0.1:4096?#", "http://127.0.0.1:4096/", "query", &[]),
        // The other refusals clear the query and fragment too.
        (
            "http://alice:hunter2@127.0.0.1:4096/?view=amber",
            "http://127.0.0.1:4096/",
            "user name or password",
            &["alice", "hunter2", "view", "amber"],
        ),
        (
            "ftp://example.invalid/?view=amber#footer-tide",
            "ftp://example.invalid/",
            "http or https",
            &["view", "amber", "footer-tide"],
        ),
        (" http://127.0.0.1:4096", "http://127.0.0.1:4096/", "either end", &[]),
        ("http://127.0.0.1:4096 ", "http://127.0.0.1:4096/", "either end", &[]),
        ("\thttp://127.0.0.1:4096/", "http://127.0.0.1:4096/", "either end", &[]),
        ("http://127.0.0.1:4096/ganja\n", "http://127.0.0.1:4096/ganja", "either end", &[]),
        ("http://127.0.0.1:4096\r\n", "http://127.0.0.1:4096/", "either end", &[]),
        ("http://127.0.0.1:4096\u{0}", "http://127.0.0.1:4096/", "either end", &[]),
        // The parser drops a tab, LF or CR wherever it is, and
        // percent-encodes any other control character inside a path.
        ("http://127.0.0.1:40\t96", "http://127.0.0.1:4096/", "control character", &[]),
        ("http://127.0.0.1:4096/gan\nja", "http://127.0.0.1:4096/ganja", "control character", &[]),
        ("http://127.0.0.1:4096/gan\rja", "http://127.0.0.1:4096/ganja", "control character", &[]),
        (
            "http://127.0.0.1:4096/gan\u{1b}ja",
            "http://127.0.0.1:4096/gan%1Bja",
            "control character",
            &[],
        ),
        (
            "http://127.0.0.1:4096/gan\u{7f}ja",
            "http://127.0.0.1:4096/gan%7Fja",
            "control character",
            &[],
        ),
        // C1 controls too: U+009B opens a terminal control sequence.
        (
            "http://127.0.0.1:4096/gan\u{9b}ja",
            "http://127.0.0.1:4096/gan%C2%9Bja",
            "control character",
            &[],
        ),
        (
            "http://127.0.0.1:4096/gan\u{85}ja",
            "http://127.0.0.1:4096/gan%C2%85ja",
            "control character",
            &[],
        ),
        // Trailing slashes are dropped from the text kept, so a space or a
        // tab before one would end it.
        ("http://127.0.0.1:1\t/", "http://127.0.0.1:1/", "control character", &[]),
        ("http://127.0.0.1:4096/ganja /", "http://127.0.0.1:4096/ganja%20/", "either end", &[]),
    ] {
        let error = Client::new(address, None).expect_err("the address is refused");
        assert!(
            matches!(
                &error,
                ClientError::Address { address: Some(said), reason: why }
                    if said == shown && why.contains(reason)
            ),
            "{address:?}: expected {shown:?} refused for {reason:?}, got {error:?}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            for text in hidden.iter().chain(&["?", "#"]) {
                assert!(
                    !rendered.contains(text),
                    "{address:?} put {text:?} in its refusal: {rendered}"
                );
            }
            assert!(
                !rendered.contains(char::is_control),
                "{address:?} put a control character in its refusal: {rendered:?}"
            );
        }
    }

    // A space between a port and a trailing slash is not a port: the text
    // does not parse, and none of it is shown.
    let unread = Client::new("http://127.0.0.1:1 /", None).expect_err("the port is refused");
    assert!(matches!(unread, ClientError::Address { address: None, .. }), "{unread:?}");
}

/// What the refusals above leave alone: https anywhere, http on loopback, a
/// port, a path a proxy mounts the server under, a space inside that path,
/// and a trailing slash, which is dropped so a route is never spelled with
/// two.
#[test]
fn an_address_with_a_port_a_path_or_a_trailing_slash_is_accepted() {
    for (address, kept) in [
        ("http://127.0.0.1:4096/behind a proxy", "http://127.0.0.1:4096/behind a proxy"),
        ("https://example.invalid", "https://example.invalid"),
        ("https://example.invalid:8443/", "https://example.invalid:8443"),
        ("http://127.0.0.1:4096", "http://127.0.0.1:4096"),
        ("http://localhost:4096/", "http://localhost:4096"),
        ("http://[::1]:4096", "http://[::1]:4096"),
        ("http://127.0.0.1:4096/behind/a/proxy", "http://127.0.0.1:4096/behind/a/proxy"),
        ("https://example.invalid/ganja/", "https://example.invalid/ganja"),
    ] {
        let client = Client::new(address, None)
            .unwrap_or_else(|error| panic!("{address:?} is refused: {error}"));
        assert_eq!(client.address(), kept, "{address:?} is shown as given, less its slash");
        assert_eq!(client.base, kept, "{address:?} spells its routes under what it was given");
    }
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

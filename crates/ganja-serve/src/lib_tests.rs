use super::{ServeError, resolve_hostname};

#[test]
fn localhost_and_loopback_addresses_resolve_and_names_are_refused() {
    assert!(resolve_hostname("127.0.0.1").expect("an IPv4 literal").is_loopback());
    assert!(resolve_hostname("::1").expect("an IPv6 literal").is_loopback());
    assert!(resolve_hostname("localhost").expect("the one name").is_loopback());
    assert!(resolve_hostname("LOCALHOST").expect("case cannot matter").is_loopback());
    assert!(!resolve_hostname("0.0.0.0").expect("unspecified").is_loopback());
    assert!(!resolve_hostname("192.168.1.10").expect("a lan address").is_loopback());

    let refused = resolve_hostname("example.internal");
    assert!(
        matches!(refused, Err(ServeError::UnknownHostname { ref hostname }) if hostname == "example.internal"),
        "a name this build cannot resolve is refused, not guessed: {refused:?}"
    );
}

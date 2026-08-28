use std::path::Path;

use ganja_protocol::SessionId;

use super::{EXTENSION, SHORTEST_NAME, candidates, peer_allowed};

#[test]
fn a_session_is_named_by_its_first_eight_hex_digits_then_one_more_per_step() {
    let id = SessionId::from("0198C1A2-3B4C-7D5E-8F60-718293A4B5C6".to_owned());
    let names: Vec<String> = candidates(Path::new("/tmp/ganja-501"), &id)
        .map(|path| path.display().to_string())
        .collect();

    assert_eq!(names.len(), 32 - SHORTEST_NAME + 1, "eight digits through the whole id");
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

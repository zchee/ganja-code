use axum::http::{HeaderMap, HeaderValue, header};
use base64::Engine as _;
use secrecy::SecretString;

use super::{Credentials, authorized, eq_fold, query_param};

fn expected() -> Credentials {
    Credentials { username: "ganja".to_owned(), password: SecretString::from("hunter2") }
}

fn basic(user: &str, password: &str) -> HeaderMap {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Basic {token}")).expect("ascii"),
    );

    headers
}

#[test]
fn a_basic_header_and_an_auth_token_present_the_same_credential() {
    let headers = basic("ganja", "hunter2");
    assert!(authorized(&headers, None, &expected()));

    let token = base64::engine::general_purpose::STANDARD.encode("ganja:hunter2");
    let query = format!("auth_token={token}");
    assert!(authorized(&HeaderMap::new(), Some(&query), &expected()));
}

#[test]
fn the_wrong_password_the_wrong_user_and_no_credential_are_all_refused() {
    assert!(!authorized(&basic("ganja", "hunter3"), None, &expected()));
    assert!(!authorized(&basic("admin", "hunter2"), None, &expected()));
    assert!(!authorized(&HeaderMap::new(), None, &expected()));
    assert!(!authorized(&HeaderMap::new(), Some("auth_token=%%%broken"), &expected()));
    // Base64 that decodes but carries no colon is upstream's empty
    // credential: refused, not crashed on.
    let colonless = base64::engine::general_purpose::STANDARD.encode("no-separator");
    assert!(!authorized(&HeaderMap::new(), Some(&format!("auth_token={colonless}")), &expected()));
}

#[test]
fn the_comparison_is_a_whole_fold_not_a_prefix_check() {
    assert!(eq_fold(b"same", b"same"));
    assert!(!eq_fold(b"same", b"sama"));
    assert!(!eq_fold(b"same", b"samely"));
    assert!(!eq_fold(b"", b"x"));
    assert!(eq_fold(b"", b""));
}

#[test]
fn a_query_parameter_is_percent_decoded_the_way_urlsearchparams_decodes() {
    assert_eq!(
        query_param("a=1&auth_token=abc%2Fdef%3D&b=2", "auth_token").as_deref(),
        Some("abc/def=")
    );
    assert_eq!(query_param("x=a+b", "x").as_deref(), Some("a b"));
    assert_eq!(query_param("flag", "flag").as_deref(), Some(""));
    assert_eq!(query_param("a=1", "missing"), None);
    assert_eq!(query_param("bad=%zz", "bad"), None);
}

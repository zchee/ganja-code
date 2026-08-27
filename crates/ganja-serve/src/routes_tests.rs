//! What this crate can only assert by reading itself: the socket table's own
//! documented argument (**AC-44**).
//!
//! `socket_routes` is private, so no doc test can read its documentation and
//! `cargo doc` does not render it without `--document-private-items`. The
//! argument still has to be asserted present, because it is the standing
//! contract a route added to that table later is checked against — so it is
//! asserted as **source text**: this file reads `routes.rs` through
//! `include_str!`, cuts the doc block immediately above `fn socket_routes`,
//! and requires each clause of the argument to be in it. The same paragraph
//! in its public, rendering form lives in `crate`'s own doc, which is the
//! artifact the evidence bundle carries.

/// This module's own source, read at compile time. `include_str!` resolves
/// against the file it appears in, and `#[path]` keeps this file beside the
/// one it is reading.
const ROUTES_SOURCE: &str = include_str!("routes.rs");

/// The `///` block immediately above `fn socket_routes`, joined — everything
/// from the last blank line before the function back to the `fn` itself.
fn socket_routes_doc() -> String {
    let (before, _) = ROUTES_SOURCE
        .split_once("\nfn socket_routes()")
        .expect("routes.rs declares `fn socket_routes()`");

    let mut block: Vec<&str> = before
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .collect();
    block.reverse();

    block.join("\n")
}

/// **AC-44.** The doc above the socket's route table names the fourth route
/// and carries the argument for why it does not weaken the posture the other
/// three are kept under. Each clause is asserted on its own, so a future
/// edit that trims one of them fails here naming which.
#[test]
fn the_socket_tables_doc_argues_for_the_receipt_route_it_added() {
    let doc = socket_routes_doc();

    for (clause, why) in [
        ("/peer/receipt", "the route the argument is about is named"),
        (
            "no write API without a credential",
            "the posture the argument has to preserve is named",
        ),
        (
            "volatile",
            "what the route settles — a volatile in-memory map — is stated",
        ),
        (
            "the whole capability",
            "the id, and only the id, is what a poster must already hold",
        ),
        (
            "answers identically",
            "the route cannot be used to enumerate what a session is waiting on",
        ),
    ] {
        assert!(
            doc.contains(clause),
            "`socket_routes`' doc is missing {clause:?} — {why}. What it says:\n{doc}"
        );
    }
}

/// The table itself is four entries and no more, read off the same source:
/// the doc above it claims exactly four, and a fifth added without a
/// decision about the argument reddens here as well as in `tests/team.rs`.
#[test]
fn the_socket_table_registers_exactly_the_four_routes_its_doc_claims() {
    let (_, body) = ROUTES_SOURCE
        .split_once("\nfn socket_routes()")
        .expect("routes.rs declares `fn socket_routes()`");
    let body = body
        .split_once("\n}")
        .expect("the function has a closing brace")
        .0;

    let registered: Vec<&str> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix(".route(\""))
        .filter_map(|line| line.split_once('"'))
        .map(|(route, _)| route)
        .collect();

    assert_eq!(
        registered,
        vec![
            "/global/health",
            "/team",
            "/team/{name}/message",
            "/peer/receipt",
        ],
        "the socket's table is the four routes `socket_routes`' doc argues for"
    );
    assert!(
        socket_routes_doc().contains("**exactly four**"),
        "and the doc still counts them"
    );
}

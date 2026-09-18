//! The two spellings of "may this URL carry a credential" say the same
//! thing.
//!
//! **D564.** `ganja-tool` may not name `ganja-provider` — its internal
//! dependency set is asserted to be exactly `ganja-permission` — so the
//! predicate `Settings::base_from` applies to a TypeSafe base URL is a
//! *copy* of `provider::reachable_in_the_clear`. A copy held by review alone
//! drifts; this file is the gate, and it runs from `ganja-core`, the one
//! crate that can see both (`lib.rs`'s `pub use ganja_tool as tool`, and
//! `provider.rs`'s `pub use ganja_provider::provider::*`).
//!
//! One shared table, asserted in **both** directions, so neither side can
//! quietly become the stricter one. An unparseable row is not a reason to
//! abort: it is a row both sides must refuse, which is a claim worth making.
//!
//! The TypeSafe side is deliberately stricter in exactly one respect, and
//! that is why the query and fragment rows sit **outside** the shared table:
//! joining the endpoint path onto a base drops a query and a fragment
//! silently, and a gateway URL is where somebody puts a token in a query
//! string. The provider has no such join and so has no such rule.

use ganja_core::provider::reachable_in_the_clear;
use ganja_core::tool::typesafe::Settings;
use url::Url;

/// Every shape either side has an opinion about, including the three
/// bypasses that beat a text comparison: a host that *contains* a loopback
/// address, a host hidden behind userinfo, and a domain that merely starts
/// with `localhost`. All three are ordinary hosts belonging to whoever
/// registered them.
const SHAPES: &[&str] = &[
    // Accepted by both.
    "https://api.typesafe.ai",
    "https://eu.example",
    "https://eu.example/typesafe",
    "http://127.0.0.1:1",
    "http://127.0.0.1:8080/v1",
    "http://[::1]:1",
    "http://localhost:1",
    "http://localhost",
    // Refused by both: plain HTTP off loopback.
    "http://example.com",
    "http://10.0.0.1",
    "http://[2001:db8::1]:80",
    // Refused by both: not a scheme either will speak.
    "ftp://x",
    "file:///etc/passwd",
    "ws://localhost:1",
    // Refused by both: not a URL at all.
    "not a url",
    "",
    "//example.com",
    "127.0.0.1:1",
    // The three bypasses.
    "http://127.0.0.1.evil.com",
    "http://127.0.0.1@evil.com",
    "http://localhost.evil.com",
    // And their neighbours, which the same parse settles.
    "http://localhost:1@evil.com",
    "https://127.0.0.1.evil.com",
    "http://[::1].evil.com",
];

#[test]
fn the_typesafe_base_url_check_is_the_provider_check_in_another_crate() {
    for shape in SHAPES {
        let canonical = Url::parse(shape).is_ok_and(|url| reachable_in_the_clear(&url));
        let copy = Settings::base_from(shape).is_ok();

        assert_eq!(
            copy, canonical,
            "{shape:?}: the TypeSafe copy says {copy}, the provider says {canonical}"
        );
    }
}

/// The one place the copy is stricter, stated rather than left to be
/// discovered: a base carrying a query or a fragment is refused here and
/// accepted there, because only this side joins a path onto it.
#[test]
fn a_base_carrying_a_query_or_a_fragment_is_refused_only_on_the_typesafe_side() {
    for carried in ["https://eu.example/?token=secret", "https://eu.example/#tail"] {
        let url = Url::parse(carried).expect("these are URLs");

        assert!(reachable_in_the_clear(&url), "{carried} is fine for a provider base");
        assert!(
            Settings::base_from(carried).is_err(),
            "{carried} is refused here, because the join would drop what it carries"
        );
    }
}

/// The table is worth having only if it actually exercises both answers.
#[test]
fn the_shared_table_covers_both_verdicts() {
    let accepted = SHAPES.iter().filter(|shape| Settings::base_from(shape).is_ok()).count();

    assert!(accepted >= 8, "the table accepts {accepted} of {}", SHAPES.len());
    assert!(
        accepted < SHAPES.len(),
        "and refuses the rest, or it is proving nothing about refusal"
    );
}

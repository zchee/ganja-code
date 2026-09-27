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
//! The TypeSafe side is deliberately stricter in one respect — a base must be
//! something the endpoint path can be joined onto as written — and that one
//! respect has three spellings, which is why the userinfo, query and fragment
//! rows sit **outside** the shared table: joining the endpoint path onto a
//! base drops a query and a fragment, a gateway URL is where somebody puts a
//! token in a query string, and the TypeSafe SDK refuses all three when its
//! client is built. The provider has no such join and so has no such rule.
//!
//! It is stricter in a second respect: a base's host must be one the HTTP
//! client's URI parser accepts, so that the refusal is the base's rather than
//! one reported against the key once the TypeSafe client is built; and, by
//! ganja's own limit, a base is at most `MAX_BASE` bytes once parsed. Those
//! rows sit outside the table too.

use ganja_core::provider::reachable_in_the_clear;
use ganja_core::tool::typesafe::{Error, MAX_BASE, Settings};
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
/// discovered: a base carrying userinfo, a query or a fragment is refused here
/// and accepted there, because only this side joins a path onto it and hands
/// it to an SDK that refuses all three.
#[test]
fn a_base_carrying_userinfo_a_query_or_a_fragment_is_refused_only_on_the_typesafe_side() {
    for carried in [
        "https://eu.example/?token=secret",
        "https://eu.example/#tail",
        "https://tok:pw@eu.example",
        "http://user@127.0.0.1:1",
    ] {
        let url = Url::parse(carried).expect("these are URLs");

        assert!(reachable_in_the_clear(&url), "{carried} is fine for a provider base");
        assert!(
            Settings::base_from(carried).is_err(),
            "{carried} is refused here, because the join would drop what it carries"
        );
    }
}

/// The second respect: a host `url` parses and an HTTP request cannot carry,
/// and a base past `MAX_BASE`, are refused here as the base; a provider base,
/// checked only for the clear, is not. The same characters in a path are
/// percent-encoded, and a base of exactly `MAX_BASE` bytes is inside the
/// bound, so both sides accept those.
#[test]
fn a_base_an_http_request_cannot_carry_is_refused_only_on_the_typesafe_side() {
    let head = "https://eu.example/";
    let too_long = format!("{head}{}", "a".repeat(MAX_BASE + 1 - head.len()));
    for uncarried in [
        "https://{{host}}/v1",
        "https://${host}",
        "https://a\"b.example",
        "https://a`b.example",
        too_long.as_str(),
    ] {
        let url = Url::parse(uncarried).expect("url parses each of these");

        assert!(reachable_in_the_clear(&url), "{uncarried} is fine for a provider base");
        assert_eq!(
            Settings::base_from(uncarried),
            Err(Error::RefusedBase),
            "{uncarried} is refused here, as the base"
        );
    }

    let longest = format!("{head}{}", "a".repeat(MAX_BASE - head.len()));
    for carried in
        ["https://eu.example/{{prefix}}/v1", "https://eu.example/a\"b`c", longest.as_str()]
    {
        let url = Url::parse(carried).expect("these are URLs");

        assert!(reachable_in_the_clear(&url), "{carried} is fine for a provider base");
        assert!(Settings::base_from(carried).is_ok(), "{carried} is accepted here too");
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

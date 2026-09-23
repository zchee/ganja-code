//! What the judge does about the credential it reads from the environment
//! (**D567**, criterion 15): with no key, or with settings refused, there is
//! no judge at all — and with a key and an accepted base there is one, the
//! positive control without which every refusal below would also pass for a
//! `Judge::configured` that never built anything.
//!
//! **One test, one binary.** It removes `TYPESAFE_API_KEY`,
//! `TYPESAFE_BASE_URL` and `TYPESAFE_DEFAULT_MODEL` first — so a key a
//! developer exported can never make this binary, or any judge it builds,
//! reach the vendor — and then sets them, which is process-wide state; a
//! second test here would silently invalidate the `// SAFETY:` comments
//! below. The one judge it builds carries a dummy key and a loopback base,
//! and building one sends nothing. `Judge::configured` is the only reader of
//! the environment the judge has; every other suite builds from
//! `Judge::from_settings`.

use ganja_core::judge::{Judge, Screen};

#[test]
fn only_a_key_with_an_accepted_base_and_a_named_source_builds_a_judge() {
    let everything = Screen { webfetch: true, websearch: true, mcp: ["github".to_owned()].into() };

    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are removed. A second test here
    // would silently invalidate that, which is the rule this directory keeps.
    unsafe {
        std::env::remove_var("TYPESAFE_API_KEY");
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }
    assert!(Judge::configured(everything.clone()).is_none(), "no key, no judge");

    // SAFETY: as above.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "   ");
    }
    assert!(Judge::configured(everything.clone()).is_none(), "a blank key is no key");

    // A key with a base URL that would put it on the wire in the clear is a
    // refusal, not a fallback to the default endpoint.
    // SAFETY: as above.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "sk-configured");
        std::env::set_var("TYPESAFE_BASE_URL", "http://example.com");
    }
    assert!(Judge::configured(everything.clone()).is_none(), "a refused base URL is no judge");

    // A default model that is not a usable id is refused too, though the
    // judge never sends it: settings that fail their own rule are refused
    // whole rather than half-used.
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::set_var("TYPESAFE_DEFAULT_MODEL", "jev-latest · 0 B");
    }
    assert!(Judge::configured(everything.clone()).is_none(), "a refused default model is no judge");

    // And a screen that names nothing is no judge whatever the environment
    // holds: nothing would ever be sent.
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }
    assert!(Judge::configured(Screen::default()).is_none(), "nothing to screen, no judge");

    // The positive control: a dummy key and a loopback base on the discard
    // port — values that can reach no vendor — build a judge, so each `None`
    // above is a refusal rather than a door that never opens.
    // SAFETY: as above.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "sk-configured");
        std::env::set_var("TYPESAFE_BASE_URL", "http://127.0.0.1:9");
    }
    assert!(Judge::configured(everything).is_some(), "a key and an accepted base build a judge");

    // SAFETY: as above.
    unsafe {
        std::env::remove_var("TYPESAFE_API_KEY");
        std::env::remove_var("TYPESAFE_BASE_URL");
    }
}

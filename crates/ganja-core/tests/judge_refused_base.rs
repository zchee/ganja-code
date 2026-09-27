//! What the judge's warning says when the environment's TypeSafe base URL is
//! refused (**D567**): the variable, and every rule a base answers to, never
//! one rule it may have passed and never the value.
//!
//! The refusal does not say which rule refused the base, so a warning that
//! named one — "not https", say, about a base refused for its query — would
//! send somebody to fix the part that was fine. The rows here are each
//! refused by a different rule.
//!
//! **One test, one binary**, for `judge_env.rs`'s and `judge_log.rs`'s
//! reasons together: it sets and removes the `TYPESAFE_*` variables, and the
//! warning is read back through the process's **global** subscriber, both of
//! which are process-wide state. The key is a dummy and every base here is
//! refused before a client exists, so nothing is sent anywhere.

use ganja_core::judge::{Judge, Screen};
use ganja_core::tool::typesafe::Error;
use ganja_testkit::LogCapture;

/// Base URLs each refused by a different rule, beside the rule. The host and
/// the secret each one carries are what the echo check looks for.
const REFUSED_BASES: &[(&str, &str)] = &[
    ("it would put the key on the wire in the clear", "http://jev.invalid"),
    ("it carries userinfo", "https://reader:opensesame@jev.invalid"),
    ("it carries a query", "https://jev.invalid/v1?token=opensesame"),
];

#[test]
fn a_refused_base_url_is_warned_about_by_every_rule_rather_than_one_it_may_have_passed() {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("ganja_core=warn"))
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary installs exactly one global subscriber");
    let screen = Screen { webfetch: true, ..Screen::default() };
    let every_rule = Error::RefusedBase.to_string();

    for &(rule, base) in REFUSED_BASES {
        // SAFETY: this binary holds exactly one test, so no other thread is
        // reading the environment while these are set. A second test here
        // would silently invalidate that, which is the rule this directory
        // keeps.
        unsafe {
            std::env::set_var("TYPESAFE_API_KEY", "sk-refused-base-never-sent");
            std::env::set_var("TYPESAFE_BASE_URL", base);
            std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
        }
        let before = capture.logged().len();

        assert!(
            Judge::configured(screen.clone()).is_none(),
            "a base refused because {rule} is no judge"
        );
        let warned = capture.logged().split_off(before);
        assert_eq!(warned.lines().filter(|line| line.contains("WARN")).count(), 1, "{warned}");
        assert!(
            warned.contains(r#"variable="TYPESAFE_BASE_URL""#),
            "the warning names the variable that was refused because {rule}: {warned}"
        );
        assert!(warned.contains("screening is off"), "and says what it costs: {warned}");
        assert!(
            !warned.contains("https or loopback"),
            "a base refused because {rule} is not blamed on one rule: {warned}"
        );
        assert!(
            warned.contains(&every_rule),
            "the warning lists every rule a base answers to, so the one that refused \
             it is among them: {warned}"
        );
        for part in ["jev.invalid", "opensesame", "sk-refused-base"] {
            assert!(
                !warned.contains(part),
                "neither the refused URL nor the key is echoed ({part}): {warned}"
            );
        }
    }

    // SAFETY: as above.
    unsafe {
        std::env::remove_var("TYPESAFE_API_KEY");
        std::env::remove_var("TYPESAFE_BASE_URL");
    }
}

//! A key the TypeSafe client will not send leaves `evaluate` unoffered
//! (**D569**).
//!
//! `TYPESAFE_API_KEY` passes the environment's own blank rule here, so
//! [`EvaluateTool::configured`] gets as far as building the client, and the
//! SDK refuses the key there: it holds a space. That is configuration, like
//! a refused base URL, so no tool is offered and one warning says why. The
//! warning is read back to prove it names the variable and repeats no part
//! of the key, because a log line is where a key would otherwise go.
//!
//! **One test, one binary**, for `evaluate_refusal.rs`'s two reasons: it
//! sets and removes the `TYPESAFE_*` variables, which is process-wide state,
//! and the warning is read back through the process's **global** subscriber.
//! No path here opens a socket: the key is refused before a client exists.

mod support;

use ganja_tool::evaluate::EvaluateTool;

/// A key with a space in it, which the SDK will not put in a header.
const REFUSED_KEY: &str = "sk-refused-key never-sent";

#[test]
fn a_key_the_client_will_not_send_offers_no_tool_and_says_which_variable() {
    let capture = support::Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("ganja_tool=warn"))
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary installs exactly one global subscriber");

    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are set and removed. A second test
    // here would silently invalidate that, which is the rule this directory
    // keeps.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", REFUSED_KEY);
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }

    assert!(EvaluateTool::configured().is_none(), "a key the client will not send is no tool");

    let warned = capture.take();
    assert_eq!(warned.lines().filter(|line| line.contains("WARN")).count(), 1, "{warned}");
    assert!(warned.contains("TYPESAFE_API_KEY"), "the variable is named: {warned}");
    for fragment in ["sk-refused-key", "never-sent"] {
        assert!(!warned.contains(fragment), "no part of the key ({fragment}): {warned}");
    }
}

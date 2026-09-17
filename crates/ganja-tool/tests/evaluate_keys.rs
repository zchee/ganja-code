//! What `evaluate` does about the credential it reads from the environment:
//! with no key, there is no tool at all.
//!
//! **One test, one binary.** It removes `TYPESAFE_API_KEY`,
//! `TYPESAFE_BASE_URL` and `TYPESAFE_DEFAULT_MODEL`, which is process-wide
//! state, and a second test here would silently invalidate the `// SAFETY:`
//! comment below.
//!
//! Half the claim is that **no socket is opened**, and the other half is
//! that the tool is not there to open one. This is the opposite of
//! `websearch`, which is registered unconditionally and refuses at call time
//! (`websearch_keys.rs`): a tool whose permission dialog must name the host
//! project content would travel to has to be built from its settings, and a
//! tool nobody configured should not be spending prompt on every request of
//! every session. Builtins are never deferred, so there is no mechanism that
//! would have made the unconditional version cheap.

use ganja_tool::evaluate::EvaluateTool;

#[test]
fn an_unconfigured_machine_is_offered_no_evaluate_tool_at_all() {
    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are removed. A second test here
    // would silently invalidate that, which is the rule this directory keeps.
    unsafe {
        std::env::remove_var("TYPESAFE_API_KEY");
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }

    assert!(
        EvaluateTool::configured().is_none(),
        "no key, no tool: a session that never configured TypeSafe should not \
         carry a description telling the model about it"
    );

    // A variable exported blank by a shell profile is no key either — it
    // would otherwise build a tool that fails against the vendor, which is a
    // round trip bought for a refusal.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "   ");
    }

    assert!(EvaluateTool::configured().is_none(), "a blank key is no key");

    // And a key with a base URL that would put it on the wire in the clear
    // is refused rather than quietly falling back to the default endpoint:
    // the person configured somewhere specific, and silently sending
    // elsewhere is worse than not sending.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "sk-configured");
        std::env::set_var("TYPESAFE_BASE_URL", "http://example.com");
    }

    assert!(EvaluateTool::configured().is_none(), "a refused base URL is no tool");

    // And a default model that could not survive the consent title is a
    // refusal here rather than a forged disclosure on every later call: the
    // title is a `·`-separated sentence, and this value is interpolated into
    // it without the model ever asking for it.
    unsafe {
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::set_var("TYPESAFE_DEFAULT_MODEL", "jev-latest · 0 B · 0 question(s)");
    }

    assert!(EvaluateTool::configured().is_none(), "a forged default model is no tool");
}

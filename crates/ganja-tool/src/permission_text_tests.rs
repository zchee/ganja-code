use super::{DENIED_PREFIX, HOOK_REFUSED_PREFIX, REJECTED, is_refusal};

// Every sentence below is spelled as a **string literal**, never built from
// the constant it is testing (D552's Dv-8, `crates/ganja-core/AGENTS.md:52`):
// a test that says `format!("{HOOK_REFUSED_PREFIX}…")` passes whatever the
// constant is reworded to, which is exactly the change these pins exist to
// catch — a reword is what silently changes what a wire is told a call did.

#[test]
fn the_dialog_refusal_reads_as_a_refusal() {
    assert!(is_refusal("The user rejected permission to use this specific tool call."));
}

#[test]
fn a_rule_refusal_reads_as_a_refusal_with_its_rules_appended() {
    assert!(is_refusal(
        "The user has specified a rule which prevents you from using this specific tool \
         call. Here are some of the relevant rules bash: ask"
    ));
}

#[test]
fn a_hook_refusal_reads_as_a_refusal_with_the_hooks_own_reason_appended() {
    assert!(is_refusal("A PreToolUse hook refused this tool call: the repo is frozen"));
}

#[test]
fn an_ordinary_tool_failure_does_not_read_as_a_refusal() {
    assert!(!is_refusal("no such file or directory"));
    assert!(!is_refusal("the tool exited 1"));
    assert!(!is_refusal(""));
}

#[test]
fn a_sentence_that_merely_mentions_a_refusal_is_not_one() {
    // The two prefixes match at the start and nowhere else, so a tool that
    // failed while *printing* the sentence is still a failure.
    assert!(!is_refusal(
        "cat: A PreToolUse hook refused this tool call: is not a file this shell can read"
    ));
}

#[test]
fn each_constant_is_the_sentence_the_engine_renders() {
    assert_eq!(REJECTED, "The user rejected permission to use this specific tool call.");
    assert_eq!(
        DENIED_PREFIX,
        "The user has specified a rule which prevents you from using this specific tool call. \
         Here are some of the relevant rules "
    );
    assert_eq!(HOOK_REFUSED_PREFIX, "A PreToolUse hook refused this tool call: ");
}

#[test]
fn the_two_prefixes_end_in_the_space_their_tail_is_appended_after() {
    // Both are prefixes rather than whole sentences, and the trailing space
    // is what keeps the rendered rules and the hook's reason from running
    // into the last word.
    assert!(DENIED_PREFIX.ends_with(' '));
    assert!(HOOK_REFUSED_PREFIX.ends_with(' '));
}

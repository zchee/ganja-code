//! What `websearch` does about the credentials it reads from the environment.
//!
//! Its own binary, and one test in it, for the reason every environment test in
//! this workspace gets one: the variables are process-wide, and a neighbour
//! reading `EXA_API_KEY` while this one clears it would fail by scheduling.
//! What is pinned here is the half of the tool that `src/websearch.rs`'s own
//! tests deliberately do not reach — everything above the request, where the
//! environment decides which service is asked and whether anything is asked at
//! all.

use std::{path::PathBuf, sync::Arc};

use ganja_tool::{Credentials, FileTimes, Tool as _, ToolCtx, ToolError, websearch::WebsearchTool};
use tokio_util::sync::CancellationToken;

/// A context with nothing to guard: no file is read on this path.
fn ctx() -> ToolCtx {
    ToolCtx {
        cwd: PathBuf::from("."),
        cancel: CancellationToken::new(),
        call_id: "call-1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        ask: None,
        switch: None,
    }
}

/// Upstream sends Exa an unauthenticated request when no key is set. A search
/// nobody can pay for is not a search, and a request made anyway is one a
/// third party sees; the model is told which variable to set instead, and no
/// socket is opened saying so.
#[tokio::test]
async fn a_search_with_no_key_set_names_the_variables_rather_than_asking_anyone() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        std::env::remove_var("EXA_API_KEY");
        std::env::remove_var("PARALLEL_API_KEY");
        std::env::remove_var("GANJA_WEBSEARCH_PROVIDER");
    }

    let refused = WebsearchTool::new()
        .run(serde_json::json!({ "query": "rust ports" }), &ctx())
        .await
        .expect_err("there is nothing to search with");

    let ToolError::Failed(message) = &refused else {
        panic!("a missing credential is a failure the model reads: {refused:?}");
    };
    assert!(
        message.contains("EXA_API_KEY") && message.contains("PARALLEL_API_KEY"),
        "the refusal names both variables: {message}"
    );

    // Named without its key: the refusal narrows to the one variable that
    // would fix it.
    // SAFETY: as above.
    unsafe {
        std::env::set_var("GANJA_WEBSEARCH_PROVIDER", "parallel");
    }

    let refused = WebsearchTool::new()
        .run(serde_json::json!({ "query": "rust ports" }), &ctx())
        .await
        .expect_err("parallel was named and has no key");

    let ToolError::Failed(message) = &refused else {
        panic!("got {refused:?}");
    };
    assert!(
        message.contains("PARALLEL_API_KEY") && !message.contains("EXA_API_KEY"),
        "the refusal names the service that was asked for: {message}"
    );

    // And a blank export is no key at all, rather than a key that fails at the
    // service.
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("GANJA_WEBSEARCH_PROVIDER");
        std::env::set_var("EXA_API_KEY", "   ");
    }

    let refused = WebsearchTool::new()
        .run(serde_json::json!({ "query": "rust ports" }), &ctx())
        .await
        .expect_err("a blank key is not a key");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("EXA_API_KEY")),
        "got {refused:?}"
    );
}

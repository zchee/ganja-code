//! What a provider-run tool's answer leaves on disk and in the transcript
//! (**D563**, AC-27): an image goes to a file under the data home, owner-only,
//! and the row records its path; an oversized text answer becomes a spill
//! path. In neither case do megabytes reach a stored row.
//!
//! A binary of its own holding exactly one test, because it points
//! `XDG_DATA_HOME` somewhere private and that is process-wide state.

use std::sync::Arc;

use ganja_core::permission::Permissions;
use ganja_core::protocol::{FinishReason, PartBody};
use ganja_core::provider::responses::CHATGPT_ID;
use ganja_core::provider::{Blob, ProviderEvent};
use ganja_core::tool::Registry;
use ganja_core::tool::truncate::MAX_CHARS;
use ganja_core::{Engine, Storage};
use ganja_testkit::{ScriptedProvider, drain, prompt};
use serde_json::json;

/// A one-pixel PNG as the vendor sends it: base64, which no stored row should
/// carry.
const IMAGE: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[tokio::test]
async fn a_server_tools_bytes_land_on_disk_and_its_row_holds_a_path() {
    // SAFETY: the only test in this binary, and no thread has started yet.
    let home = unsafe { ganja_testkit::redirect_xdg_data_home() };
    let directory = tempfile::tempdir().expect("a temporary directory");
    let long = "x".repeat(300 * 1024);

    let (provider, _) = ScriptedProvider::named(
        CHATGPT_ID,
        vec![vec![
            ProviderEvent::ServerTool {
                tool: "image_generation_call".to_owned(),
                input: serde_json::Value::Null,
                output: String::new(),
                blob: Some(Blob { mime: "image/png".to_owned(), base64: IMAGE.to_owned() }),
            },
            ProviderEvent::ServerTool {
                tool: "web_search_call".to_owned(),
                input: json!({"type": "search", "query": "q"}),
                output: long.clone(),
                blob: None,
            },
            ProviderEvent::TextDelta("drawn".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]],
    );
    let engine = Engine::persistent(
        provider,
        "gpt-5.5",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt("draw a dot")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let session = engine.current_session().expect("the prompt minted a session");
    let transcript = Storage::open(directory.path().join("storage"))
        .load_transcript(&session.id)
        .expect("the transcript reads");
    let rows: Vec<(&str, &str, &ganja_core::protocol::PartId)> = transcript
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match &part.body {
            PartBody::ServerTool { tool, output, .. } => {
                Some((tool.as_str(), output.as_str(), &part.id))
            }
            _ => None,
        })
        .collect();
    let [(_, image_output, image_id), (_, search_output, _)] = rows.as_slice() else {
        panic!("both rows were stored: {transcript:?}");
    };

    let expected = home
        .path()
        .join("ganja")
        .join("server-tool-output")
        .join(format!("{}.png", image_id.as_str()));
    assert_eq!(*image_output, expected.display().to_string(), "the row records the path");
    let written = std::fs::read(&expected).expect("the image is on disk");
    assert_eq!(written.first(), Some(&0x89), "decoded bytes, not the base64 text");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&expected).expect("it exists").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the image is its owner's alone");
    }

    assert!(
        search_output.len() < MAX_CHARS,
        "the row is under the clamp: {} bytes",
        search_output.len()
    );
    assert!(search_output.contains("truncated"), "and says it was cut: {search_output:.200}");

    let stored = serde_json::to_string(&transcript).expect("the transcript serializes");
    assert!(!stored.contains(IMAGE), "no base64 reached a stored row");
    assert!(!stored.contains(&long), "nor the oversized answer");
}

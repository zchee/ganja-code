//! `ganja run --json-schema` (**D563**): the one door onto the Responses API's
//! `text.format`, driven through the shipped binary against a loopback
//! recorder.
//!
//! The binary is really run, which is the point: what a person types reaches a
//! clap value parser, a provider selection and a request body, and only a test
//! that crosses all three can say the flag arrives. The far end is
//! [`ganja_testkit::responses_server`] — the recorder `ganja-core`'s wire suite
//! drives — reached through `OPENAI_BASE_URL`, so no request leaves the
//! machine and every one of them is readable afterwards.
//!
//! **Why `openai` and not the seat.** Both ids speak this wire, and only one of
//! them can be pointed somewhere by an environment variable: the seat's
//! endpoint is reached with a stored OAuth login this suite has no business
//! manufacturing. The flag is the same flag on either.

use std::path::Path;
use std::process::Command;

use ganja_testkit::{Homes, responses_server};
use serde_json::{Value, json};

/// The schema every case passes, small enough to read in a failure message and
/// shaped like what a script would really ask for — closed with
/// `additionalProperties: false`, which the seat's strict mode requires
/// (probe 2026-09-17) and the flag refuses a schema without.
const SCHEMA: &str = r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}"#;

/// What the scripted turn answers, which is the document the schema describes:
/// the assistant's text *is* the answer under this flag, so a case that could
/// not tell the answer from prose would not be testing the flag.
const ANSWER: &str = r#"{"ok":true}"#;

/// A model this build sizes offline, so the run never reaches for a catalog.
const MODEL: &str = "gpt-5.5";

/// One whole Responses turn whose only reply is [`ANSWER`].
fn answering() -> String {
    [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#,
        &format!(
            r#"data: {{"type":"response.output_text.delta","item_id":"msg_1","delta":{}}}"#,
            json!(ANSWER)
        ),
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":4}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}

/// A run of the shipped binary against `base`, with every home pinned to
/// `homes` and the platform selected.
///
/// The `fake` provider the pin selects is replaced here, and its script
/// variable removed with it: what this suite measures is a **Responses**
/// request, which only the real wire builds.
fn ganja(homes: &Homes, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, Path::new("unused.json"));
    command
        .env("GANJA_PROVIDER", "openai")
        .env("GANJA_MODEL", MODEL)
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base)
        .env_remove("GANJA_FAKE_SCRIPT")
        .stdin(std::process::Stdio::null());

    command
}

/// The `text.format` object of the one recorded request.
fn sent_format(endpoint: &responses_server::Endpoint) -> Value {
    let seen = endpoint.seen();
    assert!(!seen.is_empty(), "the run reached the recorder at all");

    seen[0].json()["text"]["format"].clone()
}

/// **D563, AC-31.** An inline document rides the request as `text.format`,
/// under this door's own name and strictly — and the answer the model wrote
/// comes back as the run's `text` event.
#[tokio::test(flavor = "multi_thread")]
async fn an_inline_schema_rides_the_request_and_its_answer_is_the_document() {
    let endpoint = responses_server::serve().await;
    endpoint.answers_turns_with(answering());
    let homes = Homes::new();

    let output = ganja(&homes, &endpoint.base_url)
        .args(["run", "--format", "json", "--json-schema", SCHEMA, "is it ok"])
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("nd-JSON is text");
    assert!(
        output.status.success(),
        "the run exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        sent_format(&endpoint),
        json!({
            "type": "json_schema",
            "name": "ganja_run",
            "schema": serde_json::from_str::<Value>(SCHEMA).expect("the fixture is JSON"),
            "strict": true,
        }),
        "the document is wrapped, not sent bare"
    );

    // Whatever else the run asked for — a title is the one other request a
    // headless turn can make — carries no `text` at all: the caller's schema
    // holds *this turn's answer*, and a summary written to it would be neither
    // a title nor the document the flag was passed for.
    if let Some(other) = endpoint.seen().get(1) {
        assert!(
            other.json().get("text").is_none(),
            "only the turn's own request carries the format; got {}",
            other.json()
        );
    }

    let said: Vec<String> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap_or_else(|_| panic!("JSON: {line}")))
        .filter(|row| row["type"] == "text")
        .filter_map(|row| row["part"]["text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        said.iter().any(|text| text.contains(ANSWER)),
        "the answer is the document itself; got {said:?}"
    );
}

/// **D563, AC-31.** A path that is there is read, so a schema too long to type
/// lives in a file — the case the flag exists for.
#[tokio::test(flavor = "multi_thread")]
async fn a_path_that_is_there_is_read_as_the_document() {
    let endpoint = responses_server::serve().await;
    endpoint.answers_turns_with(answering());
    let homes = Homes::new();
    let path = homes.project().join("schema.json");
    std::fs::write(&path, SCHEMA).expect("the fixture is writable");

    let output = ganja(&homes, &endpoint.base_url)
        .args(["run", "--json-schema", "./schema.json", "is it ok"])
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "the run exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        sent_format(&endpoint)["schema"],
        serde_json::from_str::<Value>(SCHEMA).expect("the fixture is JSON"),
        "the file's contents, not its name"
    );
}

/// **D563, AC-31.** A value that is neither is refused at the clap boundary:
/// before a socket is opened, so the recorder sees nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_that_is_neither_a_file_nor_json_is_refused_before_any_request() {
    let endpoint = responses_server::serve().await;
    let homes = Homes::new();

    let output = ganja(&homes, &endpoint.base_url)
        .args(["run", "--json-schema", "nope", "is it ok"])
        .output()
        .expect("the binary runs");
    let stderr = String::from_utf8(output.stderr).expect("text");

    assert!(!output.status.success(), "a value the flag cannot read is a failed run");
    assert!(
        stderr.contains(
            "--json-schema takes a path to a JSON file or an inline JSON document; \"nope\" is neither: no such file, and not JSON"
        ),
        "the sentence names the value and both ways it failed: {stderr}"
    );
    assert!(endpoint.seen().is_empty(), "nothing was asked of any backend");
}

/// **D563, AC-31.** A file that is there and holds no JSON is **not** the
/// sentence above: "no such file" would be false about the one case a person
/// most needs told apart from a missing one.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_holds_no_json_says_so_about_the_file() {
    let endpoint = responses_server::serve().await;
    let homes = Homes::new();
    let path = homes.project().join("broken.json");
    std::fs::write(&path, "{not json").expect("the fixture is writable");

    let output = ganja(&homes, &endpoint.base_url)
        .args(["run", "--json-schema", "./broken.json", "is it ok"])
        .output()
        .expect("the binary runs");
    let stderr = String::from_utf8(output.stderr).expect("text");

    assert!(!output.status.success());
    assert!(
        stderr.contains("--json-schema could not read the JSON in \"./broken.json\""),
        "the file is named, and no claim is made that it is missing: {stderr}"
    );
    assert!(!stderr.contains("no such file"), "got {stderr}");
    assert!(endpoint.seen().is_empty());
}

/// **D563, AC-31.** A provider that does not speak the Responses API is
/// refused before a session exists — so a script that mistyped its provider
/// finds no half-started conversation in the store afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_cannot_carry_it_is_refused_before_a_session_exists() {
    let homes = Homes::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, Path::new("unused.json"));
    let output = command
        .env("GANJA_PROVIDER", "anthropic")
        .env("ANTHROPIC_API_KEY", "test-key")
        .env_remove("GANJA_FAKE_SCRIPT")
        .env_remove("GANJA_MODEL")
        .stdin(std::process::Stdio::null())
        .args(["run", "--json-schema", SCHEMA, "is it ok"])
        .output()
        .expect("the binary runs");
    let stderr = String::from_utf8(output.stderr).expect("text");

    assert!(!output.status.success());
    assert!(
        stderr.contains(
            "--json-schema rides the Responses API's text.format; the selected provider anthropic does not speak it"
        ),
        "got {stderr}"
    );
    // No project directory at all, which is a stronger claim than an empty
    // store and the one this AC is about: the refusal landed before the run
    // had opened anything to store into.
    assert!(
        !homes.data().join("ganja").join("project").exists(),
        "the refusal came before anything was stored"
    );
}

/// **D563, AC-32.** A configured `service_tier` reaches a headless run's
/// request without any flag at all: `run` takes the configuration.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_tier_reaches_a_headless_request() {
    let endpoint = responses_server::serve().await;
    endpoint.answers_turns_with(answering());
    let homes = Homes::new();
    std::fs::write(
        homes.project().join("ganja.toml"),
        "[provider.openai.options]\nservice_tier = \"default\"\n",
    )
    .expect("the fixture is writable");

    let output = ganja(&homes, &endpoint.base_url)
        .args(["run", "is it ok"])
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "the run exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let seen = endpoint.seen();
    assert_eq!(seen[0].json()["service_tier"], json!("default"), "the configured tier is sent");
}

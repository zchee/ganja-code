use axum::http::StatusCode;
use ganja_core::EngineError;
use ganja_protocol::SessionId;

use super::ApiError;

/// The table the routes hang on: a client switches on these statuses, so
/// each engine refusal must land on exactly its own.
#[test]
fn the_engine_refusals_map_to_their_statuses_and_nothing_else_moves() {
    let not_found = ApiError::from(EngineError::SessionNotFound {
        id: SessionId::from("ses_missing".to_owned()),
    });
    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

    // The client-fault group: a refusal the caller can act on, rather than a
    // fault of the server carrying out its own config.
    for refused in [
        EngineError::HookRefused {
            event: "UserPromptSubmit",
            reason: "not while the release is out".to_owned(),
        },
        // `/team` given one of `/teammate`'s own subcommands (**D547**, bead
        // 2m46): refused before a turn started, and the sentence carries the
        // line that was meant, which is what makes a 400 body worth reading.
        EngineError::MisdirectedCommand { meant: "/teammate list".to_owned() },
    ] {
        let mapped = ApiError::from(refused);
        assert_eq!(mapped.status(), StatusCode::BAD_REQUEST, "{mapped:?}");
    }
    let misdirected =
        ApiError::from(EngineError::MisdirectedCommand { meant: "/teammate list".to_owned() });
    assert!(
        misdirected.message().contains("/teammate list"),
        "the corrected line reaches the caller: {misdirected:?}",
    );

    let busy = ApiError::from(EngineError::Busy);
    assert_eq!(busy.status(), StatusCode::CONFLICT);

    for other in [
        EngineError::Ephemeral,
        EngineError::NoAgents,
        EngineError::NothingToUndo,
        EngineError::NothingToRedo,
        EngineError::NoSnapshots,
        EngineError::UnknownAgent { name: "nobody".to_owned() },
    ] {
        let mapped = ApiError::from(other);
        assert_eq!(
            mapped.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "everything the table does not name is a 500: {mapped:?}"
        );
    }
}

#[test]
fn a_body_is_tagged_json_naming_what_went_wrong() {
    let error = ApiError::Conflict("a turn is already streaming".to_owned());
    assert_eq!(error.tag(), "conflict");
    assert_eq!(error.message(), "a turn is already streaming");

    assert_eq!(ApiError::NotFound(String::new()).tag(), "not_found");
    assert_eq!(ApiError::Invalid(String::new()).tag(), "invalid_request");
    assert_eq!(ApiError::Internal(String::new()).tag(), "unknown");
}

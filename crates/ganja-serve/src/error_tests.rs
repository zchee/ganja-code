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

    let refused = ApiError::from(EngineError::HookRefused {
        event: "UserPromptSubmit",
        reason: "not while the release is out".to_owned(),
    });
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

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

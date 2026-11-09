use super::*;

#[test]
fn bootstrap_and_author_bodies_refuse_unknown_fields() {
    let body = serde_json::json!({
        "repository": "https://example.test/repo", "reference": "main", "branch": "work",
        "author": {"name": "worker", "email": "worker@example.test"}, "deadline_secs": 5,
    });
    assert!(serde_json::from_value::<Bootstrap>(body.clone()).is_ok());
    let mut outer = body.clone();
    outer["unexpected"] = true.into();
    assert!(serde_json::from_value::<Bootstrap>(outer).is_err());
    let mut nested = body;
    nested["author"]["unexpected"] = true.into();
    assert!(serde_json::from_value::<Bootstrap>(nested).is_err());
}

#[test]
fn failure_logs_preserve_diagnostics_but_redact_request_credentials() {
    let request = Bootstrap {
        repository: "https://example.test/repo".into(),
        reference: "main".into(),
        branch: "work".into(),
        author: Author { name: "worker".into(), email: "worker@example.test".into() },
        deadline_secs: 5,
    };
    let credentials = Credentials { username: "ganja".into(), password: "canary".into() };
    let text = redact(
        format!("canary {} {}", request.repository, "界".repeat(5000)),
        &request,
        &credentials,
    );
    assert!(!text.contains("canary"));
    assert!(!text.contains(&request.repository));
    assert_eq!(text, format!("[redacted] [redacted] {}", "界".repeat(5000)));
}

#[test]
fn status_bodies_are_strict_and_use_the_public_stage_names() {
    for status in [
        Status::Idle {},
        Status::InProgress {},
        Status::AlreadyServing {},
        Status::Cloned { head: "sha".into(), branch: "work".into() },
        Status::Failed {
            stage: Stage::Deadline,
            detail: "budget expired".into(),
            cleanup_in_progress: false,
        },
    ] {
        let value = serde_json::to_value(&status).expect("status serializes");
        assert_eq!(serde_json::from_value::<Status>(value.clone()).expect("status parses"), status);
        let mut unknown = value;
        unknown["unexpected"] = true.into();
        assert!(serde_json::from_value::<Status>(unknown).is_err());
    }
}

use super::*;

#[test]
fn bootstrap_debug_never_prints_repository_userinfo() {
    for repository in [
        "https://encoded%2Duser:encoded%2Dpassword@example.test/repo",
        "https://plain-user:plain-password@example.test/repo",
        "not-a-url-with-secret",
    ] {
        let bootstrap = Bootstrap {
            repository: repository.into(),
            reference: "main".into(),
            branch: "work".into(),
            author: BootstrapAuthor { name: "worker".into(), email: "worker@example.test".into() },
        };
        let debug = format!("{bootstrap:?}");
        assert!(!debug.contains(repository), "repository must be redacted: {debug}");
        assert!(!debug.contains("encoded"), "encoded userinfo must be redacted: {debug}");
        assert!(!debug.contains("plain-password"), "plain userinfo must be redacted: {debug}");
        assert!(debug.contains("<redacted>"));
    }
}

//! Malformed provider variable names never reach the platform environment lookup.

#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use ganja_provider::provider::{ProviderError, configured_key, setting};

#[test]
fn provider_key_lookup_rejects_equals_and_nul_in_variable_names() {
    const TEST: &str = "provider_key_lookup_rejects_equals_and_nul_in_variable_names";
    const CHILD: &str = "GANJA_TEST_KEY_LOOKUP_CHILD";
    if std::env::var(CHILD).as_deref() != Ok(TEST) {
        let home = tempfile::tempdir().expect("isolated data home");
        let data = home.path().join("ganja");
        std::fs::create_dir(&data).expect("credential directory");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(data.join("auth.json"))
            .expect("credential store")
            .write_all(br#"{"fixture":{"type":"api","key":"synthetic-stored-key"}}"#)
            .expect("stored credential");
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, TEST)
            .env("XDG_DATA_HOME", home.path())
            .env("GANJA_TEST_PROVIDER_KEY", "synthetic-exported-key")
            .output()
            .expect("isolated lookup test");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for variable in
        ["GANJA_TEST_PROVIDER_KEY=", "GANJA_TEST_PROVIDER_KEY=x", "GANJA_TEST_PROVIDER_KEY\0x"]
    {
        assert!(setting(variable).is_none(), "malformed variable was looked up");
        let error = configured_key("fixture", Some(variable)).expect_err("invalid variable name");
        let ProviderError::Auth(message) = error else {
            panic!("expected authentication error, got {error:?}");
        };
        assert!(message.contains("is not an environment variable name"), "{message}");
        assert!(!message.contains(variable), "malformed name was echoed");
        assert!(!message.contains("synthetic-"), "credential was echoed");
    }
    assert!(configured_key("fixture", None).is_ok(), "ordinary stored key remains usable");
}

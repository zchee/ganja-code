//! A refused credential store must say so, and say what repairs it.
//!
//! The failure this guards against is not losing the error — it is degrading
//! it. "no credential" and "there is a credential and it was refused" are
//! different situations with different fixes, and collapsing the second into
//! the first tells someone whose `auth.json` is group-readable to go and set
//! the variable they already set. The store's own error names the file, the
//! mode and the `chmod` that fixes it; all this proves is that the message
//! survives the trip to a startup failure without picking up key material.
//!
//! Unix only, because the permission check it exercises is.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.
#![cfg(unix)]

use std::{env, fs, os::unix::fs::PermissionsExt as _};

use ganja_core::{
    auth,
    provider::{self, AnthropicProvider},
};

/// The key the store is loaded with. Nothing may render it.
const CANARY: &str = "sk-test-canary-XYZ";

/// Permissions that expose the store to everyone on the machine.
const WORLD_READABLE: u32 = 0o644;

/// Permissions the store is supposed to have.
const OWNER_ONLY: u32 = 0o600;

#[test]
fn a_refused_credential_store_reports_why_and_how_to_repair_it() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::set_var("GANJA_PROVIDER", "anthropic");
        // Removed so the stored file is what answers: an exported key would
        // win, and then nothing would ever read the store.
        env::remove_var("ANTHROPIC_API_KEY");
        env::remove_var("OPENAI_API_KEY");
        env::remove_var("GANJA_MODEL");
    }

    auth::set_credential("anthropic", CANARY).expect("the store is writable");
    let path = auth::store_path().expect("the store has a path");

    // The stored key answers while the file is private, which is what makes
    // the refusal below a refusal rather than an absence.
    provider::from_env().expect("a private store is readable");

    fs::set_permissions(&path, fs::Permissions::from_mode(WORLD_READABLE))
        .expect("the fixture can be exposed");

    let refusal = provider::from_env().expect_err("an exposed store is refused");
    let rendered = format!("{refusal} / {refusal:?}");

    assert!(
        !rendered.contains(CANARY),
        "the refusal carried the key it refused: {rendered}"
    );
    assert!(
        rendered.contains(&path.display().to_string()),
        "the refusal should name the file to repair: {rendered}"
    );
    assert!(
        rendered.contains("chmod 600"),
        "the refusal should carry the command that repairs it: {rendered}"
    );
    assert!(
        !rendered.contains("is unset"),
        "a refused store is not a missing one, and saying so sends someone to \
         set a variable they already set: {rendered}"
    );

    // The same reason has to reach a provider built on its own, because that is
    // the path `ganja auth`-adjacent tooling and the tests take.
    let direct = AnthropicProvider::from_env().expect_err("an exposed store is refused");
    assert!(format!("{direct}").contains("chmod 600"), "got {direct}");
    assert!(!format!("{direct} / {direct:?}").contains(CANARY));

    // And repairing it is enough: the reason was advice, not a dead end.
    fs::set_permissions(&path, fs::Permissions::from_mode(OWNER_ONLY))
        .expect("the fixture can be repaired");
    let selection = provider::from_env().expect("a repaired store is readable again");
    assert_eq!(selection.provider.id(), "anthropic");
}

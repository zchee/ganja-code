use std::path::Path;
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use prost::Message as _;
use sha2::{Digest, Sha256};

use super::ateapi_v1::{
    DeleteActorRequest, DeleteOptions, DurabilityMode, ObjectRef, ResumeActorRequest,
    SuspendActorRequest, SuspendOptions,
};

// The tag alone can move: pin the source commit and the exact installed-wire bytes.
const UPSTREAM_COMMIT: &str = "b34fbee742906494b5eb4f1f36987176bf8770a4";
// SHA-256 values use base64 so the existing codec can render them without a hex helper.
const PROTO_HASH: &str = "tbs0jwS4+NnyTEyIDh9BdnceFVbP0M/5WpfKi+4HlyI=";
const ARTIFACTS: [(&str, &str); 2] = [
    ("substrate.ateapi.v1.rs", "sf/D42Lv56JM/Y5cXRPW+ctUcGVFNtWZIipAs5I/nTA="),
    ("substrate.ateapi.v1.tonic.rs", "W1SlYm/fvdEsUCKk8y11ehsvUY8NCDqajhjxFVJK01o="),
];

#[test]
fn the_published_proto_and_generated_artifacts_keep_their_hashes() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provisioner/substrate");
    let proto = std::fs::read(source.join("proto/api/ateapi/v1/ateapi.proto"))
        .expect("read the vendored installed-wire proto");
    assert_eq!(STANDARD.encode(Sha256::digest(proto)), PROTO_HASH, "wire at {UPSTREAM_COMMIT}");
    for (name, hash) in ARTIFACTS {
        let artifact = std::fs::read(source.join("ateapi_v1/substrate/ateapi/v1").join(name))
            .expect("read the checked-in generated artifact");
        assert_eq!(STANDARD.encode(Sha256::digest(artifact)), hash, "{name}");
    }
}

#[test]
fn the_pinned_generators_reproduce_the_checked_in_wire_when_buf_is_available() {
    match Command::new("buf").arg("--version").output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        result => assert!(result.expect("query buf version").status.success()),
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = tempfile::tempdir().expect("create generation directory");
    let result = Command::new("buf")
        .current_dir(&root)
        .args(["generate", "--template", "crates/ganja-teammate-cloud/buf.gen.yaml", "--output"])
        .arg(output.path())
        .output()
        .expect("run the pinned protobuf generators");
    assert!(
        result.status.success(),
        "protobuf generation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    for (name, _) in ARTIFACTS {
        let path = Path::new(
            "crates/ganja-teammate-cloud/src/provisioner/substrate/ateapi_v1/substrate/ateapi/v1",
        )
        .join(name);
        assert_eq!(
            std::fs::read(root.join(&path)).expect("read checked-in artifact"),
            std::fs::read(output.path().join(path)).expect("read regenerated artifact"),
            "regeneration changed {name}",
        );
    }
}

#[test]
fn lifecycle_requests_carry_the_published_actor_and_uid_fields() {
    let actor = Some(ObjectRef { atespace: "g".to_owned(), name: "a".to_owned() });
    let delete = DeleteActorRequest {
        actor: actor.clone(),
        options: Some(DeleteOptions { uid: "u".to_owned(), version: 0 }),
    };
    // Independent bytes pin ObjectRef nesting and the UID's field number.
    assert_eq!(delete.encode_to_vec(), b"\x0a\x06\x0a\x01g\x12\x01a\x12\x03\x12\x01u");
    assert_eq!(
        ResumeActorRequest { actor: actor.clone() }.encode_to_vec(),
        b"\x0a\x06\x0a\x01g\x12\x01a"
    );
    let suspend = SuspendActorRequest {
        actor,
        options: Some(SuspendOptions { mode: DurabilityMode::Asynchronous.into() }),
    };
    assert_eq!(suspend.encode_to_vec(), b"\x0a\x06\x0a\x01g\x12\x01a\x12\x02\x08\x02");
}

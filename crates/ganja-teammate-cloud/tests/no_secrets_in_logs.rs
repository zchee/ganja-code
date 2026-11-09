//! Credential-bearing wrappers never format the values they carry. Transport
//! failure paths are exercised with the real client once a worker is running.

use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap, Endpoint, ProvisionError, ProvisionErrorKind, Worker, WorkerHandle,
};
use http::{HeaderMap, HeaderValue};
use secrecy::SecretString;

#[test]
fn credential_content_is_absent_from_cloud_wrapper_formatting() {
    let canary = "cloud-canary-no-real-credential";
    let mut headers = HeaderMap::new();
    headers.insert("x-worker-routing", HeaderValue::from_str(canary).expect("header"));
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:4096/{canary}"),
        headers,
        username: canary.to_owned(),
        password: SecretString::from(canary.to_owned()),
    };
    let worker = Worker { handle: WorkerHandle::new(canary), endpoint };
    let bootstrap = Bootstrap {
        repository: format!("https://{canary}@example.invalid/repository"),
        reference: canary.to_owned(),
        branch: canary.to_owned(),
        author: Author { name: canary.to_owned(), email: canary.to_owned() },
        deadline_secs: 180,
    };
    let error = ProvisionError { operation: "resume", kind: ProvisionErrorKind::Ambiguous };
    for rendered in
        [format!("{worker:?}"), format!("{bootstrap:?}"), format!("{error:?}"), error.to_string()]
    {
        assert!(
            !rendered.contains(canary),
            "a credential-bearing wrapper must redact its contents"
        );
    }
}

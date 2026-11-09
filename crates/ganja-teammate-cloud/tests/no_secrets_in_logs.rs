//! Synthetic credentials traverse the real client, member and server failures.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ganja_core::teammate::TeammateRegistry;
use ganja_protocol::team::{Frame, IdleNotification};
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};
use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap, Endpoint, ProvisionError, ProvisionErrorKind, Worker, WorkerHandle,
};
use ganja_teammate_cloud::state::Deadlines;
use ganja_teammate_cloud::{CloudBackend, control};
use ganja_testkit::provisioner::{FakeProvisioner, ProvisionOperation};
use http::{HeaderMap, HeaderValue};
use secrecy::SecretString;

const PASSWORD: &str = "cloud-password-canary-no-real-credential";
const TOKEN: &str = "cloud-token-canary-no-real-credential";
const KEY: &str = "cloud-key-canary-no-real-credential";

fn assert_redacted(text: &str) {
    for canary in [PASSWORD, TOKEN, KEY] {
        assert!(!text.contains(canary), "raw synthetic credential escaped");
        assert!(
            !text.contains(&base64::prelude::BASE64_STANDARD.encode(canary)),
            "encoded synthetic credential escaped"
        );
    }
    assert!(
        !text.contains(&base64::prelude::BASE64_STANDARD.encode(format!("{TOKEN}:{PASSWORD}"))),
        "Basic credential escaped"
    );
}

fn wrapper_formatting_is_redacted() {
    let mut headers = HeaderMap::new();
    headers.insert("x-worker-routing", HeaderValue::from_static(TOKEN));
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:4096/{KEY}"),
        headers,
        username: TOKEN.to_owned(),
        password: SecretString::from(PASSWORD),
    };
    let worker = Worker { handle: WorkerHandle::new(KEY), endpoint };
    let bootstrap = Bootstrap {
        repository: format!("https://{TOKEN}@example.invalid/repository"),
        reference: KEY.to_owned(),
        branch: KEY.to_owned(),
        author: Author { name: TOKEN.to_owned(), email: KEY.to_owned() },
        deadline_secs: 180,
    };
    let error = ProvisionError { operation: "resume", kind: ProvisionErrorKind::Ambiguous };
    for rendered in
        [format!("{worker:?}"), format!("{bootstrap:?}"), format!("{error:?}"), error.to_string()]
    {
        assert_redacted(&rendered);
    }
}

/// A Kubernetes control plane whose every credential and failure body is a canary.
async fn canary_kubernetes(deny_token: bool) -> (kube::Client, tokio::task::JoinHandle<()>) {
    use axum::extract::{Request, State};

    let ca_key = rcgen::KeyPair::generate().expect("fixture CA key");
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_pem = ca_params.self_signed(&ca_key).expect("fixture CA").pem();

    async fn answer(
        State((ca_pem, deny_token)): State<(String, bool)>,
        request: Request,
    ) -> (http::StatusCode, axum::Json<serde_json::Value>) {
        let path = request.uri().path().to_owned();
        let body = |value| (http::StatusCode::OK, axum::Json(value));
        if path == "/apis/certificates.k8s.io/v1" {
            return body(
                serde_json::json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":"certificates.k8s.io/v1","resources":[
                    {"name":"clustertrustbundles","kind":"ClusterTrustBundle","namespaced":false,"verbs":["list"]},
                    {"name":"podcertificaterequests","kind":"PodCertificateRequest","namespaced":true,"verbs":["create","get","delete"]}
                ]}),
            );
        }
        if path == "/api/v1/namespaces/ate-users/serviceaccounts/ganja/token" {
            if deny_token {
                return (
                    http::StatusCode::FORBIDDEN,
                    axum::Json(
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":403,"message":TOKEN}),
                    ),
                );
            }
            let expiry = jiff::Timestamp::now()
                .checked_add(std::time::Duration::from_secs(3600))
                .expect("expiry")
                .to_string();
            return (
                http::StatusCode::CREATED,
                axum::Json(
                    serde_json::json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{},"status":{"token":TOKEN,"expirationTimestamp":expiry}}),
                ),
            );
        }
        if path.ends_with("/clustertrustbundles") {
            return body(
                serde_json::json!({"apiVersion":"certificates.k8s.io/v1","kind":"ClusterTrustBundleList","metadata":{},"items":[
                    {"apiVersion":"certificates.k8s.io/v1","kind":"ClusterTrustBundle","metadata":{"name":"serving"},"spec":{"signerName":"servicedns.podcert.ate.dev/identity","trustBundle":ca_pem}}
                ]}),
            );
        }
        if path.contains("/podcertificaterequests") {
            if request.method() == http::Method::POST {
                let mut object: serde_json::Value = serde_json::json!({"apiVersion":"certificates.k8s.io/v1","kind":"PodCertificateRequest","metadata":{"name":"ganja-denied","namespace":"ate-system"}});
                object["status"] = serde_json::json!({"conditions":[{"type":"Denied","status":"True","message":KEY}]});
                return (http::StatusCode::CREATED, axum::Json(object));
            }
            if request.method() == http::Method::GET {
                return body(
                    serde_json::json!({"apiVersion":"certificates.k8s.io/v1","kind":"PodCertificateRequest","metadata":{"name":"ganja-denied","namespace":"ate-system"},"status":{"conditions":[{"type":"Denied","status":"True","message":KEY}]}}),
                );
            }
            return body(serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"}));
        }
        (
            http::StatusCode::NOT_FOUND,
            axum::Json(
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":KEY}),
            ),
        )
    }

    let listener =
        tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("fake Kubernetes listener");
    let address = listener.local_addr().expect("fake Kubernetes address");
    let router = axum::Router::new().fallback(answer).with_state((ca_pem, deny_token));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve fake Kubernetes");
    });
    let config = kube::Config::new(format!("http://{address}").parse().expect("loopback URI"));
    let client = ganja_teammate_cloud::provisioner::substrate::kube::client_from_config(config)
        .expect("fixture client");
    (client, server)
}

/// The kube/ateapi half: ganja's formatting and shipped filters redact credentials.
async fn substrate_credentials_are_redacted() {
    use ganja_teammate_cloud::provisioner::substrate::identity::AteapiIdentity;
    use ganja_teammate_cloud::provisioner::substrate::kube::KubeTunnel;

    let (client, server) = canary_kubernetes(false).await;
    let identity = AteapiIdentity::new(client.clone()).await.expect("canary bearer issued");
    let request = identity.authorize(()).await.expect("authorized control request");
    assert_redacted(&format!("{identity:?} {:?}", request.metadata()));

    // Denied router issuance carries the key canary in its condition message.
    let denied = KubeTunnel::connect(client).await.expect_err("denied router identity");
    assert_redacted(&format!("{denied:?} {denied}"));
    server.abort();

    let (client, server) = canary_kubernetes(true).await;
    let refused = AteapiIdentity::new(client).await.expect_err("denied bearer issuance");
    assert_redacted(&format!("{refused:?} {refused}"));
    server.abort();

    let settings = ganja_teammate_cloud::provisioner::substrate::settings::Settings::parse(&format!(
        "kubeconfig = '/operator/{KEY}'\ntemplate = 'worker'\ngit_credential_uri = 'ate-secret://kubernetes/ganja-cloud/git/token'\n"
    ))
    .expect("settings with canary path");
    assert_redacted(&format!("{settings:?}"));
    let invalid = ganja_teammate_cloud::provisioner::substrate::settings::Settings::parse(&format!(
        "kubeconfig = '/operator/config'\ntemplate = 'worker'\ngit_credential_uri = 'ate-secret://kubernetes/ganja-cloud/git/token'\npassword = '{PASSWORD}'\n"
    ))
    .expect_err("unknown key refused");
    assert_redacted(&format!("{invalid:?} {invalid}"));
}

#[test]
fn substrate_credentials_stay_redacted_with_both_shipped_log_filters() {
    const FILTER_ENV: &str = "GANJA_TEST_SUBSTRATE_LOG_FILTER";
    if let Ok(filter) = std::env::var(FILTER_ENV) {
        let capture = ganja_testkit::LogCapture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_ansi(false)
                .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
                .finish(),
        )
        .expect("isolated child subscriber");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("canary runtime")
            .block_on(substrate_credentials_are_redacted());
        assert_redacted(&capture.logged());
        return;
    }
    // Each subscriber is process-wide, so isolate filter variants from one another
    // and from the member/serve canary test in this binary.
    for filter in ["info", "info,ganja=debug"] {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "substrate_credentials_stay_redacted_with_both_shipped_log_filters",
                "--nocapture",
            ])
            .env(FILTER_ENV, filter)
            .output()
            .expect("run filter canary child");
        assert!(
            output.status.success(),
            "filter {filter}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_redacted(&String::from_utf8_lossy(&output.stdout));
        assert_redacted(&String::from_utf8_lossy(&output.stderr));
    }
}

struct EchoFailure;

impl ganja_serve::bootstrap::Executor for EchoFailure {
    fn execute(
        &self,
        _request: ganja_serve::bootstrap::Bootstrap,
        _deadline: tokio::time::Instant,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ganja_serve::bootstrap::Cloned, ganja_serve::bootstrap::Failure>,
    > {
        Box::pin(async {
            Err(ganja_serve::bootstrap::Failure {
                stage: ganja_serve::bootstrap::Stage::Clone,
                detail: format!("clone refused: {PASSWORD}"),
            })
        })
    }
}

#[tokio::test]
async fn credential_content_is_absent_from_runtime_errors_mail_and_logs() {
    let capture = ganja_testkit::LogCapture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            // The shipped verbose filter keeps dependency targets at info.
            // Enabling third-party HTTP/Kubernetes debug targets is unsupported:
            // kube_client can print raw remote Status bodies at that level.
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,ganja=debug"))
            .finish(),
    )
    .expect("one subscriber in this test binary");
    tracing::info!("cloud runtime canary capture is active");
    wrapper_formatting_is_redacted();
    substrate_credentials_are_redacted().await;

    let home = tempfile::tempdir().expect("home");
    let bootstrap = ganja_serve::bootstrap::Service::new(
        ganja_serve::Credentials {
            username: TOKEN.to_owned(),
            password: SecretString::from(PASSWORD),
        },
        home.path().to_owned(),
        Arc::new(EchoFailure),
    )
    .serve(tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener"))
    .expect("bootstrap service");
    let client = ganja_client::Client::new(
        &format!("http://{}", bootstrap.address()),
        Some(ganja_client::Credentials::new(TOKEN, PASSWORD)),
    )
    .expect("client");
    let outcome = client
        .bootstrap(
            &ganja_client::Bootstrap {
                repository: "https://example.invalid/repo.git".to_owned(),
                reference: "main".to_owned(),
                branch: "worker".to_owned(),
                author: ganja_client::BootstrapAuthor {
                    name: "worker".to_owned(),
                    email: "worker@example.invalid".to_owned(),
                },
            },
            Duration::from_secs(5),
        )
        .await
        .expect("typed bootstrap refusal");
    assert!(matches!(outcome, ganja_client::BootstrapOutcome::Failed { .. }));
    assert_redacted(&format!("{outcome:?}"));
    assert_redacted(&format!("{:?}", client.bootstrap_status().await.expect("failure status")));
    bootstrap.shutdown().await.expect("bootstrap stopped");

    let storage = ganja_core::Storage::open(home.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);
    let (provider, _) = ganja_testkit::ScriptedProvider::new(vec![
        ganja_testkit::says("first answer"),
        ganja_testkit::says("second answer"),
    ]);
    let engine = Arc::new(ganja_core::Engine::persistent(
        provider,
        "fake",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
        storage.clone(),
    ));
    engine.resume(&session).await.expect("seeded session");
    let mut config = ganja_serve::ServeConfig::in_directory(home.path().to_owned());
    config.storage = Some(storage);
    let (provisioner, server) =
        FakeProvisioner::serving(engine, config, TOKEN, PASSWORD).await.expect("real worker");
    let provisioner = Arc::new(provisioner);
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("canary-test").expect("team");
    let name = "canary-worker";
    let registry = Arc::new(TeammateRegistry::new(root.clone(), team.clone(), "lead", home.path()));
    let cloud = CloudBackend::configured(
        ganja_core::config::CloudConfig {
            provisioner: "fake".to_owned(),
            username: TOKEN.to_owned(),
            no_reply_email: "worker@example.invalid".to_owned(),
            git_host: "example.invalid".to_owned(),
            bootstrap: ganja_core::config::CloudBootstrap {
                repository: "https://example.invalid/repo.git".to_owned(),
                reference: "main".to_owned(),
                branch_prefix: "workers".to_owned(),
            },
        },
        provisioner.clone(),
        Some(SecretString::from(PASSWORD)),
        Deadlines {
            launch: Duration::from_secs(5),
            bootstrap: Duration::from_secs(5),
            health: Duration::from_secs(3),
            quiesce: Duration::from_millis(200),
            cleanup: Duration::from_millis(200),
            terminal: Duration::from_secs(2),
        },
    );
    assert_redacted(&format!("{cloud:?}"));
    let door = ganja_core::Teammates::new(
        registry.clone(),
        ganja_core::Backends::new().with(Arc::new(cloud)),
    );
    door.start(
        ganja_testkit::spawn_with_prompt(name, Some("cloud"), "first request"),
        &ganja_testkit::caller(home.path()),
        &ganja_testkit::AllowSpawn,
    )
    .await
    .expect("start member");
    let member = control(name).expect("control");
    tokio::time::timeout(Duration::from_secs(5), async {
        while member.phase() != ganja_teammate_cloud::state::Phase::Suspended {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first suspended boundary");
    server.reply_next("/event", 503);
    let shed = member.resume_for_inspection().await.expect_err("shed reconnect");
    assert_redacted(&format!("{shed:?}"));
    assert_redacted(&shed.to_string());
    mailbox::write(
        &root.inbox_path(&team, &MemberName::parse(name).expect("name")),
        ganja_team::MailboxMessage::new(
            "team-lead",
            "next request",
            ganja_team::record::now_iso8601(),
        ),
    )
    .expect("second request");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mail = mailbox::read(&registry.lead_inbox()).expect("mail");
            let idle = mail
                .valid
                .iter()
                .filter(|message| {
                    matches!(
                        serde_json::from_str::<Frame>(&message.text),
                        Ok(Frame::IdleNotification(IdleNotification { .. }))
                    )
                })
                .count();
            if idle == 2 && member.phase() == ganja_teammate_cloud::state::Phase::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("uncertain worker continues without automatic suspend");
    provisioner.fail_next(ProvisionOperation::Kill, ProvisionErrorKind::Refused);
    let error = member.retire().await.expect_err("unconfirmed cleanup");
    assert_redacted(&format!("{error:?}"));
    assert_redacted(&error.to_string());
    let mail = mailbox::read(&registry.lead_inbox()).expect("mail");
    assert!(mail.valid.iter().any(|message| message.text.contains("automatic suspend is off")));
    assert!(mail.valid.iter().any(|message| message.text.contains("deletion is unconfirmed")));
    for message in mail.valid {
        assert_redacted(&message.text);
    }
    registry.shutdown().await;
    server.close().await.expect("server closed");
    let logs = capture.logged();
    assert!(logs.contains("cloud runtime canary capture is active"));
    assert_redacted(&logs);
}

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
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
    .expect("one subscriber in this test binary");
    tracing::info!("cloud runtime canary capture is active");
    wrapper_formatting_is_redacted();

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
    let member = control(&registry, name).expect("control");
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

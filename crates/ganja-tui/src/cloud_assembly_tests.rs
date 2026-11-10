use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt as _;
use ganja_protocol::MemberBackend;

use super::*;

fn assert_local_backends(backends: &Backends) {
    for backend in [
        MemberBackend::Ganja,
        MemberBackend::Claude,
        MemberBackend::Codex,
        MemberBackend::Agy,
        MemberBackend::Grok,
    ] {
        assert!(backends.of(backend).is_some(), "missing local backend: {backend:?}");
    }
    assert!(
        backends.of(MemberBackend::InProcess).is_none(),
        "the engine owns the in-process backend"
    );
}

#[tokio::test]
async fn an_unconfigured_lead_keeps_local_backends_without_cloud_io() {
    let backends = backends(
        &Config::default(),
        false,
        || panic!("unconfigured startup must not resolve cloud home"),
        Duration::from_secs(1),
        |_, _| async { panic!("unconfigured startup must not connect") },
    )
    .await
    .unwrap();
    assert_local_backends(&backends);
    assert!(backends.of(MemberBackend::Cloud).is_none());
}

fn configured() -> Config {
    Config {
        cloud: Some(CloudConfig {
            provisioner: "substrate".to_owned(),
            username: "ganja".to_owned(),
            no_reply_email: "worker@example.invalid".to_owned(),
            git_host: "example.invalid".to_owned(),
            bootstrap: ganja_core::config::CloudBootstrap {
                repository: "https://example.invalid/team/repo.git".to_owned(),
                reference: "main".to_owned(),
                branch_prefix: "worker".to_owned(),
            },
        }),
        ..Config::default()
    }
}

fn cloud_backend(config: CloudConfig) -> Arc<dyn TeammateBackend> {
    let endpoint = ganja_teammate_cloud::provisioner::Endpoint {
        base_url: "http://127.0.0.1:1".to_owned(),
        headers: Default::default(),
        username: "ganja".to_owned(),
        password: "test-only".to_owned().into(),
    };
    Arc::new(ganja_teammate_cloud::CloudBackend::configured(
        config,
        Arc::new(ganja_testkit::provisioner::FakeProvisioner::from_endpoint(endpoint)),
        None,
        ganja_teammate_cloud::state::Deadlines::default(),
    ))
}

#[tokio::test]
async fn a_configured_lead_adds_cloud_without_replacing_local_backends() {
    let connected = AtomicBool::new(false);
    let backends = backends(
        &configured(),
        false,
        || Some(PathBuf::from("/test-config")),
        Duration::from_secs(1),
        |config, home| {
            let connected = &connected;
            async move {
                assert_eq!(config.provisioner, "substrate");
                assert_eq!(home, PathBuf::from("/test-config"));
                connected.store(true, Ordering::SeqCst);
                Ok(cloud_backend(config))
            }
        },
    )
    .await
    .unwrap();
    assert!(connected.load(Ordering::SeqCst));
    assert_local_backends(&backends);
    assert!(backends.of(MemberBackend::Cloud).is_some());
}

#[tokio::test]
async fn a_pane_member_never_connects_a_cloud_provisioner() {
    let backends = backends(
        &configured(),
        true,
        || panic!("members do not read cloud settings"),
        Duration::from_secs(1),
        |_, _| async { panic!("members do not connect cloud provisioners") },
    )
    .await
    .unwrap();
    assert_local_backends(&backends);
    assert!(backends.of(MemberBackend::Cloud).is_none());
}

#[tokio::test]
async fn an_unknown_provisioner_fails_before_any_connection() {
    let mut config = configured();
    config.cloud.as_mut().unwrap().provisioner = "unknown-sensitive-setting".to_owned();
    let result = backends(
        &config,
        false,
        || panic!("unknown provisioner must fail before resolving home"),
        Duration::from_secs(1),
        |_, _| async { panic!("unknown provisioner must not connect") },
    )
    .await;
    assert_eq!(result.err().unwrap().to_string(), "cloud configuration failed");
}

#[tokio::test]
async fn a_missing_cloud_home_reports_a_sanitized_configuration_error() {
    let result = backends(
        &configured(),
        false,
        || None,
        Duration::from_secs(1),
        |_, _| async { panic!("missing cloud home must fail before connection") },
    )
    .await;
    assert_eq!(result.err().unwrap().to_string(), "cloud configuration failed");
}

#[tokio::test]
async fn a_failed_cloud_connector_reports_no_private_details() {
    let result = backends(
        &configured(),
        false,
        || Some(PathBuf::from("/test-config")),
        Duration::from_secs(1),
        |_, _| async { Err(anyhow::anyhow!("private-endpoint-header-and-token-canary")) },
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(format!("{error:#}"), "cloud connection failed");
    assert!(!format!("{error:?}").contains("canary"));
}

#[tokio::test(start_paused = true)]
async fn a_stalled_cloud_connector_does_not_hold_startup_forever() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        backends(
            &configured(),
            false,
            || Some(PathBuf::from("/test-config")),
            Duration::from_secs(1),
            |_, _| async move {
                let _guard = guard;
                std::future::pending().await
            },
        ),
    )
    .await
    .expect("assembly must enforce its own connection budget");
    assert_eq!(result.err().unwrap().to_string(), "cloud connection timed out");
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert!(dropped.load(Ordering::SeqCst), "expiry must drop the connector");
}

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn an_interrupt_between_startup_stages_is_not_lost() {
    let mut interrupt: Option<Interrupt> = Some(async { Ok(()) }.boxed());
    let result = during_startup(&mut interrupt, async { 7 }).await;
    assert_eq!(result.unwrap_err().to_string(), "cloud connection interrupted");
}

#[tokio::test]
async fn the_same_interrupt_survives_a_completed_startup_stage() {
    let (send, receive) = tokio::sync::oneshot::channel();
    let mut interrupt: Option<Interrupt> = Some(
        async move {
            receive.await.expect("the test sends an interrupt");
            Ok(())
        }
        .boxed(),
    );
    assert_eq!(during_startup(&mut interrupt, async { 7 }).await.unwrap(), 7);
    send.send(()).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let result = during_startup(&mut interrupt, async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    })
    .await;
    assert_eq!(result.unwrap_err().to_string(), "cloud connection interrupted");
    assert!(dropped.load(Ordering::SeqCst), "interruption must drop the startup operation");
}

#[tokio::test]
async fn startup_without_an_interrupt_listener_preserves_its_result() {
    assert_eq!(during_startup(&mut None, async { 7 }).await.unwrap(), 7);
}

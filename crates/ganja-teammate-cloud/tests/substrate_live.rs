//! Live smoke tests against the owner's Agent Substrate installation.
//!
//! Mock first, live second: the lifecycle state machine, reconciliation and
//! redaction are pinned by the loopback suites. What this adds is the one
//! thing a loopback fixture cannot — that the identities, trust bundles and
//! RPCs this build sends are ones the installed release accepts today.
//!
//! Live tests are `#[ignore]`, so ordinary `cargo test` runs only off-cluster checks;
//! `cargo test -- --ignored` finds live tests inert unless `GANJA_LIVE_TEST=1`.
//! With the opt-in set, a missing input is a hard failure that names the
//! variable, never a silent pass. The owner runs them:
//!
//! ```sh
//! GANJA_LIVE_TEST=1 \
//! GANJA_SUBSTRATE_CONFIG_HOME=~/.config/ganja \
//! cargo test -p ganja-teammate-cloud --test substrate_live -- --ignored control_plane_authorized_reads
//! ```
//!
//! `actor_lifecycle` additionally needs `GANJA_CLOUD_PASSWORD` and a worker
//! template whose golden snapshot is ready; it creates, resumes, suspends and
//! deletes one Actor it owns and touches no shared component.

use std::future::Future;
use std::path::PathBuf;

use ganja_team::MemberName;
use ganja_teammate_cloud::provisioner::substrate::SubstrateProvisioner;
use ganja_teammate_cloud::provisioner::substrate::ateapi_v1::control_client::ControlClient;
use ganja_teammate_cloud::provisioner::substrate::ateapi_v1::{
    GetAtespaceRequest, ListActorTemplatesRequest, ListOptions, ObjectRef,
};
use ganja_teammate_cloud::provisioner::substrate::identity::AteapiIdentity;
use ganja_teammate_cloud::provisioner::substrate::kube::{KubeTunnel, client_from_config};
use ganja_teammate_cloud::provisioner::substrate::kubectl::KubectlTunnel;
use ganja_teammate_cloud::provisioner::substrate::settings::Settings;
use ganja_teammate_cloud::provisioner::substrate::tunnel::Tunnel as _;
use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap, ProvisionError, ProvisionErrorKind, Provisioner as _, SuspendOutcome,
    WorkerSpec, WorkerState,
};

/// Variable that has to be `1` before any of this talks to the cluster.
const LIVE_ENV: &str = "GANJA_LIVE_TEST";

/// Where the owner's `cloud/substrate.toml` lives for the live run.
const CONFIG_HOME_ENV: &str = "GANJA_SUBSTRATE_CONFIG_HOME";

/// True when the owner opted in; `--ignored` alone stays inert.
fn opted_in() -> bool {
    let opted = std::env::var(LIVE_ENV).is_ok_and(|value| value == "1");
    if !opted {
        eprintln!("skipping: {LIVE_ENV}=1 is not set, so nothing talks to the cluster");
    }
    opted
}

/// Reads a required live input, failing loudly rather than skipping.
fn required(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => panic!("{LIVE_ENV}=1 is set but {name} is missing; export it for the live run"),
    }
}

async fn live_provisioner() -> SubstrateProvisioner {
    let config_home = PathBuf::from(required(CONFIG_HOME_ENV));
    SubstrateProvisioner::connect(&config_home, "ganja")
        .await
        .expect("connect to the installed Substrate control plane")
}

/// Proves scoped control reads, not a spawned-actor proxy connection.
///
/// The strict settings file must be complete, but its named template is not
/// looked up and no worker password is read. The optional command connector
/// contributes its control channel only; it does not start an actor proxy.
#[tokio::test]
#[ignore = "talks to the owner's cluster; needs GANJA_LIVE_TEST=1 and GANJA_SUBSTRATE_CONFIG_HOME"]
async fn control_plane_authorized_reads() {
    if !opted_in() {
        return;
    }
    let config_home = PathBuf::from(required(CONFIG_HOME_ENV));
    let settings = Settings::load(&config_home).expect("owner settings");

    let kubeconfig =
        kube::config::Kubeconfig::read_from(&settings.kubeconfig).expect("operator kubeconfig");
    let options =
        kube::config::KubeConfigOptions { context: settings.context.clone(), ..Default::default() };
    let config =
        kube::Config::from_custom_kubeconfig(kubeconfig, &options).await.expect("client config");
    let client = client_from_config(config).expect("operator client");

    // TokenRequest and the live trust bundle, through both connectors.
    let identity = AteapiIdentity::new(client.clone()).await.expect("ate-users/ganja token");
    identity.authorize(()).await.expect("renewable bearer");
    let in_process =
        KubeTunnel::with_services(client.clone(), &settings.api_service, &settings.router_service)
            .await
            .expect("in-process connector: trust bundle, control channel, router certificate");
    let mut channels = vec![in_process.control().await.expect("in-process control channel")];
    if let Some(kubectl) = settings.kubectl.clone() {
        let command = KubectlTunnel::connect(
            client.clone(),
            kubectl,
            settings.kubeconfig.clone(),
            settings.context.clone(),
            &settings.api_service,
        )
        .await
        .expect("command connector over the same serving trust");
        channels.push(command.control().await.expect("command control channel"));
    }

    for channel in channels {
        let mut control = ControlClient::new(channel);
        let atespace = control
            .get_atespace(
                identity
                    .authorize(GetAtespaceRequest {
                        atespace: Some(ObjectRef {
                            atespace: String::new(),
                            name: settings.atespace.clone(),
                        }),
                    })
                    .await
                    .expect("authorize atespace read"),
            )
            .await
            .expect("GetAtespace as the ganja editor")
            .into_inner();
        assert_eq!(atespace.metadata.expect("atespace metadata").name, settings.atespace);
        let templates = control
            .list_actor_templates(
                identity
                    .authorize(ListActorTemplatesRequest {
                        atespace: settings.atespace.clone(),
                        options: Some(ListOptions { page_size: 100, page_token: String::new() }),
                    })
                    .await
                    .expect("authorize template list"),
            )
            .await
            .expect("ListActorTemplates")
            .into_inner();
        eprintln!("live control reads: {} template(s) listed", templates.actor_templates.len());
    }
}

#[tokio::test]
#[ignore = "creates and deletes one Actor; needs GANJA_LIVE_TEST=1, GANJA_SUBSTRATE_CONFIG_HOME, GANJA_CLOUD_PASSWORD and a ready worker template"]
async fn actor_lifecycle() {
    if !opted_in() {
        return;
    }
    required("GANJA_CLOUD_PASSWORD");
    let provisioner = live_provisioner().await;
    let spec = WorkerSpec {
        name: MemberName::parse("live-probe").expect("member name"),
        lead: MemberName::parse("team-lead").expect("lead name"),
        bootstrap: Bootstrap {
            repository: "https://github.com/zchee/ganja-code.git".to_owned(),
            reference: "main".to_owned(),
            branch: "workers/live-probe".to_owned(),
            author: Author {
                name: "ganja-cloud-live-probe".to_owned(),
                email: "noreply@example.invalid".to_owned(),
            },
            deadline_secs: 180,
        },
        git_host: "github.com".to_owned(),
    };
    let worker = provisioner.spawn(&spec).await.expect("spawn a SUSPENDED Actor");
    eprintln!("live actor: {}", provisioner.describe(&worker));
    let outcome = finish_with_cleanup(
        async {
            let initial = provisioner.state(&worker).await?;
            let resumed = provisioner.resume(&worker).await?;
            let suspended = provisioner.suspend(&worker).await?;
            provisioner.resume(&worker).await?;
            Ok::<_, ganja_teammate_cloud::provisioner::ProvisionError>((
                initial, resumed, suspended,
            ))
        },
        provisioner.kill(&worker),
        ProvisionError { operation: "live lifecycle timeout", kind: ProvisionErrorKind::Transient },
    )
    .await;
    // Assertions run only after UID-guarded deletion, including failed RPCs.
    let (initial, resumed, suspended) = outcome.expect("live lifecycle and cleanup");
    assert_eq!(initial, WorkerState::Suspended);
    assert!(resumed.was_suspended, "the created Actor resumes from SUSPENDED");
    assert_eq!(suspended, SuspendOutcome::Suspended, "a running Actor suspends on request");
    assert_eq!(provisioner.state(&worker).await.expect("reconciling read"), WorkerState::Gone);
}

async fn finish_with_cleanup<T, E>(
    operation: impl Future<Output = Result<T, E>>,
    cleanup: impl Future<Output = Result<(), E>>,
    timeout_error: E,
) -> Result<T, E> {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), operation)
        .await
        .unwrap_or(Err(timeout_error));
    cleanup.await?;
    outcome
}

#[tokio::test(start_paused = true)]
async fn a_hung_lifecycle_still_runs_cleanup_before_reporting_its_timeout() {
    let cleaned = std::cell::Cell::new(false);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(301),
        finish_with_cleanup(
            std::future::pending::<Result<(), &str>>(),
            async {
                cleaned.set(true);
                Ok(())
            },
            "lifecycle timed out",
        ),
    )
    .await;
    assert!(cleaned.get(), "the lifecycle deadline must not bypass cleanup");
    assert_eq!(outcome.expect("bounded lifecycle and cleanup"), Err("lifecycle timed out"));
}

#[tokio::test]
async fn lifecycle_outcomes_are_inspected_only_after_cleanup() {
    use std::cell::RefCell;

    for outcome in [Ok(()), Err("lifecycle failed")] {
        let events = RefCell::new(Vec::new());
        let result = finish_with_cleanup(
            async {
                events.borrow_mut().push("lifecycle");
                outcome
            },
            async {
                events.borrow_mut().push("cleanup");
                Ok(())
            },
            "lifecycle timed out",
        )
        .await;
        events.borrow_mut().push("inspect");
        assert_eq!(result, outcome);
        assert_eq!(*events.borrow(), ["lifecycle", "cleanup", "inspect"]);
    }
    assert_eq!(
        finish_with_cleanup(
            async { Ok(()) },
            async { Err("cleanup failed") },
            "lifecycle timed out"
        )
        .await,
        Err("cleanup failed")
    );
}

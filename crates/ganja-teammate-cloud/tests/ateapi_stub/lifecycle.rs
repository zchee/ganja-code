//! Real HTTP/2 gRPC fixture for the public provisioner boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::future::BoxFuture;
use ganja_team::MemberName;
use ganja_teammate_cloud::provisioner::substrate::SubstrateProvisioner;
use ganja_teammate_cloud::provisioner::substrate::ateapi_v1::*;
use ganja_teammate_cloud::provisioner::substrate::identity::AteapiIdentity;
use ganja_teammate_cloud::provisioner::substrate::settings::Settings;
use ganja_teammate_cloud::provisioner::substrate::tunnel::{Tunnel, TunnelIo};
use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap, ProvisionError, ProvisionErrorKind, Provisioner, SuspendOutcome, WorkerSpec,
    WorkerState,
};
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tonic::body::Body;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use tower_service::Service;

use super::{KubeFixture, TOKEN_CANARY};

const ERROR_CANARY: &str = "synthetic-control-error-secret-canary";

#[derive(Debug, Default)]
struct ControlState {
    actors: BTreeMap<String, Actor>,
    policies: BTreeMap<String, EgressPolicy>,
    calls: Vec<&'static str>,
    retired: Vec<String>,
    lost: BTreeSet<&'static str>,
    template_ready: bool,
    deny_policy: bool,
    deny_delete: bool,
    wrong_created_identity: bool,
    wrong_read_identity: bool,
    replace_on_revert: bool,
    deny_revert: bool,
    noop_revert: bool,
    held_policy: Option<(Arc<Notify>, Arc<Notify>)>,
    held_create_reply: Option<(Arc<Notify>, Arc<Notify>)>,
    held_get_reply: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[derive(Clone)]
struct ControlStub(Arc<Mutex<ControlState>>);

struct Unary<I, O> {
    state: Arc<Mutex<ControlState>>,
    operation: &'static str,
    handler: fn(&mut ControlState, I) -> Result<O, Status>,
    marker: PhantomData<fn(I) -> O>,
}

impl<I: Send + 'static, O: Send + 'static> tonic::server::UnaryService<I> for Unary<I, O> {
    type Response = O;
    type Future = BoxFuture<'static, Result<Response<O>, Status>>;

    fn call(&mut self, request: Request<I>) -> Self::Future {
        let state = Arc::clone(&self.state);
        let operation = self.operation;
        let handler = self.handler;
        Box::pin(async move {
            assert_eq!(
                request
                    .metadata()
                    .get("authorization")
                    .expect("control auth")
                    .to_str()
                    .expect("ASCII bearer"),
                format!("Bearer {TOKEN_CANARY}-1")
            );
            let held = {
                let mut state = state.lock().expect("control lock");
                state.calls.push(operation);
                (operation == "create policy").then(|| state.held_policy.clone()).flatten()
            };
            if let Some((entered, release)) = held {
                entered.notify_one();
                release.notified().await;
            }
            let (response, lost, held_reply) = {
                let mut state = state.lock().expect("control lock");
                let response = handler(&mut state, request.into_inner())?;
                let lost = state.lost.remove(operation);
                let held = match operation {
                    "create" => state.held_create_reply.clone(),
                    "get" => state.held_get_reply.take(),
                    _ => None,
                };
                (response, lost, held)
            };
            if let Some((entered, release)) = held_reply {
                entered.notify_one();
                release.notified().await;
            }
            if lost {
                return Err(Status::unavailable(ERROR_CANARY));
            }
            Ok(Response::new(response))
        })
    }
}

impl Service<http::Request<Body>> for ControlStub {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        macro_rules! method {
            ($input:ty, $output:ty, $operation:literal, $handler:expr) => {{
                let method = Unary::<$input, $output> {
                    state: Arc::clone(&self.0),
                    operation: $operation,
                    handler: $handler,
                    marker: PhantomData,
                };
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                        $output,
                        $input,
                    >::default());
                    Ok(grpc.unary(method, request).await)
                })
            }};
        }
        match request.uri().path() {
            "/substrate.ateapi.v1.Control/GetActorTemplate" => {
                method!(GetActorTemplateRequest, ActorTemplate, "get template", |state, request| {
                    assert_eq!(request.actor_template.expect("template reference").name, "worker");
                    Ok(template(state.template_ready))
                })
            }
            "/substrate.ateapi.v1.Control/CreateActor" => {
                method!(CreateActorRequest, Actor, "create", |state, request| {
                    let mut actor =
                        request.actor.ok_or_else(|| Status::invalid_argument(ERROR_CANARY))?;
                    let metadata = actor.metadata.as_mut().expect("actor metadata");
                    assert_eq!(metadata.atespace, "ganja");
                    assert_eq!(actor.source_tag.as_ref().expect("golden source").name, "golden");
                    metadata.uid = uuid::Uuid::now_v7().to_string();
                    metadata.version = 1;
                    actor.status = Some(ActorStatus {
                        state: ActorState::Suspended.into(),
                        ..Default::default()
                    });
                    state.actors.insert(metadata.name.clone(), actor.clone());
                    if state.wrong_created_identity {
                        actor.metadata.as_mut().expect("metadata").name = "unrelated".to_owned();
                    }
                    Ok(actor)
                })
            }
            "/substrate.ateapi.v1.Control/GetActor" => {
                method!(GetActorRequest, Actor, "get", |state, request| {
                    let name = request.actor.expect("actor reference").name;
                    let mut actor = state
                        .actors
                        .get(&name)
                        .cloned()
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))?;
                    if state.wrong_read_identity {
                        actor.metadata.as_mut().expect("metadata").name = "unrelated".to_owned();
                    }
                    Ok(actor)
                })
            }
            "/substrate.ateapi.v1.Control/ResumeActor" => {
                method!(ResumeActorRequest, ResumeActorResponse, "resume", |state, request| {
                    let name = request.actor.expect("actor reference").name;
                    let actor = state
                        .actors
                        .get_mut(&name)
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))?;
                    let status = actor.status.as_mut().expect("status");
                    if status.state == i32::from(ActorState::Crashed) {
                        return Err(Status::failed_precondition(ERROR_CANARY));
                    }
                    let resumed = status.state != i32::from(ActorState::Running);
                    status.state = ActorState::Running.into();
                    Ok(ResumeActorResponse { actor: Some(actor.clone()), resumed })
                })
            }
            "/substrate.ateapi.v1.Control/SuspendActor" => {
                method!(SuspendActorRequest, SuspendActorResponse, "suspend", |state, request| {
                    assert_eq!(
                        request.options.expect("explicit durability").mode,
                        i32::from(DurabilityMode::Asynchronous)
                    );
                    let name = request.actor.expect("actor reference").name;
                    let actor = state
                        .actors
                        .get_mut(&name)
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))?;
                    actor.status.as_mut().expect("status").state = ActorState::Suspended.into();
                    Ok(SuspendActorResponse { actor: Some(actor.clone()) })
                })
            }
            "/substrate.ateapi.v1.Control/DeleteActor" => {
                method!(DeleteActorRequest, (), "delete", |state, request| {
                    if state.deny_delete {
                        return Err(Status::permission_denied(ERROR_CANARY));
                    }
                    let name = request.actor.expect("actor reference").name;
                    let actor =
                        state.actors.get(&name).ok_or_else(|| Status::not_found(ERROR_CANARY))?;
                    let options = request.options.expect("deletion identity guard");
                    assert_eq!(options.version, 0);
                    if options.uid != actor.metadata.as_ref().expect("metadata").uid {
                        return Err(Status::failed_precondition(ERROR_CANARY));
                    }
                    state.actors.remove(&name);
                    state.policies.remove(&name);
                    Ok(())
                })
            }
            "/substrate.ateapi.v1.Control/CreateActorEgressPolicy" => method!(
                CreateActorEgressPolicyRequest,
                EgressPolicy,
                "create policy",
                |state, request| {
                    if state.deny_policy {
                        return Err(Status::permission_denied(ERROR_CANARY));
                    }
                    let name = request.actor.expect("parent reference").name;
                    let mut policy = request.egress_policy.expect("policy");
                    let metadata = policy.metadata.as_mut().expect("policy metadata");
                    assert_eq!(metadata.name, "default");
                    assert_eq!(metadata.atespace, "ganja");
                    metadata.uid = uuid::Uuid::now_v7().to_string();
                    metadata.version = 1;
                    state.policies.insert(name, policy.clone());
                    Ok(policy)
                }
            ),
            "/substrate.ateapi.v1.Control/GetActorEgressPolicy" => method!(
                GetActorEgressPolicyRequest,
                EgressPolicy,
                "get policy",
                |state, request| {
                    assert_eq!(request.name, "default");
                    state
                        .policies
                        .get(&request.actor.expect("actor reference").name)
                        .cloned()
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))
                }
            ),
            "/substrate.ateapi.v1.Control/UpdateActorEgressPolicy" => method!(
                UpdateActorEgressPolicyRequest,
                EgressPolicy,
                "update policy",
                |state, request| {
                    let name = request.actor.expect("actor reference").name;
                    let mut policy = request.egress_policy.expect("policy");
                    let previous = state
                        .policies
                        .get(&name)
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))?
                        .metadata
                        .as_ref()
                        .expect("previous metadata");
                    let next = policy.metadata.as_mut().expect("replacement metadata");
                    assert_eq!(next.uid, previous.uid);
                    assert_eq!(next.version, previous.version);
                    next.version += 1;
                    state.policies.insert(name, policy.clone());
                    Ok(policy)
                }
            ),
            "/substrate.ateapi.v1.Control/RevertActor" => {
                method!(RevertActorRequest, RevertActorResponse, "revert", |state, request| {
                    if state.deny_revert {
                        return Err(Status::permission_denied(ERROR_CANARY));
                    }
                    let actor = state
                        .actors
                        .get_mut(&request.actor.expect("actor reference").name)
                        .ok_or_else(|| Status::not_found(ERROR_CANARY))?;
                    if state.noop_revert {
                        return Ok(RevertActorResponse { actor: Some(actor.clone()) });
                    }
                    if state.replace_on_revert {
                        actor.metadata.as_mut().expect("metadata").uid =
                            uuid::Uuid::now_v7().to_string();
                    }
                    let status = actor.status.as_mut().expect("status");
                    status.state = ActorState::Suspended.into();
                    status.crash = None;
                    status.assigned_node.clear();
                    Ok(RevertActorResponse { actor: Some(actor.clone()) })
                })
            }
            "/substrate.ateapi.v1.Control/GetAtespace" => {
                method!(GetAtespaceRequest, Atespace, "get atespace", |_state, request| {
                    assert_eq!(request.atespace.expect("atespace reference").name, "ganja");
                    Ok(Atespace {
                        metadata: Some(ResourceMetadata {
                            name: "ganja".to_owned(),
                            ..Default::default()
                        }),
                    })
                })
            }
            "/substrate.ateapi.v1.Control/ListActorTemplates" => method!(
                ListActorTemplatesRequest,
                ListActorTemplatesResponse,
                "list templates",
                |state, request| {
                    assert_eq!(request.atespace, "ganja");
                    Ok(ListActorTemplatesResponse {
                        actor_templates: vec![template(state.template_ready)],
                        next_page_token: String::new(),
                    })
                }
            ),
            "/substrate.ateapi.v1.Control/CreateActorTemplate" => method!(
                CreateActorTemplateRequest,
                ActorTemplate,
                "create template",
                |_state, request| { Ok(request.actor_template.expect("template")) }
            ),
            _ => Box::pin(async {
                Ok(Status::unimplemented("unsupported fixture method").into_http())
            }),
        }
    }
}

fn template(ready: bool) -> ActorTemplate {
    ActorTemplate {
        metadata: Some(ResourceMetadata {
            atespace: "ganja".to_owned(),
            name: "worker".to_owned(),
            ..Default::default()
        }),
        status: Some(ActorTemplateStatus {
            golden_snapshot_status: Some(GoldenSnapshotStatus {
                golden_tag: ready
                    .then(|| ObjectRef { atespace: "ganja".to_owned(), name: "golden".to_owned() }),
                ..Default::default()
            }),
        }),
        ..Default::default()
    }
}

struct ControlFixture {
    state: Arc<Mutex<ControlState>>,
    task: JoinHandle<()>,
}

impl Drop for ControlFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
struct ControlTunnel(Channel, Arc<Mutex<ControlState>>);

#[async_trait]
impl Tunnel for ControlTunnel {
    async fn control(&self) -> Result<Channel, ProvisionError> {
        Ok(self.0.clone())
    }
    async fn actor(&self, _actor: &ObjectRef) -> Result<TunnelIo, ProvisionError> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut bytes = [0; 4096];
            assert_ne!(server.read(&mut bytes).await.expect("read worker request"), 0);
            server.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.expect("worker response");
            while server.read(&mut bytes).await.expect("worker stream") != 0 {}
        });
        Ok(Box::new(client))
    }

    async fn retire(&self, actor: &ObjectRef) {
        self.1.lock().expect("control state").retired.push(actor.name.clone());
    }
}

async fn fixture() -> (KubeFixture, ControlFixture, SubstrateProvisioner) {
    let kube = KubeFixture::start("v1", 0).await;
    let identity = AteapiIdentity::new(kube.client.clone()).await.expect("API identity");
    let state = Arc::new(Mutex::new(ControlState { template_ready: true, ..Default::default() }));
    let service = ControlStub(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gRPC fixture");
    let address = listener.local_addr().expect("gRPC fixture address");
    let incoming = futures::stream::unfold(listener, |listener| async {
        let connection = listener.accept().await.map(|(stream, _)| stream);
        Some((connection, listener))
    });
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .serve_with_incoming(service, incoming)
            .await
            .expect("serve gRPC without router");
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .expect("loopback endpoint")
        .connect()
        .await
        .expect("connect fixture");
    let settings = Settings::parse("kubeconfig = '/operator/config'\ntemplate = 'worker'\ngit_credential_uri = 'ate-secret://kubernetes/ganja-cloud/git/token'\ngit_credential_prefix = 'Bearer '\n").expect("settings");
    let provisioner = SubstrateProvisioner::with_tunnel(
        settings,
        "ganja".to_owned(),
        SecretString::from("synthetic-worker-password"),
        Arc::new(ControlTunnel(channel, Arc::clone(&state))),
        identity,
    )
    .await
    .expect("construct provisioner");
    (kube, ControlFixture { state, task }, provisioner)
}

fn spec() -> WorkerSpec {
    WorkerSpec {
        name: MemberName::parse("worker").expect("name"),
        lead: MemberName::parse("team-lead").expect("lead"),
        bootstrap: Bootstrap {
            repository: "https://example.invalid/repo.git".to_owned(),
            reference: "main".to_owned(),
            branch: "workers/test".to_owned(),
            author: Author {
                name: "worker".to_owned(),
                email: "worker@example.invalid".to_owned(),
            },
            deadline_secs: 180,
        },
        git_host: "example.invalid".to_owned(),
    }
}

#[tokio::test]
async fn the_grpc_state_machine_preserves_suspension_resume_results_and_running_deletion() {
    let (_kube, control, provisioner) = fixture().await;
    let worker = provisioner.spawn(&spec()).await.expect("spawn suspended");
    assert_eq!(provisioner.state(&worker).await.expect("state"), WorkerState::Suspended);
    let policy =
        control.state.lock().expect("state lock").policies.values().next().expect("policy").clone();
    let https = policy.rules[0].https.as_ref().expect("HTTPS rule");
    assert_eq!(https.hostnames, ["example.invalid"]);
    assert_eq!(https.effects.as_ref().expect("effects").replace_headers[0].header, "Authorization");
    assert_eq!(https.effects.as_ref().expect("effects").replace_headers[0].prefix, "Bearer ");
    assert_eq!(
        worker.endpoint.headers["ate-target-actor"].to_str().expect("route"),
        worker.handle.as_str().rsplit_once('/').expect("UID suffix").0
    );
    assert!(provisioner.resume(&worker).await.expect("resume").was_suspended);
    assert!(!provisioner.resume(&worker).await.expect("already running").was_suspended);
    assert_eq!(provisioner.suspend(&worker).await.expect("suspend"), SuspendOutcome::Suspended);
    assert_eq!(
        provisioner.suspend(&worker).await.expect("already suspended"),
        SuspendOutcome::AlreadySuspended
    );
    provisioner.resume(&worker).await.expect("running again");
    control.state.lock().expect("state lock").calls.clear();
    provisioner.kill(&worker).await.expect("delete running actor");
    assert_eq!(control.state.lock().expect("state lock").calls, ["delete", "get"]);
    assert_eq!(provisioner.state(&worker).await.expect("gone"), WorkerState::Gone);
    assert!(!format!("{worker:?} {provisioner:?}").contains("synthetic-worker-password"));
}

#[tokio::test]
async fn deleting_an_actor_closes_its_established_proxy_stream() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (_kube, _control, provisioner) = fixture().await;
    let worker = provisioner.spawn(&spec()).await.expect("spawn suspended");
    let url = url::Url::parse(&worker.endpoint.base_url).expect("worker URL");
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", url.port().expect("port")))
        .await
        .expect("connect proxy");
    let mut request = String::from("GET / HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in &worker.endpoint.headers {
        request.push_str(&format!("{name}: {}\r\n", value.to_str().expect("header")));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("send request");
    let mut response = [0; 19];
    stream.read_exact(&mut response).await.expect("read worker response");
    assert_eq!(&response, b"HTTP/1.1 200 OK\r\n\r\n");
    provisioner.kill(&worker).await.expect("delete actor");
    let mut remaining = Vec::new();
    tokio::time::timeout(std::time::Duration::from_millis(200), stream.read_to_end(&mut remaining))
        .await
        .expect("deletion closes the established stream")
        .expect("closed stream");
}

#[tokio::test]
async fn lost_mutation_responses_are_reconciled_without_repeating_the_mutation() {
    for operation in ["create", "create policy", "resume", "suspend", "delete"] {
        let (_kube, control, provisioner) = fixture().await;
        if operation == "create" || operation == "create policy" {
            control.state.lock().expect("state lock").lost.insert(operation);
        }
        let worker = provisioner.spawn(&spec()).await.expect("reconciled creation");
        if operation == "resume" {
            control.state.lock().expect("state lock").lost.insert(operation);
        }
        assert!(provisioner.resume(&worker).await.expect("reconciled resume").was_suspended);
        if operation == "suspend" {
            control.state.lock().expect("state lock").lost.insert(operation);
        }
        provisioner.suspend(&worker).await.expect("reconciled suspension");
        if operation == "delete" {
            control.state.lock().expect("state lock").lost.insert(operation);
        }
        provisioner.kill(&worker).await.expect("reconciled delete");
        let state = control.state.lock().expect("state lock");
        assert_eq!(state.calls.iter().filter(|call| **call == operation).count(), 1, "{operation}");
        assert!(state.actors.is_empty());
    }
}

#[tokio::test]
async fn template_readiness_and_policy_failure_prevent_an_unreported_worker() {
    let (_kube, control, provisioner) = fixture().await;
    control.state.lock().expect("state lock").template_ready = false;
    assert_eq!(
        provisioner.spawn(&spec()).await.expect_err("unready template").kind,
        ProvisionErrorKind::Refused
    );
    assert!(control.state.lock().expect("state lock").actors.is_empty());
    control.state.lock().expect("state lock").template_ready = true;
    control.state.lock().expect("state lock").deny_policy = true;
    let error = provisioner.spawn(&spec()).await.expect_err("policy failure");
    assert_eq!(error.kind, ProvisionErrorKind::Refused);
    assert!(control.state.lock().expect("state lock").actors.is_empty());
    assert!(!format!("{error:?} {error}").contains(ERROR_CANARY));
}

#[tokio::test]
async fn cancelling_a_committed_create_cleans_up_only_the_created_uid() {
    let capture = ganja_testkit::LogCapture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_writer(capture.clone()).with_ansi(false).finish(),
    )
    .expect("cleanup log capture");
    for replace_uid in [false, true] {
        let (_kube, control, provisioner) = fixture().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        control.state.lock().expect("state lock").held_create_reply =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        let spawn = tokio::spawn(async move { provisioner.spawn(&spec()).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("create committed");
        spawn.abort();
        assert!(spawn.await.expect_err("cancelled caller").is_cancelled());
        if replace_uid {
            control
                .state
                .lock()
                .expect("state lock")
                .actors
                .values_mut()
                .next()
                .expect("actor")
                .metadata
                .as_mut()
                .expect("metadata")
                .uid = "replacement-uid".to_owned();
        }
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let reconciled = control
                    .state
                    .lock()
                    .expect("state lock")
                    .calls
                    .windows(2)
                    .any(|calls| calls == ["delete", "get"]);
                if reconciled
                    && (!replace_uid || capture.logged().contains("spawn cleanup unconfirmed"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned creation cleanup attempted");
        let state = control.state.lock().expect("state lock");
        assert_eq!(state.calls.iter().filter(|call| **call == "delete").count(), 1);
        assert_eq!(state.actors.len(), usize::from(replace_uid));
        assert_eq!(state.retired.len(), 1, "abandoned creation retires local resources");
        if replace_uid {
            assert_eq!(
                state
                    .actors
                    .values()
                    .next()
                    .expect("replacement")
                    .metadata
                    .as_ref()
                    .expect("metadata")
                    .uid,
                "replacement-uid"
            );
        }
    }
}

#[tokio::test]
async fn cancellation_during_identity_reconciliation_retires_the_created_actor() {
    let (_kube, control, provisioner) = fixture().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let mut state = control.state.lock().expect("state lock");
        state.wrong_created_identity = true;
        state.held_get_reply = Some((Arc::clone(&entered), Arc::clone(&release)));
    }
    let spawn = tokio::spawn(async move { provisioner.spawn(&spec()).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("identity reconciliation reached after create returned");
    assert_eq!(control.state.lock().expect("state lock").actors.len(), 1);
    spawn.abort();
    assert!(spawn.await.expect_err("cancelled spawn").is_cancelled());
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let complete = {
                let state = control.state.lock().expect("state lock");
                state.actors.is_empty() && state.calls.ends_with(&["delete", "get"])
            };
            if complete {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned reconciliation cleans up the actor");
    let state = control.state.lock().expect("state lock");
    assert_eq!(state.calls, ["get template", "create", "get", "delete", "get"]);
    assert_eq!(state.retired.len(), 1);
    assert!(state.policies.is_empty());
}

#[tokio::test]
async fn cancelling_policy_installation_deletes_the_created_actor_by_uid() {
    let (_kube, control, provisioner) = fixture().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    control.state.lock().expect("state lock").held_policy =
        Some((Arc::clone(&entered), Arc::clone(&release)));
    let spawn = tokio::spawn(async move { provisioner.spawn(&spec()).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("policy request entered");
    assert_eq!(control.state.lock().expect("state lock").actors.len(), 1);
    spawn.abort();
    assert!(spawn.await.expect_err("cancelled spawn").is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if control.state.lock().expect("state lock").actors.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned cleanup deletes actor");
    let state = control.state.lock().expect("state lock");
    assert_eq!(state.calls.iter().filter(|call| **call == "delete").count(), 1);
    assert_eq!(state.retired.len(), 1, "cancelled policy retires local resources");
    release.notify_one();
}

#[tokio::test]
async fn crashed_reverted_gone_and_unconfirmed_delete_states_remain_distinct() {
    let (_kube, control, provisioner) = fixture().await;
    let worker = provisioner.spawn(&spec()).await.expect("spawn");
    control
        .state
        .lock()
        .expect("state lock")
        .actors
        .values_mut()
        .next()
        .expect("actor")
        .status
        .as_mut()
        .expect("status")
        .state = ActorState::Crashed.into();
    assert_eq!(provisioner.state(&worker).await.expect("crashed state"), WorkerState::Crashed);
    assert_eq!(
        provisioner.resume(&worker).await.expect_err("crashed resume").kind,
        ProvisionErrorKind::Crashed
    );
    provisioner.revert(&worker).await.expect("owner revert");
    assert_eq!(provisioner.state(&worker).await.expect("reverted state"), WorkerState::Suspended);
    control.state.lock().expect("state lock").deny_delete = true;
    assert!(provisioner.kill(&worker).await.is_err());
    assert_eq!(provisioner.state(&worker).await.expect("not falsely gone"), WorkerState::Suspended);
    control.state.lock().expect("state lock").deny_delete = false;
    provisioner.kill(&worker).await.expect("confirmed delete");
    assert_eq!(provisioner.state(&worker).await.expect("gone state"), WorkerState::Gone);
}

#[tokio::test]
async fn creation_reconciles_identity_but_never_returns_an_unrelated_worker() {
    let (_kube, control, provisioner) = fixture().await;
    control.state.lock().expect("state lock").wrong_created_identity = true;
    let worker = provisioner.spawn(&spec()).await.expect("reconcile malformed create reply");
    assert!(!worker.handle.as_str().contains("unrelated"));
    provisioner.kill(&worker).await.expect("cleanup");
    control.state.lock().expect("state lock").wrong_read_identity = true;
    let error = provisioner.spawn(&spec()).await.expect_err("reject mismatched reconciliation");
    assert_eq!(error.kind, ProvisionErrorKind::Ambiguous);
    assert!(control.state.lock().expect("state lock").policies.is_empty());
}

#[tokio::test]
async fn reverts_do_not_claim_success_for_refusal_noop_or_lost_responses() {
    for (case, expected) in [
        ("refused", ProvisionErrorKind::Refused),
        ("noop", ProvisionErrorKind::Ambiguous),
        ("lost-before-apply", ProvisionErrorKind::Ambiguous),
        ("lost-after-apply", ProvisionErrorKind::Ambiguous),
    ] {
        let (_kube, control, provisioner) = fixture().await;
        let worker = provisioner.spawn(&spec()).await.expect("spawn");
        {
            let mut state = control.state.lock().expect("state lock");
            state.deny_revert = case == "refused";
            state.noop_revert = matches!(case, "noop" | "lost-before-apply");
            if case.starts_with("lost") {
                state.lost.insert("revert");
            }
            if case == "lost-after-apply" {
                state
                    .actors
                    .values_mut()
                    .next()
                    .expect("actor")
                    .status
                    .as_mut()
                    .expect("status")
                    .assigned_node = "worker-node".to_owned();
            }
        }
        let error = provisioner.revert(&worker).await.expect_err(case);
        assert_eq!(error.kind, expected, "{case}");
        let state = control.state.lock().expect("state lock");
        assert_eq!(state.calls.iter().filter(|call| **call == "revert").count(), 1);
        if case == "lost-after-apply" {
            assert!(
                state
                    .actors
                    .values()
                    .next()
                    .expect("actor")
                    .status
                    .as_ref()
                    .expect("status")
                    .assigned_node
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn reverting_a_suspended_local_snapshot_requires_a_changed_status() {
    let (_kube, control, provisioner) = fixture().await;
    let worker = provisioner.spawn(&spec()).await.expect("spawn");
    control
        .state
        .lock()
        .expect("state lock")
        .actors
        .values_mut()
        .next()
        .expect("actor")
        .status
        .as_mut()
        .expect("status")
        .assigned_node = "worker-node".to_owned();
    provisioner.revert(&worker).await.expect("local snapshot reverted");
    assert!(
        control
            .state
            .lock()
            .expect("state lock")
            .actors
            .values()
            .next()
            .expect("actor")
            .status
            .as_ref()
            .expect("status")
            .assigned_node
            .is_empty()
    );
}

#[tokio::test]
async fn a_replaced_actor_cannot_confirm_revert_or_be_deleted_by_an_old_handle() {
    let (_kube, control, provisioner) = fixture().await;
    let worker = provisioner.spawn(&spec()).await.expect("spawn");
    control.state.lock().expect("state lock").replace_on_revert = true;
    assert_eq!(
        provisioner.revert(&worker).await.expect_err("replacement is not recovery").kind,
        ProvisionErrorKind::Ambiguous
    );
    assert_eq!(provisioner.state(&worker).await.expect("old identity gone"), WorkerState::Gone);
    assert!(provisioner.kill(&worker).await.is_err());
    assert_eq!(control.state.lock().expect("state lock").actors.len(), 1);
}

#[tokio::test]
async fn control_wrappers_keep_policy_updates_version_guarded_and_atespace_scoped() {
    let (_kube, _control, provisioner) = fixture().await;
    assert_eq!(
        provisioner.get_atespace().await.expect("atespace").metadata.expect("metadata").name,
        "ganja"
    );
    assert_eq!(
        provisioner.list_actor_templates(String::new()).await.expect("list").actor_templates.len(),
        1
    );
    provisioner.create_actor_template(template(true)).await.expect("create template");
    let worker = provisioner.spawn(&spec()).await.expect("spawn");
    provisioner.update_egress_policy(&worker, Vec::new()).await.expect("guarded policy update");
}

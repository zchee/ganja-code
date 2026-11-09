//! Loopback control-plane fixtures never use the operator's cluster or credentials.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ganja_teammate_cloud::provisioner::ProvisionError;
use ganja_teammate_cloud::provisioner::substrate::ateapi_v1::*;
use ganja_teammate_cloud::provisioner::substrate::kube::KubeTunnel;
use ganja_teammate_cloud::provisioner::substrate::tunnel::{ActorProxy, Tunnel, TunnelIo};
use http::{Method, StatusCode, Uri};
use rcgen::{CertificateParams, CertificateSigningRequestParams, IsCa, Issuer, KeyPair, SanType};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

// The lifecycle fixture shares this binary's fake Kubernetes control plane.
#[path = "ateapi_stub/lifecycle.rs"]
mod lifecycle;

const CERT_CANARY: &str = "synthetic-router-certificate-private-key-canary";
const TOKEN_CANARY: &str = "synthetic-ateapi-bearer-canary";

struct KubeState {
    version: &'static str,
    mode: AtomicU8,
    requests: AtomicUsize,
    deletes: AtomicUsize,
    issuer: Issuer<'static, KeyPair>,
    ca_pem: String,
    objects: Mutex<BTreeMap<String, Value>>,
    router_enabled: AtomicBool,
    pod_lookups: AtomicUsize,
    forwarded_pods: Mutex<Vec<String>>,
    connect_headers: Mutex<Vec<String>>,
    tokens: AtomicUsize,
    token_mode: AtomicU8,
    token_ttl: AtomicUsize,
}

struct KubeFixture {
    client: kube::Client,
    state: Arc<KubeState>,
    server: JoinHandle<()>,
}

impl Drop for KubeFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl KubeFixture {
    async fn start(version: &'static str, mode: u8) -> Self {
        let key = KeyPair::generate().expect("generate local CA");
        let mut parameters = CertificateParams::default();
        parameters.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = parameters.self_signed(&key).expect("issue local CA");
        let state = Arc::new(KubeState {
            version,
            mode: AtomicU8::new(mode),
            requests: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
            issuer: Issuer::new(parameters, key),
            ca_pem: ca.pem(),
            objects: Mutex::new(BTreeMap::new()),
            router_enabled: AtomicBool::new(false),
            pod_lookups: AtomicUsize::new(0),
            forwarded_pods: Mutex::new(Vec::new()),
            connect_headers: Mutex::new(Vec::new()),
            tokens: AtomicUsize::new(0),
            token_mode: AtomicU8::new(0),
            token_ttl: AtomicUsize::new(3600),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake Kubernetes API");
        let address = listener.local_addr().expect("Kubernetes fixture address");
        let router = axum::Router::new()
            .route(
                "/api/v1/namespaces/ate-system/pods/{pod}/portforward",
                axum::routing::get(router_upgrade),
            )
            .fallback(kube_request)
            .with_state(Arc::clone(&state));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve fake Kubernetes API");
        });
        let config = kube::Config::new(format!("http://{address}").parse().expect("loopback URI"));
        let client = ganja_teammate_cloud::provisioner::substrate::kube::client_from_config(config)
            .expect("loopback Kubernetes client");
        Self { client, state, server }
    }

    async fn wait_for_cleanup(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.state.deletes.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("every created certificate request is deleted");
    }
}

async fn kube_request(
    State(state): State<Arc<KubeState>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let missing = || {
        (
            StatusCode::NOT_FOUND,
            Json(
                json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":CERT_CANARY}),
            ),
        )
    };
    let path = uri.path();
    if path == "/api/v1/namespaces/ate-users/serviceaccounts/ganja/token" {
        assert_eq!(method, Method::POST);
        let request: Value = serde_json::from_slice(&body).expect("TokenRequest JSON");
        assert_eq!(request["spec"]["audiences"], json!(["api.ate-system.svc"]));
        assert_eq!(request["spec"]["expirationSeconds"], 3600);
        let count = state.tokens.fetch_add(1, Ordering::SeqCst) + 1;
        let mode = state.token_mode.load(Ordering::SeqCst);
        if mode == 1 {
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":403,"message":TOKEN_CANARY}),
                ),
            );
        }
        let expiry = if mode == 2 {
            "2000-01-01T00:00:00Z".to_owned()
        } else {
            jiff::Timestamp::now()
                .checked_add(Duration::from_secs(state.token_ttl.load(Ordering::SeqCst) as u64))
                .expect("token expiry")
                .to_string()
        };
        return (
            StatusCode::CREATED,
            Json(
                json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{},"status":{"token":format!("{TOKEN_CANARY}-{count}"),"expirationTimestamp":expiry}}),
            ),
        );
    }
    if state.router_enabled.load(Ordering::SeqCst) {
        if path == "/api/v1/namespaces/ate-system/services/atenet-router" {
            return (
                StatusCode::OK,
                Json(
                    json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"atenet-router"},"spec":{"selector":{"app":"router"},"ports":[{"port":8444,"targetPort":8444}]}}),
                ),
            );
        }
        if path == "/api/v1/namespaces/ate-system/pods" {
            let index = state.pod_lookups.fetch_add(1, Ordering::SeqCst) + 1;
            return (
                StatusCode::OK,
                Json(
                    json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[{"metadata":{"name":format!("router-{index}")},"spec":{"containers":[]},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}),
                ),
            );
        }
    }
    let discovery = format!("/apis/certificates.k8s.io/{}", state.version);
    if path == discovery {
        return (
            StatusCode::OK,
            Json(
                json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":format!("certificates.k8s.io/{}",state.version),"resources":[
                    {"name":"clustertrustbundles","kind":"ClusterTrustBundle","namespaced":false,"verbs":["list"]},
                    {"name":"podcertificaterequests","kind":"PodCertificateRequest","namespaced":true,"verbs":["create","get","delete"]}
                ]}),
            ),
        );
    }
    if !path.starts_with(&discovery) {
        return missing();
    }
    if path.ends_with("/clustertrustbundles") {
        assert!(uri.query().is_some_and(|query| query.contains("labelSelector=")));
        return (
            StatusCode::OK,
            Json(
                json!({"apiVersion":format!("certificates.k8s.io/{}",state.version),"kind":"ClusterTrustBundleList","metadata":{},"items":[
                    {"apiVersion":format!("certificates.k8s.io/{}",state.version),"kind":"ClusterTrustBundle","metadata":{"name":"serving"},"spec":{"signerName":"servicedns.podcert.ate.dev/identity","trustBundle":state.ca_pem}}
                ]}),
            ),
        );
    }
    if !path.contains("/namespaces/ate-system/podcertificaterequests") {
        return missing();
    }
    if method == Method::POST {
        state.requests.fetch_add(1, Ordering::SeqCst);
        let mut object: Value = serde_json::from_slice(&body).expect("typed certificate request");
        assert_eq!(object["spec"]["signerName"], "podidentity.podcert.ate.dev/identity");
        assert_eq!(object["spec"]["serviceAccountName"], "atenet-client");
        assert_eq!(object["spec"]["maxExpirationSeconds"], 86400);
        assert!(object["spec"].get("privateKey").is_none());
        let mode = state.mode.load(Ordering::SeqCst);
        if mode == 1 || mode == 2 {
            object["status"] = json!({"conditions":[{"type":if mode == 1 { "Denied" } else { "Failed" },"status":"True","message":CERT_CANARY}]});
        } else if mode == 3 {
            object["status"] = json!({"conditions":[]});
        } else {
            let csr = STANDARD
                .decode(object["spec"]["stubPKCS10Request"].as_str().expect("CSR field"))
                .expect("CSR base64");
            let mut request =
                CertificateSigningRequestParams::from_der(&csr.into()).expect("valid signed CSR");
            request.params.subject_alt_names = vec![SanType::URI(
                "spiffe://cluster.local/ns/ate-system/sa/atenet-client"
                    .try_into()
                    .expect("SPIFFE URI"),
            )];
            let year = i32::from(jiff::Zoned::now().year());
            request.params.not_before = rcgen::date_time_ymd(year - 20, 1, 1);
            request.params.not_after =
                rcgen::date_time_ymd(if mode == 4 { year - 1 } else { year + 1 }, 1, 1);
            let certificate = request.signed_by(&state.issuer).expect("issue requested public key");
            object["status"] = json!({"conditions":[{"type":"Issued","status":"True"}],"certificateChain":format!("{}{}",certificate.pem(),state.ca_pem)});
        }
        let name = object["metadata"]["name"].as_str().expect("request name").to_owned();
        state.objects.lock().expect("objects lock").insert(name, object.clone());
        return (StatusCode::CREATED, Json(object));
    }
    let name = path.rsplit('/').next().expect("last path component");
    if method == Method::GET {
        return state
            .objects
            .lock()
            .expect("objects lock")
            .get(name)
            .cloned()
            .map(|object| (StatusCode::OK, Json(object)))
            .unwrap_or_else(missing);
    }
    if method == Method::DELETE {
        state.objects.lock().expect("objects lock").remove(name);
        state.deletes.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::OK,
            Json(json!({"apiVersion":"v1","kind":"Status","status":"Success"})),
        );
    }
    missing()
}

#[tokio::test]
async fn ateapi_tokens_use_the_scoped_audience_and_renew_from_the_returned_expiry() {
    use ganja_teammate_cloud::provisioner::substrate::identity::AteapiIdentity;

    let fixture = KubeFixture::start("v1", 0).await;
    fixture.state.token_ttl.store(3, Ordering::SeqCst);
    let identity =
        AteapiIdentity::new(fixture.client.clone()).await.expect("request initial token");
    let first = identity.authorize(()).await.expect("authenticate control request");
    assert_eq!(
        first.metadata().get("authorization").expect("bearer").to_str().expect("ASCII"),
        format!("Bearer {TOKEN_CANARY}-1")
    );
    assert!(!format!("{identity:?} {first:?}").contains(TOKEN_CANARY));
    assert_eq!(fixture.state.tokens.load(Ordering::SeqCst), 1);
    fixture.state.token_ttl.store(3600, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second = identity.authorize(()).await.expect("renew expired token");
    assert_eq!(
        second.metadata().get("authorization").expect("bearer").to_str().expect("ASCII"),
        format!("Bearer {TOKEN_CANARY}-2")
    );
    identity.authorize(()).await.expect("reuse unexpired token");
    assert_eq!(fixture.state.tokens.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_and_expired_ateapi_credentials_fail_closed_without_exposing_tokens() {
    use ganja_teammate_cloud::provisioner::substrate::identity::AteapiIdentity;

    for mode in [1, 2] {
        let fixture = KubeFixture::start("v1", 0).await;
        fixture.state.token_mode.store(mode, Ordering::SeqCst);
        let error =
            AteapiIdentity::new(fixture.client.clone()).await.expect_err("refuse invalid identity");
        assert!(!format!("{error:?} {error}").contains(TOKEN_CANARY));
    }
    let fixture = KubeFixture::start("v1", 0).await;
    fixture.state.token_ttl.store(3, Ordering::SeqCst);
    let identity = AteapiIdentity::new(fixture.client.clone()).await.expect("initial identity");
    fixture.state.token_mode.store(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let error =
        identity.authorize(()).await.expect_err("failed renewal does not send cached bearer");
    assert!(!format!("{error:?} {error}").contains(TOKEN_CANARY));
}

#[tokio::test]
async fn certificate_discovery_uses_stable_or_beta_and_refreshes_after_half_life() {
    for version in ["v1", "v1beta1"] {
        let fixture = KubeFixture::start(version, 0).await;
        let tunnel = KubeTunnel::connect(fixture.client.clone())
            .await
            .expect("initial certificate issuance");
        assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 1);
        let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
        // No fake pods exist: the actor dial fails after exercising renewal.
        assert!(tunnel.actor(&actor).await.is_err());
        assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 2);
        fixture.state.mode.store(1, Ordering::SeqCst);
        assert!(tunnel.actor(&actor).await.is_err());
        assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 3);
        fixture.wait_for_cleanup(3).await;
    }
}

#[tokio::test]
async fn denied_failed_and_expired_certificates_are_redacted_and_cleaned_up() {
    for mode in [1, 2, 4] {
        let fixture = KubeFixture::start("v1", mode).await;
        let error = KubeTunnel::connect(fixture.client.clone())
            .await
            .expect_err("invalid certificate must fail closed");
        assert!(!format!("{error:?} {error}").contains(CERT_CANARY));
        fixture.wait_for_cleanup(1).await;
    }
}

#[tokio::test]
async fn cancelled_issuance_deletes_the_request_without_waiting_for_the_signer() {
    let fixture = KubeFixture::start("v1", 3).await;
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        KubeTunnel::connect(fixture.client.clone()),
    )
    .await;
    assert!(result.is_err());
    fixture.wait_for_cleanup(1).await;
}

async fn router_upgrade(
    State(state): State<Arc<KubeState>>,
    Path(pod): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    state.forwarded_pods.lock().expect("forwarded pods lock").push(pod);
    upgrade
        .protocols(["v4.channel.k8s.io"])
        .on_upgrade(move |socket| router_socket(socket, state))
        .into_response()
}

async fn router_socket(mut socket: WebSocket, state: Arc<KubeState>) {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let key = KeyPair::generate().expect("router serving key");
    let certificate = CertificateParams::new(vec!["atenet-router.ate-system.svc".to_owned()])
        .expect("router SAN")
        .signed_by(&key, &state.issuer)
        .expect("router certificate");
    let mut roots = rustls::RootCertStore::empty();
    for root in CertificateDer::pem_slice_iter(state.ca_pem.as_bytes()) {
        roots.add(root.expect("fixture root")).expect("valid root");
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::clone(&provider),
    )
    .build()
    .expect("router client verifier");
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![certificate.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("router serving configuration");
    let (tls_side, bridge_side) = tokio::io::duplex(16 * 1024);
    let serve = async move {
        let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(tls_side)
            .await
            .expect("router mutual TLS");
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            assert!(header.len() < 4096);
            header.push(stream.read_u8().await.expect("read CONNECT"));
        }
        let header = String::from_utf8(header).expect("CONNECT header");
        assert_eq!(
            header,
            "CONNECT worker:80 HTTP/1.1\r\nHost: worker:80\r\nate-target-actor: ganja/worker\r\n\r\n"
        );
        state.connect_headers.lock().expect("CONNECT headers lock").push(header);
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nbuffered")
            .await
            .expect("accept CONNECT");
        let mut request = [0; 5];
        stream.read_exact(&mut request).await.expect("read inner bytes");
        assert_eq!(&request, b"inner");
        stream.write_all(b"reply").await.expect("write inner response");
        stream.shutdown().await.expect("finish router stream");
    };
    let bridge = async move {
        // kube's port-forward protocol initializes both data and error channels.
        let port = 8444u16.to_le_bytes();
        for channel in [0, 1] {
            socket
                .send(Message::Binary(vec![channel, port[0], port[1]].into()))
                .await
                .expect("initialize port-forward channel");
        }
        let (mut read, mut write) = tokio::io::split(bridge_side);
        let mut bytes = [0; 4096];
        loop {
            tokio::select! {
                message = socket.recv() => match message {
                    Some(Ok(Message::Binary(data))) if data.first() == Some(&0) => {
                        if write.write_all(&data[1..]).await.is_err() { break; }
                    },
                    Some(Ok(Message::Ping(data))) => { let _ = socket.send(Message::Pong(data)).await; },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {},
                },
                length = read.read(&mut bytes) => {
                    let Ok(length) = length else { break };
                    if length == 0 { let _ = socket.send(Message::Close(None)).await; break; }
                    let mut data = Vec::with_capacity(length + 1);
                    data.push(0);
                    data.extend_from_slice(&bytes[..length]);
                    if socket.send(Message::Binary(data.into())).await.is_err() { break; }
                }
            }
        }
    };
    tokio::join!(serve, bridge);
}

#[tokio::test]
async fn each_actor_connection_resolves_a_new_ready_router_and_preserves_connect_bytes() {
    let fixture = KubeFixture::start("v1", 0).await;
    fixture.state.router_enabled.store(true, Ordering::SeqCst);
    let tunnel =
        KubeTunnel::connect(fixture.client.clone()).await.expect("create in-process tunnel");
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    for _ in 0..2 {
        let mut stream = tokio::time::timeout(Duration::from_secs(3), tunnel.actor(&actor))
            .await
            .expect("bounded actor tunnel")
            .expect("mTLS CONNECT through Kubernetes");
        let mut prefix = [0; 8];
        stream.read_exact(&mut prefix).await.expect("read buffered CONNECT bytes");
        assert_eq!(&prefix, b"buffered");
        stream.write_all(b"inner").await.expect("send inner HTTP bytes");
        let mut response = [0; 5];
        stream.read_exact(&mut response).await.expect("read inner response");
        assert_eq!(&response, b"reply");
    }
    assert_eq!(
        *fixture.state.forwarded_pods.lock().expect("forwarded pods lock"),
        ["router-1", "router-2"]
    );
    assert_eq!(fixture.state.connect_headers.lock().expect("CONNECT headers lock").len(), 2);
}

#[derive(Debug, Default)]
struct RecordingTunnel {
    actors: Mutex<Vec<ObjectRef>>,
    release: Arc<tokio::sync::Notify>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for RecordingTunnel {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("tasks lock").drain(..) {
            task.abort();
        }
    }
}

#[async_trait::async_trait]
impl Tunnel for RecordingTunnel {
    async fn control(&self) -> Result<tonic::transport::Channel, ProvisionError> {
        Ok(tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy())
    }

    async fn actor(&self, actor: &ObjectRef) -> Result<TunnelIo, ProvisionError> {
        self.actors.lock().expect("actors lock").push(actor.clone());
        let (client, mut server) = tokio::io::duplex(4096);
        let release = Arc::clone(&self.release);
        let task = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(server.read_u8().await.expect("read inner request"));
            }
            let request = String::from_utf8(request).expect("HTTP fixture headers");
            assert!(!request.contains("ate-target-actor"));
            assert!(request.contains("Authorization: Basic inner-canary"));
            if request.starts_with("GET /event") {
                server.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.expect("start event stream");
                release.notified().await;
            } else {
                server
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .expect("answer POST");
            }
        });
        self.tasks.lock().expect("tasks lock").push(task);
        Ok(Box::new(client))
    }
}

#[tokio::test]
async fn a_long_lived_event_stream_does_not_block_a_separate_actor_post() {
    let tunnel = Arc::new(RecordingTunnel::default());
    let proxy =
        ActorProxy::bind(Arc::clone(&tunnel) as Arc<dyn Tunnel>).await.expect("bind actor proxy");
    let address = proxy.base_url().trim_start_matches("http://").to_owned();
    let mut stream = TcpStream::connect(&address).await.expect("connect event stream");
    stream.write_all(b"GET /event HTTP/1.1\r\nHost: localhost\r\nate-target-actor: ganja/first\r\nAuthorization: Basic inner-canary\r\n\r\n").await.expect("request event stream");
    let mut first = [0; 128];
    let length = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut first))
        .await
        .expect("event stream is established")
        .expect("read event response");
    assert!(first[..length].starts_with(b"HTTP/1.1 200"));

    let mut post = TcpStream::connect(&address).await.expect("connect independent POST");
    post.write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nate-target-actor: ganja/second\r\nAuthorization: Basic inner-canary\r\n\r\n").await.expect("request POST");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), post.read_to_end(&mut response))
        .await
        .expect("POST is independent of event stream")
        .expect("read POST response");
    assert!(response.ends_with(b"ok"));
    assert_eq!(
        tunnel
            .actors
            .lock()
            .expect("actors lock")
            .iter()
            .map(|actor| actor.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    tunnel.release.notify_one();
}

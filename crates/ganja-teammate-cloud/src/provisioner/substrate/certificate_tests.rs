use rcgen::{BasicConstraints, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyUsagePurpose, SanType};
use rustls::ServerConfig;
use rustls::pki_types::ServerName;

use super::*;

#[test]
fn router_identity_requires_the_local_key_and_the_allowed_spiffe_uri() {
    let key = KeyPair::generate().expect("generate client key");
    let other_key = KeyPair::generate().expect("generate unrelated key");
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::URI(IDENTITY.try_into().expect("valid URI"))];
    let certificate = params.self_signed(&key).expect("issue client certificate");
    let pem = certificate.pem();
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let issued = validate(&pem, &key, pem.as_bytes(), now).expect("accept matching identity");
    assert!(issued.refresh_at < issued.expires_at);
    assert!(validate(&pem, &other_key, pem.as_bytes(), now).is_err());
    assert!(validate(&pem, &key, b"not a trust bundle", now).is_err());
    let expired = UNIX_EPOCH + Duration::from_secs(100_000_000_000);
    assert!(validate(&pem, &key, pem.as_bytes(), expired).is_err());

    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::URI(
        "spiffe://cluster.local/ns/other/sa/other".try_into().expect("valid URI"),
    )];
    let certificate = params.self_signed(&key).expect("issue wrong identity");
    assert!(validate(&certificate.pem(), &key, pem.as_bytes(), now).is_err());
}

async fn handshake(server_name: &'static str, correct_trust: bool) -> bool {
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
    let issuer = Issuer::new(ca_params, ca_key);
    let client_key = KeyPair::generate().expect("client key");
    let mut client_params = CertificateParams::default();
    client_params.subject_alt_names = vec![SanType::URI(IDENTITY.try_into().expect("URI"))];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &issuer).expect("client certificate");
    let alternative_key = KeyPair::generate().expect("unrelated CA key");
    let alternative =
        CertificateParams::default().self_signed(&alternative_key).expect("unrelated CA");
    let trust = if correct_trust { ca.pem() } else { alternative.pem() };
    let client = validate(&client.pem(), &client_key, trust.as_bytes(), SystemTime::now())
        .expect("local identity");
    let server_key = KeyPair::generate().expect("server key");
    let server = CertificateParams::new(vec!["atenet-router.ate-system.svc".to_owned()])
        .expect("server parameters")
        .signed_by(&server_key, &issuer)
        .expect("server certificate");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).expect("client trust root");
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::clone(&provider),
    )
    .build()
    .expect("mutual TLS verifier");
    let server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .expect("server TLS");
    let (left, right) = tokio::io::duplex(16 * 1024);
    let server = tokio::spawn(async move {
        tokio_rustls::TlsAcceptor::from(Arc::new(server)).accept(right).await.is_ok()
    });
    let client = tokio_rustls::TlsConnector::from(client.config)
        .connect(ServerName::try_from(server_name).expect("DNS name"), left)
        .await;
    let accepted = client.is_ok();
    let server_accepted = server.await.expect("join TLS fixture");
    drop(client);
    accepted && server_accepted
}

#[tokio::test]
async fn connection_timeout_does_not_cancel_shared_certificate_issuance() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::body::Bytes;
    use axum::extract::State;
    use http::{Method, Uri};
    use rcgen::CertificateSigningRequestParams;
    use serde_json::{Value, json};
    use tokio::sync::Notify;

    struct IssuerState {
        issuer: Issuer<'static, KeyPair>,
        ca: String,
        object: std::sync::Mutex<Value>,
        entered: Notify,
        release: Notify,
        creates: AtomicUsize,
        deletes: AtomicUsize,
    }
    async fn answer(
        State(state): State<Arc<IssuerState>>,
        method: Method,
        uri: Uri,
        body: Bytes,
    ) -> Json<Value> {
        if uri.path().ends_with("/v1") {
            return Json(
                json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":"certificates.k8s.io/v1","resources":[{"name":"podcertificaterequests","kind":"PodCertificateRequest","namespaced":true,"verbs":["create","get","delete"]}]}),
            );
        }
        if method == Method::POST {
            state.creates.fetch_add(1, Ordering::SeqCst);
            let mut object: Value = serde_json::from_slice(&body).expect("request");
            let csr = STANDARD
                .decode(object["spec"]["stubPKCS10Request"].as_str().expect("CSR"))
                .expect("base64 CSR");
            let mut request =
                CertificateSigningRequestParams::from_der(&csr.into()).expect("signed CSR");
            request.params.subject_alt_names =
                vec![SanType::URI(IDENTITY.try_into().expect("URI"))];
            let certificate = request.signed_by(&state.issuer).expect("issue certificate");
            object["status"] = json!({"conditions":[{"type":"Issued","status":"True"}],"certificateChain":format!("{}{}",certificate.pem(),state.ca)});
            *state.object.lock().expect("object") = object.clone();
            return Json(object);
        }
        if method == Method::DELETE {
            state.deletes.fetch_add(1, Ordering::SeqCst);
            return Json(json!({"apiVersion":"v1","kind":"Status","status":"Success"}));
        }
        state.entered.notify_one();
        state.release.notified().await;
        Json(state.object.lock().expect("object").clone())
    }
    let key = KeyPair::generate().expect("CA key");
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = params.self_signed(&key).expect("CA").pem();
    let state = Arc::new(IssuerState {
        issuer: Issuer::new(params, key),
        ca: ca.clone(),
        object: std::sync::Mutex::new(Value::Null),
        entered: Notify::new(),
        release: Notify::new(),
        creates: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let app = axum::Router::new().fallback(answer).with_state(Arc::clone(&state));
    let server =
        tokio::spawn(async move { axum::serve(listener, app).await.expect("issuer server") });
    let client =
        Client::try_from(kube::Config::new(format!("http://{address}").parse().expect("URI")))
            .expect("client");
    let identity = Arc::new(RouterIdentity {
        client,
        trust: ca.into_bytes().into(),
        current: Arc::new(Mutex::new(None)),
        renewal: Arc::new(Mutex::new(None)),
    });
    let first = {
        let identity = Arc::clone(&identity);
        tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(30), identity.config()).await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .expect("issuer is pending");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(first.await.expect("first caller").is_err());
    tokio::time::resume();
    state.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), identity.config())
        .await
        .expect("next connection deadline")
        .expect("completed shared certificate");
    assert_eq!(state.creates.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn router_tls_requires_the_serving_name_root_and_client_identity() {
    assert!(handshake("atenet-router.ate-system.svc", true).await);
    assert!(!handshake("wrong.ate-system.svc", true).await);
    assert!(!handshake("atenet-router.ate-system.svc", false).await);
}

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
async fn router_tls_requires_the_serving_name_root_and_client_identity() {
    assert!(handshake("atenet-router.ate-system.svc", true).await);
    assert!(!handshake("wrong.ate-system.svc", true).await);
    assert!(!handshake("atenet-router.ate-system.svc", false).await);
}

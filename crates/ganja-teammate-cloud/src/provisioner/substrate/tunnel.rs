//! The same control and actor connection seam for in-process and command proxies.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use http::HeaderValue;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

use super::ateapi_v1::ObjectRef;
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

const HEADER_LIMIT: usize = 64 * 1024;
pub(super) const TOKEN_HEADER: &str = "x-ganja-proxy-token";

/// An owned bidirectional tunnel, including any underlying forward's lifetime.
pub trait TunnelStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> TunnelStream for T {}

/// Erased stream returned by either tunnel implementation.
pub type TunnelIo = Box<dyn TunnelStream>;

/// Separate control-plane and actor transports; credentials cannot cross paths.
#[async_trait]
pub trait Tunnel: fmt::Debug + Send + Sync {
    /// Returns a TLS-verified control-plane channel without a bearer attached.
    ///
    /// # Errors
    /// Returns a sanitized connection or serving-trust failure.
    async fn control(&self) -> Result<Channel, ProvisionError>;

    /// Opens an admitted actor connection without sending worker HTTP headers.
    ///
    /// # Errors
    /// Returns a sanitized admission, TLS or connection failure.
    async fn actor(&self, actor: &ObjectRef) -> Result<TunnelIo, ProvisionError>;

    /// Retires persistent local resources belonging to one actor.
    ///
    /// Streams without persistent resources are closed by the ingress owner.
    async fn retire(&self, _actor: &ObjectRef) {}
}

pub(super) fn failure(operation: &'static str, kind: ProvisionErrorKind) -> ProvisionError {
    ProvisionError { operation, kind }
}

/// Stable loopback ingress that consumes the actor routing header outside Basic auth.
#[derive(Debug)]
pub struct ActorProxy {
    address: std::net::SocketAddr,
    atespace: String,
    token: HeaderValue,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    tunnel: Arc<dyn Tunnel>,
    actors: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
}

impl ActorProxy {
    /// Binds a private listener without opening any actor connection.
    ///
    /// # Errors
    /// Fails when the operating system cannot allocate a loopback listener.
    pub async fn bind(atespace: &str, tunnel: Arc<dyn Tunnel>) -> Result<Self, ProvisionError> {
        parse_actor(&format!("{atespace}/proxy"))?;
        let mut random = [0; 32];
        getrandom::fill(&mut random)
            .map_err(|_| failure("proxy entropy", ProvisionErrorKind::Refused))?;
        let mut token =
            HeaderValue::try_from(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
                .map_err(|_| failure("proxy token", ProvisionErrorKind::Refused))?;
        token.set_sensitive(true);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| failure("proxy bind", ProvisionErrorKind::Transient))?;
        let address = listener
            .local_addr()
            .map_err(|_| failure("proxy address", ProvisionErrorKind::Transient))?;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let actors = Arc::new(Mutex::new(BTreeMap::new()));
        let routes = Arc::clone(&actors);
        let backend = Arc::clone(&tunnel);
        let credential = token.clone();
        let scope = atespace.to_owned();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let tunnel = Arc::clone(&backend);
                        let actors = Arc::clone(&routes);
                        let token = credential.clone();
                        let atespace = scope.clone();
                        connections.spawn(async move {
                            let _ = forward_worker(socket, tunnel, actors, &atespace, &token).await;
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Self { address, atespace: atespace.to_owned(), token, cancel, task, tunnel, actors })
    }

    /// Admits an actor after its handle is ready to be returned to the caller.
    ///
    /// # Errors
    /// Refuses invalid names and actors outside this provisioner's atespace.
    pub fn admit(&self, actor: &ObjectRef) -> Result<(), ProvisionError> {
        let key = format!("{}/{}", actor.atespace, actor.name);
        parse_actor(&key)?;
        if actor.atespace != self.atespace {
            return Err(failure("proxy admission", ProvisionErrorKind::Refused));
        }
        self.actors.lock().expect("actor routes lock").entry(key).or_default();
        Ok(())
    }

    /// Returns the sensitive local credential, never sent into the actor tunnel.
    pub fn token_header(&self) -> HeaderValue {
        self.token.clone()
    }

    /// Closes existing actor streams and retires any persistent command proxy.
    pub async fn retire(&self, actor: &ObjectRef) {
        let key = format!("{}/{}", actor.atespace, actor.name);
        if let Some(cancel) = self.actors.lock().expect("actor routes lock").remove(&key) {
            cancel.cancel();
        }
        self.tunnel.retire(actor).await;
    }

    /// The loopback base URL shared by this provisioner's workers.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for ActorProxy {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

async fn forward_worker(
    mut socket: TcpStream,
    tunnel: Arc<dyn Tunnel>,
    actors: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
    atespace: &str,
    token: &HeaderValue,
) -> Result<(), ProvisionError> {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let bytes = read_head(&mut socket).await?;
        let (actor, request, remaining_body) = route_request(&bytes, token)?;
        if actor.atespace != atespace {
            return Err(failure("proxy admission", ProvisionErrorKind::Refused));
        }
        let cancel = actors
            .lock()
            .expect("actor routes lock")
            .get(&format!("{}/{}", actor.atespace, actor.name))
            .cloned()
            .ok_or_else(|| failure("proxy admission", ProvisionErrorKind::Refused))?;
        let remote = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(failure("proxy retired", ProvisionErrorKind::Gone)),
            result = async {
                let mut remote = tunnel.actor(&actor).await?;
                remote
                    .write_all(&request)
                    .await
                    .map_err(|_| failure("proxy request", ProvisionErrorKind::Transient))?;
                // Relay only the declared first body, never another request head.
                let copied = tokio::io::copy(&mut (&mut socket).take(remaining_body), &mut remote)
                    .await.map_err(|_| failure("proxy request", ProvisionErrorKind::Transient))?;
                if copied != remaining_body {
                    return Err(failure("proxy body", ProvisionErrorKind::Refused));
                }
                Ok::<_, ProvisionError>(remote)
            } => result,
        }?;
        Ok::<_, ProvisionError>((remote, cancel))
    })
    .await;
    match result {
        Ok(Ok((mut remote, cancel))) => {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                result = tokio::io::copy(&mut remote, &mut socket) => {
                    result.map_err(|_| failure("proxy stream", ProvisionErrorKind::Transient))?;
                }
            }
            Ok(())
        }
        _ => {
            // Remote bodies and certificate failures never reach a worker or a log.
            let _ = socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            Err(failure("proxy connect", ProvisionErrorKind::Transient))
        }
    }
}

async fn read_head(io: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, ProvisionError> {
    let mut bytes = Vec::with_capacity(4096);
    loop {
        let mut chunk = [0; 4096];
        let length = io
            .read(&mut chunk)
            .await
            .map_err(|_| failure("proxy header", ProvisionErrorKind::Transient))?;
        if length == 0 {
            return Err(failure("proxy header", ProvisionErrorKind::Transient));
        }
        bytes.extend_from_slice(&chunk[..length]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(bytes);
        }
        if bytes.len() >= HEADER_LIMIT {
            return Err(failure("proxy header", ProvisionErrorKind::Refused));
        }
    }
}

fn route_request(
    bytes: &[u8],
    token: &HeaderValue,
) -> Result<(ObjectRef, Vec<u8>, u64), ProvisionError> {
    let refused = || failure("proxy routing", ProvisionErrorKind::Refused);
    let mut expected = [0_u8; 32];
    if !matches!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode_slice(token.as_bytes(), &mut expected),
        Ok(32)
    ) {
        return Err(refused());
    }
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    let httparse::Status::Complete(head_length) = request.parse(bytes).map_err(|_| refused())?
    else {
        return Err(refused());
    };
    if head_length > HEADER_LIMIT {
        return Err(refused());
    }
    let mut target = None;
    let mut authenticated = false;
    let mut content_length = None;
    let mut result = Vec::with_capacity(bytes.len());
    let method = request.method.ok_or_else(refused)?;
    let path = request.path.ok_or_else(refused)?;
    if method == "CONNECT" || !path.starts_with('/') {
        return Err(refused());
    }
    result.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());
    for header in request.headers {
        if header.name.eq_ignore_ascii_case("upgrade")
            || header.name.eq_ignore_ascii_case("transfer-encoding")
            || (header.name.eq_ignore_ascii_case("connection")
                && header
                    .value
                    .split(|byte| *byte == b',')
                    .any(|value| value.trim_ascii().eq_ignore_ascii_case(b"upgrade")))
        {
            return Err(refused());
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let text = std::str::from_utf8(header.value).map_err(|_| refused())?;
            if text.is_empty()
                || !text.bytes().all(|byte| byte.is_ascii_digit())
                || content_length.is_some()
            {
                return Err(refused());
            }
            content_length = Some(text.parse::<u64>().map_err(|_| refused())?);
        }
        if header.name.eq_ignore_ascii_case("ate-target-actor") {
            if target.is_some() {
                return Err(refused());
            }
            target = Some(parse_actor(std::str::from_utf8(header.value).map_err(|_| refused())?)?);
        } else if header.name.eq_ignore_ascii_case(TOKEN_HEADER) {
            let mut presented = [0_u8; 32];
            if authenticated
                || !matches!(
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode_slice(header.value, &mut presented),
                    Ok(32)
                )
                || !bool::from(presented.ct_eq(&expected))
            {
                return Err(refused());
            }
            authenticated = true;
        } else if !header.name.eq_ignore_ascii_case("connection") {
            result.extend_from_slice(header.name.as_bytes());
            result.extend_from_slice(b": ");
            result.extend_from_slice(header.value);
            result.extend_from_slice(b"\r\n");
        }
    }
    if !authenticated {
        return Err(refused());
    }
    // One request per connection: buffered or later follow-up bytes are never
    // forwarded. Only the declared first body and its streamed response survive.
    result.extend_from_slice(b"Connection: close\r\n\r\n");
    let body_length = content_length.unwrap_or(0);
    let buffered =
        (bytes.len() - head_length).min(usize::try_from(body_length).unwrap_or(usize::MAX));
    result.extend_from_slice(&bytes[head_length..head_length + buffered]);
    Ok((target.ok_or_else(refused)?, result, body_length - buffered as u64))
}

pub(super) fn parse_actor(value: &str) -> Result<ObjectRef, ProvisionError> {
    let (atespace, name) = value
        .split_once('/')
        .ok_or_else(|| failure("actor identity", ProvisionErrorKind::Refused))?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 63
            && part.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && part.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    if !valid(atespace) || !valid(name) {
        return Err(failure("actor identity", ProvisionErrorKind::Refused));
    }
    Ok(ObjectRef { atespace: atespace.to_owned(), name: name.to_owned() })
}

/// Opens the HTTP CONNECT hop after the caller has verified router TLS.
pub(super) async fn connect_actor(
    mut io: TunnelIo,
    actor: &ObjectRef,
) -> Result<TunnelIo, ProvisionError> {
    parse_actor(&format!("{}/{}", actor.atespace, actor.name))?;
    let request = format!(
        "CONNECT {}:80 HTTP/1.1\r\nHost: {}:80\r\nate-target-actor: {}/{}\r\n\r\n",
        actor.name, actor.name, actor.atespace, actor.name
    );
    io.write_all(request.as_bytes())
        .await
        .map_err(|_| failure("router CONNECT", ProvisionErrorKind::Transient))?;
    let bytes = read_head(&mut io).await?;
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut response = httparse::Response::new(&mut headers);
    let httparse::Status::Complete(length) = response
        .parse(&bytes)
        .map_err(|_| failure("router CONNECT", ProvisionErrorKind::Refused))?
    else {
        return Err(failure("router CONNECT", ProvisionErrorKind::Refused));
    };
    match response.code {
        Some(200..=299) if length <= HEADER_LIMIT => {
            Ok(Box::new(PrefixedIo { prefix: Cursor::new(bytes[length..].to_vec()), io }))
        }
        Some(401 | 403) => Err(failure("router CONNECT", ProvisionErrorKind::Refused)),
        _ => Err(failure("router CONNECT", ProvisionErrorKind::Transient)),
    }
}

struct PrefixedIo {
    prefix: Cursor<Vec<u8>>,
    io: TunnelIo,
}

impl AsyncRead for PrefixedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            return Pin::new(&mut self.prefix).poll_read(cx, buffer);
        }
        Pin::new(&mut self.io).poll_read(cx, buffer)
    }
}
impl AsyncWrite for PrefixedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;

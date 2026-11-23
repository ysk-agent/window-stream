//! WebTransport session wrapper for window-stream.
//!
//! Channel allocation (fixed by the design, see `README.md`):
//!
//! | Traffic | Channel |
//! |---|---|
//! | Control (`Hello`, `Window`) | one reliable bidirectional stream |
//! | `Frame` | datagrams |
//! | `Input` | datagrams |
//! | `FrameAck` | datagrams |
//!
//! Datagrams are never retransmitted by us and QUIC datagrams are not
//! retransmitted by the transport either: timed data that is lost is
//! dropped, and holes are closed by reference invalidation (`FrameAck`).
//!
//! Control-stream framing: `[tag: u8][len: u32 BE][prost bytes]`, where
//! tag is `1 = Hello`, `2 = Window`. Length-prefixed because a QUIC
//! stream is a byte stream with no message boundaries.
//!
//! Certificates: self-signed, generated with `rcgen` (ECDSA P-256,
//! 14-day validity, as required for WebTransport certificate-hash
//! pinning). Clients pin the certificate by its SHA-256 hash
//! ([`SelfSignedIdentity::cert_hash`]); `with_no_cert_validation` is
//! available only as an explicit dev escape hatch.

use bytes::{Bytes, BytesMut, BufMut};
use prost::Message as _;
use std::net::SocketAddr;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::tls::{Certificate, CertificateChain, PrivateKey, Sha256Digest};
use wtransport::{ClientConfig, Endpoint, Identity, ServerConfig};
use window_stream_proto::{Hello, Window};

pub const TAG_HELLO: u8 = 1;
pub const TAG_WINDOW: u8 = 2;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("certificate generation failed: {0}")]
    Cert(String),
    #[error("webtransport: {0}")]
    WebTransport(String),
    #[error("control stream closed before a full message arrived")]
    ControlClosed,
    #[error("unknown control message tag: {0:#04x}")]
    UnknownTag(u8),
    #[error("prost decode: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// A self-signed host identity generated with `rcgen`.
pub struct SelfSignedIdentity {
    identity: Identity,
    cert_hash: Sha256Digest,
    cert_hash_hex: String,
}

impl SelfSignedIdentity {
    /// Generate a fresh ECDSA P-256 self-signed certificate for `sans`
    /// (e.g. `["localhost", "127.0.0.1", "::1"]`).
    pub fn generate(sans: &[&str]) -> Result<Self, TransportError> {
        let subject_alt_names: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
        let certified = rcgen::generate_simple_self_signed(subject_alt_names)
            .map_err(|e| TransportError::Cert(e.to_string()))?;
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let certificate = Certificate::from_der(cert_der)
            .map_err(|e| TransportError::Cert(e.to_string()))?;
        let cert_hash = certificate.hash();
        let cert_hash_hex = cert_hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let identity = Identity::new(
            CertificateChain::single(certificate),
            PrivateKey::from_der_pkcs8(key_der),
        );
        Ok(Self { identity, cert_hash, cert_hash_hex })
    }

    /// SHA-256 hash of the certificate (for client pinning / browsers'
    /// `serverCertificateHashes`).
    pub fn cert_hash(&self) -> Sha256Digest {
        self.cert_hash.clone()
    }

    /// The same hash as a plain lowercase hex string (64 chars).
    pub fn cert_hash_hex(&self) -> &str {
        &self.cert_hash_hex
    }

    pub fn into_identity(self) -> Identity {
        self.identity
    }
}

/// Parse a plain hex SHA-256 certificate hash (as printed by the host).
pub fn parse_cert_hash_hex(hex: &str) -> Result<Sha256Digest, TransportError> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return Err(TransportError::Cert(format!(
            "certificate hash must be 64 hex chars, got {}",
            hex.len()
        )));
    }
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| TransportError::Cert(e.to_string()))?;
    }
    Ok(Sha256Digest::new(bytes))
}

/// Host side: a bound WebTransport endpoint accepting sessions.
pub struct HostEndpoint {
    endpoint: Endpoint<Server>,
    local_addr: SocketAddr,
}

impl HostEndpoint {
    pub fn bind(addr: SocketAddr, identity: SelfSignedIdentity) -> Result<Self, TransportError> {
        let config = ServerConfig::builder()
            .with_bind_address(addr)
            .with_identity(identity.into_identity())
            .build();
        let endpoint = Endpoint::server(config)?;
        let local_addr = endpoint.local_addr()?;
        Ok(Self { endpoint, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Accept the next incoming session (completes the WebTransport
    /// handshake). Errors are returned, not swallowed: the caller
    /// decides whether to keep accepting.
    pub async fn accept(&self) -> Result<Session, TransportError> {
        let incoming = self.endpoint.accept().await;
        let request = incoming
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?;
        let connection = request
            .accept()
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?;
        Ok(Session { connection })
    }
}

/// Local bind address for a client connecting to `url`. Binding an
/// IPv4 socket for IPv4 / `localhost` targets matters in practice: with
/// the dual-stack default bind, quinn-udp sends to a v4-mapped IPv6
/// destination, which some sandboxes and firewalls reject with EPERM
/// on `sendmsg` (observed on the Muse VM: connection timeout with
/// `quinn_udp: sendmsg error: PermissionDenied` to `[::ffff:127.0.0.1]`).
fn client_bind_addr(url: &str) -> SocketAddr {
    if url.contains("://[") {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    }
}

/// Client side: connect to a host, pinning its self-signed certificate
/// by hash. This is the intended path for dev certificates.
pub async fn connect(url: &str, cert_hash: Sha256Digest) -> Result<Session, TransportError> {
    let config = ClientConfig::builder()
        .with_bind_address(client_bind_addr(url))
        .with_server_certificate_hashes([cert_hash])
        .build();
    connect_with_config(url, config).await
}

/// Client side with certificate validation disabled. Dev only: prefer
/// [`connect`] with hash pinning whenever the hash is known.
pub async fn connect_insecure_dev_only(url: &str) -> Result<Session, TransportError> {
    let config = ClientConfig::builder()
        .with_bind_address(client_bind_addr(url))
        .with_no_cert_validation()
        .build();
    connect_with_config(url, config).await
}

async fn connect_with_config(url: &str, config: ClientConfig) -> Result<Session, TransportError> {
    let endpoint = Endpoint::client(config)?;
    let connection = endpoint
        .connect(url)
        .await
        .map_err(|e| TransportError::WebTransport(e.to_string()))?;
    Ok(Session { connection })
}

/// One WebTransport session. Cloneable handle around the connection;
/// stream/datagram allocation is by purpose, per the table above.
#[derive(Clone)]
pub struct Session {
    connection: wtransport::Connection,
}

pub type SendStream = wtransport::stream::SendStream;
pub type RecvStream = wtransport::stream::RecvStream;

impl Session {
    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Max datagram payload size detected by QUIC for this session, if
    /// datagrams are enabled. Fragment sizing MUST come from here, not
    /// from a hard-coded MTU (the outer layer may be WireGuard or QUIC).
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    // ---- Control (reliable bidirectional stream) ----

    /// Open the control stream (client side).
    pub async fn open_control(&self) -> Result<(SendStream, RecvStream), TransportError> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?;
        Ok((send, recv))
    }

    /// Accept the control stream (host side).
    pub async fn accept_control(&self) -> Result<(SendStream, RecvStream), TransportError> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?;
        Ok((send, recv))
    }

    // ---- Datagrams (Frame / Input / FrameAck) ----

    pub fn send_datagram(&self, data: impl AsRef<[u8]>) -> Result<(), TransportError> {
        self.connection
            .send_datagram(data)
            .map_err(|e| TransportError::WebTransport(e.to_string()))
    }

    pub async fn receive_datagram(&self) -> Result<Bytes, TransportError> {
        let dg = self
            .connection
            .receive_datagram()
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?;
        Ok(Bytes::from(dg.payload().to_vec()))
    }

    // ---- Lifecycle ----

    pub fn close(&self, code: u32, reason: &str) {
        self.connection.close(code.into(), reason.as_bytes());
    }

    pub async fn closed(&self) {
        self.connection.closed().await;
    }
}

// ---- Control-stream message framing ----

fn frame_message(tag: u8, msg: &impl prost::Message) -> Bytes {
    let body = msg.encode_to_vec();
    let mut buf = BytesMut::with_capacity(5 + body.len());
    buf.put_u8(tag);
    buf.put_u32(body.len() as u32);
    buf.put_slice(&body);
    buf.freeze()
}

async fn write_frame(send: &mut SendStream, framed: &[u8]) -> Result<(), TransportError> {
    use tokio::io::AsyncWriteExt;
    send.write_all(framed)
        .await
        .map_err(|e| TransportError::WebTransport(e.to_string()))?;
    send.flush()
        .await
        .map_err(|e| TransportError::WebTransport(e.to_string()))?;
    Ok(())
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = recv
            .read(&mut buf[filled..])
            .await
            .map_err(|e| TransportError::WebTransport(e.to_string()))?
            .ok_or(TransportError::ControlClosed)?;
        if n == 0 {
            return Err(TransportError::ControlClosed);
        }
        filled += n;
    }
    Ok(())
}

/// Read one framed control message; returns its tag and prost body.
pub async fn read_control_message(recv: &mut RecvStream) -> Result<(u8, Bytes), TransportError> {
    let mut tag = [0u8; 1];
    read_exact(recv, &mut tag).await?;
    let mut len_buf = [0u8; 4];
    read_exact(recv, &mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    read_exact(recv, &mut body).await?;
    Ok((tag[0], Bytes::from(body)))
}

pub async fn send_hello(send: &mut SendStream, hello: &Hello) -> Result<(), TransportError> {
    write_frame(send, &frame_message(TAG_HELLO, hello)).await
}

pub async fn send_window(send: &mut SendStream, window: &Window) -> Result<(), TransportError> {
    write_frame(send, &frame_message(TAG_WINDOW, window)).await
}

pub fn decode_hello(tag: u8, body: &[u8]) -> Result<Hello, TransportError> {
    if tag != TAG_HELLO {
        return Err(TransportError::UnknownTag(tag));
    }
    Ok(Hello::decode(body)?)
}

pub fn decode_window(tag: u8, body: &[u8]) -> Result<Window, TransportError> {
    if tag != TAG_WINDOW {
        return Err(TransportError::UnknownTag(tag));
    }
    Ok(Window::decode(body)?)
}

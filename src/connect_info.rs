use std::net::SocketAddr;
use axum::extract::connect_info::Connected;

/// TLS session metadata captured after a successful TLS handshake.
///
/// When the `tls-rustls` feature is enabled, the fields use native `rustls`
/// types so downstream code can inspect them without extra conversion.
/// When the feature is disabled (plain TCP), only `sni` and `alpn` are
/// available and they are empty `Option::None`.
#[derive(Clone, Debug)]
pub struct TlsSessionInfo {
    /// Server Name Indication (SNI) hostname sent by the client.
    pub sni: Option<String>,

    /// ALPN protocol negotiated during the handshake (e.g. `b"h2"` or `b"http/1.1"`).
    pub alpn: Option<Vec<u8>>,

    /// The TLS protocol version (TLS 1.2 or TLS 1.3), using the native `rustls` enum.
    #[cfg(feature = "tls-rustls")]
    pub protocol_version: Option<rustls::ProtocolVersion>,
    #[cfg(not(feature = "tls-rustls"))]
    pub protocol_version: Option<String>,

    /// The cipher suite that was selected for this connection.
    #[cfg(feature = "tls-rustls")]
    pub cipher_suite: Option<rustls::SupportedCipherSuite>,
    #[cfg(not(feature = "tls-rustls"))]
    pub cipher_suite: Option<String>,

    /// The client's certificate chain (mTLS), as owned DER bytes.
    ///
    /// `CertificateDer<'static>` owns its bytes via a `Cow`, so it is safe to
    /// store across `'static` boundaries (Axum request extensions, task spawns,
    /// etc.) and is dropped together with the `TlsSessionInfo` at request end.
    ///
    /// Use [`TlsSessionInfo::parse_peer_certs`] to decode these into
    /// `webpki::EndEntityCert` for subject / SAN inspection.
    #[cfg(feature = "tls-rustls")]
    pub peer_certs: Option<Vec<rustls::pki_types::CertificateDer<'static>>>,

    /// Raw QUIC transport parameters, present only when the underlying
    /// transport is QUIC (e.g. HTTP/3 via `quinn`).  For standard TCP/TLS
    /// connections this will always be `None`.
    ///
    /// When HTTP/3 support is added in the future, these bytes can be decoded
    /// by the QUIC engine's own parameter parser (e.g. `quinn_proto`).
    pub quic_transport_parameters: Option<Vec<u8>>,
}

#[cfg(feature = "tls-rustls")]
impl TlsSessionInfo {
    /// Parse the client certificate chain into `EndEntityCert` structures
    /// using `rustls-webpki`.
    ///
    /// Each `EndEntityCert<'_>` borrows from the owned DER bytes inside
    /// `self.peer_certs`, so its lifetime is tied to `&self`. Use the
    /// returned objects to inspect the subject DN, SANs, public key, and
    /// validity period of each client certificate.
    ///
    /// Returns an empty `Vec` if no peer certificates were presented.
    pub fn parse_peer_certs(
        &self,
    ) -> Result<Vec<rustls_webpki::EndEntityCert<'_>>, rustls_webpki::Error> {
        match &self.peer_certs {
            Some(certs) => certs
                .iter()
                .map(|c| rustls_webpki::EndEntityCert::try_from(c))
                .collect(),
            None => Ok(Vec::new()),
        }
    }
}

/// Connection metadata injected into every Axum request by the cranker router.
///
/// Contains the remote peer address and, for TLS connections, the full
/// [`TlsSessionInfo`] captured after the handshake.
#[derive(Clone, Debug)]
pub struct CrankerConnectInfo {
    pub remote_addr: SocketAddr,
    pub tls_info: Option<TlsSessionInfo>,
}

// ── Plain TCP ──────────────────────────────────────────────────────────────────

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for CrankerConnectInfo {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        CrankerConnectInfo {
            remote_addr: *stream.remote_addr(),
            tls_info: None,
        }
    }
}

// ── Native Axum TLS (tokio-rustls) ─────────────────────────────────────────────

/// A concrete `Listener` implementation that wraps a `TcpListener` with a
/// `tokio_rustls::TlsAcceptor`, enabling native `axum::serve` TLS hosting.
///
/// Use this with `.into_make_service_with_connect_info::<CrankerConnectInfo>()`
/// to automatically populate [`CrankerConnectInfo`] (including full TLS session
/// metadata) into every request's extensions.
#[cfg(feature = "tls-rustls")]
pub struct RustlsListener {
    pub tcp_listener: tokio::net::TcpListener,
    pub tls_acceptor: tokio_rustls::TlsAcceptor,
}

#[cfg(feature = "tls-rustls")]
impl axum::serve::Listener for RustlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp_listener.accept().await {
                Ok((tcp_stream, remote_addr)) => {
                    match self.tls_acceptor.accept(tcp_stream).await {
                        Ok(tls_stream) => return (tls_stream, remote_addr),
                        Err(err) => {
                            log::error!("TLS handshake error: {:?}", err);
                        }
                    }
                }
                Err(err) => {
                    log::error!("TCP accept error: {:?}", err);
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp_listener.local_addr()
    }
}

#[cfg(feature = "tls-rustls")]
impl Connected<axum::serve::IncomingStream<'_, RustlsListener>> for CrankerConnectInfo {
    fn connect_info(stream: axum::serve::IncomingStream<'_, RustlsListener>) -> Self {
        let io = stream.io();
        let (_, session) = io.get_ref();

        let sni = session.server_name().map(String::from);
        let alpn = session.alpn_protocol().map(|p| p.to_vec());
        let protocol_version = session.protocol_version();
        let cipher_suite = session.negotiated_cipher_suite();
        let peer_certs = session
            .peer_certificates()
            .map(|certs| certs.iter().map(|c| c.clone().into_owned()).collect());
        // quic_transport_parameters() is only available via rustls's QUIC API
        // (rustls::quic::Connection), not on a TCP ServerConnection. This field
        // is reserved for future HTTP/3 (QUIC) integration.
        let quic_transport_parameters = None;

        CrankerConnectInfo {
            remote_addr: *stream.remote_addr(),
            tls_info: Some(TlsSessionInfo {
                sni,
                alpn,
                protocol_version,
                cipher_suite,
                peer_certs,
                quic_transport_parameters,
            }),
        }
    }
}

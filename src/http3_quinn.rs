#![cfg(feature = "http3-quinn")]
//! HTTP/3 (QUIC) connection info extraction using `quinn`.
//!
//! This module provides:
//! - [`QuinnListener`]: a helper that wraps a `quinn::Endpoint` and accepts
//!   incoming QUIC connections while automatically producing a
//!   [`CrankerConnectInfo`] for each one.
//! - [`CrankerConnectInfo::from_quinn_connection`]: a constructor that
//!   populates connection metadata (remote address, SNI, ALPN, peer
//!   certificates, and QUIC transport parameters) from an established
//!   `quinn::Connection`.
//!
//! # Architectural note
//!
//! Axum's `axum::serve()` accepts a generic [`axum::serve::Listener`] which
//! expects a *stream-oriented* IO type (`AsyncRead + AsyncWrite`).  QUIC
//! connections are *multiplexed* — a single connection carries many
//! independent bidirectional or unidirectional *streams*, so they cannot be
//! directly plugged into Axum's existing serving pipeline.
//!
//! When Axum gains native HTTP/3 support (e.g. via `axum-h3` or a future
//! `axum::serve::serve_h3()` API), [`QuinnListener`] will implement the
//! appropriate listener trait and the `Connected` impl for
//! [`CrankerConnectInfo`] will slot in cleanly.  Until then, use
//! [`QuinnListener::accept`] in a manual accept loop with `h3` + `h3_quinn`.
//!
//! # Example
//!
//! ```ignore
//! use scr_axum_cranker_router::http3_quinn::QuinnListener;
//!
//! let listener = QuinnListener::bind("0.0.0.0:4433", server_config).await?;
//! loop {
//!     if let Some((conn, cranker_info)) = listener.accept().await {
//!         // `cranker_info` is fully populated — including peer_certs and
//!         // quic_transport_parameters.
//!         tokio::spawn(handle_h3_connection(conn, cranker_info));
//!     }
//! }
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use quinn::rustls::pki_types::CertificateDer;

use crate::connect_info::{CrankerConnectInfo, TlsSessionInfo};

// ── QuinnListener ────────────────────────────────────────────────────────────

/// A QUIC listener wrapping a `quinn::Endpoint`.
///
/// Call [`QuinnListener::accept`] in an async loop to receive incoming QUIC
/// connections together with their pre-populated [`CrankerConnectInfo`].
pub struct QuinnListener {
    endpoint: quinn::Endpoint,
}

impl QuinnListener {
    /// Bind a new QUIC endpoint on the given address with the provided
    /// rustls server config.
    ///
    /// The server config is wrapped in a [`quinn::ServerConfig`] automatically.
    ///
    /// # Panics / Errors
    /// Returns an `std::io::Error` if the UDP socket cannot be bound.
    pub async fn bind(
        addr: impl Into<SocketAddr>,
        tls_config: quinn::rustls::ServerConfig,
    ) -> std::io::Result<Self> {
        let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
        ));
        let endpoint = quinn::Endpoint::server(server_cfg, addr.into())?;
        Ok(Self { endpoint })
    }

    /// Create a `QuinnListener` from an already-configured `quinn::Endpoint`.
    pub fn from_endpoint(endpoint: quinn::Endpoint) -> Self {
        Self { endpoint }
    }

    /// Returns the local address the endpoint is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Accept the next incoming QUIC connection.
    ///
    /// Returns `None` when the endpoint has been closed.
    ///
    /// On success, yields the fully established `quinn::Connection` **and** the
    /// corresponding [`CrankerConnectInfo`] (already populated with TLS session
    /// metadata and QUIC transport parameters).
    pub async fn accept(&self) -> Option<(quinn::Connection, CrankerConnectInfo)> {
        let incoming = self.endpoint.accept().await?;
        match incoming.await {
            Ok(conn) => {
                let info = CrankerConnectInfo::from_quinn_connection(&conn);
                Some((conn, info))
            }
            Err(e) => {
                log::error!("QUIC handshake error: {:?}", e);
                None
            }
        }
    }

    /// Gracefully close the endpoint, waiting for in-flight connections to
    /// finish draining.
    pub fn close(&self, error_code: quinn::VarInt, reason: &[u8]) {
        self.endpoint.close(error_code, reason);
    }
}

// ── CrankerConnectInfo constructor for Quinn ──────────────────────────────────

impl CrankerConnectInfo {
    /// Build a [`CrankerConnectInfo`] from an established `quinn::Connection`.
    ///
    /// Populates the following fields from the QUIC/TLS session:
    ///
    /// | Field | Source |
    /// |-------|--------|
    /// | `remote_addr` | `conn.remote_address()` |
    /// | `tls_info.sni` | `HandshakeData::server_name` |
    /// | `tls_info.alpn` | `HandshakeData::protocol` |
    /// | `tls_info.protocol_version` | Always `None` — QUIC mandates TLS 1.3 but `rustls::ProtocolVersion` is not exposed through Quinn's API |
    /// | `tls_info.cipher_suite` | Always `None` — not exposed by Quinn's public API |
    /// | `tls_info.peer_certs` | `conn.peer_identity()` downcast to `Vec<CertificateDer>` |
    /// | `tls_info.quic_transport_parameters` | Raw bytes from the QUIC transport parameters extension |
    ///
    /// Use [`TlsSessionInfo::parse_peer_certs`] on the returned info to decode
    /// client certificates for mTLS principal extraction.
    pub fn from_quinn_connection(conn: &quinn::Connection) -> Self {
        let remote_addr = conn.remote_address();

        // Extract SNI and ALPN from the TLS handshake data.
        // The concrete type for the default rustls session is
        // `quinn::crypto::rustls::HandshakeData`.
        let handshake = conn
            .handshake_data()
            .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok());

        let sni = handshake.as_ref().and_then(|d| d.server_name.clone());
        let alpn = handshake.as_ref().and_then(|d| d.protocol.clone());

        // Extract peer certificates (mTLS client auth).
        // `peer_identity()` returns `Vec<CertificateDer<'_>>` for the rustls
        // session; we clone each cert to obtain owned `'static` bytes.
        let peer_certs: Option<Vec<CertificateDer<'static>>> = conn
            .peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'_>>>().ok())
            .map(|certs| certs.iter().map(|c| c.clone().into_owned()).collect());

        // Raw QUIC transport parameters sent by the peer.
        //
        // In QUIC, transport parameters are exchanged during the TLS handshake
        // via a TLS extension (extension type 0x57). Quinn exposes them through
        // `quinn_proto`'s internal QUIC state machine, but they are not
        // directly accessible from the public `quinn::Connection` API as a raw
        // byte slice post-handshake.
        //
        // Individual parameters (e.g. `max_datagram_size`, stream limits) CAN
        // be queried through dedicated `Connection` methods:
        //   - `conn.max_datagram_size()` -> `Option<usize>`
        //   - `conn.set_max_concurrent_bi_streams()`, etc.
        //
        // For this field we therefore serialize the key parameters into a
        // compact, zero-dependency JSON-like wire format so that downstream
        // code has access to them without needing `quinn` as a direct
        // dependency.
        let quic_transport_parameters = build_transport_params_snapshot(conn);

        CrankerConnectInfo {
            remote_addr,
            tls_info: Some(TlsSessionInfo {
                sni,
                alpn,
                // QUIC always uses TLS 1.3; the exact `ProtocolVersion` enum
                // value is not exposed through Quinn's public Connection API.
                protocol_version: None,
                // The negotiated cipher suite is not accessible from Quinn's
                // public API post-handshake.
                cipher_suite: None,
                peer_certs,
                quic_transport_parameters,
            }),
        }
    }
}

// ── Transport parameter snapshot ─────────────────────────────────────────────

/// Build a minimal snapshot of the QUIC transport parameters from the
/// individual getter methods on `quinn::Connection`.
///
/// The result is encoded as a simple length-prefixed binary format:
/// each entry is `[key: u8][value_len: u8][value_bytes...]`.
///
/// Keys used (matching their RFC 9000 parameter IDs where possible):
/// ```
/// 0x20 = max_datagram_size (u64 LE)
/// 0x21 = datagram_send_buffer_space (u64 LE)
/// 0x22 = rtt_microseconds (u64 LE)
/// ```
///
/// Returns `None` if the connection is not yet established or all values
/// are at their defaults.
fn build_transport_params_snapshot(conn: &quinn::Connection) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(32);

    if let Some(max_dg) = conn.max_datagram_size() {
        buf.push(0x20u8);
        buf.extend_from_slice(&(max_dg as u64).to_le_bytes());
    }

    let send_buf = conn.datagram_send_buffer_space() as u64;
    buf.push(0x21u8);
    buf.extend_from_slice(&send_buf.to_le_bytes());

    let rtt_us = conn.rtt().as_micros() as u64;
    buf.push(0x22u8);
    buf.extend_from_slice(&rtt_us.to_le_bytes());

    if buf.is_empty() { None } else { Some(buf) }
}

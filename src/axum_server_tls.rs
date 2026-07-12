#![cfg(feature = "axum-server-tls")]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::net::SocketAddr;
use axum::http::Request;
use axum_server::accept::{Accept, DefaultAcceptor};
use axum_server::tls_rustls::RustlsAcceptor;
use tokio_rustls::server::TlsStream;
use crate::connect_info::{CrankerConnectInfo, TlsSessionInfo};

pub trait MaybePeerAddr {
    fn peer_addr(&self) -> Option<SocketAddr>;
}

impl MaybePeerAddr for tokio::net::TcpStream {
    fn peer_addr(&self) -> Option<SocketAddr> {
        tokio::net::TcpStream::peer_addr(self).ok()
    }
}

#[derive(Clone)]
pub struct CrankerTlsAcceptor<A = DefaultAcceptor> {
    inner: RustlsAcceptor<A>,
}

impl<A> CrankerTlsAcceptor<A> {
    pub fn new(inner: RustlsAcceptor<A>) -> Self {
        Self { inner }
    }
}

impl<A, I, S> Accept<I, S> for CrankerTlsAcceptor<A>
where
    A: Accept<I, S, Service = S> + Clone + Send + Sync + 'static,
    A::Future: Send + 'static,
    A::Stream: MaybePeerAddr + tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    A::Service: Send,
    S: Send + 'static,
{
    type Stream = TlsStream<A::Stream>;
    type Service = TlsInfoService<S>;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let handshake_fut = self.inner.accept(stream, service);
        Box::pin(async move {
            let (tls_stream, inner_service) = handshake_fut.await?;
            let (raw_stream, session) = tls_stream.get_ref();
            
            let remote_addr = raw_stream.peer_addr().unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
            let sni = session.server_name().map(String::from);
            let alpn = session.alpn_protocol().map(|p| p.to_vec());
            let protocol_version = session.protocol_version().map(|v| format!("{:?}", v));
            let cipher_suite = session.negotiated_cipher_suite().map(|c| format!("{:?}", c.suite()));

            let conn_info = CrankerConnectInfo {
                remote_addr,
                tls_info: Some(TlsSessionInfo {
                    sni,
                    alpn,
                    protocol_version,
                    cipher_suite,
                }),
            };

            let wrapped_service = TlsInfoService {
                inner: inner_service,
                conn_info,
            };

            Ok((tls_stream, wrapped_service))
        })
    }
}

#[derive(Clone)]
pub struct TlsInfoService<S> {
    inner: S,
    conn_info: CrankerConnectInfo,
}

impl<S, ReqBody> tower_service::Service<Request<ReqBody>> for TlsInfoService<S>
where
    S: tower_service::Service<Request<ReqBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        req.extensions_mut().insert(self.conn_info.clone());
        self.inner.call(req)
    }
}

use std::net::SocketAddr;
use axum::extract::connect_info::Connected;

#[derive(Clone, Debug)]
pub struct TlsSessionInfo {
    pub sni: Option<String>,
    pub alpn: Option<Vec<u8>>,
    pub protocol_version: Option<String>,
    pub cipher_suite: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CrankerConnectInfo {
    pub remote_addr: SocketAddr,
    pub tls_info: Option<TlsSessionInfo>,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for CrankerConnectInfo {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        CrankerConnectInfo {
            remote_addr: *stream.remote_addr(),
            tls_info: None,
        }
    }
}

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
        let protocol_version = session.protocol_version().map(|v| format!("{:?}", v));
        let cipher_suite = session.negotiated_cipher_suite().map(|c| format!("{:?}", c.suite()));

        CrankerConnectInfo {
            remote_addr: *stream.remote_addr(),
            tls_info: Some(TlsSessionInfo {
                sni,
                alpn,
                protocol_version,
                cipher_suite,
            }),
        }
    }
}

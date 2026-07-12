use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use axum::{routing::any, Router};
use scr_axum_cranker_router::{CrankerRouterBuilder, CrankerConnectInfo, RustlsListener};

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load certs
    let cert_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("self_signed_certs")
        .join("cert.bin");
    let key_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("self_signed_certs")
        .join("key.bin");

    let cert_data = std::fs::read(cert_path).expect("failed to read cert");
    let key_data = std::fs::read(key_path).expect("failed to read key");

    let mut cert_reader = std::io::BufReader::new(&cert_data[..]);
    let mut key_reader = std::io::BufReader::new(&key_data[..]);

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .expect("failed to parse certs");
    let key: rustls::pki_types::PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .expect("failed to parse private key")
        .expect("no private key found");

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("failed to build ServerConfig");

    let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
    let tcp_listener = TcpListener::bind("127.0.0.1:4003").await.unwrap();

    let listener = RustlsListener {
        tcp_listener,
        tls_acceptor,
    };

    let cranker_router = CrankerRouterBuilder::new()
        .build();

    let app = Router::new()
        .route("/", any(scr_axum_cranker_router::CrankerRouter::visit_portal))
        .route("/{*any}", any(scr_axum_cranker_router::CrankerRouter::visit_portal))
        .with_state(cranker_router.state());

    // ConnectInfo<CrankerConnectInfo> is extracted automatically here!
    let service = app.into_make_service_with_connect_info::<CrankerConnectInfo>();

    println!("Starting native Axum TLS server on https://127.0.0.1:4003");

    axum::serve(listener, service)
        .await
        .unwrap();
}

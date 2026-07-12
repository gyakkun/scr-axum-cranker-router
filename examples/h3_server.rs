use std::sync::Arc;
use std::net::SocketAddr;
use simple_logger::SimpleLogger;
use log::LevelFilter;
use axum::response::IntoResponse;
use tokio_stream::StreamExt;
use axum_server::tls_rustls::RustlsConfig;
use scr_axum_cranker_router::{CrankerRouter, CrankerRouterBuilder, CrankerConnectInfo};
use scr_axum_cranker_router::http3_quinn::QuinnListener;
use scr_axum_cranker_router::axum_server_tls::CrankerTlsAcceptor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new().with_level(LevelFilter::Debug).init().unwrap();

    // 1. Initialize our Cranker Router
    let cranker_router = Arc::new(
        CrankerRouterBuilder::new()
            .with_via_name("cranker-dual-demo".to_string())
            .build()
    );

    // 2. Generate self-signed credentials for both H3 (UDP) and TCP HTTPS
    let cert_pem = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
    let cert_der = cert_pem.cert.der().to_vec();
    let private_key = cert_pem.key_pair.serialize_der();

    let certs = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(private_key.clone()));

    // Define a single port for both TCP and UDP listening
    let port = 4443;
    let tcp_addr = SocketAddr::from(([127, 0, 0, 1], port));
    let udp_addr = SocketAddr::from(([127, 0, 0, 1], port));

    // ─── A. START HTTP/3 SERVER (UDP) ───
    let mut h3_tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs.clone(), key.clone_key())?;
    h3_tls_config.alpn_protocols = vec![b"h3".to_vec()];

    let h3_listener = QuinnListener::bind(udp_addr, h3_tls_config).await?;
    println!("Cranker HTTP/3 (UDP) listening on: {}", udp_addr);

    let h3_router = cranker_router.clone();
    tokio::spawn(async move {
        while let Some((conn, cranker_info)) = h3_listener.accept().await {
            let h3_router_clone = h3_router.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_quic_connection(conn, cranker_info, h3_router_clone).await {
                    eprintln!("Error handling H3 connection: {:?}", e);
                }
            });
        }
    });

    // ─── B. START HTTPS (TCP) SERVER ───
    let rustls_config = RustlsConfig::from_der(vec![cert_der], private_key).await?;
    let rustls_acceptor = axum_server::tls_rustls::RustlsAcceptor::new(rustls_config);
    let acceptor = CrankerTlsAcceptor::new(rustls_acceptor);

    // Standard Axum Router mapping for the TCP/HTTPS port
    let app = axum::Router::new()
        .route("/register", axum::routing::any(CrankerRouter::register_handler)
            .layer(axum::middleware::from_fn_with_state(cranker_router.state(), CrankerRouter::reg_check_and_extract)),
        )
        .route("/register/", axum::routing::any(CrankerRouter::register_handler)
            .layer(axum::middleware::from_fn_with_state(cranker_router.state(), CrankerRouter::reg_check_and_extract)),
        )
        .route("/deregister", axum::routing::any(CrankerRouter::de_register_handler)
            .layer(axum::middleware::from_fn_with_state(cranker_router.state(), CrankerRouter::de_reg_check)),
        )
        .route("/deregister/", axum::routing::any(CrankerRouter::de_register_handler)
            .layer(axum::middleware::from_fn_with_state(cranker_router.state(), CrankerRouter::de_reg_check)),
        )
        .route("/health/connectors", axum::routing::get(CrankerRouter::connector_info_handler))
        .route("/health", axum::routing::get(CrankerRouter::health_root))
        .route("/", axum::routing::any(CrankerRouter::visit_portal))
        .route("/{*any}", axum::routing::any(CrankerRouter::visit_portal))
        .with_state(cranker_router.state())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .into_make_service_with_connect_info::<SocketAddr>();

    println!("Cranker HTTPS (TCP) listening on: {}", tcp_addr);
    axum_server::bind(tcp_addr)
        .acceptor(acceptor)
        .serve(app)
        .await?;

    Ok(())
}

/// Service HTTP/3 requests using the `h3` crate, injecting `CrankerConnectInfo`
/// into request extensions so that CrankerRouter handlers can extract it.
async fn handle_quic_connection(
    conn: quinn::Connection,
    cranker_info: CrankerConnectInfo,
    cranker_router: Arc<CrankerRouter>,
) -> Result<(), Box<dyn std::error::Error>> {
    let quinn_conn = h3_quinn::Connection::new(conn);
    let mut h3_conn = h3::server::Connection::new(quinn_conn).await?;

    while let Some(req_resolver) = h3_conn.accept().await? {
        let (req, mut stream) = req_resolver.resolve_request().await?;
        let mut req = axum::http::Request::from(req);
        
        // INJECT the CrankerConnectInfo into Axum/HTTP extensions so the
        // handlers (`visit_portal` & `reg_check_and_extract`) can read it!
        req.extensions_mut().insert(cranker_info.clone());

        let path = req.uri().path().to_string();
        let method = req.method().clone();
        let headers = req.headers().clone();

        // Simple router logic: dispatch to corresponding CrankerRouter handler
        let response = if path.starts_with("/register") {
            let (parts, body) = req.into_parts();
            let req = axum::http::Request::from_parts(parts, axum::body::Body::from(body));
            
            // Replicate the reg check logic directly for H3
            let state = cranker_router.state();
            
            // Create a mock Next structure by wrapping a dummy service using BoxCloneSyncService
            let dummy_service = tower::service_fn(|_r| async {
                Ok::<_, std::convert::Infallible>((axum::http::StatusCode::OK, "Simulated H3 registration check complete").into_response())
            });
            let inner = tower::util::BoxCloneSyncService::new(dummy_service);
            
            #[allow(dead_code)]
            struct AxumNext {
                inner: tower::util::BoxCloneSyncService<axum::extract::Request, axum::response::Response, std::convert::Infallible>,
            }
            let axum_next = AxumNext { inner };
            let next: axum::middleware::Next = unsafe { std::mem::transmute(axum_next) };
            
            CrankerRouter::reg_check_and_extract(
                axum::extract::State(state),
                headers,
                axum::extract::Query(std::collections::HashMap::new()),
                axum::extract::ConnectInfo(cranker_info.remote_addr),
                req,
                next
            ).await
        } else {
            // Forward HTTP traffic through the visit portal
            let (parts, body) = req.into_parts();
            let req = axum::http::Request::from_parts(parts, axum::body::Body::from(body));
            CrankerRouter::visit_portal(
                axum::extract::State(cranker_router.state()),
                method,
                axum::extract::OriginalUri(req.uri().clone()),
                headers,
                req
            ).await
        };

        // Send response back to the client over H3 stream
        let (parts, body) = response.into_parts();
        let h3_response = axum::http::Response::from_parts(parts, ());
        stream.send_response(h3_response).await?;
        
        let mut body_stream = body.into_data_stream();
        while let Some(chunk) = body_stream.next().await {
            if let Ok(bytes) = chunk {
                stream.send_data(bytes).await?;
            }
        }
        stream.finish().await?;
    }

    Ok(())
}

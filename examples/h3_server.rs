use std::sync::Arc;
use std::net::SocketAddr;
use simple_logger::SimpleLogger;
use log::LevelFilter;
use axum::response::IntoResponse;
use tokio_stream::StreamExt;
use scr_axum_cranker_router::{CrankerRouter, CrankerRouterBuilder, CrankerConnectInfo};
use scr_axum_cranker_router::http3_quinn::QuinnListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new().with_level(LevelFilter::Debug).init().unwrap();

    // 1. Initialize our Cranker Router
    let cranker_router = Arc::new(
        CrankerRouterBuilder::new()
            .with_via_name("cranker-h3-demo".to_string())
            .build()
    );

    // 2. Load TLS credentials for QUIC
    let cert_pem = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert_der = cert_pem.cert.der().to_vec();
    let private_key = cert_pem.key_pair.serialize_der();

    let certs = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(private_key));

    // Create standard rustls ServerConfig supporting HTTP/3 ALPN ("h3")
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    // 3. Bind our QuinnListener to run HTTP/3
    let addr: SocketAddr = "127.0.0.1:4433".parse()?;
    let listener = QuinnListener::bind(addr, tls_config).await?;
    println!("Cranker H3 Router listening on udp://{}", addr);

    // 4. Accept incoming QUIC connections in a loop
    while let Some((conn, cranker_info)) = listener.accept().await {
        println!("Accepted new QUIC connection from {}", conn.remote_address());
        
        // Retrieve and inspect our newly populated quic_transport_parameters!
        if let Some(ref tls) = cranker_info.tls_info {
            if let Some(ref params) = tls.quic_transport_parameters {
                println!(
                    "  [QUIC Params Snapshot] len={}, bytes={:?}", 
                    params.len(), 
                    params
                );
            }
        }

        let cranker_router = cranker_router.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_quic_connection(conn, cranker_info, cranker_router).await {
                eprintln!("Error handling connection: {:?}", e);
            }
        });
    }

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

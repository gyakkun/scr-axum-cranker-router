use log::LevelFilter;
use simple_logger::SimpleLogger;
use std::env;
use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::TcpListener;
use std::path::PathBuf;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::{Json, Router};
use axum::routing::{any, get, post};
use axum_core::response::{IntoResponse, Response};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Server;
use tower_http::limit;
use scr_axum_cranker_router::rustls::pki_types::pem::PemObject;
use scr_axum_cranker_router::{CrankerRouter, CrankerRouterBuilder, CrankerRouterState};
use scr_axum_cranker_router::axum_server_tls::CrankerTlsAcceptor;
use scr_axum_cranker_router::dark_host::DarkHost;
use scr_axum_cranker_router::websocket_farm::WebSocketFarmInterface;

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let mut reg_port = 3000;
    let mut visit_port = 3002;
    let mut single_port = None;
    let mut routes_keep_time_millis = 5000;
    let mut connector_max_wait_time_millis = 5000;
    let mut via_name = "scr-axum".to_string();
    let mut discard_client_forwarded_headers = false;
    let mut send_legacy_forwarded_headers = false;
    let mut allow_catch_all = true;
    let mut log_level = LevelFilter::Debug;
    let mut use_domain_filter = false;
    let mut idle_read_timeout_ms = 60000;
    let mut use_tls = false;
    let mut use_http2 = true;
    let mut proxy_host_header = true;

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                single_port = Some(u16::from_str(&args[i + 1]).unwrap());
                i += 2;
            }
            "--reg-port" => {
                reg_port = u16::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--visit-port" => {
                visit_port = u16::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--routes-keep-time-millis" => {
                routes_keep_time_millis = i64::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--connector-max-wait-time-millis" => {
                connector_max_wait_time_millis = i64::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--via-name" => {
                via_name = args[i + 1].clone();
                i += 2;
            }
            "--discard-client-forwarded-headers" => {
                discard_client_forwarded_headers = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--send-legacy-forwarded-headers" => {
                send_legacy_forwarded_headers = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--allow-catch-all" => {
                allow_catch_all = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--log-level" => {
                log_level = LevelFilter::from_str(&args[i + 1]).unwrap_or(LevelFilter::Debug);
                i += 2;
            }
            "--use-domain-filter" => {
                use_domain_filter = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--idle-read-timeout-ms" => {
                idle_read_timeout_ms = i64::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--tls" => {
                use_tls = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--http2" => {
                use_http2 = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            "--proxy-host-header" => {
                proxy_host_header = bool::from_str(&args[i + 1]).unwrap();
                i += 2;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                i += 1;
            }
        }
    }

    SimpleLogger::new().with_level(log_level).init().unwrap();

    let mut cranker_router_builder = CrankerRouterBuilder::new()
        .with_routes_keep_time_millis(routes_keep_time_millis)
        .with_connector_max_wait_time_millis(connector_max_wait_time_millis)
        .with_via_name(via_name)
        .with_discard_client_forwarded_headers(discard_client_forwarded_headers)
        .with_send_legacy_forwarded_headers(send_legacy_forwarded_headers)
        .should_allow_catch_all(allow_catch_all)
        .with_idle_read_timeout_ms(idle_read_timeout_ms)
        .should_proxy_host_header(proxy_host_header);

    if use_domain_filter {
        cranker_router_builder = cranker_router_builder.with_router_socket_filter(Arc::new(
            scr_axum_cranker_router::router_socket_filter::DomainRouterSocketFilter::new()
        ));
    }

    let cranker_router = cranker_router_builder.build();
    let cranker_router = Arc::new(cranker_router);
    let unified_router = Router::new()
        .route("/register", any(CrankerRouter::register_handler)
            .layer(from_fn_with_state(cranker_router.state(), CrankerRouter::reg_check_and_extract)),
        )
        .route("/register/", any(CrankerRouter::register_handler)
            .layer(from_fn_with_state(cranker_router.state(), CrankerRouter::reg_check_and_extract)),
        )
        .route("/deregister", any(CrankerRouter::de_register_handler)
            .layer(from_fn_with_state(cranker_router.state(), CrankerRouter::de_reg_check)),
        )
        .route("/deregister/", any(CrankerRouter::de_register_handler)
            .layer(from_fn_with_state(cranker_router.state(), CrankerRouter::de_reg_check)),
        )
        .route("/health/connectors", get(CrankerRouter::connector_info_handler))
        .route("/dark-mode/enable", post(enable_dark_mode_handler))
        .route("/dark-mode/disable", post(disable_dark_mode_handler))
        .route("/dark-mode/hosts", get(get_dark_hosts_handler))
        .route("/health", get(CrankerRouter::health_root))
        .route("/", any(CrankerRouter::visit_portal))
        .route("/{*any}", any(CrankerRouter::visit_portal))
        .with_state(cranker_router.state())
        .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .into_make_service_with_connect_info::<SocketAddr>();

    let mut bind_ports = Vec::new();
    if let Some(port) = single_port {
        bind_ports.push(port);
    } else {
        bind_ports.push(reg_port);
        if visit_port != reg_port {
            bind_ports.push(visit_port);
        }
    }

    let mut servers = Vec::new();

    // 1. HTTP Listeners
    for port in &bind_ports {
        let listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            *port,
        ))
        .await
        .unwrap();
        println!("Unified HTTP router listening on port: {}", port);
        servers.push(tokio::spawn(axum::serve(listener, unified_router.clone()).into_future()));
    }

    // 2. HTTPS Listeners (if --tls true)
    if use_tls {
        let cert_bytes = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("self_signed_certs")
                .join("cert.bin"),
        ).unwrap();
        let key_bytes = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("self_signed_certs")
                .join("key.bin"),
        ).unwrap();

        let cert: Vec<rustls::pki_types::CertificateDer> =
            rustls::pki_types::CertificateDer::pem_slice_iter(&cert_bytes[..])
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

        let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_bytes[..])
            .unwrap();

        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert, key)
            .unwrap();

        if use_http2 {
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        } else {
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
        }

        let rustls_config = RustlsConfig::from_config(Arc::new(config));

        for port in &bind_ports {
            let tls_port = if *port > 50000 { *port - 10000 } else { *port + 10000 };
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), tls_port);
            println!("Unified HTTPS router listening on port: {}", tls_port);
            
            let rustls_acceptor = axum_server::tls_rustls::RustlsAcceptor::new(rustls_config.clone());
            let acceptor = CrankerTlsAcceptor::new(rustls_acceptor);
            // This is to pass the max header size test
            // which is actually testing mu-server api
            // instead of the cranker router part.
            // so we hardcode a reasonable large limit
            // to just pass the tests
            let mut srv = Server::bind(addr);
            if use_http2 {
                srv.http_builder()
                    .http2()
                    .max_frame_size(Some(131072))
                    .max_header_list_size(131072);
            }
            let srv = srv.acceptor(acceptor)
                .serve(unified_router.clone());
            servers.push(tokio::spawn(srv.into_future()));
        }
    }

    let _ = futures::future::join_all(servers).await;
}

async fn enable_dark_mode_handler(
    State(app_state): State<Arc<CrankerRouterState>>,
    axum::Json(dark_host): axum::Json<DarkHost>,
) -> Response {
    app_state.websocket_farm.enable_dark_mode(dark_host);
    (StatusCode::OK, "").into_response()
}

async fn disable_dark_mode_handler(
    State(app_state): State<Arc<CrankerRouterState>>,
    axum::Json(dark_host): axum::Json<DarkHost>,
) -> Response {
    app_state.websocket_farm.disable_dark_mode(dark_host);
    (StatusCode::OK, "").into_response()
}

async fn get_dark_hosts_handler(
    State(app_state): State<Arc<CrankerRouterState>>,
) -> Response {
    let hosts = app_state.websocket_farm.get_dark_hosts();
    let list: Vec<DarkHost> = hosts.into_iter().collect();
    (StatusCode::OK, Json(list)).into_response()
}

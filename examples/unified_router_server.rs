use log::LevelFilter;
use simple_logger::SimpleLogger;
use std::env;
use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::TcpListener;
use std::path::PathBuf;
use axum_server::tls_rustls::RustlsConfig;

use scr_axum_cranker_router::CrankerRouterBuilder;

#[tokio::main]
async fn main() {
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
        .with_idle_read_timeout_ms(idle_read_timeout_ms);

    if use_domain_filter {
        cranker_router_builder = cranker_router_builder.with_router_socket_filter(Arc::new(
            scr_axum_cranker_router::router_socket_filter::DomainRouterSocketFilter::new()
        ));
    }

    let cranker_router = cranker_router_builder.build();
    let cranker_router = Arc::new(cranker_router);
    let unified_router = cranker_router.unified_axum_router();

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
        let rustls_config = RustlsConfig::from_pem_file(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("self_signed_certs")
                .join("cert.bin"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("self_signed_certs")
                .join("key.bin"),
        ).await.unwrap();

        for port in &bind_ports {
            let tls_port = if *port > 50000 { *port - 10000 } else { *port + 10000 };
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), tls_port);
            println!("Unified HTTPS router listening on port: {}", tls_port);
            let srv = axum_server::bind_rustls(addr, rustls_config.clone())
                .serve(unified_router.clone());
            servers.push(tokio::spawn(srv.into_future()));
        }
    }

    let _ = futures::future::join_all(servers).await;
}

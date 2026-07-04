use std::env;
use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::{signal, try_join};
use log::LevelFilter;
use simple_logger::SimpleLogger;

use scr_axum_cranker_router::{CrankerRouter, CrankerRouterBuilder};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut reg_port = 3000;
    let mut visit_port = 3002;
    let mut routes_keep_time_millis = 5000;
    let mut connector_max_wait_time_millis = 5000;
    let mut via_name = "scr-axum".to_string();
    let mut discard_client_forwarded_headers = false;
    let mut send_legacy_forwarded_headers = false;
    let mut allow_catch_all = true;
    let mut log_level = LevelFilter::Debug;
    let mut use_domain_filter = false;
    let mut idle_read_timeout_ms = 60000;

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
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
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                i += 1;
            }
        }
    }

    SimpleLogger::new()
        .with_level(log_level)
        .init()
        .unwrap();

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

    let reg_listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), reg_port))
        .await
        .unwrap();
    let visit_listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), visit_port))
        .await
        .unwrap();

    let reg_router = cranker_router.registration_axum_router();
    let visit_router = cranker_router.visit_portal_axum_router();

    println!("Router started: reg_port={}, visit_port={}", reg_port, visit_port);

    async fn sig(cranker_router: Arc<CrankerRouter>) {
        match signal::ctrl_c().await {
            Ok(()) => {
                eprintln!("Stopping cranker router");
                cranker_router.stop();
            }
            Err(err) => {
                eprintln!("Unable to listen for shutdown signal: {}", err);
                cranker_router.stop();
            }
        }
    }

    let _ = try_join!(
        axum::serve(reg_listener, reg_router).with_graceful_shutdown(sig(cranker_router.clone()).into_future()).into_future(),
        axum::serve(visit_listener, visit_router).with_graceful_shutdown(sig(cranker_router.clone()).into_future()).into_future(),
    );
}

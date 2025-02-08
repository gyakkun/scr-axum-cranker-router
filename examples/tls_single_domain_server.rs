use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::{AcqRel, Acquire};

use axum::handler::HandlerWithoutStateExt;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Redirect;
use axum_core::BoxError;
use axum_extra::extract::Host;
use axum_server::tls_rustls::RustlsConfig;
use log::{debug, info, LevelFilter, warn};
use log::LevelFilter::Debug;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use tokio::try_join;

use scr_axum_cranker_router::CrankerRouterBuilder;
use scr_axum_cranker_router::exceptions::CrankerRouterException;
use scr_axum_cranker_router::proxy_info::ProxyInfo;
use scr_axum_cranker_router::proxy_listener::ProxyListener;
use scr_axum_cranker_router::router_socket_filter::DomainRouterSocketFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // To generate self-signed certificate please install openssl
    // and run the /self_signed_certs/openssl_command.sh first.
    // Reference:
    // https://github.com/tokio-rs/axum/blob/axum-v0.7.5/examples/tls-rustls/src/main.rs
    //
    // For graceful shutdown amid rustls, check the tls-graceful-shutdown example
    // https://github.com/tokio-rs/axum/blob/axum-v0.7.5/examples/tls-graceful-shutdown/src/main.rs

    let test_env = get_test_env();
    if !test_env.is_nop_logger {
        SimpleLogger::new()
            .with_level(test_env.log_level)
            .init()
            .unwrap();
    }

    let listeners: Vec<Arc<dyn ProxyListener>> = vec![Arc::new(DemoProxyListener::new())];
    let cranker_router = CrankerRouterBuilder::new()
        .with_proxy_listeners(listeners)
        .with_routes_keep_time_millis(5000)
        .with_router_socket_filter(Arc::new(DomainRouterSocketFilter::new()))
        .build();
    let cranker_router = Arc::new(cranker_router);

    let ports_reg = Ports {
        http: test_env.reg_port,
        https: test_env.reg_port + 10000,
    };

    let ports_visit = Ports {
        http: test_env.visit_port,
        https: test_env.visit_port + 10000,
    };

    if test_env.redirect_https {
        tokio::spawn(redirect_http_to_https(ports_reg.clone(), test_env.clone()));
        tokio::spawn(redirect_http_to_https(ports_visit.clone(), test_env.clone()));
    } else {
        let reg_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.reg_port))
            .await
            .unwrap();
        let visit_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.visit_port))
            .await
            .unwrap();
        tokio::spawn(axum::serve(reg_listener, cranker_router.registration_axum_router()).into_future());
        tokio::spawn(axum::serve(visit_listener, cranker_router.visit_portal_axum_router()).into_future());
    }


    // Please run the openssl_command.sh first
    // configure certificate and private key used by https
    let rustls_config = RustlsConfig::from_pem_file(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("self_signed_certs")
            .join("cert.bin"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("self_signed_certs")
            .join("key.bin"),
    ).await.unwrap();

    // reg
    let reg_https = axum_server::bind_rustls(
        SocketAddr::from_str(format!("{}:{}", test_env.addr, ports_reg.https).as_str()).unwrap(),
        rustls_config.clone(),
    ).serve(cranker_router.registration_axum_router());

    let visit_https = axum_server::bind_rustls(
        SocketAddr::from_str(format!("{}:{}", test_env.addr, ports_visit.https).as_str()).unwrap(),
        rustls_config.clone(),
    ).serve(cranker_router.visit_portal_axum_router());

    println!("trying to start simple_v1 server at - wss reg {}:{} (https {}) , visit portal {}:{} (https {})",
             test_env.addr, test_env.reg_port, ports_reg.https,
             test_env.addr, test_env.visit_port, ports_visit.https
    );

    let _ = try_join!(
        reg_https.into_future(),
        visit_https.into_future(),
    );
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
struct Ports {
    http: u16,
    https: u16,
}

#[allow(dead_code)]
async fn redirect_http_to_https(ports: Ports, test_env: TestEnv) -> std::io::Result<()> {
    fn make_https(host: String, uri: Uri, ports: Ports) -> Result<Uri, BoxError> {
        let mut parts = uri.into_parts();

        parts.scheme = Some(axum::http::uri::Scheme::HTTPS);

        if parts.path_and_query.is_none() {
            parts.path_and_query = Some("/".parse().unwrap());
        }

        let https_host = host.replace(&ports.http.to_string(), &ports.https.to_string());
        parts.authority = Some(https_host.parse()?);

        Ok(Uri::from_parts(parts)?)
    }

    let redirect = move |Host(host): Host, uri: Uri| async move {
        match make_https(host, uri, ports) {
            Ok(uri) => Ok(Redirect::permanent(&uri.to_string())),
            Err(error) => {
                warn!("failed to convert URI to HTTPS: {:?}", error);
                Err(StatusCode::BAD_REQUEST)
            }
        }
    };

    let listener = TcpListener::bind(
        format!("{}:{}", test_env.addr, ports.http)
    ).await.unwrap();
    debug!("listening http on {} and will redirect to https on port {}", listener.local_addr().unwrap(), ports.https);
    axum::serve(listener, redirect.into_make_service())
        .await
        .unwrap();
    Ok(())
}

struct DemoProxyListener {
    counter: AtomicU64,
}

impl DemoProxyListener {
    pub fn new() -> Self {
        Self { counter: AtomicU64::new(0) }
    }
}

impl ProxyListener for DemoProxyListener {
    fn on_before_proxy_to_target(&self, _info: &dyn ProxyInfo, request_headers_to_target: &mut HeaderMap) -> Result<(), CrankerRouterException> {
        let id = 1 + self.counter.fetch_add(1, AcqRel);
        info!("[{}] on_before_proxy_to_target: header={:?}", id, request_headers_to_target);
        Ok(())
    }

    fn on_after_target_to_proxy_headers_received(&self, _proxy_info: &dyn ProxyInfo, status: u16, headers: Option<&HeaderMap>) -> Result<(), CrankerRouterException> {
        info!("[{}] on_after_target_to_proxy_headers_received: {:?} {:?}", self.counter.load(Acquire), status, headers);
        Ok(())
    }

    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
        false
    }

    fn really_need_on_request_body_chunk_sent_to_target(&self) -> bool {
        false
    }
}

fn get_test_env() -> TestEnv {
    let addr = std::env::var("ACT_ADDR").ok()
        .and_then(|s| IpAddr::from_str(&s).ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::from([127, 0, 0, 1])));
    let is_nop_logger = std::env::var("ACT_IS_NOP_LOGGER").ok()
        .and_then(|s| bool::from_str(&s).ok())
        .unwrap_or(true);
    let log_level = std::env::var("ACT_LOG_LEVEL").ok()
        .and_then(|s| LevelFilter::from_str(&s).ok())
        .unwrap_or(Debug);
    let reg_port = std::env::var("ACT_REG_PORT").ok()
        .and_then(|s| u16::from_str(&s).ok())
        .unwrap_or(3000);
    let visit_port = std::env::var("ACT_VISIT_PORT").ok()
        .and_then(|s| u16::from_str(&s).ok())
        .unwrap_or(3002);
    let redirect_https = std::env::var("ACT_REDIRECT_HTTPS").ok()
        .and_then(|s| bool::from_str(&s).ok())
        .unwrap_or(true);
    TestEnv {
        addr,
        is_nop_logger,
        log_level,
        reg_port,
        visit_port,
        redirect_https,
    }
}

#[derive(Copy, Clone)]
struct TestEnv {
    addr: IpAddr,
    is_nop_logger: bool,
    log_level: LevelFilter,
    reg_port: u16,
    visit_port: u16,
    redirect_https: bool,
}
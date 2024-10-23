use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::{AcqRel, Acquire};

use axum::http::HeaderMap;
use axum::middleware::from_fn_with_state;
use axum::Router;
use axum::routing::{any, get};
use log::{info, LevelFilter};
use log::LevelFilter::{Debug};
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use tokio::{signal, try_join};
use tower_http::limit;

use scr_axum_cranker_router::{CrankerRouter, CrankerRouterBuilder};
use scr_axum_cranker_router::exceptions::CrankerRouterException;
use scr_axum_cranker_router::proxy_info::ProxyInfo;
use scr_axum_cranker_router::proxy_listener::ProxyListener;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
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
        .build();
    let cranker_router = Arc::new(cranker_router);

    let custom_axum_wss_router = Router::new()
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
        // No health endpoint exposed here
        .with_state(cranker_router.state())
        .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .into_make_service_with_connect_info::<SocketAddr>();

    let custom_axum_visit_portal_router = Router::new()
        // Expose health handler to visit portal
        .route("/health/connectors", get(CrankerRouter::connector_info_handler))
        .route("/health", get(CrankerRouter::health_root))
        .route("/*any", any(CrankerRouter::visit_portal))
        .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .with_state(cranker_router.state())
        .into_make_service_with_connect_info::<SocketAddr>();

    let reg_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.reg_port))
        .await
        .unwrap();
    let visit_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.visit_port))
        .await
        .unwrap();

    let reg_router = custom_axum_wss_router;
    let visit_router = custom_axum_visit_portal_router;

    println!("trying to start harder server at - wss reg {}:{}, visit portal {}:{}",
             test_env.addr, test_env.reg_port,
             test_env.addr, test_env.visit_port
    );

    async fn sig(cranker_router:  Arc<CrankerRouter>) {
        match signal::ctrl_c().await {
            Ok(()) => {
                eprintln!("stopping cranker router");
                cranker_router.stop();
                eprintln!("cranker router stopped");
            }
            Err(err) => {
                eprintln!("Unable to listen for shutdown signal: {}", err);
                eprintln!("stopping cranker router");
                cranker_router.stop();
                eprintln!("cranker router stopped");
            }
        }
    }

    let _ = try_join!(
        axum::serve(reg_listener, reg_router).with_graceful_shutdown(sig(cranker_router.clone()).into_future()).into_future(),
        axum::serve(visit_listener, visit_router).with_graceful_shutdown(sig(cranker_router.clone()).into_future()).into_future(),
    );

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
    TestEnv {
        addr,
        is_nop_logger,
        log_level,
        reg_port,
        visit_port
    }
}


struct TestEnv {
    addr: IpAddr,
    is_nop_logger: bool,
    log_level: LevelFilter,
    reg_port: u16,
    visit_port: u16,
}
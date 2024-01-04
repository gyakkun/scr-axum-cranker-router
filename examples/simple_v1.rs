use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

use axum::http::HeaderMap;
use log::{info, LevelFilter};
use log::LevelFilter::Info;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use tokio::try_join;

use scr_axum_cranker_router::CrankerRouterBuilder;
use scr_axum_cranker_router::exceptions::CrankerRouterException;
use scr_axum_cranker_router::proxy_info::ProxyInfo;
use scr_axum_cranker_router::proxy_listener::ProxyListener;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let test_env = get_test_env();
    if !test_env.is_nop_logger {
        SimpleLogger::new()
            .with_local_timestamps()
            .with_level(test_env.log_level)
            .init()
            .unwrap();
    }

    let listeners: Vec<Arc<dyn ProxyListener>> = vec![Arc::new(DemoProxyListener::new())];
    let cranker_router = CrankerRouterBuilder::new()
        .with_proxy_listeners(listeners)
        .with_routes_keep_time_millis(5000)
        .build();

    let reg_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.reg_port))
        .await
        .unwrap();
    let visit_listener = TcpListener::bind(format!("{}:{}", test_env.addr, test_env.visit_port))
        .await
        .unwrap();

    let reg_router = cranker_router.registration_axum_router();
    let visit_router = cranker_router.visit_portal_axum_router();

    let _ = try_join!(
        axum::serve(reg_listener, reg_router).into_future(),
        axum::serve(visit_listener, visit_router).into_future()
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
        let id = 1 + self.counter.fetch_add(1, SeqCst);
        info!("[{}] on_before_proxy_to_target: header={:?}", id, request_headers_to_target);
        Ok(())
    }

    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
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
        .unwrap_or(Info);
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
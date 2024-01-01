use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use axum::http::HeaderMap;
use bytes::Bytes;
use log::info;
use log::LevelFilter::{Debug, Info};
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;

use scr_axum_cranker_router::CrankerRouter;
use scr_axum_cranker_router::exceptions::CrankerRouterException;
use scr_axum_cranker_router::proxy_info::ProxyInfo;
use scr_axum_cranker_router::proxy_listener::ProxyListener;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    SimpleLogger::new()
        .with_local_timestamps()
        .with_level(Info)
        .init()
        .unwrap();

    let v:Vec<Arc<dyn ProxyListener>> = vec![Arc::new(DemoProxyListener::new())];
    let cranker_router = CrankerRouter::new(v);

    let reg_listener = TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    let visit_listener = TcpListener::bind("127.0.0.1:3002")
        .await
        .unwrap();

    let reg_router = cranker_router.registration_axum_router();
    let visit_router = cranker_router.visit_portal_axum_router();

    tokio::join!(
        async {axum::serve(reg_listener, reg_router).await.unwrap(); },
        async {axum::serve(visit_listener, visit_router).await.unwrap();}
    );
}

struct DemoProxyListener {
    counter: AtomicU64
}

impl DemoProxyListener {
    pub fn new() -> Self {
        Self { counter: AtomicU64::new(0) }
    }
}

impl ProxyListener for DemoProxyListener {
    fn on_before_proxy_to_target(&self, _info: &dyn ProxyInfo, request_headers_to_target: &HeaderMap) -> Result<(), CrankerRouterException> {
        let id = 1 + self.counter.fetch_add(1, SeqCst);
        info!("[{}] on_before_proxy_to_target: header={:?}", id, request_headers_to_target);
        Ok(())
    }

}
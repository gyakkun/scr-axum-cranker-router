use log::LevelFilter::Debug;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;

use scr_axum_cranker_router::CrankerRouter;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    SimpleLogger::new()
        .with_local_timestamps()
        .with_level(Debug)
        .init()
        .unwrap();

    let v = vec![];
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

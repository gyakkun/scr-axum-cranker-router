use std::fmt::Display;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::RwLock;

use axum::{async_trait, BoxError};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;

use crate::exceptions::CrankerRouterException;

// use crate::proxy_listener::ProxyListener;
// use crate::RouterSocketV1;
// use crate::web_socket_farm::WebSocketFarm;

const RESPONSE_HEADERS_TO_NOT_SEND_BACK: &[&str] = &["server"];
const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub trait WebSocketListener {
    // this should be done in on_upgrade, so ignore it
    // fn on_connect(&self, wss_tx: SplitSink<WebSocket, Message>) -> Result<(), CrankerRouterException>;
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException>;
    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
    async fn on_pong(&self, pong_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException>;
}

#[async_trait]
pub trait RouterSocket: WebSocketListener {
    // accept a client req
    async fn on_client_req(&self, headers: &HeaderMap, opt_body: Option<mpsc::Receiver<Bytes>>);

    /* async */ fn on_client_req_error(&self, reason: String);

    // accept response from target
    async fn on_target_res(&self, headers: &HeaderMap, opt_body: Option<mpsc::Receiver<Bytes>>);

    /* async */ fn on_target_res_error(&self, reason: String);
}


pub struct RouterSocketV1 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    // pub web_socket_farm: Option<Weak<Mutex<WebSocketFarm>>>,
    pub connector_instance_id: String,
    // pub proxy_listeners: Vec<&'static dyn ProxyListener>,
    pub wss_tx: SplitSink<WebSocket, Message>,
    pub wss_rx: SplitStream<WebSocket>,
    // on_ready_for_action: &'static dyn Fn() -> (),
    pub remote_address: SocketAddr,
    pub is_removed: bool,
    pub has_response: bool,
    pub bytes_received: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub binary_frame_received: AtomicI64,
    // TODO
    // async_handle: Box<dyn Future<Output=hyper::body::Body>>,
    // response: Option<axum::http::Response<Body>>,
    // client_request: Option<axum::http::Request<Body>>,
    pub socket_wait_in_millis: i64,
    pub error: Option<BoxError>,
    pub duration_millis: i64,
    // TODO: seems axum receive websocket message in a Message level rather than a Frame level
    // so maybe no need to create buffer for frame of TEXT message
    // on_text_buffer: Vec<char>,

    // below should be private for inner routine


    target_res_header_received: AtomicBool,
    header_string_buf: RwLock<String>,
    target_res_header_sent: AtomicBool,
    target_res_body_received: AtomicBool,
    websocket_closed: AtomicBool,
}

impl RouterSocketV1 {
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               connector_instance_id: String,
               wss_tx: SplitSink<WebSocket, Message>,
               wss_rx: SplitStream<WebSocket>,
               remote_address: SocketAddr,
    ) -> Self {
        Self {
            route,
            component_name,
            router_socket_id,
            connector_instance_id,
            wss_tx,
            wss_rx,
            remote_address,
            is_removed: false,
            has_response: false,
            bytes_received: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            binary_frame_received: AtomicI64::new(0),
            // response: None,
            // client_request: None,
            socket_wait_in_millis: -1,
            error: None,
            duration_millis: -1,

            target_res_header_received: AtomicBool::new(false),
            header_string_buf: RwLock::new("".to_string()),
            target_res_header_sent: AtomicBool::new(false),
            target_res_body_received: AtomicBool::new(false),
            websocket_closed: AtomicBool::new(false),
        }
    }
}


#[async_trait]
impl WebSocketListener for RouterSocketV1 {
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException> {
        self.target_res_header_received.store(true, SeqCst);
        if self.target_res_body_received.load(SeqCst) {
            let failed_reason = "res body already received but still receiving text message which is not expected!".to_string();
            self.on_target_res_error(failed_reason.clone());
            return Err(CrankerRouterException::new(failed_reason));
        }
        let text_len = text_msg.len();
        if text_len + self.header_string_buf.read().unwrap().len() > HEADER_MAX_SIZE {
            let failed_reason = format!("Header too large after appending: before {} bytes, after {} bytes, max {} bytes",
                                        text_len, self.header_string_buf.read().unwrap().len(), HEADER_MAX_SIZE);
            self.on_target_res_error(failed_reason.clone());
            return Err(CrankerRouterException::new(failed_reason));
        }
        // FIXME: Is it necessary to handle lock error here?
        match self.header_string_buf.try_write() {
            Ok(mut hsb) => {
                hsb.push_str(text_msg.as_str());
            }
            Err(e) => {
                let failed_reason = format!("failed to lock header_string_buf: {:?}", e).to_string();
                self.on_target_res_error(failed_reason.clone());
                return Err(CrankerRouterException::new(failed_reason));
            }
        }

        Ok(())
    }

    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        self.target_res_body_received.store(true, SeqCst);
        if !self.target_res_header_received.load(SeqCst) {
            let failed_reason = "res header not received yet but binary comes first which is not expected!".to_string();
            self.on_client_req_error(failed_reason.clone());
            return Err(CrankerRouterException::new(failed_reason));
        }
        Ok(())
    }

    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        todo!()
    }

    async fn on_pong(&self, pong_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        todo!()
    }

    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        todo!()
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    async fn on_client_req(&self, headers: &HeaderMap, opt_body: Option<Receiver<Bytes>>) {
        todo!()
    }

    fn on_client_req_error(&self, reason: String) {
        todo!()
    }

    async fn on_target_res(&self, headers: &HeaderMap, opt_body: Option<Receiver<Bytes>>) {
        todo!()
    }

    fn on_target_res_error(&self, reason: String) {
        todo!()
    }
}
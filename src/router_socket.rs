use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicI64;

use axum::body::Body;
use axum::BoxError;
use axum::extract::ws::{Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};

// use crate::proxy_listener::ProxyListener;
// use crate::RouterSocketV1;
// use crate::web_socket_farm::WebSocketFarm;

const RESPONSE_HEADERS_TO_NOT_SEND_BACK: &[&str] = &["server"];

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
    remote_address: SocketAddr,
    is_removed: bool,
    has_response: bool,
    bytes_received: AtomicI64,
    bytes_sent: AtomicI64,
    binary_frame_received: AtomicI64,
    // TODO
    // async_handle: Box<dyn Future<Output=hyper::body::Body>>,
    // response: Option<axum::http::Response<Body>>,
    // client_request: Option<axum::http::Request<Body>>,
    socket_wait_in_millis: i64,
    error: Option<BoxError>,
    duration_millis: i64,
    // TODO: seems axum receive websocket message in a Message level rather than a Frame level
    // so maybe no need to create buffer for frame of TEXT message
    // on_text_buffer: Vec<char>,
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
        }
    }
}
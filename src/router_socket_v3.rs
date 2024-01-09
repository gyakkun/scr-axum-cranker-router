use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};

use async_channel::Receiver;
use axum::extract::OriginalUri;
use axum::extract::ws::{Message, WebSocket};
use axum::http::{HeaderMap, Method, Response, Version};
use axum_core::body::{Body, BodyDataStream};
use dashmap::DashMap;
use futures::stream::{SplitSink, SplitStream};
use tokio::sync::{Mutex, Notify};
use crate::ACRState;

use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::router_socket::RouterSocket;
use crate::websocket_farm::WebSocketFarm;

static MESSAGE_TYPE_DATA: u8 = 0;
static MESSAGE_TYPE_HEADER: u8 = 1;
static MESSAGE_TYPE_RST_STREAM: u8 = 3;
static MESSAGE_TYPE_WINDOW_UPDATE: u8 = 8;
static ERROR_INTERNAL: u8 = 1;


pub struct RouterSocketV3 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    pub web_socket_farm: Weak<WebSocketFarm>,
    pub connector_id: String,
    pub proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    pub discard_client_forwarded_headers: bool,
    pub send_legacy_forwarded_headers: bool,
    pub via_value: String,
    // private Runnable onReadyForAction;
    pub remote_address: SocketAddr,

    pub is_removed: Arc<AtomicBool>,

    pub context_map: DashMap<i32, Arc<RequestContext>>,
    pub id_maker: AtomicI32,

    pub wss_exchange: Arc<WssExchangeV3>,
}

impl RouterSocketV3 {
    pub fn new(route: String,
               domain: String, // V3 only field
               component_name: String,
               router_socket_id: String,
               websocket_farm: Weak<WebSocketFarm>,
               connector_instance_id: String,
               remote_address: SocketAddr,
               underlying_wss_tx: SplitSink<WebSocket, Message>,
               underlying_wss_rx: SplitStream<WebSocket>,
               proxy_listeners: Vec<Arc<dyn ProxyListener>>,
               idle_read_timeout_ms: i64,
               ping_sent_after_no_write_for_ms: i64,
    ) -> Arc<Self> {
        todo!()
    }
}

impl RouterSocket for RouterSocketV3 {
    fn component_name(&self) -> String {
        todo!()
    }

    fn connector_id(&self) -> String {
        todo!()
    }

    fn is_removed(&self) -> bool {
        todo!()
    }

    fn cranker_version(&self) -> &'static str {
        todo!()
    }

    async fn on_client_req(self: Arc<Self>, app_state: ACRState, http_version: &Version, method: &Method, original_uri: &OriginalUri, headers: &HeaderMap, addr: &SocketAddr, opt_body: Option<BodyDataStream>) -> Result<Response<Body>, CrankerRouterException> {
        todo!()
    }
}


static WATER_MARK_HIGH: i32 = 64 * 1024;
static WATER_MARK_LOW: i32 = 16 * 1024;

struct RequestContext {
    // wss tunnel
    pub wss_received_ack_bytes: AtomicI32,
    pub is_wss_sending: AtomicI32,
    // init true
    pub is_wss_writable: AtomicBool,
    // init false
    pub is_wss_writing: AtomicBool,
    // wssWriteCallbacks : ConcurrentLinkedQueue< > (), ???
    pub wss_on_binary_call_count: AtomicI64,

    pub request_id: i32,
    // MuRequest request,
    // MuResponse response,
    // AsyncHandle asyncHandle,
    pub tgt_res_hdr_rx: Receiver<Result<String, CrankerRouterException>>,
    pub tgt_res_bdy_rx: Receiver<Result<Vec<u8>, CrankerRouterException>>,

    // client
    pub from_client_bytes: AtomicI64,
    pub to_client_bytes: AtomicI64,

    pub duration_millis: AtomicI64,
    pub error: Mutex<Option<CrankerRouterException>>,
    // init false
    pub is_rst_stream_sent: AtomicBool,
    // init OPEN
    pub state: Mutex<StreamState>,
    pub header_line_builder: Mutex<String>,

    pub wss_exchange: Arc<WssExchangeV3>,

    // use notify to flow control???
    pub can_write_notify: Notify,
    pub pause_write_notify: Notify
}

impl ProxyInfo for RequestContext {
    fn is_catch_all(&self) -> bool {
        todo!()
    }

    fn connector_instance_id(&self) -> String {
        todo!()
    }

    fn service_address(&self) -> SocketAddr {
        todo!()
    }

    fn route(&self) -> String {
        todo!()
    }

    fn router_socket_id(&self) -> String {
        todo!()
    }

    fn duration_millis(&self) -> i64 {
        todo!()
    }

    fn bytes_received(&self) -> i64 {
        todo!()
    }

    fn bytes_sent(&self) -> i64 {
        todo!()
    }

    fn response_body_frames(&self) -> i64 {
        todo!()
    }

    fn error_if_any(&self) -> Option<CrankerRouterException> {
        todo!()
    }

    fn socket_wait_in_millis(&self) -> i64 {
        todo!()
    }
}

pub enum StreamState {
    Open,
    HalfClose,
    Closed,
    Error,
}

impl StreamState {
    #[inline]
    fn is_completed(&self) -> bool {
        match self {
            StreamState::Open | StreamState::HalfClose => false,
            StreamState::Closed | StreamState::Error => true
        }
    }
}

struct WssExchangeV3 {
    underlying_wss_tx: SplitSink<WebSocket, Message>,
    underlying_wss_rx: SplitStream<WebSocket>,

    wss_send_task_rx: Receiver<Message>,
    // recv a msg from uwss, and dispatch to exact req context
    wss_recv_dispatcher: DashMap<i32, Weak<RequestContext>>,

}


impl RouterSocketV3 {
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               websocket_farm: Weak<WebSocketFarm>,
               connector_instance_id: String,
               remote_address: SocketAddr,
               underlying_wss_tx: SplitSink<WebSocket, Message>,
               underlying_wss_rx: SplitStream<WebSocket>,
               proxy_listeners: Vec<Arc<dyn ProxyListener>>,
               idle_read_timeout_ms: i64,
               ping_sent_after_no_write_for_ms: i64,
    ) {}
}
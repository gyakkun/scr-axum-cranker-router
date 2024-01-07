use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};

use async_channel::Receiver;
use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures::stream::{SplitSink, SplitStream};
use tokio::sync::Mutex;

use crate::exceptions::CrankerRouterException;
use crate::proxy_listener::ProxyListener;
use crate::websocket_farm::WebSocketFarm;

static MESSAGE_TYPE_DATA: u8 = 0;
static MESSAGE_TYPE_HEADER: u8 = 1;
static MESSAGE_TYPE_RST_STREAM: u8 = 3;
static MESSAGE_TYPE_WINDOW_UPDATE: u8 = 8;
static ERROR_INTERNAL: u8 = 1;


pub struct RouterSocketV3 {
    route: String,
    component_name: String,
    router_socket_id: String,
    web_socket_farm: Weak<WebSocketFarm>,
    connector_id: String,
    proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    discard_client_forwarded_headers: bool,
    send_legacy_forwarded_headers: bool,
    via_value: String,
    // private Runnable onReadyForAction;
    remote_address: SocketAddr,

    is_removed: Arc<AtomicBool>,

    context_map: DashMap<i32, Arc<RequestContext>>,
    id_maker: AtomicI32,
}

static WATER_MARK_HIGH: i32 = 64 * 1024;
static WATER_MARK_LOW: i32 = 16 * 1024;

struct RequestContext {
    // wss tunnel
    wss_received_ack_bytes: AtomicI32,
    is_wss_sending: AtomicI32,
    // init true
    is_wss_writable: AtomicBool,
    // init false
    is_wss_writing: AtomicBool,
    // wssWriteCallbacks : ConcurrentLinkedQueue< > (), ???
    wss_on_binary_call_count: AtomicI64,

    request_id: i32,
    // MuRequest request,
    // MuResponse response,
    // AsyncHandle asyncHandle,
    tgt_res_hdr_rx: Receiver<Result<String, CrankerRouterException>>,
    tgt_res_bdy_rx: Receiver<Result<Vec<u8>, CrankerRouterException>>,

    // client
    from_client_bytes: AtomicI64,
    to_client_bytes: AtomicI64,

    duration_millis: AtomicI64,
    error: Mutex<Option<CrankerRouterException>>,
    // init false
    is_rst_stream_sent: AtomicBool,
    // init OPEN
    state: StreamState,
    header_line_builder: Mutex<String>,
}

enum StreamState {
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
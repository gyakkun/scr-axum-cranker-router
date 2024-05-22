use std::fmt::format;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::atomic::Ordering::{Acquire, SeqCst};

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::extract::OriginalUri;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, Method, Response, Version};
use axum_core::body::{Body, BodyDataStream};
use bytes::{Buf, Bytes};
use dashmap::DashMap;
use futures::stream::{SplitSink, SplitStream};
use futures::TryFutureExt;
use log::{debug, error, warn};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::{ACRState, CRANKER_V_3_0};
use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::router_socket::{RouteIdentify, RouterSocket};
use crate::websocket_farm::WebSocketFarm;
use crate::websocket_listener::WebSocketListener;

const _MSG_TYPE_LEN_IN_BYTES: usize = 1;
// u8
const _FLAGS_LEN_IN_BYTES: usize = 1;
// u8
const _REQ_ID_LEN_IN_BYTES: usize = 4; // i32

const MESSAGE_TYPE_DATA: u8 = 0;
const MESSAGE_TYPE_HEADER: u8 = 1;
const MESSAGE_TYPE_RST_STREAM: u8 = 3;
const MESSAGE_TYPE_WINDOW_UPDATE: u8 = 8;
const ERROR_INTERNAL: u8 = 1;

const RCTX_INVALID_UPPER_RS3_MSG: String = "(invalid router socket v3)".to_string();


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

    // The only place to store strong Arc of RequestContext
    pub context_map: DashMap<i32, Arc<RequestContext>>,
    pub id_maker: AtomicI32,

    pub wss_exchange: Arc<WssExchangeV3>,
}


impl RouteIdentify for RouterSocketV3 {
    fn router_socket_id(&self) -> String {
        return self.router_socket_id.clone();
    }

    fn route(&self) -> String {
        return self.route.clone();
    }

    fn service_address(&self) -> SocketAddr {
        return self.service_address().clone();
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV3 {
    fn component_name(&self) -> String {
        return self.component_name.clone();
    }

    fn connector_id(&self) -> String {
        return self.connector_id.clone();
    }

    fn is_removed(&self) -> bool {
        return self.is_removed.load(SeqCst);
    }

    fn cranker_version(&self) -> &'static str {
        return CRANKER_V_3_0;
    }

    async fn on_client_req(self: Arc<Self>, app_state: ACRState, http_version: &Version, method: &Method, original_uri: &OriginalUri, headers: &HeaderMap, addr: &SocketAddr, opt_body: Option<BodyDataStream>) -> Result<Response<Body>, CrankerRouterException> {
        todo!()
    }
}


const WATER_MARK_HIGH: i32 = 64 * 1024;
const WATER_MARK_LOW: i32 = 16 * 1024;

struct RequestContext {
    // To mimic RouterSocketV3 in Java, we need a weak reference to
    //  the outer class
    pub weak_outer_router_socket_v3: Weak<RouterSocketV3>,
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
    pub tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    pub tgt_res_hdr_rx: Receiver<Result<String, CrankerRouterException>>,
    pub tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    pub tgt_res_bdy_rx: Receiver<Result<Vec<u8>, CrankerRouterException>>,

    // client
    pub from_client_bytes: AtomicI64,
    pub to_client_bytes: AtomicI64,

    pub duration_millis: AtomicI64,
    pub error: RwLock<Option<CrankerRouterException>>,
    // init false
    pub is_rst_stream_sent: AtomicBool,
    // init OPEN
    pub state: RwLock<StreamState>,
    pub header_line_builder: RwLock<String>,

    pub wss_exchange: Arc<WssExchangeV3>,

    // use notify to flow control???
    pub can_write_notify: Notify,
    pub pause_write_notify: Notify,
}

impl RouteIdentify for RequestContext {
    fn router_socket_id(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.router_socket_id();
        }
        return RCTX_INVALID_UPPER_RS3_MSG;
    }

    fn route(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.route();
        }
        return RCTX_INVALID_UPPER_RS3_MSG;
    }

    fn service_address(&self) -> SocketAddr {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.service_address();
        }
        return SocketAddr::new([255, 255, 255, 255].into(), 65535)
    }
}

impl ProxyInfo for RequestContext {
    fn is_catch_all(&self) -> bool {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.route.eq("*");
        }
        return false;
    }

    fn connector_id(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.connector_id.clone();
        }
        return RCTX_INVALID_UPPER_RS3_MSG;
    }

    fn duration_millis(&self) -> i64 {
        return self.duration_millis.load(Acquire)
    }

    fn bytes_received(&self) -> i64 {
        return self.from_client_bytes.load(Acquire)
    }

    fn bytes_sent(&self) -> i64 {
        return self.to_client_bytes.load(Acquire)
    }

    fn response_body_frames(&self) -> i64 {
        return self.wss_on_binary_call_count.load(Acquire);
    }

    fn error_if_any(&self) -> Option<CrankerRouterException> {
        return self.error.try_read()
            .ok()
            .and_then(|ok|
                ok.clone().map(|some| some.clone())
            )
    }

    fn socket_wait_in_millis(&self) -> i64 {
        return 0;
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
    route: String,
    router_socket_id: String,
    underlying_wss_tx: SplitSink<WebSocket, Message>,
    underlying_wss_rx: SplitStream<WebSocket>,

    err_chan_tx: Sender<CrankerRouterException>,
    err_chan_rx: Receiver<CrankerRouterException>,

    rs_is_removed: Arc<AtomicBool>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_rx: Receiver<Message>,
    // recv a msg from uwss, and dispatch to exact req context
    wss_recv_dispatcher: DashMap<i32, Weak<RequestContext>>,
}

// Handle binary msg
impl WssExchangeV3 {
    pub async fn handle_data(&self, flags: u8, req_id: i32, bin: Bytes) -> Result<(), CrankerRouterException> {
        let opt_ctx = self.wss_recv_dispatcher.get(&req_id);
        if opt_ctx.is_none() {
            return Err(CrankerRouterException::new(format!(
                "can not found ctx for req id={} in router_socket_id={}",
                req_id, self.router_socket_id
            )));
        }
        let ctx = opt_ctx.unwrap().upgrade();
        if ctx.is_none() {
            return Err(CrankerRouterException::new(format!(
                "found ctx for req id={} but seems already dropped in router_socket_id={}",
                req_id, self.router_socket_id
            )));
        }
        let ctx = ctx.unwrap();

        let is_end_stream = is_end_stream(flags);
        let len = bin.remaining();
        if len == 0 {
            if is_end_stream {
                self.notify_client_request_close(req_id, 1000/*ws status code*/);
            }
            return Ok(());
        }

        ctx.wss_on_binary_call_count.fetch_add(1, SeqCst);
        if self.rs_is_removed.load(SeqCst) {
            if is_end_stream {
                self.notify_client_request_close(req_id, 1000/*ws status code*/);
            }
            return Err(CrankerRouterException::new(format!(
                "recv bin msg from connector but router socket already removed. req_id={}, flags={}", req_id, flags
            )));
        }

        debug!("route={}, router_socket_id={}, sending {} bytes to client", self.route, self.router_socket_id, len);
        ctx.tgt_res_bdy_tx.send(Ok(bin.to_vec())).await
            .map(|ok|{
                // TODO: I think we should notify_xxx at the end of [s]loop[/s] no loop.
                // here should be fine I think
                if is_end_stream {
                    self.notify_client_request_close(req_id, 1000);
                }
                ok
            })
            .map_err(|e|
                // TODO: in mu they handle error in the asyncHandle.write callback
                // where should we deal with this?
                CrankerRouterException::new(format!(
                    "rare ex failed to send bin to tgt_res_bdy chan: {:?}", e
                )))
    }


    // pretty like on_close in RouterSocketV1
    fn notify_client_request_close(&self, req_id: i32, ws_status_code: u16) {
        todo!()
    }
}

const _END_STREAM_FLAG_MASK: u8 = 0b00000001;
const _END_HEADER_FLAG_MASK: u8 = 0b00000100;

#[inline]
fn is_end_stream(flags: u8) -> bool {
    flags & _END_STREAM_FLAG_MASK == _END_STREAM_FLAG_MASK
}

#[inline]
fn is_end_header(flags: u8) -> bool {
    flags & _END_HEADER_FLAG_MASK == _END_HEADER_FLAG_MASK
}

impl WssExchangeV3 {
    pub fn reset_request_context_by_id(&self, req_ctx_id: i32) {
        if let Some(weak_req_ctx) = self.wss_recv_dispatcher.get(&req_ctx_id) {
            if let Some(req_ctx) = weak_req_ctx.upgrade() {
                // req_ctx.reset_and_close(); // TODO: async / bg method, take &Arc<Self> as first param
            }
        }
    }

    pub fn reset_all(&self) {
        self.wss_recv_dispatcher.iter().for_each(|e| {
            if let Some(req_ctx) = e.value().upgrade() {
                // req_ctx.reset_and_close() // TODO
            }
        })
    }
}

#[async_trait]
impl WebSocketListener for WssExchangeV3 {
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException> {
        // should we on error here?
        Err(CrankerRouterException::new(format!("v3 should not send txt msg bug got: {:?}", text_msg)))
    }

    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        // TODO: Can we make here zero copy???
        // TODO Add remaining length check here otherwise it will panic
        let mut bin = Bytes::from(binary_msg);
        if bin.remaining() < _MSG_TYPE_LEN_IN_BYTES {
            return Err(CrankerRouterException::new(
                "recv bin msg len less than 1 to read msg type byte".to_string()
            ));
        }
        let msg_type_byte = bin.get_u8();
        if bin.remaining() < _FLAGS_LEN_IN_BYTES {
            return Err(CrankerRouterException::new(
                "recv bin msg len less than 2 to read flags byte".to_string()
            ));
        }
        let flags_byte = bin.get_u8();
        if bin.remaining() < _REQ_ID_LEN_IN_BYTES {
            return Err(CrankerRouterException::new(
                "recv bin msg len less than 6 to read request id".to_string()
            ));
        }
        let req_id_int = bin.get_i32(); // big endian

        match msg_type_byte {
            MESSAGE_TYPE_DATA => {
                self.handle_data(flags_byte, req_id_int, bin)?
            }
            MESSAGE_TYPE_HEADER => {
                self.handle_header(flags_byte, req_id_int, bin)?
            }
            MESSAGE_TYPE_RST_STREAM => {
                self.handle_rst_stream(flags_byte /*can be ignored*/, req_id_int, bin)?
            }
            MESSAGE_TYPE_WINDOW_UPDATE => {
                self.handle_window_update(flags_byte, req_id_int, bin)?
            }
            _ => {
                // TODO: Should we on_error here?
                let failed_reason = format!("Received unknown type: {}. router_socket_id={}", msg_type_byte, self.router_socket_id);
                // return Err(CrankerRouterException::new(failed_reason.clone()));
                error!("{}", failed_reason);
            }
        }
        Ok(())
    }

    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        // Same as RouterSocketV1
        if let Err(e) = self.wss_send_task_tx.send(Message::Pong(ping_msg)).await {
            return self.on_error(CrankerRouterException::new(format!(
                "failed to pong back {:?}", e
            )));
        }
        Ok(())
    }

    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        warn!("uwss get close frame: {:?}. router_socket_id={}", close_msg, self.router_socket_id);
        // TODO: Ref v1
        self.reset_all();
        Ok(())
    }

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        // [s]Same as RouterSocketV1[/s]
        // see todo below
        self.err_chan_tx.send_blocking(err)
            .map(|ok| {
                // TODO: reset close self here or in rs.ws_ex.reset_all or?
                // let _ = self.raise_completion_event();
                ok
            })
            .map_err(|se| CrankerRouterException::new(format!(
                "rare ex that failed to send error to err chan: {:?}", se
            )))
    }
}

impl RouterSocketV3 {
    pub fn new(route: String,
               domain: String,
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

#[cfg(test)]
mod tests {
    #[test]
    pub fn test_from_vec_v8_to_i32() {
        // useless, decide to use Bytes directly
        let mut i: Vec<u8> = Vec::new();
        i.push(0);
        i.push(0);
        i.push(0);
        i.push(1);
        let j = i32::from_be_bytes(i[0..4].try_into().unwrap());
        assert_eq!(j, 1);
    }
}
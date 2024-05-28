use std::fmt::format;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::atomic::Ordering::{Acquire, SeqCst};

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::extract::OriginalUri;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, Method, Request, Response, Version};
use axum_core::body::{Body, BodyDataStream};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use futures::{SinkExt, TryFutureExt};
use futures::stream::{FusedStream, SplitSink, SplitStream};
use log::{debug, error, warn};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::{ACRState, CRANKER_V_3_0};
use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::router_socket::{ClientRequestIdentifier, RouteIdentify, RouterSocket};
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

const REQ_CTX_INVALID_OUTER_RS3_MSG: String = "(invalid router socket v3)".to_string();


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
    pub req_id_generator: AtomicI32,

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

        // For the response part
        // We need a latch / Notify here to let this function know if the corresponding request context
        // has already received the response header from target

        // For flow control, when reading chunks of opt_body, we need to check the req ctx is_wss_writable
        // everytime a chunk is received. And if it's not writable, wait on the Notify
        // (typical wait on condition (Notify) with endless loop
        /// ```rust
        /// use std::sync::atomic::Ordering::SeqCst;
        /// use super::router_socket_v3::RequestContext;
        /// let ctx = RequestContext::new();
        /// while !ctx.is_wss_writable.load(SeqCst) {
        ///     ctx.is_wss_writable_notify.notified().await;
        /// }
        /// ```
    }
}


const WATER_MARK_HIGH: i32 = 64 * 1024;
const WATER_MARK_LOW: i32 = 16 * 1024;

struct RequestContext {
    // To mimic RouterSocketV3 in Java, we need a weak reference to
    //  the outer class
    pub weak_outer_router_socket_v3: Weak<RouterSocketV3>,
    // wss tunnel
    pub wss_tgt_connector_ack_bytes: AtomicI32,
    pub wss_rtr_to_tgt_pending_ack_bytes: AtomicI32,
    // init true
    pub is_wss_writable: AtomicBool,
    // init false
    pub is_wss_writing: AtomicBool,
    // wssWriteCallbacks : ConcurrentLinkedQueue< > (), ???
    pub should_keep_read_from_cli_tx: Sender<Result<ShouldKeepReadFromCli, CrankerRouterException>>,
    pub should_keep_read_from_cli_rx: Receiver<Result<ShouldKeepReadFromCli, CrankerRouterException>>,
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
    pub can_write: AtomicBool,
    pub can_write_notify: Notify,
    pub pause_write_notify: Notify,

    // use to notify there's a target response available
    pub should_have_response: AtomicBool,
}

impl RequestContext {
    fn target_connector_ack_bytes(self: &Self, ack: i32) {
        self.wss_tgt_connector_ack_bytes.fetch_add(ack, SeqCst);
        self.wss_rtr_to_tgt_pending_ack_bytes.fetch_add(-ack, SeqCst);
        if self.wss_rtr_to_tgt_pending_ack_bytes.load(SeqCst) < WATER_MARK_LOW {
            if let Ok(_) = self.is_wss_writable.compare_exchange(false, true, SeqCst, SeqCst) {
                self.write_it_maybe();
            }
        }
    }

    fn write_it_maybe(self: &Self) {
        // if self.is_wss_writable.load(SeqCst) && !
        if self.is_wss_writable.load(SeqCst) && (!self.should_keep_read_from_cli_rx.is_empty()
            && !self.should_keep_read_from_cli_rx.is_closed()
            && !self.should_keep_read_from_cli_rx.is_terminated()
        ) {
            if let Ok(_) = self.is_wss_writing.compare_exchange(
                false, true, SeqCst, SeqCst,
            ) {
                while self.is_wss_writable.load(SeqCst)
                    && (!self.should_keep_read_from_cli_rx.is_empty()
                    && !self.should_keep_read_from_cli_rx.is_closed()
                    && !self.should_keep_read_from_cli_rx.is_terminated()
                ) {
                    // async or sync?
                    // try sync first
                    if let Ok(cbrs) = self.should_keep_read_from_cli_rx.try_recv() {
                        match cbrs {
                            Ok(cb) => {
                                // let _ = cb.should_read_from_cli.compare_exchange(false, true, SeqCst, SeqCst);
                                cb.should_read_from_cli.store(true, SeqCst);
                                cb.should_read_from_cli_notify.notify_waiters();
                            }
                            Err(err) => {
                                error!("received error during write it maybe: {:?}", err);
                                break;
                            }
                        }
                    }
                }
                self.is_wss_writing.store(false, SeqCst);
                self.write_it_maybe();
            }
        }
    }
}

impl RouteIdentify for RequestContext {
    fn router_socket_id(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.router_socket_id();
        }
        return REQ_CTX_INVALID_OUTER_RS3_MSG;
    }

    fn route(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.route();
        }
        return REQ_CTX_INVALID_OUTER_RS3_MSG;
    }

    fn service_address(&self) -> SocketAddr {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.service_address();
        }
        return SocketAddr::new([255, 255, 255, 255].into(), 65535);
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
        return REQ_CTX_INVALID_OUTER_RS3_MSG;
    }

    fn duration_millis(&self) -> i64 {
        return self.duration_millis.load(SeqCst);
    }

    fn bytes_received(&self) -> i64 {
        return self.from_client_bytes.load(SeqCst);
    }

    fn bytes_sent(&self) -> i64 {
        return self.to_client_bytes.load(SeqCst);
    }

    fn response_body_frames(&self) -> i64 {
        return self.wss_on_binary_call_count.load(SeqCst);
    }

    fn error_if_any(&self) -> Option<CrankerRouterException> {
        return self.error.try_read()
            .ok()
            .and_then(|ok|
                ok.clone().map(|some| some.clone())
            );
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
    weak_outer_router_socket_v3: Weak<RouterSocketV3>,

    // FIXME: We copy the route and router socket id in case
    //  of the outer router socket v3 is being removed
    route: String,
    router_socket_id: String,
    underlying_wss_tx: SplitSink<WebSocket, Message>,
    underlying_wss_rx: SplitStream<WebSocket>,

    err_chan_tx: Sender<CrankerRouterException>,
    err_chan_rx: Receiver<CrankerRouterException>,

    // FIXME: This should be no longer used, should use the weaker_outer_router_socket_v3.upgrade().is_removed()
    //  see self.is_upper_rs3_removed()
    // rs_is_removed: Arc<AtomicBool>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_rx: Receiver<Message>,
    // recv a msg from uwss, and dispatch to exact req context
    wss_recv_dispatcher: DashMap<i32, Weak<RequestContext>>,
}

// Handle binary msg
impl WssExchangeV3 {
    pub async fn handle_data(&self, ctx: &RequestContext, flags: u8, req_id: i32, mut bin: Bytes) -> Result<(), CrankerRouterException> {
        let is_end_stream = judge_is_stream_end_from_flags(flags);
        let len = bin.remaining();
        if len == 0 {
            if is_end_stream {
                let _ = self.notify_client_request_close(ctx, 1000/*ws status code*/, None).await;
            }
            return Ok(());
        }

        ctx.wss_on_binary_call_count.fetch_add(1, SeqCst);
        if self.is_upper_rs3_removed() {
            if is_end_stream {
                let _ = self.notify_client_request_close(ctx, 1000/*ws status code*/, None).await;
            }
            return Err(CrankerRouterException::new(format!(
                "recv bin msg from connector but router socket already removed. req_id={}, flags={}", req_id, flags
            )));
        }

        debug!("route={}, router_socket_id={}, sending {} bytes to client", self.route, self.router_socket_id, len);
        // FIXME: bin.to_vec() is underlying copying the bin
        //  try to do zero-copy here!
        //  We should probably use `bin.into_vec()` here that seems cost-free
        ctx.tgt_res_bdy_tx.send(Ok(bin.to_vec())).await
            .map(|ok| async {
                // TODO: I think we should notify_xxx at the end of [s]loop[/s] no loop.
                // here should be fine I think
                if is_end_stream {
                    let _ = self.notify_client_request_close(ctx, 1000, None).await;
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


    pub async fn handle_header(&self, ctx: &RequestContext, flags: u8, req_id: i32, mut bin: Bytes) -> Result<(), CrankerRouterException> {
        let is_stream_end = judge_is_stream_end_from_flags(flags);
        let is_header_end = judge_is_header_end_from_flags(flags);
        let byte_len: i32 = bin.remaining().try_into().map_err(|e| {
            CrankerRouterException::new(format!(
                "failed to handle remaining bin msg len, it's even larger than i32::MAX, req id = {} , router socket id = {} , route = {}",
                req_id, ctx.router_socket_id(), ctx.route())
            )
        })?;
        // FIXME: bin.to_vec() is underlying copying the bin
        //  try to do zero-copy here!
        let content = String::from_utf8(bin.to_vec()).map_err(|e| {
            Err(CrankerRouterException::new(format!(
                "failed to convert binary to header text in utf8, req id = {} , router socket id = {} , route = {}",
                req_id, ctx.router_socket_id(), ctx.route())
            ))
        })?;
        // FIXME: This try method returns error if can't release write lock immediately
        // ctx.header_line_builder.try_write()
        //     .map(|mut hlb| {
        //         hlb.push_str(content.as_str())
        //     })
        //     .map_err(|e| {
        //         CrankerRouterException::new(format!(
        //             "failed to write lock header line builder, req id = {} , router socket id = {} , route = {}",
        //             req_id, ctx.router_socket_id(), ctx.route()
        //         ))
        //     })?;
        {
            let mut hlb = ctx.header_line_builder.write().await;
            hlb.push_str(content.as_str());
        }
        if is_header_end {
            let full_content = ctx.header_line_builder.read().await.clone();
            self.do_send_header_to_cli(ctx, full_content)?;
        }
        if is_stream_end {
            let _ = self.notify_client_request_close(ctx, 1000, None).await; // TODO: What does 1000 mean?
        }
        let window_update_message = window_update_message(req_id, byte_len);
        let _ = self.wss_send_task_tx.send(Message::Binary(window_update_message.into_vec())).await;
        // no mu callbacks needed here ? (doneAndPullData, SeqCstBuffer)
        Ok(())
    }

    pub async fn handle_rst_stream(&self, ctx: &RequestContext, flags: u8, req_id: i32, mut bin: Bytes) -> Result<(), CrankerRouterException> {
        let error_code = get_error_code(&mut bin);
        let error_message = get_error_message(&mut bin);
        self.notify_client_request_error(ctx, CrankerRouterException::new(format!(
            "stream closed by connector, error code: {}, error message: {}, req id = {} , router socket id = {} , route = {}",
            error_code, error_message, req_id, ctx.router_socket_id(), ctx.route()
        )));
        Ok(())
    }

    pub async fn handle_window_update(&self, ctx: &RequestContext, flags: u8, req_id: i32, mut bin: Bytes) -> Result<(), CrankerRouterException> {
        let window_update = bin.get_i32();
        ctx.target_connector_ack_bytes(window_update);
        Ok(())
    }


    fn do_send_header_to_cli(&self, ctx: &RequestContext, full_content: String) -> Result<(), CrankerRouterException> {
        ctx.should_have_response.store(true, SeqCst);
        if true { return Ok(()); }

        // FIXME: The following lines should move to the `async on_cli_req()` method of
        //  `impl RouterSocket for RouterSocketV3` block
        let cpr = CrankerProtocolResponse::try_from_string(full_content)?;
        let status_code = cpr.status;
        let res_builder = cpr.build()?;
        {
            if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
                for i in rs3.proxy_listeners.iter() {
                    i.on_before_responding_to_client(self.as_ref())?;
                    i.on_after_target_to_proxy_headers_received(
                        self.as_ref(), status_code, res_builder.headers_ref(),
                    )?;
                }
            } else {
                return Err(CrankerRouterException::new(format!(
                    "failed to invoke on before responding to client of proxy listeners because the router socket v3 no longer exists, req id = {} , router socket id = {} , route = {}",
                    ctx.request_id, ctx.router_socket_id(), ctx.route()
                )));
            }
        }

        // let wrapped_stream = ctx.tgt_res_bdy_rx.clone();
        // let stream_body = Body::from_stream(wrapped_stream);

        Ok(())
    }

    fn is_upper_rs3_removed(&self) -> bool {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.is_removed();
        }
        return true;
    }


    fn check_if_context_exists(&self, req_id: &i32) -> Result<Arc<RequestContext>, Result<(), CrankerRouterException>> {
        let opt_ctx = self.wss_recv_dispatcher.get(&req_id);
        if opt_ctx.is_none() {
            return Err(Err(CrankerRouterException::new(format!(
                "can not found ctx for req id={} in router_socket_id={}",
                req_id, self.router_socket_id
            ))));
        }
        let ctx = opt_ctx.unwrap().upgrade();
        if ctx.is_none() {
            return Err(Err(CrankerRouterException::new(format!(
                "found ctx for req id={} but seems already dropped in router_socket_id={}",
                req_id, self.router_socket_id
            ))));
        }
        let ctx = ctx.unwrap();
        Ok(ctx)
    }

    // pretty like on_close in RouterSocketV1
    async fn notify_client_request_close(&self, ctx: &RequestContext, ws_status_code: u16, opt_reason: Option<String>) -> Result<(), CrankerRouterException> {
        // TODO: make this upgrade a function call
        let rs3 = self.weak_outer_router_socket_v3.upgrade().ok_or(
            CrankerRouterException::new(format!(
                "failed to get upper router socket v3 arc when notify_client_request_close.req id = {} , router socket id = {} , route = {}",
                ctx.request_id, ctx.router_socket_id(), ctx.route()
            )))?;

        rs3.proxy_listeners.iter().for_each(|pl| {
            let _ = pl.on_response_body_chunk_received(ctx);
        });
        let code = ws_status_code;
        let reason = opt_reason.unwrap_or("closed by router".to_string());

        if ctx.should_have_response.load(SeqCst)
            && ctx.bytes_received().load(Acquire) == 0 {
            // since here nothing has been sent to cli yet
            // we can send a crex to the ctx.tgt_res_hdr_tx
            // and in case at this moment it's going to response,
            // also send a crex to ctx.tgt_res_bdy_tx (will be wrapped
            // into stream body)
            if code == 1011 {
                self.cli_fail_prior_to_tgt_res(
                    ctx,
                    Some("ws code 1011. should res to cli 502".to_string()),
                    Some(502),
                ).await;
            } else if code == 1008 {
                self.cli_fail_prior_to_tgt_res(
                    ctx,
                    "ws code 1008. should res to cli 400".to_string(),
                    Some(400),
                ).await;
            }
        }

        if !ctx.tgt_res_hdr_rx.is_closed() || !ctx.tgt_res_bdy_rx.is_closed() {
            if code == 1000 {
                // ALL GOOD
            } else {
                error!("closing client request early due to cranker wss connection close with status code={}, reason={}", code, reason);
                let ex = CrankerRouterException::new(format!(
                    "upstream server error: ws code={}, reason={}", code, reason
                ));
                // I think here same as asyncHandle.complete(exception) in mu cranker router
                // FIXME: It occurs that the client browser will hang if ex sent here
                // FIXME: 240528 what the heck is this in v1, it doesn't do anything
                //  but defines an not invoked future!!!1
                let _ = async { let _ = ctx.tgt_res_hdr_tx.send(Err(ex.clone())).await; };
                let _ = async { let _ = ctx.tgt_res_bdy_tx.send(Err(ex.clone())).await; };
            }
            ctx.tgt_res_hdr_rx.close();
            ctx.tgt_res_bdy_rx.close();
        }

        // TODO: total_err??? v1
        let may_ex = rs3.raise_completion_event(Some(ClientRequestIdentifier {
            request_id: ctx.request_id,
        }));
        if may_ex.is_err() {
            return may_ex;
        }
        Ok(())
    }

    async fn cli_fail_prior_to_tgt_res(
        &self,
        ctx: &RequestContext,
        opt_reason: Option<String>,
        opt_status_code_to_cli: Option<u16>,
    ) {
        let failed_reason = opt_reason.unwrap_or("unknown early failed reason".to_string());
        let failed_code = opt_status_code_to_cli.unwrap_or(500);
        // TODO: Make crex support define status code
        let ex = CrankerRouterException::new(failed_reason.to_string());
        let _ = ctx.tgt_res_hdr_tx.send(Err(ex.clone())).await;
        let _ = ctx.tgt_res_bdy_tx.send(Err(ex)).await;
    }

    fn notify_client_request_error(&self, ctx: &RequestContext, crex: CrankerRouterException) {
        // TODO: Here need to judge if the crex is Timeout Exception or not
        //  It would be good to define our own ErrorKind with `thiserror` crate ASAP!!!
        todo!()
    }
}


const _END_STREAM_FLAG_MASK: u8 = 0b00000001;
const _END_HEADER_FLAG_MASK: u8 = 0b00000100;

#[inline]
fn judge_is_stream_end_from_flags(flags: u8) -> bool {
    flags & _END_STREAM_FLAG_MASK == _END_STREAM_FLAG_MASK
}

#[inline]
fn judge_is_header_end_from_flags(flags: u8) -> bool {
    flags & _END_HEADER_FLAG_MASK == _END_HEADER_FLAG_MASK
}

fn window_update_message(req_id: i32, window_update: i32) -> Bytes {
    let mut bm = BytesMut::with_capacity(10);
    bm.put_u8(MESSAGE_TYPE_WINDOW_UPDATE);
    bm.put_u8(0);
    bm.put_i32(req_id);
    bm.put_i32(window_update); // FIXME
    bm.into()
}

fn get_error_code(bin: &mut Bytes) -> i32 {
    if bin.remaining() >= 4 { return bin.get_i32(); }
    return -1;
}

fn get_error_message(bin: &mut Bytes) -> String {
    // TODO: Simplify it
    if bin.remaining() > 0 {
        let msg = String::from_utf8(bin.into_vec()).ok();
        if let Some(msg) = msg {
            return msg;
        }
    }
    return String::new();
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
        // We may need a Notify / Channel here to tell this handler to `doneAndPullData` next chunk / ByteBuffer / Bytes
        // on websocket
        // Unlike Java, the binary_msg (bytebuffer) will be SeqCstd once out of scope, so no `SeqCstBuffer`
        // callback is not needed
        // It turns out that mu-cranker-router calls `doneAndPullData` immediately when one binary
        // is arrived.
        // The flow control happens in the direction where tgt->router->cli
        // this should be controlled and triggered in `while(body.next())` in `on_client_req` method


        // FIXME: We convert binary_msg to Bytes to make use of its APIs for convenience
        //  but it means some operations are not cost-free

        // From Rust API Guidelines - Naming (https://rust-lang.github.io/api-guidelines/naming.html)
        // as_	Free	borrowed -> borrowed
        // to_	Expensive	borrowed -> borrowed
        // into_	Variable	owned -> owned (non-Copy types)

        // TODO: Can we make here zero copy???
        // TODO Add remaining length check here otherwise it will panic
        let mut bin = Bytes::from(binary_msg); // This Bytes::from is likely to be cost-free
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

        let ctx = match self.check_if_context_exists(&req_id_int) {
            Ok(value) => value,
            Err(value) => return value,
        };

        match msg_type_byte {
            MESSAGE_TYPE_DATA => {
                self.handle_data(&ctx, flags_byte, req_id_int, bin)?
            }
            MESSAGE_TYPE_HEADER => {
                self.handle_header(&ctx, flags_byte, req_id_int, bin)?
            }
            MESSAGE_TYPE_RST_STREAM => {
                self.handle_rst_stream(&ctx, flags_byte /*can be ignored*/, req_id_int, bin)?
            }
            MESSAGE_TYPE_WINDOW_UPDATE => {
                self.handle_window_update(&ctx, flags_byte, req_id_int, bin)?
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

struct ShouldKeepReadFromCli {
    pub should_read_from_cli: AtomicBool,
    pub should_read_from_cli_notify: Notify,
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
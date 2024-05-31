use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};
use std::sync::atomic::Ordering::{Release, SeqCst};

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::extract::OriginalUri;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, Method, Response, StatusCode, Version};
use axum_core::body::{Body, BodyDataStream};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use futures::{TryStreamExt};
use futures::stream::{FusedStream, SplitSink, SplitStream};
use log::{debug, error, info, trace, warn};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use crate::{ACRState, CRANKER_V_3_0, time_utils};
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::exceptions::{CrankerRouterException, CrexKind};
use crate::http_utils::set_target_request_headers;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::router_socket::{ClientRequestIdentifier, create_request_line, pipe_and_queue_the_wss_send_task_and_handle_err_chan, pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary, RouteIdentify, RouterSocket};
use crate::router_socket_v3::StreamState::Open;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};
use crate::websocket_listener::WebSocketListener;

// u8
const _MSG_TYPE_LEN_IN_BYTES: usize = 1;
// u8
const _FLAGS_LEN_IN_BYTES: usize = 1;
// i32
const _REQ_ID_LEN_IN_BYTES: usize = 4;

const HEADER_LEN_IN_BYTES: usize =
    _MSG_TYPE_LEN_IN_BYTES + _FLAGS_LEN_IN_BYTES + _REQ_ID_LEN_IN_BYTES;

const _END_STREAM_FLAG_MASK: u8 = 0b00000001;

const _END_HEADER_FLAG_MASK: u8 = 0b00000100;

const MESSAGE_TYPE_DATA: u8 = 0;
const MESSAGE_TYPE_HEADER: u8 = 1;
const MESSAGE_TYPE_RST_STREAM: u8 = 3;
const MESSAGE_TYPE_WINDOW_UPDATE: u8 = 8;
const ERROR_INTERNAL: i32 = 1;

const REQ_CTX_INVALID_OUTER_RS3_MSG: &str = "(invalid router socket v3)";

pub(crate) struct RouterSocketV3 {
    pub weak_self: RwLock<Option<Weak<RouterSocketV3>>>,
    pub route: String,
    // pub domain:String,
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

    pub idle_read_timeout_ms: i64,
    pub ping_sent_after_no_write_for_ms: i64,

    // The only place to store strong Arc of RequestContext
    pub context_map: DashMap<i32, Arc<RequestContext>>,
    // idMaker
    pub req_id_generator: AtomicI32,

    // START WSS EXCHANGE PART
    // underlying_wss_tx: SplitSink<WebSocket, Message>,
    // underlying_wss_rx: SplitStream<WebSocket>,

    err_chan_tx: Sender<CrankerRouterException>,
    err_chan_rx: Receiver<CrankerRouterException>,

    wss_send_task_tx: Sender<Message>,
    // wss_send_task_rx: Receiver<Message>,
    // END OF WSS EXCHANGE PART

    wss_recv_pipe_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    wss_send_task_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl RouteIdentify for RouterSocketV3 {
    fn router_socket_id(&self) -> String {
        return self.router_socket_id.clone();
    }

    fn route(&self) -> String {
        return self.route.clone();
    }

    fn service_address(&self) -> SocketAddr {
        return self.remote_address.clone();
    }
}


impl RouterSocketV3 {
    // A wrapper for consolidating error handling
    async fn send_request_over_websocket_v3(
        self: Arc<Self>,
        app_state: ACRState,
        http_version: &Version,
        method: &Method,
        original_uri: &OriginalUri,
        headers: &HeaderMap,
        addr: &SocketAddr,
        opt_body: Option<BodyDataStream>,
        req_id: i32,
        cli_req_ident: Option<ClientRequestIdentifier>,
        ctx: Arc<RequestContext>,
    ) -> Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException> {
        // 1. Cli header processing (fast)
        let mut hdr_to_tgt = HeaderMap::new();
        set_target_request_headers(
            headers,
            &mut hdr_to_tgt,
            &app_state,
            http_version,
            addr,
            original_uri,
        );
        trace!("4");

        // 2. No text based request line is needed, Skip

        for i in self.proxy_listeners.iter() {
            i.on_before_proxy_to_target(ctx.as_ref(), &mut hdr_to_tgt)?;
        }

        let header_text = create_request_line(method, original_uri);
        trace!("5");

        // WARNING: Start from this line, we are dealing with network I/O
        // Be careful to handle errors. Notify client error ASAP.

        // 3. No text based end marker is needed,
        // but we need to decide the frame type and flag.
        // As always, header first, later body

        let is_stream_end = opt_body.is_none(); // if no body, stream should end right now
        let header_bytes = header_messages(req_id, true, is_stream_end, header_text);
        ctx.is_tgt_can_send_res_now_according_to_rfc2616.store(true, SeqCst);
        for hb in header_bytes {
            let payload_len = (hb.len() - HEADER_LEN_IN_BYTES) as i32;
            ctx.inc_rtr_to_tgt_pending_ack_bytes(payload_len);
            self.send_data(Message::Binary(hb.into())).await?;
            ctx.from_client_bytes.fetch_add(payload_len as i64, SeqCst);
        }
        trace!("6");

        for i in self.proxy_listeners.iter() {
            i.on_after_proxy_to_target_headers_sent(ctx.as_ref(), Some(headers))?;
            i.on_request_body_sent_to_target(ctx.as_ref())?;
        }
        trace!("7");

        if opt_body.is_some() {
            let mut body = opt_body
                .unwrap()
                .map_err(|e| CrankerRouterException::new(format!("{:?}", e)));
            while let Some(req_body_chunk) = body.next().await {
                let bytes = req_body_chunk?;
                let byte_len = bytes.len();
                let mut opt_copy = None;
                let need_to_copy_byte =
                    self.proxy_listeners.iter().all(|i| i.really_need_on_request_body_chunk_sent_to_target());
                if need_to_copy_byte {
                    opt_copy.replace(bytes.clone());
                }
                let data = data_messages(req_id, false, Some(bytes));
                ctx.inc_rtr_to_tgt_pending_ack_bytes((data.len() - HEADER_LEN_IN_BYTES) as i32);
                self.send_data(Message::Binary(data.to_vec())).await?;

                // Here need to deal with flow control
                ctx.from_client_bytes.fetch_add(byte_len as i64, SeqCst);

                if let Some(bytes_copy) = opt_copy {
                    for i in self.proxy_listeners.iter() {
                        i.on_request_body_chunk_sent_to_target(ctx.as_ref(), &bytes_copy)?;
                    }
                }

                // void flowControl
                if ctx.is_wss_writable.load(SeqCst) && !ctx.is_wss_writing.load(SeqCst) {
                    continue;
                } else {
                    let should_read_from_cli = Arc::new(AtomicBool::new(false));
                    let should_read_from_cli_notify = Arc::new(Notify::new());
                    ctx.should_keep_read_from_cli_tx
                        .send(Ok(ShouldKeepReadFromCli {
                            should_read_from_cli: should_read_from_cli.clone(),
                            should_read_from_cli_notify: should_read_from_cli_notify.clone(),
                        }))
                        .await
                        .map_err(|e| {
                            CrankerRouterException::new(format!(
                                "failed to send to should_keep_read_from_cli_tx: {:?}",
                                e
                            ))
                        })?;
                    // FIXME: Here will block, so I think the atomic bool in should read from cli
                    //  is unnecessary

                    // FIXME: Don't know A/B which one should go first
                    // A)
                    ctx.write_it_maybe();
                    // B)
                    should_read_from_cli_notify.notified().await;

                    if !should_read_from_cli.load(SeqCst) {
                        let failed_reason = format!(
                            "very rare and weird that the should_read_from_cli_notify is notified but \
                            should_read_from_cli is still false. route = {}, router socket id = {} , \
                            req id = {}",
                            self.route(), self.router_socket_id(), req_id
                        );
                        error!("{}", failed_reason);
                        let crex = CrankerRouterException::new(failed_reason);
                        return Err(crex);
                    }
                }
            }
        }

        trace!("8");

        // FIXME: The following lines should move to the `async on_cli_req()` method of
        //  `impl RouterSocket for RouterSocketV3` block
        let full_content = ctx.tgt_res_hdr_rx.recv().await.map_err(|e| {
            CrankerRouterException::new(format!(
                "failed to receive header from ctx: {:?}", e
            ))
        })??;
        trace!("9");

        trace!("10");

        let cpr = CrankerProtocolResponse::try_from_string(full_content)?;
        trace!("11");
        let status_code = cpr.status;
        let res_builder = cpr.build()?;
        trace!("12");

        for i in self.proxy_listeners.iter() {
            i.on_before_responding_to_client(ctx.as_ref())?;
            i.on_after_target_to_proxy_headers_received(
                ctx.as_ref(),
                status_code,
                res_builder.headers_ref(),
            )?;
        }
        trace!("13");
        // todo!();
        let wrapped_stream = ctx.tgt_res_bdy_rx.clone();
        let stream_body = Body::from_stream(wrapped_stream);
        trace!("14");
        res_builder
            .body(stream_body)
            .map(|body| {
                (body, cli_req_ident)
            })
            .map_err(|ie| CrankerRouterException::new(
                format!("failed to build body: {:?}", ie)
            ))
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

    fn raise_completion_event(&self, opt_cli_req_id: Option<ClientRequestIdentifier>) -> Result<(), CrankerRouterException> {
        if let Some(cli_req_ident) = opt_cli_req_id {
            let req_id = cli_req_ident.request_id;
            let mut opt_ctx_arc = None;
            if let Some(kv) = self.context_map.get(&req_id) {
                let ctx_arc = kv.value().clone();
                opt_ctx_arc.replace(ctx_arc);
            }
            if let Some(ctx_arc) = opt_ctx_arc {
                ctx_arc.duration_millis.store(
                    time_utils::current_time_millis() - ctx_arc.req_start_time,
                    Release,
                );
                for i in self.proxy_listeners.iter() {
                    if let Err(crex) = i.on_complete(ctx_arc.as_ref()) {
                        warn!("Error thrown by on_complete() : {:?}", crex);
                    }
                }
            }
        }
        Ok(())
    }

    fn inflight_count(&self) -> i32 {
        self.context_map.len() as i32
    }

    async fn on_client_req(
        self: Arc<Self>,
        app_state: ACRState,
        http_version: &Version,
        method: &Method,
        original_uri: &OriginalUri,
        headers: &HeaderMap,
        addr: &SocketAddr,
        opt_body: Option<BodyDataStream>,
    ) -> Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException>
    {
        trace!("1");
        // 0. if is removed then should not run into this method (fast)
        if self.is_removed() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a is_removed router socket. router_socket_id={}",
                &self.router_socket_id
            )));
        }
        trace!("2");
        let req_id = self.req_id_generator.fetch_add(1, SeqCst) + 1;
        let cli_req_ident = Some(ClientRequestIdentifier { request_id: req_id });
        let ctx = Arc::new(RequestContext::new(
            Arc::downgrade(&self),
            req_id,
            method.clone(),
            original_uri.clone(),
        ));
        let old_v = self.context_map.insert(req_id, ctx.clone());
        assert!(old_v.is_none());
        trace!("3");

        return match self.clone().send_request_over_websocket_v3(
            app_state,
            http_version,
            method,
            original_uri,
            headers,
            addr,
            opt_body,
            req_id,
            cli_req_ident,
            ctx.clone()
        ).await {
            Ok(res) => {
                Ok(res)
            }
            Err(crex) => {
                let _ = self
                    .notify_client_request_error(ctx.clone(), crex.clone())
                    .await;
                // TODO: Unwrap this `handle_on_cli_request_err`
                let _ = self.handle_on_cli_request_err(ctx.as_ref(), crex.clone());
                Err(crex)
            }
        }

    }

    async fn send_ws_msg_to_uwss(
        self: Arc<Self>,
        message: Message,
    ) -> Result<(), CrankerRouterException> {
        self.send_data(message).await
    }

    // onError
    async fn terminate_all_conn(
        self: Arc<Self>,
        opt_crex: Option<CrankerRouterException>,
    ) -> Result<(), CrankerRouterException>
    {
        let crex = opt_crex.unwrap_or(CrankerRouterException::new(
            "terminating all connections on cranker router v3 for unknown reason".to_string(),
        ));

        let _ = futures::future::join_all(
            self.context_map
                .iter()
                .map(|i| (self.clone(), i.value().clone(), crex.clone()))
                .map(|(self_arc, ctx_arc, crex_clone)| async move {
                    let _ = self_arc
                        .notify_client_request_error(ctx_arc, crex_clone)
                        .await;
                }),
        ).await;

        assert!(self.context_map.is_empty());

        if !self.context_map.is_empty() {
            error!("seems our dirty deadlock experiment failed! Manually retain all false");
            // TODO: Better method to remove all ?
            self.context_map.retain(|_, _| return false);
        }

        Ok(())
    }
    
    fn inc_bytes_received_from_cli(&self, _: i32) {
        // In V3 it offloads to RequestContext
        // Need to do nothing
    }

    fn try_provide_general_error(
        &self,
        opt_crex: Option<CrankerRouterException>,
    ) -> Result<(), CrankerRouterException> {
        if let Some(crex) = opt_crex {
            self.context_map.iter().for_each(|i| {
                let ctx = i.value();
                let _ = ctx.error.try_write().map(|mut s| s.replace(crex.clone()));
                // ignore failure
            })
        };
        Ok(())
    }

    fn get_opt_arc_websocket_farm(&self) -> Option<Arc<WebSocketFarm>> {
        self.web_socket_farm.upgrade()
    }
}

const WATER_MARK_HIGH: i32 = 64 * 1024;
const WATER_MARK_LOW: i32 = 16 * 1024;

pub(crate) struct RequestContext {
    // To mimic RouterSocketV3 in Java, we need a weak reference to
    //  the outer class
    pub weak_outer_router_socket_v3: Weak<RouterSocketV3>,

    /* wss tunnel related */
    // wssReceivedAckBytes
    pub wss_tgt_connector_ack_bytes: AtomicI32,
    // isWssSending
    pub wss_rtr_to_tgt_pending_ack_bytes: AtomicI32,
    // init true
    pub is_wss_writable: AtomicBool,
    // init false
    pub is_wss_writing: AtomicBool,
    // wssWriteCallbacks : ConcurrentLinkedQueue< > (), ???
    pub should_keep_read_from_cli_tx: Sender<Result<ShouldKeepReadFromCli, CrankerRouterException>>,
    pub should_keep_read_from_cli_rx:
    Receiver<Result<ShouldKeepReadFromCli, CrankerRouterException>>,
    pub wss_on_binary_call_count: AtomicI64,

    pub request_id: i32,
    // MuRequest request,
    // MuResponse response,
    // AsyncHandle asyncHandle,
    pub req_method: Method,
    pub req_uri: OriginalUri,
    pub req_start_time: i64,

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

    // use to notify there's a target response available
    // init false
    pub is_tgt_can_send_res_now_according_to_rfc2616: AtomicBool,
}

impl RequestContext {
    fn new(
        router_socket_v3: Weak<RouterSocketV3>,
        req_id: i32,
        req_method: Method,
        req_uri: OriginalUri,
    ) -> Self {
        let (should_keep_read_from_cli_tx, should_keep_read_from_cli_rx) =
            async_channel::unbounded();
        let (tgt_res_hdr_tx, tgt_res_hdr_rx) = async_channel::unbounded();
        let (tgt_res_bdy_tx, tgt_res_bdy_rx) = async_channel::unbounded();
        RequestContext {
            // To mimic RouterSocketV3 in Java, we need a weak reference to
            //  the outer class
            weak_outer_router_socket_v3: router_socket_v3,
            // wss tunnel
            wss_tgt_connector_ack_bytes: AtomicI32::new(0),
            wss_rtr_to_tgt_pending_ack_bytes: AtomicI32::new(0),
            // init true
            is_wss_writable: AtomicBool::new(true),
            // init false
            is_wss_writing: AtomicBool::new(false),
            // wssWriteCallbacks : ConcurrentLinkedQueue< > (), ???
            should_keep_read_from_cli_tx,
            should_keep_read_from_cli_rx,
            wss_on_binary_call_count: AtomicI64::new(0),

            request_id: req_id,
            // MuRequest request,
            // MuResponse response,
            // AsyncHandle asyncHandle,
            req_method,
            req_uri,
            req_start_time: time_utils::current_time_millis(),

            tgt_res_hdr_tx,
            tgt_res_hdr_rx,
            tgt_res_bdy_tx,
            tgt_res_bdy_rx,

            // client
            from_client_bytes: AtomicI64::new(0),
            to_client_bytes: AtomicI64::new(0),

            duration_millis: AtomicI64::new(0),
            error: RwLock::new(None),
            // init false
            is_rst_stream_sent: AtomicBool::new(false),
            // init OPEN
            state: RwLock::new(Open),
            header_line_builder: RwLock::new(String::new()),

            // use to notify there's a target response available
            is_tgt_can_send_res_now_according_to_rfc2616: AtomicBool::new(false),
        }
    }

    // void sendingBytes
    fn inc_rtr_to_tgt_pending_ack_bytes(self: &Self, pending: i32) {
        self.wss_rtr_to_tgt_pending_ack_bytes.fetch_add(pending, SeqCst);
        if self.wss_rtr_to_tgt_pending_ack_bytes.load(SeqCst) > WATER_MARK_HIGH {
            if let Ok(_) = self.is_wss_writable
                .compare_exchange(true, false, SeqCst, SeqCst) {
                error!("Should block reading from client!");
            }
        }
    }

    // void ackedBytes
    fn target_connector_ack_bytes(self: &Self, ack: i32) {
        self.wss_tgt_connector_ack_bytes.fetch_add(ack, SeqCst);
        self.wss_rtr_to_tgt_pending_ack_bytes
            .fetch_add(-ack, SeqCst);
        if self.wss_rtr_to_tgt_pending_ack_bytes.load(SeqCst) < WATER_MARK_LOW {
            if let Ok(_) = self
                .is_wss_writable
                .compare_exchange(false, true, SeqCst, SeqCst)
            {
                error!("Now can unblock reading from client");
                self.write_it_maybe();
            }
        }
    }

    fn write_it_maybe(self: &Self) {
        // if self.is_wss_writable.load(SeqCst) && !
        if self.is_wss_writable.load(SeqCst)
            && (!self.should_keep_read_from_cli_rx.is_empty()
            && !self.should_keep_read_from_cli_rx.is_closed()
            && !self.should_keep_read_from_cli_rx.is_terminated())
        {
            if let Ok(_) = self
                .is_wss_writing
                .compare_exchange(false, true, SeqCst, SeqCst)
            {
                while self.is_wss_writable.load(SeqCst)
                    && (!self.should_keep_read_from_cli_rx.is_empty()
                    && !self.should_keep_read_from_cli_rx.is_closed()
                    && !self.should_keep_read_from_cli_rx.is_terminated())
                {
                    if let Ok(cbrs) = self.should_keep_read_from_cli_rx.try_recv() {
                        match cbrs {
                            Ok(cb) => {
                                cb.should_read_from_cli.store(true, SeqCst);
                                // FIXME: We do two notifications here.
                                //  According to doc of Notify, `notify_one` will
                                //  provide one permit, and if there's no one waiting
                                //  the next waiter will be wakened immediately,
                                //  where `notify_all` needs to be called after a waiter
                                //  starts `notified().await`
                                //  In our case, in `on_client_req` method, I'm currently
                                //  not sure what's the best practice for the sequence
                                //  whether to `notified().await` first or `write_it_maybe().await`
                                //  first. One of the following should be removed once
                                //  the behaviour is confirmed.
                                cb.should_read_from_cli_notify.notify_one();
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

    fn request_id(&self) -> i32 {
        self.request_id
    }
}

impl RouteIdentify for RequestContext {
    fn router_socket_id(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.router_socket_id();
        }
        return REQ_CTX_INVALID_OUTER_RS3_MSG.to_string();
    }

    fn route(&self) -> String {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.route();
        }
        return REQ_CTX_INVALID_OUTER_RS3_MSG.to_string();
    }

    fn service_address(&self) -> SocketAddr {
        if let Some(rs3) = self.weak_outer_router_socket_v3.upgrade() {
            return rs3.service_address();
        }
        return SocketAddr::new([u8::MAX, u8::MAX, u8::MAX, u8::MAX].into(), u16::MAX);
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
        return REQ_CTX_INVALID_OUTER_RS3_MSG.to_string();
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
        return self
            .error
            .try_read()
            .ok()
            .and_then(|ok| ok.clone().map(|some| some.clone()));
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
            StreamState::Closed | StreamState::Error => true,
        }
    }
}

// WSS EXCHANGE PART
impl RouterSocketV3 {
    pub async fn send_data(&self, wss_msg: Message) -> Result<(), CrankerRouterException> {
        self.wss_send_task_tx.send(wss_msg).await.map_err(|e| {
            let failed_reason = format!(
                "failed to send_data: {:?}. route = {} , router socket id = {}",
                e, self.route, self.router_socket_id
            );
            error!("{}", failed_reason);
            CrankerRouterException::new(failed_reason)
        })
    }

    fn handle_on_cli_request_err(
        &self,
        ctx: &RequestContext,
        crex: CrankerRouterException,
    ) -> Result<(), CrankerRouterException> {
        let _ = self.reset_stream(ctx, 1001, "Going away".to_string());
        Err(crex)
    }

    pub async fn handle_data(
        &self,
        ctx: Arc<RequestContext>,
        flags: u8,
        req_id: i32,
        bin: Bytes,
    ) -> Result<(), CrankerRouterException> {
        self.check_if_tgt_can_send_res_first(&ctx, req_id).await?;
        let is_end_stream = judge_is_stream_end_from_flags(flags);
        let len = bin.remaining();
        let mut opt_copy = None;
        let really_need_on_response_body_chunk_received_from_target
            = self.proxy_listeners.iter().all(|i| { i.really_need_on_request_body_chunk_sent_to_target() });
        if really_need_on_response_body_chunk_received_from_target {
            opt_copy.replace(bin.clone());
        }
        if len == 0 {
            if is_end_stream {
                let _ = self
                    .notify_client_request_close(ctx, 1000 /*ws status code*/, None)
                    .await;
            }
            return Ok(());
        }

        ctx.wss_on_binary_call_count.fetch_add(1, SeqCst);
        if self.is_removed() {
            if is_end_stream {
                let _ = self
                    .notify_client_request_close(ctx, 1000 /*ws status code*/, None)
                    .await;
            }
            return Err(CrankerRouterException::new(format!(
                "recv bin msg from connector but router socket already removed. req_id={}, flags={}", req_id, flags
            )));
        }

        debug!(
            "route={}, router_socket_id={}, sending {} bytes to client",
            self.route, self.router_socket_id, len
        );
        // FIXME: bin.to_vec() is underlying copying the bin
        //  try to do zero-copy here!
        //  We should probably use `bin.into_vec()` here that seems cost-free
        let send_tgt_bdy_to_chan_res = ctx.tgt_res_bdy_tx.send(Ok(bin.to_vec())).await;
        let res = match send_tgt_bdy_to_chan_res {
            Ok(_) => {
                if is_end_stream {
                    let _ = self.notify_client_request_close(ctx.clone(), 1000, None).await;
                }
                ctx.to_client_bytes.fetch_add(len as i64, SeqCst);
                let win_update_msg = window_update_message(req_id, len as i32);
                let _ = self.send_data(Message::Binary(win_update_msg.to_vec())).await;
                Ok(())
            }
            Err(send_err) => {
                // TODO: in mu they handle error in the asyncHandle.write callback
                // where should we deal with this?
                info!(
                    "route = {}, router socket id = {} , could not write to client response \
                    (maybe the user closed their browser) so will cancel the request. ex: {:?}",
                    self.route(), self.router_socket_id() , send_err
                );
                let failed_reason = format!(
                    "rare ex failed to send bin to tgt_res_bdy chan: {:?}",
                    send_err
                );
                let crex = CrankerRouterException::new(failed_reason);
                let _ = self.try_provide_general_error(Some(crex.clone()));
                Err(crex)
            }
        };

        if really_need_on_response_body_chunk_received_from_target {
            let copy = opt_copy.unwrap();
            for i in self.proxy_listeners.iter() {
                if let Err(crex) = i.on_response_body_chunk_received_from_target(ctx.as_ref(), &copy) {
                    return Err(crex);
                }
            }
        }

        res
    }

    async fn check_if_tgt_can_send_res_first(
        &self,
        ctx: &Arc<RequestContext>,
        req_id: i32,
    ) -> Result<(), CrankerRouterException> {
        if !ctx.is_tgt_can_send_res_now_according_to_rfc2616.load(SeqCst) {
            let failed_reason = format!(
                "recv data/bin before handle cli req. route = {} , router socket id = {} , req id = {}",
                self.route(), self.router_socket_id(), req_id
            );
            error!("{}", failed_reason);
            let crex = CrankerRouterException::new(failed_reason);
            return Err(crex);
        }
        Ok(())
    }

    pub async fn handle_header(
        &self,
        ctx: Arc<RequestContext>,
        flags: u8,
        req_id: i32,
        bin: Bytes,
    ) -> Result<(), CrankerRouterException> {
        self.check_if_tgt_can_send_res_first(&ctx, req_id).await?;
        let is_stream_end = judge_is_stream_end_from_flags(flags);
        let is_header_end = judge_is_header_end_from_flags(flags);
        let byte_len: i32 = bin.remaining().try_into().map_err(|e| {
            CrankerRouterException::new(format!(
                "failed to handle remaining bin msg len, it's even larger than i32::MAX ? \
                ex : {:?} , req id = {} , router socket id = {} , route = {}",
                e, req_id, ctx.router_socket_id(), ctx.route())
            )
        })?;
        // FIXME: bin.to_vec() is underlying copying the bin
        //  try to do zero-copy here!
        let content = String::from_utf8(bin.to_vec())
            .map_err(|fu8e| {
                CrankerRouterException::new(format!(
                    "failed to convert binary to header text in utf8 : {:?}, req id = {} , router socket id = {} , route = {}",
                    fu8e, req_id, ctx.router_socket_id(), ctx.route()
                ))
            })?;

        {
            let mut hlb = ctx.header_line_builder.write().await;
            hlb.push_str(content.as_str());
        }
        if is_header_end {
            let full_content = ctx.header_line_builder.read().await.clone();
            ctx.tgt_res_hdr_tx.send(
                Ok(full_content)
            ).await.map_err(|e| {
                CrankerRouterException::new(format!("failed to send header text : {:?}", e))
            })?;
        }
        if is_stream_end {
            let _ = self.notify_client_request_close(ctx, 1000, None).await; // TODO: What does 1000 mean?
        }
        let win_update_msg = window_update_message(req_id, byte_len);
        self.send_data(Message::Binary(win_update_msg.to_vec()))
            .await
        // no mu callbacks needed here ? (doneAndPullData, SeqCstBuffer)
    }

    pub async fn handle_rst_stream(
        &self,
        ctx_arc: Arc<RequestContext>,
        _: u8,
        req_id: i32,
        mut bin: Bytes,
    ) -> Result<(), CrankerRouterException> {
        let error_code = get_error_code(&mut bin);
        let error_message = get_error_message(&mut bin);
        self.notify_client_request_error(ctx_arc.clone(), CrankerRouterException::new(format!(
            "stream closed by connector, error code: {}, error message: {}, req id = {} , router socket id = {} , route = {}",
            error_code, error_message, req_id, ctx_arc.router_socket_id(), ctx_arc.route()
        ))).await;
        Ok(())
    }

    pub async fn handle_window_update(
        &self,
        ctx_arc: Arc<RequestContext>,
        _: u8,
        _: i32,
        mut bin: Bytes,
    ) -> Result<(), CrankerRouterException> {
        let ctx = ctx_arc.as_ref();
        let window_update = bin.get_i32();
        ctx.target_connector_ack_bytes(window_update);
        Ok(())
    }


    fn check_if_context_exists(
        &self,
        req_id: &i32,
    ) -> Result<Arc<RequestContext>, CrankerRouterException> {
        let opt_ctx = self.context_map.get(&req_id);
        if opt_ctx.is_none() {
            return Err(CrankerRouterException::new(format!(
                "can not found ctx for req id={} in router_socket_id={}. route = {}",
                req_id,
                self.router_socket_id(),
                self.route()
            )));
        }
        let ctx = opt_ctx.unwrap().value().clone();
        Ok(ctx)
    }

    // pretty like on_close in RouterSocketV1
    async fn notify_client_request_close(
        &self,
        ctx_arc: Arc<RequestContext>,
        ws_status_code: u16,
        opt_reason: Option<String>,
    ) -> Result<(), CrankerRouterException> {
        let ctx = ctx_arc.as_ref();
        self.proxy_listeners.iter().for_each(|pl| {
            let _ = pl.on_response_body_chunk_received(ctx);
        });
        let code = ws_status_code;
        let reason = opt_reason.unwrap_or("closed by router".to_string());

        if Self::is_cli_req_not_start_to_send_to_tgt_yet(ctx) {
            // since here nothing has been sent to cli yet
            // we can send a crex to the ctx.tgt_res_hdr_tx
            // and in case at this moment it's going to response,
            // also send a crex to ctx.tgt_res_bdy_tx (will be wrapped
            // into stream body)
            if code == 1011 {
                self.cli_fail_prior_to_tgt_res(
                    ctx, Some("ws code 1011".to_string()), Some(StatusCode::BAD_GATEWAY.as_u16())
                ).await;
            } else if code == 1008 {
                self.cli_fail_prior_to_tgt_res(
                    ctx, Some("ws code 1008".to_string()), Some(StatusCode::BAD_REQUEST.as_u16())
                ).await;
            }
        }

        if !ctx.tgt_res_hdr_rx.is_closed() || !ctx.tgt_res_bdy_rx.is_closed() {
            if code == 1000 {
                // ALL GOOD
            } else {
                error!("closing client request early due to cranker wss connection close with status code={}, reason={}", code, reason);
                let ex = CrankerRouterException::new(format!(
                    "upstream server error: ws code={}, reason={}",
                    code, reason
                ));
                // I think here same as asyncHandle.complete(exception) in mu cranker router
                // FIXME: It occurs that the client browser will hang if ex sent here
                // FIXME: 240528 what the heck is this in v1, it doesn't do anything
                //  but defines an not invoked future!!!1
                let _ = async {
                    let _ = ctx.tgt_res_hdr_tx.send(Err(ex.clone())).await;
                };
                let _ = async {
                    let _ = ctx.tgt_res_bdy_tx.send(Err(ex.clone())).await;
                };
            }
            ctx.tgt_res_hdr_rx.close();
            ctx.tgt_res_bdy_rx.close();
        }

        let may_ex = self.raise_completion_event(Some(ClientRequestIdentifier {
            request_id: ctx.request_id(),
        }));
        self.context_map.remove(&ctx.request_id());
        return match may_ex {
            Ok(_) => Ok(()),
            Err(crex) => {
                let _ = ctx.error.try_write().map(|mut g| {
                    g.replace(crex.clone());
                });
                Err(crex)
            }
        };
    }

    // Including cli req header
    fn is_cli_req_not_start_to_send_to_tgt_yet(ctx: &RequestContext) -> bool {
        // NOTE:
        // is_tgt_can_send_res_now_according_to_rfc2616 is set true before
        // the actual first byte of req header being sent to target, but
        // it's almost at the same time, so it's being used for the this
        // function
        ctx.is_tgt_can_send_res_now_according_to_rfc2616.load(SeqCst) && ctx.bytes_received() == 0
    }

    async fn cli_fail_prior_to_tgt_res(
        &self,
        ctx: &RequestContext,
        opt_reason: Option<String>,
        opt_status_code_to_cli: Option<u16>,
    ) {
        let failed_reason = opt_reason.unwrap_or("unknown early failed reason".to_string());
        let failed_code = opt_status_code_to_cli.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
        // TODO: Make crex support define status code
        let ex =
            CrankerRouterException::new(failed_reason.to_string()).with_status_code(failed_code);
        let _ = ctx.tgt_res_hdr_tx.send(Err(ex.clone())).await;
        let _ = ctx.tgt_res_bdy_tx.send(Err(ex)).await;
    }

    async fn notify_client_request_error(
        &self,
        ctx_arc: Arc<RequestContext>,
        crex: CrankerRouterException,
    ) {
        let ctx = ctx_arc.as_ref();
        let _ = ctx.error.try_write().map(|mut g| g.replace(crex.clone()));
        if crex.clone().opt_err_kind.is_some_and(|ck| {
            #[allow(unreachable_patterns)]
            match ck {
                CrexKind::Timeout_0001 => true, // FIXME: Where this timeout exception is from in mu?
                _ => false,
            }
        }) {
            if Self::is_cli_req_not_start_to_send_to_tgt_yet(ctx) {
                let _ = ctx.tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = ctx.tgt_res_bdy_tx.send(Err(crex.clone())).await;
            } else {
                let failed_reason = "closing cli req early due to timeout";
                error!(
                    "{} , req id = {} , router socket id = {} , route = {}",
                    failed_reason,
                    ctx.request_id(),
                    ctx.router_socket_id(),
                    ctx.route()
                );
                let crex = crex
                    .clone()
                    .with_status_code(StatusCode::GATEWAY_TIMEOUT.as_u16())
                    .append_str(failed_reason)
                    .prepend_str("504 Gateway Timeout");
                let _ = ctx.tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = ctx.tgt_res_bdy_tx.send(Err(crex.clone())).await;
            }
        } else {
            if Self::is_cli_req_not_start_to_send_to_tgt_yet(ctx) {
                let crex = crex
                    .clone()
                    .with_status_code(StatusCode::BAD_GATEWAY.as_u16())
                    .prepend_str("502 Bad Gateway");
                let _ = ctx.tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = ctx.tgt_res_bdy_tx.send(Err(crex.clone())).await;
            } else {
                let res_str = "closing cli req early due to cranker wss conn err";
                let failed_reason = format!("{} {}", res_str, crex.reason);
                error!(
                    "{} , req id = {} , router socket id = {} , route = {}",
                    failed_reason,
                    ctx.request_id(),
                    ctx.router_socket_id(),
                    ctx.route()
                );
                let crex = crex.clone().append_str(res_str);
                let _ = ctx.tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = ctx.tgt_res_bdy_tx.send(Err(crex.clone())).await;
            }
        }
        let _ = self.raise_completion_event(Some(ClientRequestIdentifier {
            request_id: ctx.request_id(),
        }));
        self.context_map.remove(&ctx.request_id());
        warn!(
            "stream error: req id = {} , router socket id = {} , route = {}, target = {} {:?}, ex = {}",
            ctx.request_id(), ctx.router_socket_id(), ctx.route(), ctx.req_method, ctx.req_uri, crex
        );
    }
}

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
    bm.put_i32(window_update);
    bm.into()
}

fn rst_message(req_id: i32, err_code: i32, msg: String) -> Bytes {
    let mut bm = BytesMut::new();
    bm.put_u8(MESSAGE_TYPE_RST_STREAM);
    bm.put_u8(0); // No flag
    bm.put_i32(req_id);
    bm.put_i32(err_code);
    bm.put(msg.as_str().as_bytes());
    // FIXME: Use into() to convert Bytes to Vec<u8> for free. Currently Bytes.to_vec() are mostly
    //  being used.
    bm.into()
}

fn data_messages(req_id: i32, is_end: bool, opt_bin: Option<Bytes>) -> Bytes {
    let mut bm = BytesMut::new();
    bm.put_u8(MESSAGE_TYPE_DATA);
    bm.put_u8({
        if is_end {
            1u8
        } else {
            0u8
        }
    });
    bm.put_i32(req_id);
    if let Some(bin) = opt_bin {
        bm.put(bin);
    }
    bm.into()
}

fn header_messages(
    req_id: i32,
    is_header_end: bool,
    is_stream_end: bool,
    full_header_line: String,
) -> Vec<Bytes> {
    // TODO: Make it configurable?
    // FIXME: Is this a String char length or byte length?
    let chunk_size = 16000;
    let fhl_u8 = full_header_line.as_bytes();
    if fhl_u8.len() < chunk_size {
        return vec![header_message(
            req_id,
            is_header_end,
            is_stream_end,
            full_header_line,
        )];
    }

    // FIXME: In mu, it operates in String level, that each character won't be divided into
    //  multiple bytes (in case non-ASCII chars in header, which according to RFC will not
    //  happen)
    //  Here we simulate the mu approach. Getting and splitting String is expensive in Rust.
    let char_vec = full_header_line.chars().collect::<Vec<char>>();
    let mut chunks = char_vec.chunks(chunk_size);
    let mut res = Vec::new();
    let chunk_count = chunks.len();
    let mut i = 0;
    while i < chunk_count {
        let slice = chunks.next().unwrap();
        let part = slice.iter().collect();
        let is_last = i == chunk_count - 1;
        res.push(header_message(req_id, is_last, is_stream_end, part));
        i += 1;
    }
    res
}

fn header_message(
    req_id: i32,
    is_header_end: bool,
    is_stream_end: bool,
    may_partial_header_line: String,
) -> Bytes {
    let mut flags: u8 = 0;
    if is_stream_end {
        flags |= _END_STREAM_FLAG_MASK;
    }
    if is_header_end {
        flags |= _END_HEADER_FLAG_MASK;
    }
    let bytes = may_partial_header_line.as_bytes();
    let mut bm = BytesMut::new();
    bm.put_u8(MESSAGE_TYPE_HEADER);
    bm.put_u8(flags);
    bm.put_i32(req_id);
    bm.put(bytes);
    bm.into()
}

fn get_error_code(bin: &mut Bytes) -> i32 {
    if bin.remaining() >= 4 {
        return bin.get_i32();
    }
    return -1;
}

fn get_error_message(bin: &mut Bytes) -> String {
    // TODO: Simplify it
    if bin.remaining() > 0 {
        let msg = String::from_utf8(bin.to_vec()).ok();
        if let Some(msg) = msg {
            return msg;
        }
    }
    return String::new();
}

#[async_trait]
impl WebSocketListener for RouterSocketV3 {
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException> {
        // WARNING: Here must return Ok(()).
        // We can not return Error here, otherwise the websocket listener `on_error`
        // will be trigger and error sent to err chan and all connection on the same
        // websocket will all be terminated
        let failed_reason = format!(
            "v3 should not send txt msg bug got: {:?}. route = {} , router socket id = {}",
            text_msg,
            self.route(),
            self.router_socket_id()
        );
        error!("{}", failed_reason);
        Ok(())
    }

    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        debug!("v3 on binary: {}", binary_msg.len());

        // WARNING: Here must return Ok(()).
        // We can not return Error here, otherwise the websocket listener `on_error`
        // will be trigger and error sent to err chan and all connection on the same
        // websocket will all be terminated

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
            error!(
                "{:?}",
                CrankerRouterException::new(
                    "recv bin msg len less than 1 to read msg type byte".to_string()
                )
            );
            return Ok(());
        }
        let msg_type_byte = bin.get_u8();
        if bin.remaining() < _FLAGS_LEN_IN_BYTES {
            error!(
                "{:?}",
                CrankerRouterException::new(
                    "recv bin msg len less than 2 to read flags byte".to_string()
                )
            );
            return Ok(());
        }
        let flags_byte = bin.get_u8();
        if bin.remaining() < _REQ_ID_LEN_IN_BYTES {
            error!(
                "{:?}",
                CrankerRouterException::new(
                    "recv bin msg len less than 6 to read request id".to_string()
                )
            );
            return Ok(());
        }
        let req_id_int = bin.get_i32(); // big endian

        let ctx = match self.check_if_context_exists(&req_id_int) {
            Ok(ctx_arc) => ctx_arc,
            Err(err) => {
                error!("{:?}", err);
                return Ok(());
            }
        };

        let mut bin_handle_res = Ok(());
        let ctx_for_err = ctx.clone();
        match msg_type_byte {
            MESSAGE_TYPE_DATA => {
                bin_handle_res = self.handle_data(ctx, flags_byte, req_id_int, bin).await;
            }
            MESSAGE_TYPE_HEADER => {
                bin_handle_res = self.handle_header(ctx, flags_byte, req_id_int, bin).await;
            }
            MESSAGE_TYPE_RST_STREAM => {
                bin_handle_res = self
                    .handle_rst_stream(ctx, flags_byte /*can be ignored*/, req_id_int, bin)
                    .await;
            }
            MESSAGE_TYPE_WINDOW_UPDATE => {
                bin_handle_res = self
                    .handle_window_update(ctx, flags_byte, req_id_int, bin)
                    .await;
            }
            _ => {
                // TODO: Should we on_error here?
                let failed_reason = format!(
                    "Received unknown type: {}. router_socket_id={}",
                    msg_type_byte, self.router_socket_id
                );
                // return Err(CrankerRouterException::new(failed_reason.clone()));
                error!("{}", failed_reason);
            }
        }
        if let Err(crex) = bin_handle_res {
            error!("err when handling binary: {:?}", crex);
            let _ = self
                .notify_client_request_error(ctx_for_err.clone(), crex.clone())
                .await;
            let _ = self.handle_on_cli_request_err(ctx_for_err.as_ref(), crex.clone());
        }

        Ok(())
    }

    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        // FIXME: Is pong not being sent critical?
        //  Here we decide to terminate all conn (on_error) once the pong
        //  cannot even be put into the wss send task channel / queue
        if let Err(e) = self.send_data(Message::Pong(ping_msg)).await {
            return self.on_error(CrankerRouterException::new(format!(
                "failed to pong back {:?}",
                e
            )));
        }
        Ok(())
    }

    async fn on_close(
        &self,
        close_msg: Option<CloseFrame<'static>>,
    ) -> Result<(), CrankerRouterException> {
        warn!(
            "uwss get close frame: {:?}. router socket id = {}",
            close_msg, self.router_socket_id
        );
        let mut code: u16 = 4000; // reserved
        let mut reason: Option<String> = None;
        if !self.is_removed() {
            self.web_socket_farm.upgrade().map(|wsf| {
                wsf.remove_router_socket_in_background(
                    self.route(),
                    self.router_socket_id(),
                    self.get_is_removed_arc_atomic_bool(),
                );
            });
            self.is_removed.store(true, SeqCst);
        }
        if let Some(clo) = close_msg {
            let clo_code = clo.code;
            let clo_reason = clo.reason.to_string();
            code = clo_code;
            reason = Some(clo_reason.clone());
            if clo_code != 1000 {
                warn!(
                    "websocket exceptional closed from client: status code = {} , reason = {}",
                    clo_code, clo_reason
                );
            }
        }
        for i in self.context_map.iter() {
            let ctx_arc = i.value().clone();
            let _ = self.notify_client_request_close(ctx_arc, code, reason.clone());
        }
        Ok(())
    }

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        self.err_chan_tx
            .send_blocking(err)
            .map(|ok| {
                ok
            })
            .map_err(|se| {
                CrankerRouterException::new(format!(
                    "rare ex that failed to send error to err chan: {:?}",
                    se
                ))
            })
    }

    fn get_idle_read_timeout_ms(&self) -> i64 {
        self.idle_read_timeout_ms
    }

    fn get_ping_sent_after_no_write_for_ms(&self) -> i64 {
        self.ping_sent_after_no_write_for_ms
    }
}

impl RouterSocketV3 {
    #[allow(unused_variables)]
    pub fn new_arc(
        route: String,
        // TODO: Reserved for future enhancement,
        //  currently domain is not in actual use even in mu-cranker-router
        domain: String,
        component_name: String,
        router_socket_id: String,
        web_socket_farm: Weak<WebSocketFarm>,
        connector_id: String,
        remote_address: SocketAddr,
        underlying_wss_tx: SplitSink<WebSocket, Message>,
        underlying_wss_rx: SplitStream<WebSocket>,
        proxy_listeners: Vec<Arc<dyn ProxyListener>>,
        discard_client_forwarded_headers: bool,
        send_legacy_forwarded_headers: bool,
        via_value: String,
        idle_read_timeout_ms: i64,
        ping_sent_after_no_write_for_ms: i64,
    ) -> Arc<Self> {
        let (err_chan_tx, err_chan_rx) = async_channel::unbounded();
        let (wss_send_task_tx, wss_send_task_rx) = async_channel::unbounded();
        let arc_rs = Arc::new(Self {
            weak_self: RwLock::new(None),
            route: route.clone(),
            component_name,
            router_socket_id: router_socket_id.clone(),
            web_socket_farm,
            connector_id,
            proxy_listeners,
            discard_client_forwarded_headers,
            send_legacy_forwarded_headers,
            via_value,
            // private Runnable onReadyForAction;
            remote_address,

            is_removed: Arc::new(AtomicBool::new(false)),

            idle_read_timeout_ms,
            ping_sent_after_no_write_for_ms,

            // The only place to store strong Arc of RequestContext
            context_map: DashMap::new(),
            // idMaker
            req_id_generator: AtomicI32::new(0),

            // START WSS EXCHANGE PART
            // underlying_wss_tx,
            // underlying_wss_rx,

            err_chan_tx,
            err_chan_rx: err_chan_rx.clone(),

            wss_send_task_tx,
            // wss_send_task_rx,
            // END WSS EXCHANGE PART

            wss_recv_pipe_join_handle: Arc::new(Mutex::new(None)),
            wss_send_task_join_handle: Arc::new(Mutex::new(None)),
        });
        let weak_self = Arc::downgrade(&arc_rs);
        let _ = arc_rs.weak_self.try_write().map(|mut g|{
            g.replace(weak_self);
        });
        // Abort these two handles in Drop
        let wss_recv_pipe_join_handle = tokio::spawn(
            pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
                arc_rs.clone(),
                arc_rs.clone(),
                underlying_wss_rx,
            ));
        let wss_send_task_join_handle = tokio::spawn(
            pipe_and_queue_the_wss_send_task_and_handle_err_chan(
                arc_rs.clone(),
                arc_rs.clone(),
                underlying_wss_tx, wss_send_task_rx, err_chan_rx,
            ));

        let arc_rs_clone = arc_rs.clone();
        tokio::spawn(async move {
            let mut g = arc_rs_clone.wss_recv_pipe_join_handle.lock().await;
            g.replace(wss_recv_pipe_join_handle);
            let mut g = arc_rs_clone.wss_send_task_join_handle.lock().await;
            g.replace(wss_send_task_join_handle);
        });

        arc_rs
    }

    pub async fn reset_stream(&self, ctx: &RequestContext, err_code: i32, msg: String) {
        if !ctx.state.read().await.is_completed() && !ctx.is_rst_stream_sent.load(SeqCst) {
            let rst_msg = rst_message(ctx.request_id(), err_code, msg);
            let _ = self.send_data(Message::Binary(rst_msg.to_vec()));
            ctx.is_rst_stream_sent.store(true, SeqCst)
        }

        self.context_map.remove(&ctx.request_id);
    }
}

impl Drop for RouterSocketV3 {
    fn drop(&mut self) {
        trace!("70v3: dropping :{}", self.router_socket_id);
        self.weak_self
            .try_write()
            .ok()
            .and_then(|mut g| g.replace(Weak::new()))
            .and_then(|weak_rs3| weak_rs3.upgrade())
            .map(|strong_rs3| {
                tokio::spawn(async move {
                    let _ = strong_rs3.terminate_all_conn(None);
                });
            });
        trace!("71v3");
        self.wss_send_task_tx.close();
        let wrpjh = self.wss_recv_pipe_join_handle.clone();
        let wstjh = self.wss_send_task_join_handle.clone();
        tokio::spawn(async move {
            wrpjh.lock().await.as_ref()
                .map(|o| o.abort());
            wstjh.lock().await.as_ref()
                .map(|o| o.abort());
        });
        trace!("72v3");
    }
}

pub(crate) struct ShouldKeepReadFromCli {
    pub should_read_from_cli: Arc<AtomicBool>,
    pub should_read_from_cli_notify: Arc<Notify>,
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use dashmap::DashMap;
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

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

    #[test]
    pub fn test_from_u8_slice_to_socket_addr() {
        let into = SocketAddr::new([u8::MAX, u8::MAX, u8::MAX, u8::MAX].into(), u16::MAX);
        let direct = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(u8::MAX, u8::MAX, u8::MAX, u8::MAX)),
            u16::MAX,
        );
        assert_eq!(into, direct)
    }

    #[tokio::test]
    pub async fn test_dashmap_dead_lock_will_pass_but_ugly() {
        struct ForTest {
            notify: Notify,
        }
        let dm: Arc<DashMap<i32, Arc<ForTest>>> = Arc::new(DashMap::new());
        dm.insert(
            1024,
            Arc::new(ForTest {
                notify: Notify::new(),
            }),
        );

        let _ = futures::future::join_all(dm.iter().map(|i| (dm.clone(), i.key().clone())).map(
            |(dmc, k)| async move {
                dmc.remove(&k);
            },
        ))
            .await;
        assert!(dm.is_empty())
    }

    #[tokio::test]
    pub async fn test_dashmap_dead_lock_will_pass() {
        struct ForTest {
            notify: Notify,
        }
        let dm: Arc<DashMap<i32, Arc<ForTest>>> = Arc::new(DashMap::new());
        dm.insert(
            1024,
            Arc::new(ForTest {
                notify: Notify::new(),
            }),
        );
        let mut join: Option<JoinHandle<()>> = None;
        for i in dm.iter() {
            let k = i.key().clone();
            let v = i.value();
            // Will deadlock and hang here
            let dm = dm.clone();
            let ts = tokio::spawn(async move {
                dm.remove(&k);
            });
            join.replace(ts);
        }
        let _ = join.unwrap().await;
        assert!(dm.is_empty())
    }

    #[test]
    pub fn substring_test() {
        let ascii_string = "This is a normal sentence.";
        let non_ascii_string = "Ｔｈｉｓ　ｉｓ　ａ　ｓｅｎｔｅｎｃｅ　ｗｉｔｈ　ｆｕｌｌ　ｗｉｄｔｈ　ｃｈａｒａｃｔｅｒｓ．";
        // char is 4 bytes long and can represent any utf8 character
        let char_vec = non_ascii_string.chars().collect::<Vec<char>>();
        let ascii_slice_ok = &ascii_string[0..4];
        for i in char_vec.iter() {
            eprintln!("{}", i)
        }
        eprintln!("{:?}", &char_vec[0..8]);
        // In this way, we can change from String->Vec<Char>->slice of char (not byte/u8)->back to String
        eprintln!("{}", &char_vec[0..8].iter().collect::<String>());
        eprintln!("{}", ascii_slice_ok)
    }
}

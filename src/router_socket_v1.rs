use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Weak};

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method, Response, StatusCode, Version};
use axum_core::body::{Body, BodyDataStream};
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{StreamExt, TryStreamExt};
use log::{error, trace};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;

use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::dark_host::DarkHost;
use crate::exceptions::{CrankerRouterException, CrexKind};
use crate::http_utils::set_target_request_headers;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::route_identify::RouteIdentify;
use crate::router_socket::{create_request_line, pipe_and_queue_the_wss_send_task_and_handle_err_chan, pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary, wrap_async_stream_with_guard, ClientRequestIdentifier, RouterSocket, HEADER_MAX_SIZE};
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};
use crate::websocket_listener::WebSocketListener;
use crate::{exceptions, time_utils, ACRState, CRANKER_V_1_0};

pub(crate) struct RouterSocketV1 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    pub websocket_farm: Weak<WebSocketFarm>,
    pub connector_id: String,
    pub proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    pub remote_address: SocketAddr,
    pub is_removed: Arc<AtomicBool>,
    // To indicate potential out of order (e.g. binary (body) comes prior to text (header) , rarely possible)
    pub is_tgt_can_send_res_now_according_to_rfc2616: Arc<AtomicBool>,
    // from cli
    pub bytes_received: Arc<AtomicI64>,
    // to cli
    pub bytes_sent: Arc<AtomicI64>,
    // but axum receive websocket in Message level. Let's treat it as frame now
    pub binary_frame_received: AtomicI64,
    pub socket_wait_in_millis: AtomicI64,
    pub error: RwLock<Option<CrankerRouterException>>,
    pub client_req_start_ts: Arc<AtomicI64>,
    pub duration_millis: Arc<AtomicI64>,
    pub idle_read_timeout_ms: i64,
    pub ping_sent_after_no_write_for_ms: i64,
    /**** below should be private for inner routine ****/
    // Once error, call on_error and will send here (blocked)
    err_chan_tx: Sender<CrankerRouterException>,
    cli_side_res_sender: RSv1ClientSideResponseSender,
    // Send any exception immediately to this chan in on_error so that when
    // the body not sent to client yet, we can response error to client early
    tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    tgt_res_hdr_rx: Receiver<Result<String, CrankerRouterException>>,
    // Send any exception immediately to this chan in on_error so that even
    // when body is streaming, we can end the stream early to save resource
    tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    tgt_res_bdy_rx: Receiver<Result<Vec<u8>, CrankerRouterException>>,

    wss_recv_pipe_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    really_need_on_response_body_chunk_received_from_target: bool,
    really_need_on_request_body_chunk_sent_to_target: bool,

    is_close_msg_received: AtomicBool
}

impl RouterSocketV1 {
    pub fn new_arc(route: String,
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
        let (tgt_res_hdr_tx, tgt_res_hdr_rx) = async_channel::unbounded();
        let (tgt_res_bdy_tx, tgt_res_bdy_rx) = async_channel::unbounded();
        let (wss_send_task_tx, wss_send_task_rx) = async_channel::unbounded();
        let (err_chan_tx, err_chan_rx) = async_channel::unbounded();

        let bytes_received = Arc::new(AtomicI64::new(0));
        let bytes_sent = Arc::new(AtomicI64::new(0));
        let client_req_start_ts = Arc::new(AtomicI64::new(0));
        let duration_millis = Arc::new(AtomicI64::new(-time_utils::current_time_millis()));

        let is_removed = Arc::new(AtomicBool::new(false));

        let is_tgt_can_send_res_now_according_to_rfc2616 = Arc::new(AtomicBool::new(false));

        let really_need_on_response_body_chunk_received_from_target
            = proxy_listeners.iter().any(|i| i.really_need_on_response_body_chunk_received_from_target());
        let really_need_on_request_body_chunk_sent_to_target
            = proxy_listeners.iter().any(|i| i.really_need_on_request_body_chunk_sent_to_target());
        let arc_rs = Arc::new(Self {
            route,
            component_name,
            router_socket_id,
            websocket_farm,
            connector_id: connector_instance_id,
            proxy_listeners,
            remote_address,

            is_removed: is_removed.clone(),
            is_tgt_can_send_res_now_according_to_rfc2616: is_tgt_can_send_res_now_according_to_rfc2616.clone(),
            bytes_received: bytes_received.clone(),
            bytes_sent: bytes_sent.clone(),
            binary_frame_received: AtomicI64::new(0),
            socket_wait_in_millis: AtomicI64::new(-time_utils::current_time_millis()),
            error: RwLock::new(None),
            client_req_start_ts,
            duration_millis,
            idle_read_timeout_ms,
            ping_sent_after_no_write_for_ms,

            err_chan_tx,

            cli_side_res_sender: RSv1ClientSideResponseSender::new(
                bytes_sent,
                tgt_res_hdr_tx.clone(),
                tgt_res_bdy_tx.clone(),
            ),

            tgt_res_hdr_tx,
            tgt_res_hdr_rx,
            tgt_res_bdy_tx,
            tgt_res_bdy_rx,

            wss_recv_pipe_join_handle: Arc::new(Mutex::new(None)),

            wss_send_task_tx,
            wss_send_task_join_handle: Arc::new(Mutex::new(None)),

            really_need_on_response_body_chunk_received_from_target,
            really_need_on_request_body_chunk_sent_to_target,

            is_close_msg_received: AtomicBool::new(false),
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

        // Don't want to let registration handler wait so spawn these val assign in another task/thread
        let arc_rs_clone = arc_rs.clone();
        tokio::spawn(async move {
            let mut g = arc_rs_clone.wss_recv_pipe_join_handle.lock().await;
            g.replace(wss_recv_pipe_join_handle);
            let mut g = arc_rs_clone.wss_send_task_join_handle.lock().await;
            g.replace(wss_send_task_join_handle);
        });

        arc_rs
    }

    async fn split_part_one_send_or_err(
        self: Arc<Self>, opt_body: Option<BodyDataStream>, cranker_req: String,
    ) -> Result<(), CrankerRouterException> {
        // 4. Send protocol header frame (slow, blocking)
        trace!("4");
        // In theory, the target can respond once the client starts to send the request
        // Ref:
        //   check the golang http library issue:
        //   1) https://github.com/golang/go/issues/57786
        //   2) https://github.com/golang/go/issues/15527#issuecomment-218521262
        //   3) https://lists.w3.org/Archives/Public/ietf-http-wg/2004JanMar/0041.html
        //
        //   RFC2616 - 8.2.2
        //   https://datatracker.ietf.org/doc/html/rfc2616#section-8.2.2
        self.is_tgt_can_send_res_now_according_to_rfc2616.store(true, Release);
        self.wss_send_task_tx.send(Message::Text(cranker_req)).await
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send cli req hdr to tgt: {:?}", e
            )))?; // fast fail

        // 5. Pipe cli req body to underlying wss (slow, blocking)
        trace!("5");
        if let Some(body) = opt_body {
            let mut body = body.map_err(|e| CrankerRouterException::new(format!("{:?}", e)));
            trace!("5.5");
            while let Some(req_body_chunk) = body.next().await {
                let bytes = req_body_chunk?;
                // fast fail
                trace!("6");
                trace!("8");
                if self.really_need_on_request_body_chunk_sent_to_target {
                    for i in self.proxy_listeners.iter() {
                        i.on_request_body_chunk_sent_to_target(self.as_ref(), &bytes)?; // fast fail
                    }
                }
                self.wss_send_task_tx.send(Message::Binary(bytes.into())).await
                    .map_err(|e| {
                        let failed_reason = format!(
                            "error when sending req body to tgt: {:?}", e
                        );
                        error!("{}", failed_reason);
                        CrankerRouterException::new(failed_reason)
                    })?; // fast fail
            }
            // 10. Send body end marker (slow)
            trace!("10");
            let end_of_body = CrankerProtocolRequestBuilder::new().with_request_body_ended().build()?; // fast fail
            self.wss_send_task_tx.send(Message::Text(end_of_body)).await.map_err(|e| CrankerRouterException::new(format!(
                "failed to send end of body end marker to tgt: {:?}", e
            )))?; // fast fail
        }
        trace!("20");
        for i in self.proxy_listeners.iter() {
            i.on_request_body_sent_to_target(self.as_ref())?; // fast fail
        }
        trace!("21");
        Ok(())
    }

    async fn split_part_two_recv_and_res(self: Arc<Self>) -> Result<Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException>, CrankerRouterException> {
        // if the wss_recv_pipe chan is EMPTY AND CLOSED then err will come from this recv()
        trace!("22");
        // FIXME: Seems if recv Err here, the client browser will hang?
        let hdr_res = self.tgt_res_hdr_rx.recv().await
            .map_err(|recv_err|
                CrankerRouterException::new(format!(
                    "rare ex seems nothing received from tgt res hdr chan and it closed: {:?}. router_socket_id={}",
                    recv_err, self.router_socket_id
                ))
            )??;  // fast fail
        let message_to_apply = hdr_res;
        let cranker_res = CrankerProtocolResponse::try_from_string(message_to_apply)?; // fast fail

        let status_code = cranker_res.status;
        let res_builder = cranker_res.build()?; // fast fail

        for i in self.proxy_listeners.iter() {
            i.on_before_responding_to_client(self.as_ref())?;
            i.on_after_target_to_proxy_headers_received(
                self.as_ref(), status_code, res_builder.headers_ref(),
            )?;
        }
        let wrapped_stream = self.tgt_res_bdy_rx.clone();
        let stream_close_notify = Arc::new(Notify::new());
        let stream_close_notify_clone = stream_close_notify.clone();
        let another_self = self.clone();
        let wrapped_stream_further = wrap_async_stream_with_guard(wrapped_stream, stream_close_notify_clone);
        tokio::spawn(async move {
            stream_close_notify.notified().await;
            if another_self.is_close_msg_received.load(Acquire) {
                return;
            }
            // Unexpected close from client side, may due to network error
            // or long connection (SSE) closed actively by client
            let crex = CrankerRouterException::new(
                "connection to client closed unexpectedly".to_string()
            ).with_err_kind(CrexKind::BrokenConnection_0011);
            let _ = another_self.on_error(crex);
        });
        let stream_body = Body::from_stream(wrapped_stream_further);
        Ok(res_builder
            .body(stream_body)
            .map(|body| {
                (body, None)
            })
            .map_err(|ie| CrankerRouterException::new(
                format!("failed to build body: {:?}", ie)
            )))
    }


}


#[derive(Debug)]
pub(crate) struct RSv1ClientSideResponseSender {
    pub bytes_sent: Arc<AtomicI64>,

    pub tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    pub tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,

    pub is_tgt_res_hdr_received: AtomicBool,
    pub is_tgt_res_hdr_sent: AtomicBool,
    pub is_tgt_res_bdy_received: AtomicBool,
}

impl RSv1ClientSideResponseSender {
    fn new(
        bytes_sent: Arc<AtomicI64>,
        tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
        tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    ) -> Self {
        Self {
            bytes_sent,
            tgt_res_hdr_tx,
            tgt_res_bdy_tx,
            is_tgt_res_hdr_received: AtomicBool::new(false),
            is_tgt_res_hdr_sent: AtomicBool::new(false),
            is_tgt_res_bdy_received: AtomicBool::new(false),
        }
    }

    #[inline]
    async fn send_target_response_header_text_to_client(
        &self,
        txt: String,
    ) -> Result<(), CrankerRouterException> {
        // Ensure the header only sent once
        // Since axum / tungstenite expose only Message level of websocket
        // rather than Frame level, there should be only one header message
        // received as a whole String
        if self.is_tgt_res_hdr_sent.load(Acquire)
            || self.is_tgt_res_hdr_received
            .compare_exchange(false, true, Acquire, Relaxed).is_err()
        {
            return Err(CrankerRouterException::new(
                "res header already handled!".to_string()
            ));
        }

        if txt.len() > HEADER_MAX_SIZE {
            return Err(CrankerRouterException::new(
                "response header too large: over 64 * 1024 bytes.".to_string()
            ));
        }

        let txt_len = txt.len(); // bytes len, not chars len
        let res = self.tgt_res_hdr_tx.send(Ok(txt)).await
            .map(|ok| {
                self.bytes_sent.fetch_add(txt_len.try_into().unwrap(), Release);
                ok
            })
            .map_err(|e| {
                let failed_reason = format!(
                    "rare ex: failed to send txt to cli res chan: {:?}. ", e
                );
                error!("{}",failed_reason);
                CrankerRouterException::new(failed_reason)
            });
        self.tgt_res_hdr_tx.close(); // close it immediately
        self.is_tgt_res_hdr_sent.store(true, Release);
        return res;
    }

    #[inline]
    async fn send_target_response_body_binary_fragment_to_client(
        &self,
        bin: Vec<u8>,
    ) -> Result<(), CrankerRouterException> {
        // Ensure the header value already sent
        if !self.is_tgt_res_hdr_received.load(Acquire)
            || !self.is_tgt_res_hdr_sent.load(Acquire)
        {
            return Err(CrankerRouterException::new(
                "res header not handle yet but comes binary first".to_string()
            ));
        }
        if let Err(_) = self.is_tgt_res_bdy_received.compare_exchange(false, true, AcqRel, Relaxed) {
            trace!("continuous binary to res body to cli res chan");
        }

        let bin_len = bin.len() as i64;
        self.tgt_res_bdy_tx.send(Ok(bin)).await
            .map(|ok| {
                self.bytes_sent.fetch_add(bin_len, AcqRel);
                ok
            })
            .map_err(|e| {
                let failed_reason = format!(
                    "failed to send binary to cli res chan: {:?}", e
                );
                error!("{}",failed_reason);
                CrankerRouterException::new(failed_reason)
            })
    }
}

// FIXME: Necessary close all these chan explicitly?
impl Drop for RouterSocketV1 {
    fn drop(&mut self) {
        trace!("70: dropping :{}", self.router_socket_id);
        self.tgt_res_hdr_tx.close();
        self.tgt_res_hdr_rx.close();
        self.tgt_res_bdy_tx.close();
        self.tgt_res_bdy_rx.close();
        trace!("71");
        self.wss_send_task_tx.close();
        let wrpjh = self.wss_recv_pipe_join_handle.clone();
        let wstjh = self.wss_send_task_join_handle.clone();
        tokio::spawn(async move {
            wrpjh.lock().await.as_ref()
                .map(|o| o.abort());
            wstjh.lock().await.as_ref()
                .map(|o| o.abort());
        });
        trace!("72");
    }
}

#[async_trait]
impl WebSocketListener for RouterSocketV1 {
    async fn on_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        if !self.is_tgt_can_send_res_now_according_to_rfc2616.load(Acquire) {
            let failed_reason = "recv txt before handle cli req.".to_string();
            return Err(CrankerRouterException::new(failed_reason));
        }
        self.cli_side_res_sender.send_target_response_header_text_to_client(txt).await
    }

    async fn on_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        if !self.is_tgt_can_send_res_now_according_to_rfc2616.load(Acquire) {
            let failed_reason = "recv bin before handle cli req.".to_string();
            return Err(CrankerRouterException::new(failed_reason));
        }
        // slightly different from mu cranker router that mu will judge the current state of
        // websocket / should_have_response (hasResponse) first
        self.binary_frame_received.fetch_add(1, AcqRel);
        let mut opt_bin_clone_for_listeners = None;

        if self.really_need_on_response_body_chunk_received_from_target {
            // WARNING: Copy vec<u8> is expensive!
            // it's inevitable to make a clone since the
            // terminate method of wss_tx.send moves the whole Vec<u8>
            opt_bin_clone_for_listeners = Some(bin.clone());
        }

        self.cli_side_res_sender.send_target_response_body_binary_fragment_to_client(bin).await?;

        if self.really_need_on_response_body_chunk_received_from_target {
            let bin_clone = Bytes::from(opt_bin_clone_for_listeners.unwrap());
            for i in
            self.proxy_listeners
                .iter()
                .filter(|i| i.really_need_on_response_body_chunk_received_from_target())
            {
                i.on_response_body_chunk_received_from_target(self, &bin_clone)?;
            }
        }

        Ok(())
    }

    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        if let Err(e) = self.wss_send_task_tx.send(Message::Pong(ping_msg)).await {
            return self.on_error(CrankerRouterException::new(format!(
                "failed to pong back {:?}", e
            )));
        }
        Ok(())
    }

    // when receiving close frame (equals client close ?)
    // Theoretically, tungstenite should already reply a close frame to connector, but in practice chances are it wouldn't
    async fn on_close(&self, opt_close_frame: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        trace!("40");
        self.is_close_msg_received.store(true, Release);
        let _ = self.wss_send_task_tx.send(Message::Close(opt_close_frame.clone())).await;
        // ^ send it manually again? as tested, it always failed though, should be fine
        trace!("40.5");
        for i in self.proxy_listeners.iter() {
            let _ = i.on_response_body_chunk_received(self);
        }
        trace!("41");
        let mut code = 4000; // 4000-4999 is reserved
        let mut reason = String::new();
        let mut total_err: Option<CrankerRouterException> = None;
        if let Some(clo_msg) = opt_close_frame {
            // WebSocket status code: https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1
            code = clo_msg.code;
            reason = clo_msg.reason.to_string();
        }
        trace!("42");

        if self.is_tgt_can_send_res_now_according_to_rfc2616.load(Acquire)
            && self.bytes_received.load(Acquire) == 0
        {
            trace!("43");
            if code == 1011 {
                trace!("44");
                // 1011 indicates that a server is terminating the connection because
                // it encountered an unexpected condition that prevented it from
                // fulfilling the request.
                let ex = CrankerRouterException::new(
                    "ws code 1011".to_string()
                ).with_status_code(StatusCode::BAD_GATEWAY.as_u16());
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            } else if code == 1008 {
                trace!("45");
                // 1008 indicates that an endpoint is terminating the connection
                // because it has received a message that violates its policy.  This
                // is a generic status code that can be returned when there is no
                // other more suitable status code (e.g., 1003 or 1009) or if there
                // is a need to hide specific details about the policy.
                let ex = CrankerRouterException::new(
                    "ws code 1008".to_string()
                ).with_status_code(StatusCode::BAD_REQUEST.as_u16());
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            }
        }

        if !self.tgt_res_hdr_rx.is_closed() || !self.tgt_res_bdy_rx.is_closed() {
            trace!("46");
            if code == 1000 {
                // 1000 indicates a normal closure, meaning that the purpose for
                // which the connection was established has been fulfilled.
                trace!("47");
            } else {
                trace!("48");
                error!("closing client request early due to cranker wss connection close with status code={}, reason={}", code, reason);
                let ex = CrankerRouterException::new(format!(
                    "upstream server error: ws code={}, reason={}", code, reason
                ));

                // I think here same as asyncHandle.complete(exception) in mu cranker router
                // FIXME: It occurs that the client browser will hang if ex sent here
                //  240601: This change seems from the debug branch in stash???
                //  may be due to kvm proxy issue
                let _ = async { let _ = self.tgt_res_hdr_tx.send(Err(ex.clone())).await; };
                let _ = async { let _ = self.tgt_res_bdy_tx.send(Err(ex.clone())).await; };
                let may_ex = self.on_error(ex); // ?Necessary
                total_err = exceptions::compose_ex(total_err, may_ex);
            }
            // I think here same as asyncHandle.complete(null) in mu cranker router
            self.tgt_res_hdr_rx.close();
            self.tgt_res_bdy_rx.close();
        }
        trace!("49");

        let may_ex = self.raise_completion_event(None);
        total_err = exceptions::compose_ex(total_err, may_ex);
        if total_err.is_some() {
            return Err(total_err.unwrap());
        }
        trace!("49.5");
        Ok(())
    }

    #[inline]
    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        // The actual heavy burden is done in `pipe_and_queue_the_wss_send_task_and_handle_err_chan`
        self.err_chan_tx.send_blocking(err)
            .map(|ok| {
                let _ = self.raise_completion_event(None);
                ok
            })
            .map_err(|se| CrankerRouterException::new(format!(
                "rare ex that failed to send error to err chan: {:?}", se
            )))
    }

    fn get_idle_read_timeout_ms(&self) -> i64 { self.idle_read_timeout_ms }
    fn get_ping_sent_after_no_write_for_ms(&self) -> i64 { self.ping_sent_after_no_write_for_ms }
}

impl RouteIdentify for RouterSocketV1 {
    fn router_socket_id(&self) -> String {
        self.router_socket_id.clone()
    }
    fn route(&self) -> String {
        self.route.clone()
    }
    fn service_address(&self) -> SocketAddr {
        self.remote_address.clone()
    }
}

impl ProxyInfo for RouterSocketV1 {
    fn is_catch_all(&self) -> bool {
        self.route.eq("*")
    }
    fn connector_id(&self) -> String {
        self.connector_id.clone()
    }
    fn duration_millis(&self) -> i64 {
        self.duration_millis.load(Acquire)
    }
    fn bytes_received(&self) -> i64 {
        self.bytes_received.load(Acquire)
    }
    fn bytes_sent(&self) -> i64 {
        self.bytes_sent.load(Acquire)
    }
    fn response_body_frames(&self) -> i64 {
        self.binary_frame_received.load(Acquire)
    }
    fn error_if_any(&self) -> Option<CrankerRouterException> {
        self.error.try_read()
            .ok()
            .and_then(|ok|
                ok.clone().map(|some| some.clone())
            )
    }

    fn socket_wait_in_millis(&self) -> i64 {
        self.socket_wait_in_millis.load(Acquire)
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    fn component_name(&self) -> String {
        self.component_name.clone()
    }

    fn connector_id(&self) -> String {
        self.connector_id.clone()
    }

    #[inline]
    fn is_removed(&self) -> bool {
        self.is_removed.load(Acquire)
    }

    fn get_is_removed_arc_atomic_bool(&self) -> Arc<AtomicBool> {
        self.is_removed.clone()
    }

    fn cranker_version(&self) -> &'static str {
        return CRANKER_V_1_0;
    }

    fn raise_completion_event(&self, _: Option<ClientRequestIdentifier>) -> Result<(), CrankerRouterException> {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            wsf.clone().remove_router_socket_in_background(
                self.route.clone(), self.router_socket_id.clone(), self.is_removed.clone(),
            )
        }
        let cli_sta = self.client_req_start_ts.load(Acquire);
        if cli_sta == 0 || self.proxy_listeners.is_empty() {
            return Ok(());
        }
        let dur = time_utils::current_time_millis() - self.client_req_start_ts.load(Acquire);
        self.duration_millis.store(dur, Release);
        for i in self.proxy_listeners.iter() {
            i.on_complete(self)?;
        }
        Ok(())
    }

    fn is_dark_mode_on(&self, dark_hosts: &HashSet<DarkHost>) -> bool {
        let remote_ip_addr = self.remote_address.ip();
        dark_hosts.iter().any(|i| i.same_host(remote_ip_addr))
    }

    /// Return -1 to avoid Serde
    fn inflight_count(&self) -> i32 {
        -1
    }


    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           original_uri: &OriginalUri,
                           cli_headers: &HeaderMap,
                           addr: &SocketAddr,
                           opt_body: Option<BodyDataStream>,
    ) -> Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException> {
        // 0. if is removed then should not run into this method (fast)
        if self.is_removed() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a is_removed router socket. router_socket_id={}", &self.router_socket_id
            )));
        }
        let current_time_millis = time_utils::current_time_millis();
        self.socket_wait_in_millis.fetch_add(current_time_millis, AcqRel);
        self.client_req_start_ts.store(current_time_millis, Release);
        self.duration_millis.store(-current_time_millis, Release);
        // 1. Cli header processing (fast)
        trace!("1");
        let mut hdr_to_tgt = HeaderMap::new();
        set_target_request_headers(cli_headers, &mut hdr_to_tgt, &app_state, http_version, addr, original_uri);
        // 2. Build protocol request line / protocol header frame without endmarker (fast)
        trace!("2");
        let request_line = create_request_line(method, &original_uri);
        let cranker_req_bdr = CrankerProtocolRequestBuilder::new()
            .with_request_line(request_line)
            .with_request_headers(&hdr_to_tgt);

        for i in self.proxy_listeners.iter() {
            i.on_before_proxy_to_target(self.as_ref(), &mut hdr_to_tgt)?; // fast fail
        }

        // 3. Choose endmarker based on has body or not (fast)
        trace!("3");
        let cranker_req = match opt_body.is_some() {
            false => {
                cranker_req_bdr
                    .with_request_has_no_body()
                    .build()? // fast fail
            }
            true => {
                cranker_req_bdr
                    .with_request_body_pending()
                    .build()? // fast fail
            }
        };
        tokio::select! {
            Err(crex) = self.clone().split_part_one_send_or_err(opt_body, cranker_req) => {
                return Err(crex);
            }
            ok_or_err = self.split_part_two_recv_and_res()  => {
                return ok_or_err?;
            }
        }
    }

    async fn send_ws_msg_to_uwss(self: Arc<Self>, message: Message) -> Result<(), CrankerRouterException> {
        let _ = self.wss_send_task_tx.send(message).await.map_err(|e| {
            let failed_reason = format!("failed to send ws msg to wss_send_task_tx: {:?}", e);
            error!("{}", failed_reason);
            CrankerRouterException::new(failed_reason)
        })?;
        Ok(())
    }

    async fn terminate_all_conn(self: Arc<Self>, opt_crex: Option<CrankerRouterException>) -> Result<(), CrankerRouterException> {
        let crex = opt_crex.unwrap_or(CrankerRouterException::new("ex occurs and should terminate all conn.".to_string()));
        let _ = self.tgt_res_hdr_tx.send(Err(crex.clone())).await;
        let _ = self.tgt_res_bdy_tx.send(Err(crex.clone())).await;
        self.tgt_res_hdr_tx.close();
        self.tgt_res_bdy_tx.close();
        Ok(())
    }

    fn inc_bytes_received_from_cli(&self, byte_count: i32) {
        self.bytes_received.fetch_add(byte_count as i64, AcqRel);
    }

    fn try_provide_general_error(&self, opt_crex: Option<CrankerRouterException>) -> Result<(), CrankerRouterException> {
        self.error.try_write().map(|mut s| {
            *s = opt_crex;
        }).map_err(|e| {
            let failed_reason = format!(
                "failed at try_provide_general_error. router_socket_id = {} : {:?}",
                self.router_socket_id(), e
            );
            error!("{}", failed_reason);
            CrankerRouterException::new(failed_reason)
        })
    }

    fn get_opt_arc_websocket_farm(&self) -> Option<Arc<WebSocketFarm>> {
        self.websocket_farm.upgrade()
    }
}

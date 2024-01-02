use std::borrow::Cow;
use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, Release, SeqCst};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::handler::Handler;
use axum::http::{HeaderMap, Method, Response, StatusCode};
use axum::http::uri::PathAndQuery;
use bytes::Bytes;
use futures::{Sink, SinkExt, StreamExt, TryFutureExt};
use futures::stream::{SplitSink, SplitStream};
use log::{debug, error, info, trace};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{CRANKER_V_1_0, exceptions, REPRESSED_HEADERS, time_utils, TSCRState};
use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};
use crate::websocket_listener::WebSocketListener;

pub const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub(crate) trait RouterSocket: Send + Sync + ProxyInfo {
    fn is_removed(&self) -> bool;

    fn cranker_version(&self) -> &'static str;

    fn raise_completion_event(&self) -> Result<(), CrankerRouterException> {
        Ok(())
    }

    // accept a client req
    async fn on_client_req(self: Arc<Self>,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<Receiver<Result<Bytes, CrankerRouterException>>>,
    ) -> Result<Response<Body>, CrankerRouterException>;
}

pub struct RouterSocketV1 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    pub websocket_farm: Weak<WebSocketFarm>,
    pub connector_instance_id: String,
    pub proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    pub remote_address: SocketAddr,
    pub is_removed: Arc<AtomicBool>,
    // previously `is_removed`, but actually we can't remove ourself from the mpmc channel so this is just a marker
    pub has_response: Arc<AtomicBool>,
    // this should be shared because before wss acquired to handle, ping/pong/close should be handling somewhere
    pub bytes_received: Arc<AtomicI64>,
    pub bytes_sent: Arc<AtomicI64>,
    pub binary_frame_received: AtomicI64,
    // but axum receive websocket in Message level. Let's treat it as frame now
    pub socket_wait_in_millis: AtomicI64,
    pub error: RwLock<Option<CrankerRouterException>>,
    pub client_req_start_ts: Arc<AtomicI64>,
    pub duration_millis: Arc<AtomicI64>,

    // below should be private for inner routine
    err_chan_tx: Sender<CrankerRouterException>, // Once error

    cli_side_res_sender: RSv1ClientSideResponseSender,
    // Send any exception immediately to this chan in on_error so that when
    // the body not sent to client yet, we can response error to client early
    tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    tgt_res_hdr_rx: Receiver<Result<String, CrankerRouterException>>,
    // Send any exception immediately to this chan in on_error so that even
    // when body is streaming, we can end the stream early to save resource
    tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    tgt_res_bdy_rx: Receiver<Result<Vec<u8>, CrankerRouterException>>,

    wss_recv_pipe_rx: Receiver<Message>,
    wss_recv_pipe_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    has_response_notify: Arc<Notify>,
}

impl RouterSocketV1 {
    // err should use a separated channel to communicate between wss side and router socket side
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               websocket_farm: Weak<WebSocketFarm>,
               connector_instance_id: String,
               remote_address: SocketAddr,
               underlying_wss_tx: SplitSink<WebSocket, Message>,
               underlying_wss_rx: SplitStream<WebSocket>,
               proxy_listeners: Vec<Arc<dyn ProxyListener>>,
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

        let (wss_recv_pipe_tx, wss_recv_pipe_rx) = async_channel::unbounded();
        let has_response = Arc::new(AtomicBool::new(false));

        let router_socket_id_clone = router_socket_id.clone();
        let has_response_notify = Arc::new(Notify::new());

        let arc_wrapped = Arc::new(Self {
            route,
            component_name,
            router_socket_id,
            websocket_farm,
            connector_instance_id,
            proxy_listeners,
            remote_address,

            is_removed: is_removed.clone(),
            has_response: has_response.clone(),
            bytes_received: bytes_received.clone(),
            bytes_sent: bytes_sent.clone(),
            binary_frame_received: AtomicI64::new(0),
            socket_wait_in_millis: AtomicI64::new(-time_utils::current_time_millis()),
            error: RwLock::new(None),
            client_req_start_ts,
            duration_millis,

            err_chan_tx,

            cli_side_res_sender: RSv1ClientSideResponseSender::new(
                router_socket_id_clone, bytes_sent,
                is_removed, has_response,
                tgt_res_hdr_tx.clone(), tgt_res_bdy_tx.clone(),
            ),

            tgt_res_hdr_tx,
            tgt_res_hdr_rx,
            tgt_res_bdy_tx,
            tgt_res_bdy_rx,

            wss_recv_pipe_rx,
            wss_send_task_join_handle: Arc::new(Mutex::new(None)),

            wss_send_task_tx,
            wss_recv_pipe_join_handle: Arc::new(Mutex::new(None)),

            has_response_notify
        });
        let wss_recv_pipe_join_handle = tokio::spawn(pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
            arc_wrapped.clone(),
            underlying_wss_rx, wss_recv_pipe_tx,
        ));
        let wss_send_task_join_handle = tokio::spawn(
            pipe_and_queue_the_wss_send_task_and_handle_err_chan(
                arc_wrapped.clone(),
                underlying_wss_tx, wss_send_task_rx, err_chan_rx,
            ));
        let arc_wrapped_clone = arc_wrapped.clone();
        tokio::spawn(async move {
            let mut g = arc_wrapped_clone.wss_recv_pipe_join_handle.lock().await;
            *g = Some(wss_recv_pipe_join_handle);
            let mut g = arc_wrapped_clone.wss_send_task_join_handle.lock().await;
            *g = Some(wss_send_task_join_handle);
        });

        arc_wrapped
    }
}

async fn listen_tgt_res_async(another_self: Arc<RouterSocketV1>) -> Result<(), CrankerRouterException> {
    let msg_counter = AtomicUsize::new(0);
    // 11. handling what we received from wss_recv_pipe
    debug!("11");
    while let Ok(msg) = another_self.wss_recv_pipe_rx.recv().await {
        msg_counter.fetch_add(1, SeqCst);
        match msg {
            Message::Text(txt) => { another_self.on_text(txt).await?; }
            Message::Binary(bin) => { another_self.on_binary(bin).await?; }
            Message::Close(opt_clo_fra) => { another_self.on_close(opt_clo_fra).await?; }
            Message::Ping(ping) => { another_self.on_ping(ping).await?; }
            Message::Pong(pong) => { another_self.on_pong(pong).await?; }
        }
    }
    // 12. End of receiving messages from wss_recv_pipe
    debug!("12");
    debug!("Totally received {} messages after starting listen to response in router_socket_id={}", msg_counter.load(SeqCst), another_self.router_socket_id);
    Ok(())
}

// FIXME: Necessary close all these chan explicitly?
impl Drop for RouterSocketV1 {
    fn drop(&mut self) {
        debug!("70: dropping :{}", self.router_socket_id);
        self.tgt_res_hdr_tx.close();
        self.tgt_res_hdr_rx.close();
        self.tgt_res_bdy_tx.close();
        self.tgt_res_bdy_rx.close();
        debug!("71");
        self.wss_recv_pipe_rx.close();
        self.wss_send_task_tx.close();
        let wrpjh = self.wss_recv_pipe_join_handle.clone();
        let wstjh = self.wss_send_task_join_handle.clone();
        tokio::spawn(async move {
            wrpjh.lock().await.as_ref()
                .map(|o| o.abort());
            wstjh.lock().await.as_ref()
                .map(|o| o.abort());
        });
        debug!("72");
    }
}

async fn pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
    rs: Arc<RouterSocketV1>,
    mut underlying_wss_rx: SplitStream<WebSocket>,
    wss_recv_pipe_tx: Sender<Message>,
) {
    let mut local_has_response = rs.has_response.load(SeqCst);
    let mut may_ex: Option<CrankerRouterException> = None;
    loop { // @outer
        tokio::select! {
            _ = rs.has_response_notify.notified() => {
                debug!("30");
                local_has_response = true;
            }
            opt_res_msg = underlying_wss_rx.next() => {
                match opt_res_msg {
                    None => {
                        break; // @outer
                    }
                    Some(res_msg) => {
                        match res_msg {
                            Err(wss_recv_err) => {
                                let _ = wss_recv_pipe_tx.close(); // close or drop?
                                may_ex = Some(CrankerRouterException::new(format!(
                                    "underlying wss recv err: {:?}.", wss_recv_err
                                )));
                                break;
                            }
                            Ok(msg) => {
                                match msg {
                                    Message::Text(txt) => {
                                        if !local_has_response {
                                            let failed_reason = "recv txt bin before handle cli req.".to_string();
                                            may_ex = Some(CrankerRouterException::new(failed_reason));
                                            break;
                                        }
                                        debug!("31");
                                        may_ex = rs.on_text(txt).await.err();
                                        debug!("32")
                                    }
                                    Message::Binary(bin) => {
                                        if !local_has_response {
                                            let failed_reason = "recv bin before handle cli req.".to_string();
                                            may_ex = Some(CrankerRouterException::new(failed_reason));
                                            break;
                                        }
                                        may_ex = rs.on_binary(bin).await.err();
                                    }
                                    Message::Ping(ping_hello) => {
                                        may_ex = rs.on_ping(ping_hello).await.err();
                                    }
                                    Message::Pong(pong_msg) => {
                                        may_ex = rs.on_pong(pong_msg).await.err();
                                    }
                                    Message::Close(opt_clo_fra) => {
                                        may_ex = rs.on_close(opt_clo_fra).await.err();
                                        break;
                                    }
                                }
                                if let Some(ex) = may_ex {
                                    may_ex = None;
                                    let _ = rs.on_error(ex);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(ex) = may_ex {
        may_ex = None;
        let _ = rs.on_error(ex);
    }

    // recv nothing from underlying wss, implicating it already closed
    debug!("seems router socket normally closed. router_socket_id={}", rs.router_socket_id);
    // Here the is_removed is not set to true yet, and compare-and-set tends to infinite
    // loop. So choose the dumb way to wait some milliseconds. // FIXME
    tokio::time::sleep(Duration::from_millis(50)).await;
    if !rs.is_removed() {
        // means the is_removed is still false, which is not expected
        let _ = wss_recv_pipe_tx.close(); // close or drop?
        let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(
            "underlying wss already closed but is_removed=false after 50ms.".to_string()
        ));
    }
}

async fn pipe_and_queue_the_wss_send_task_and_handle_err_chan(
    rs: Arc<RouterSocketV1>,
    mut underlying_wss_tx: SplitSink<WebSocket, Message>,
    wss_send_task_rx: Receiver<Message>,
    err_chan_rx: Receiver<CrankerRouterException>,
) {
    loop {
        tokio::select! {
            Ok(crex) = err_chan_rx.recv() => {
                error!("err_chan_rx received err: {:?}. router_socket_id={}", crex.reason.as_str(), rs.router_socket_id);
                // 0. Stop receiving message, flush underlying wss tx
                let _ = wss_send_task_rx.close();
                let _ = underlying_wss_tx.flush().await;
                // 1. Shutdown the underlying wss tx
                let mut reason_in_clo_frame = crex.reason.clone();
                if reason_in_clo_frame.len() > 100 {
                    // close frame should contains no more than 125 chars ?
                    reason_in_clo_frame = reason_in_clo_frame[0..100].to_string();
                }
                let _ = underlying_wss_tx.send(Message::Close(Some(CloseFrame {
                    // 1011 indicates that a server is terminating the connection because
                    // it encountered an unexpected condition that prevented it from
                    // fulfilling the request.
                    code: 1011,
                    reason: Cow::Owned(reason_in_clo_frame)
                }))).await;
                let _ = underlying_wss_tx.close();
                // 2. Remove bad router_socket
                if !rs.is_removed() {
                    if let Some(wsf) = rs.websocket_farm.upgrade() {
                        wsf.remove_websocket_in_background(rs.route.clone(), rs.router_socket_id.clone(), rs.is_removed.clone());
                    }
                }
                // 3. Close the res to cli
                let _ = rs.tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = rs.tgt_res_bdy_tx.send(Err(crex.clone())).await;
                rs.tgt_res_hdr_tx.close();
                rs.tgt_res_bdy_tx.close();
                break;
            }
            recv_res = wss_send_task_rx.recv() => {
                match recv_res {
                    Ok(msg) => {
                        if let Message::Binary(bin) = msg {
                            rs.bytes_received.fetch_add(bin.len().try_into().unwrap(),SeqCst);
                            if let Err(e) = underlying_wss_tx.send(Message::Binary(bin)).await {
                                let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{:?}", e)));
                            }
                        } else if let Message::Text(txt) = msg {
                            rs.bytes_received.fetch_add(txt.len().try_into().unwrap(),SeqCst);
                            if let Err(e) = underlying_wss_tx.send(Message::Text(txt)).await {
                                let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{:?}", e)));
                            }
                        } else if let Err(e) = underlying_wss_tx.send(msg).await {
                            let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{:?}", e)));
                        }
                    }
                    Err(recv_err) => { // Indicates the wss_send_task_rx is EMPTY AND CLOSED
                        debug!("the wss_send_task_rx is EMPTY AND CLOSED. router_socket_id={}", rs.router_socket_id);
                        break;
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct RSv1ClientSideResponseSender {
    pub router_socket_id: String,
    pub bytes_sent: Arc<AtomicI64>,
    pub is_removed: Arc<AtomicBool>,
    pub has_response: Arc<AtomicBool>,

    pub tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    pub tgr_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,

    pub is_tgt_res_hdr_received: AtomicBool,
    pub is_tgt_res_hdr_sent: AtomicBool,
    pub is_tgt_res_bdy_received: AtomicBool,
    pub is_wss_closed: AtomicBool,
}


impl RSv1ClientSideResponseSender {
    fn new(
        router_socket_id: String,
        bytes_sent: Arc<AtomicI64>,
        is_removed: Arc<AtomicBool>,
        has_response: Arc<AtomicBool>,
        tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
        tgr_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    ) -> Self {
        Self {
            router_socket_id,
            bytes_sent,

            is_removed,
            has_response,

            tgt_res_hdr_tx,
            tgr_res_bdy_tx,
            is_tgt_res_hdr_received: AtomicBool::new(false), // 1
            is_tgt_res_hdr_sent: AtomicBool::new(false),     // 2
            is_tgt_res_bdy_received: AtomicBool::new(false), // 3
            is_wss_closed: AtomicBool::new(false),           // 4
        }
    }

    async fn send_target_response_header_text_to_client(
        &self,
        txt: String,
    ) -> Result<(), CrankerRouterException> {
        // Ensure the header only sent once
        // Since axum / tungstenite expose only Message level of websocket
        // It should be only one header message received
        if self.is_tgt_res_hdr_sent.load(SeqCst)
            || self.is_tgt_res_hdr_received
            .compare_exchange(false, true, SeqCst, SeqCst).is_err()
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
                self.bytes_sent.fetch_add(txt_len.try_into().unwrap(), SeqCst);
                ok
            })
            .map_err(|e| {
                let failed_reason = format!(
                    "rare ex: failed to send txt to cli res chan: {:?}. ", e
                );
                error!("{}",failed_reason);
                CrankerRouterException::new(failed_reason)
            });
        self.is_tgt_res_hdr_sent.store(true, SeqCst);
        return res;
    }

    async fn send_target_response_body_binary_fragment_to_client(
        &self,
        bin: Vec<u8>,
    ) -> Result<(), CrankerRouterException> {
        // Ensure the header value already sent
        if !self.is_tgt_res_hdr_received.load(SeqCst)
            || !self.is_tgt_res_hdr_sent.load(SeqCst)
        {
            return Err(CrankerRouterException::new(
                "res header not handle yet but comes binary first".to_string()
            ));
        }
        if let Err(_) = self.is_tgt_res_bdy_received.compare_exchange(false, true, AcqRel, SeqCst) {
            trace!("continuous binary to res body to cli res chan");
        }

        let bin_len = bin.len();
        self.tgr_res_bdy_tx.send(Ok(bin)).await
            .map(|ok| {
                self.bytes_sent.fetch_add(bin_len.try_into().unwrap(), SeqCst);
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

#[async_trait]
impl WebSocketListener for RouterSocketV1 {

    async fn on_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        if let Err(e) = self.cli_side_res_sender.send_target_response_header_text_to_client(txt).await {
            return self.on_error(e);
        }
        Ok(())
    }

    async fn on_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        // slightly different from mu cranker router that it will judge the current state of websocket / has_response first
        self.binary_frame_received.fetch_add(1, SeqCst);
        let mut res = Ok(());
        let mut opt_bin_clone_for_listeners = None;

        if !self.proxy_listeners.is_empty() {
            // FIXME: Copy vec<u8> is expensive!
            // it's inevitable to make a clone since the
            // terminate method of wss_tx.send moves the whole Vec<u8>
            opt_bin_clone_for_listeners = Some(bin.clone());
        }

        if let Err(e) = self.cli_side_res_sender.send_target_response_body_binary_fragment_to_client(bin).await {
            res = self.on_error(e);
        }

        if !self.proxy_listeners.is_empty() {
            let bin_clone = Bytes::from(opt_bin_clone_for_listeners.unwrap());
            for i in self.proxy_listeners.iter() {
                let _ = i.on_response_body_chunk_received_from_target(self, &bin_clone);
            }
        }

        res
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
        debug!("40");
        for i in self.proxy_listeners.iter() {
            let _ = i.on_response_body_chunk_received(self);
        }
        debug!("41");
        let mut code = 4000; // 4000-4999 is reserved
        let mut reason = String::new();
        let mut total_err: Option<CrankerRouterException> = None;
        if let Some(clo_msg) = opt_close_frame {
            // WebSocket status code: https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1
            code = clo_msg.code;
            reason = clo_msg.reason.to_string();
        }
        debug!("42");
        // TODO: Handle the reason carefully like mu cranker router
        if self.has_response.load(SeqCst) {
            debug!("43");
            if code == 1011 {
                debug!("44");
                // 1011 indicates that a server is terminating the connection because
                // it encountered an unexpected condition that prevented it from
                // fulfilling the request.
                let ex = CrankerRouterException::new("ws code 1011. should res to cli 502".to_string());
                // TODO: Handle carefully in on_error
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            } else if code == 1008 {
                debug!("45");
                // 1008 indicates that an endpoint is terminating the connection
                // because it has received a message that violates its policy.  This
                // is a generic status code that can be returned when there is no
                // other more suitable status code (e.g., 1003 or 1009) or if there
                // is a need to hide specific details about the policy.
                let ex = CrankerRouterException::new("ws code 1008. should res to cli 400".to_string());
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            }
        }

        if !self.tgt_res_hdr_rx.is_closed() || !self.tgt_res_bdy_rx.is_closed() {
            debug!("46");
            // let mut is_tgt_chan_close_as_expected = true;
            if code == 1000 {
                debug!("47");
                // 1000 indicates a normal closure, meaning that the purpose for
                // which the connection was established has been fulfilled.

                // I think here same as asyncHandle.complete(null) in mu cranker router
                self.tgt_res_hdr_rx.close();
                self.tgt_res_bdy_rx.close();

                // is_tgt_chan_close_as_expected = self.tgt_res_hdr_rx.close() && is_tgt_chan_close_as_expected;
                // is_tgt_chan_close_as_expected = self.tgt_res_bdy_rx.close() && is_tgt_chan_close_as_expected;
            } else {
                debug!("48");
                error!("closing client request early due to cranker wss connection close with status code={}, reason={}", code, reason);
                let ex = CrankerRouterException::new(format!(
                    "upstream server error: ws code={}, reason={}", code, reason
                ));

                // I think here same as asyncHandle.complete(exception) in mu cranker router
                let _ = self.tgt_res_hdr_tx.send(Err(ex.clone())).await;
                let _ = self.tgt_res_bdy_tx.send(Err(ex.clone())).await;
                self.tgt_res_hdr_rx.close();
                self.tgt_res_bdy_rx.close();
                let may_ex = self.on_error(ex); // ?Necessary
                total_err = exceptions::compose_ex(total_err, may_ex);
            }
            // if !is_tgt_chan_close_as_expected {
            //     self.on_error()
            // }
        }
        debug!("49");

        let may_ex = self.raise_completion_event();
        total_err = exceptions::compose_ex(total_err, may_ex);
        if total_err.is_some() {
            return Err(total_err.unwrap());
        }
        debug!("49.5");
        Ok(())
    }

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        // The actual heavy burden is done in `pipe_and_queue_the_wss_send_task_and_handle_err_chan`
        self.err_chan_tx.send_blocking(err)
            .map(|ok| {
                let _ = self.raise_completion_event();
                ok
            })
            .map_err(|se| CrankerRouterException::new(format!(
                "rare error that failed to send error to err chan: {:?}", se
            )))
    }
}

impl ProxyInfo for RouterSocketV1 {
    fn is_catch_all(&self) -> bool {
        self.route.eq("*")
    }

    fn connector_instance_id(&self) -> String {
        self.connector_instance_id.clone()
    }

    fn server_address(&self) -> SocketAddr {
        self.remote_address.clone()
    }

    fn route(&self) -> String {
        self.route.clone()
    }

    fn router_socket_id(&self) -> String {
        self.router_socket_id.clone()
    }

    fn duration_millis(&self) -> i64 {
        self.duration_millis.load(SeqCst)
    }

    fn bytes_received(&self) -> i64 {
        self.bytes_received.load(SeqCst)
    }

    fn bytes_sent(&self) -> i64 {
        self.bytes_sent.load(SeqCst)
    }

    fn response_body_frames(&self) -> i64 {
        self.binary_frame_received.load(SeqCst)
    }

    fn error_if_any(&self) -> Option<CrankerRouterException> {
        self.error.try_read()
            .ok()
            .and_then(|ok|
                ok.clone().map(|some| some.clone())
            )
    }

    fn socket_wait_in_millis(&self) -> i64 {
        self.socket_wait_in_millis.load(SeqCst)
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    #[inline]
    fn is_removed(&self) -> bool {
        return self.is_removed.load(SeqCst);
    }

    fn cranker_version(&self) -> &'static str {
        return CRANKER_V_1_0;
    }

    fn raise_completion_event(&self) -> Result<(), CrankerRouterException> {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            wsf.remove_websocket_in_background(
                self.route.clone(), self.router_socket_id.clone(), self.is_removed.clone()
            )
        }
        let cli_sta = self.client_req_start_ts.load(SeqCst);
        if cli_sta == 0 || self.proxy_listeners.is_empty() {
            return Ok(());
        }
        let dur = time_utils::current_time_millis() - self.client_req_start_ts.load(SeqCst);
        self.duration_millis.store(dur, SeqCst);
        for i in self.proxy_listeners.iter() {
            i.on_complete(self)?;
        }
        Ok(())
    }

    async fn on_client_req(self: Arc<Self>,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<Receiver<Result<Bytes, CrankerRouterException>>>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        // 0. if is removed then should not run into this method
        if self.is_removed() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a is_removed router socket. router_socket_id={}", &self.router_socket_id
            )));
        }

        for i in self.proxy_listeners.iter() {
            let _ = i.on_before_proxy_to_target(self.as_ref(), headers);
        }

        let current_time_millis = time_utils::current_time_millis();
        self.socket_wait_in_millis.fetch_add(current_time_millis, SeqCst);
        self.client_req_start_ts.store(current_time_millis, SeqCst);
        self.duration_millis.store(-current_time_millis, SeqCst);
        // 1. Cli header processing
        debug!("1");
        let mut client_request_headers = HeaderMap::new();
        headers.iter().for_each(|(k, v)| {
            if REPRESSED_HEADERS.contains(k.as_str().to_ascii_lowercase().as_str()) {
                // NOP
            } else {
                client_request_headers.insert(k.to_owned(), v.to_owned());
            }
        });
        // 2. Build protocol request line / protocol header frame without endmarker
        debug!("2");
        let request_line = build_request_line(method, path_and_query);
        let cranker_req_bdr = CrankerProtocolRequestBuilder::new()
            .with_request_line(request_line)
            .with_request_headers(&client_request_headers);

        // 3. Choose endmarker based on has body or not
        debug!("3");
        let cranker_req = match opt_body.is_some() {
            false => {
                cranker_req_bdr
                    .with_request_has_no_body()
                    .build()?
            }
            true => {
                cranker_req_bdr
                    .with_request_body_pending()
                    .build()?
            }
        };
        // 4. Send protocol header frame
        debug!("4");
        self.wss_send_task_tx.send(Message::Text(cranker_req)).await
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send cli req hdr to tgt: {:?}", e
            )))?;

        // 5. Pipe cli req body to underlying wss
        debug!("5");
        if let Some(mut body) = opt_body {
            debug!("5.5");
            while let Ok(res_bdy_chunk) = body.recv().await {
                debug!("6");
                match res_bdy_chunk {
                    Ok(bytes) => {
                        debug!("8");

                        for i in self.proxy_listeners.iter() {
                            let _ = i.on_request_body_chunk_sent_to_target(self.as_ref(), &bytes);
                        }

                        self.wss_send_task_tx.send(Message::Binary(bytes.to_vec())).await
                            .map_err(|e| {
                                let failed_reason = format!(
                                    "error when sending req body to tgt: {:?}", e
                                );
                                error!("{}", failed_reason);
                                CrankerRouterException::new(failed_reason)
                            })?;
                    }
                    Err(e) => {
                        debug!("9");
                        error!("error when receiving req body from cli: {:?}", &e.reason);
                        return Err(e);
                    }
                }
            }
            // 10. Send body end marker
            debug!("10");
            let end_of_body = CrankerProtocolRequestBuilder::new().with_request_body_ended().build()?;
            self.wss_send_task_tx.send(Message::Text(end_of_body)).await.map_err(|e| CrankerRouterException::new(format!(
                "failed to send end of body end marker to tgt: {:?}", e
            )))?;
        }
        debug!("20");
        for i in self.proxy_listeners.iter() {
            i.on_request_body_sent_to_target(self.as_ref())?;
        }
        debug!("21");

        let mut cranker_res: Option<CrankerProtocolResponse> = None;

        // if the wss_recv_pipe chan is EMPTY AND CLOSED then err will come from this recv()
        self.has_response.store(true, SeqCst);
        self.has_response_notify.notify_waiters();
        let another_self = self.clone();
        // tokio::spawn(listen_tgt_res_async(another_self));
        debug!("22");

        if let hdr_res = self.tgt_res_hdr_rx.recv().await
            .map_err(|recv_err|
                CrankerRouterException::new(format!(
                    "rare ex seems nothing received from tgt res hdr chan and it closed: {:?}. router_socket_id={}",
                    recv_err, self.router_socket_id
                ))
            )??
        {
            let message_to_apply = hdr_res;
            cranker_res = Some(CrankerProtocolResponse::new(message_to_apply)?);
        }

        if cranker_res.is_none() {
            // Should never run into this line
            return Ok(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(Body::new(format!(
                "rare ex failed to build response from protocol response. router_socket_id={}", self.router_socket_id
            ))).unwrap());
        }

        let cranker_res = cranker_res.unwrap();
        let status_code = cranker_res.status;
        let res_builder = cranker_res.build()?;
        {
            for i in self.proxy_listeners.iter() {
                let _ = i.on_before_responding_to_client(self.as_ref());
                let _ = i.on_after_target_to_proxy_headers_received(
                    self.as_ref(), status_code, res_builder.headers_ref(),
                );
            }
        }
        let wrapped_stream = self.tgt_res_bdy_rx.clone();
        let stream_body = Body::from_stream(wrapped_stream);
        res_builder
            .body(stream_body)
            .map_err(|ie| CrankerRouterException::new(
                format!("failed to build body: {:?}", ie)
            ))
    }
}

fn build_request_line(method: Method, path_and_query: Option<&PathAndQuery>) -> String {
    let mut res = String::new();
    res.push_str(method.as_str());
    res.push(' ');
    match path_and_query {
        Some(paq) => res.push_str(paq.as_str()),
        _ => {}
    }
    res
}


// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
pub async fn take_wss_and_create_router_socket(
    wss: WebSocket,
    state: TSCRState,
    connector_id: String,
    component_name: String,
    cranker_version: String,
    domain: String,
    route: String,
    addr: SocketAddr,
)
{
    let (wss_tx, wss_rx) = wss.split();
    let router_socket_id = Uuid::new_v4().to_string();
    let weak_wsf = Arc::downgrade(&state.websocket_farm);
    let rs = RouterSocketV1::new(
        route.clone(),
        component_name.clone(),
        router_socket_id.clone(),
        weak_wsf,
        connector_id.clone(),
        addr,
        wss_tx,
        wss_rx,
        state.proxy_listeners.clone(),
    );
    info!("Connector registered! connector_id: {}, router_socket_id: {}", connector_id, router_socket_id);
    state
        .websocket_farm
        .clone()
        .add_socket_in_background(rs);
    {
        // Prepare for some counter
        let write_guard = state.clone();
        let _ = write_guard.counter.fetch_add(1, SeqCst);
    }
}

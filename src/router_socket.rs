use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::{Debug, Display};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, SeqCst};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use axum::async_trait;
use axum::body::Body;
use axum::extract::OriginalUri;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::handler::Handler;
use axum::http::{HeaderMap, Method, Response, Version};
use axum_core::body::BodyDataStream;
use bytes::Bytes;
use futures::{Sink, SinkExt, StreamExt, TryFutureExt, TryStreamExt};
use futures::stream::{SplitSink, SplitStream};
use log::{debug, error, info, trace};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{ACRState, CRANKER_V_1_0, exceptions, time_utils};
use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::dark_host::DarkHost;
use crate::exceptions::CrankerRouterException;
use crate::http_utils::set_target_request_headers;
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};
use crate::websocket_listener::WebSocketListener;

pub const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub(crate) trait RouterSocket: Send + Sync + ProxyInfo {
    fn component_name(&self) -> String; // TODO: Should make this opt to keep in line with mu
    fn connector_id(&self) -> String;

    fn is_removed(&self) -> bool;

    fn cranker_version(&self) -> &'static str;

    fn raise_completion_event(&self) -> Result<(), CrankerRouterException> {
        Ok(())
    }

    fn is_dark_mode_on(&self, dark_hosts: &HashSet<DarkHost>) -> bool {
        false
    }

    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           original_uri: &OriginalUri,
                           headers: &HeaderMap,
                           addr: &SocketAddr,
                           opt_body: Option<BodyDataStream>,
    ) -> Result<Response<Body>, CrankerRouterException>;
}

pub struct RouterSocketV1 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    pub websocket_farm: Weak<WebSocketFarm>,
    pub connector_id: String,
    pub proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    pub remote_address: SocketAddr,
    pub is_removed: Arc<AtomicBool>,
    pub has_response: Arc<AtomicBool>,
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

    has_response_notify: Arc<Notify>,
    really_need_on_response_body_chunk_received_from_target: bool,
}

impl RouterSocketV1 {
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

        let has_response = Arc::new(AtomicBool::new(false));

        let router_socket_id_clone = router_socket_id.clone();
        let has_response_notify = Arc::new(Notify::new());
        let really_need_on_response_body_chunk_received_from_target
            = proxy_listeners.iter().any(|i| i.really_need_on_response_body_chunk_received_from_target());

        let arc_rs = Arc::new(Self {
            route,
            component_name,
            router_socket_id,
            websocket_farm,
            connector_id: connector_instance_id,
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
            idle_read_timeout_ms,
            ping_sent_after_no_write_for_ms,

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

            wss_recv_pipe_join_handle: Arc::new(Mutex::new(None)),

            wss_send_task_tx,
            wss_send_task_join_handle: Arc::new(Mutex::new(None)),

            has_response_notify,

            really_need_on_response_body_chunk_received_from_target,
        });
        // Abort these two handles in Drop
        let wss_recv_pipe_join_handle = tokio::spawn(
            pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
                arc_rs.clone(),
                underlying_wss_rx,
            ));
        let wss_send_task_join_handle = tokio::spawn(
            pipe_and_queue_the_wss_send_task_and_handle_err_chan(
                arc_rs.clone(),
                underlying_wss_tx, wss_send_task_rx, err_chan_rx,
            ));

        // Don't want to let registration handler wait so spawn these val assign somewhere
        let arc_wrapped_clone = arc_rs.clone();
        tokio::spawn(async move {
            let mut g = arc_wrapped_clone.wss_recv_pipe_join_handle.lock().await;
            *g = Some(wss_recv_pipe_join_handle);
            let mut g = arc_wrapped_clone.wss_send_task_join_handle.lock().await;
            *g = Some(wss_send_task_join_handle);
        });

        arc_rs
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

async fn pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
    rs: Arc<RouterSocketV1>,
    mut underlying_wss_rx: SplitStream<WebSocket>,
) {
    let mut local_has_response = rs.has_response.load(SeqCst);
    let mut may_ex: Option<CrankerRouterException> = None;
    let idle_read_timeout_ms = rs.idle_read_timeout_ms;
    let read_notifier = Arc::new(Notify::new());
    let read_notifier_clone = read_notifier.clone();
    let rs_weak = Arc::downgrade(&rs);
    let read_timeout_handle = tokio::spawn(async move {
        loop { // @outer
            match tokio::time::timeout(
                Duration::from_millis(idle_read_timeout_ms as u64),
                read_notifier_clone.notified(),
            ).await {
                Err(_) => {
                    // if strong arc(rs) already end of life then no need to send
                    if let Some(rs) = rs_weak.upgrade() {
                        let _ = rs.wss_send_task_tx.send(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: Cow::from("timeout"),
                        }))).await;
                        let _ = rs.on_error(CrankerRouterException::new("uwss read idle timeout".to_string()));
                    }
                    break; // @outer
                }
                _ => continue
            }
        }
    });
    // @outer
    loop {
        tokio::select! {
            _ = rs.has_response_notify.notified() => {
                trace!("30");
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
                                may_ex = Some(CrankerRouterException::new(format!(
                                    "underlying wss recv err: {:?}.", wss_recv_err
                                )));
                                break; // @outer
                            }
                            Ok(msg) => {
                                read_notifier.notify_waiters();
                                match msg {
                                    Message::Text(txt) => {
                                        if !local_has_response {
                                            let failed_reason = "recv txt bin before handle cli req.".to_string();
                                            may_ex = Some(CrankerRouterException::new(failed_reason));
                                            break; // @outer
                                        }
                                        trace!("31");
                                        may_ex = rs.on_text(txt).await.err();
                                        trace!("32")
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
                                        // break; // @outer
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

    read_timeout_handle.abort_handle().abort();

    if let Some(ex) = may_ex {
        may_ex = None;
        let _ = rs.on_error(ex);
    }

    // recv nothing from underlying wss, implicating it already closed
    debug!("seems router socket normally closed. router_socket_id={}", rs.router_socket_id);
    if !rs.is_removed() {
        // means the is_removed is still false, which is not expected
        let _ = rs.on_error(CrankerRouterException::new(
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
    let ping_sent_after_no_write_for_ms = rs.ping_sent_after_no_write_for_ms;
    let write_notifier = Arc::new(Notify::new());
    let write_notifier_clone = write_notifier.clone();
    let rs_weak = Arc::downgrade(&rs);
    let ping_handle = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(
                Duration::from_millis(ping_sent_after_no_write_for_ms as u64),
                write_notifier_clone.notified(),
            ).await {
                Err(_) => {
                    if let Some(rs) = rs_weak.upgrade() {
                        trace!("80");
                        let _ = rs.wss_send_task_tx.send(Message::Ping("ping".as_bytes().to_vec())).await;
                        trace!("83");
                    } else {
                        trace!("84");
                        return; // ^ async move
                    }
                    trace!("84.3");
                }
                _ => {
                    trace!("84.6");
                    continue;
                }
            }
        }
    });
    loop {
        tokio::select! {
            Ok(crex) = err_chan_rx.recv() => {
                if !rs.is_removed() {
                    error!("err_chan_rx received err: {:?}. router_socket_id={}", crex.reason.as_str(), rs.router_socket_id);
                }
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
                    reason: Cow::from(reason_in_clo_frame)
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
                        write_notifier.notify_waiters();
                        let may_err_msg = msg.err_msg_for_uwss();
                        if let Message::Binary(bin) = msg {
                            rs.bytes_received.fetch_add(bin.len().try_into().unwrap(),SeqCst);
                            if let Err(e) = underlying_wss_tx.send(Message::Binary(bin)).await {
                                let _ = rs.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Message::Text(txt) = msg {
                            rs.bytes_received.fetch_add(txt.len().try_into().unwrap(),SeqCst);
                            if let Err(e) = underlying_wss_tx.send(Message::Text(txt)).await {
                                let _ = rs.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Message::Close(opt_clo_fra) = msg {
                            trace!("85");
                            if let Err(e) = underlying_wss_tx.send(Message::Close(opt_clo_fra)).await {
                                trace!("86");
                                // here failing is expected when is_removed
                                let _ = rs.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                            trace!("87");
                        } else if let Err(e) = underlying_wss_tx.send(msg).await {
                            // ping / pong
                            if !rs.is_removed() {
                                trace!("88");
                                let _ = rs.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            } else {
                                trace!("89");
                                let _ = underlying_wss_tx.close().await;
                                break;
                            }
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
    trace!("89.3");
    ping_handle.abort();
    trace!("89.6");
}

#[derive(Debug)]
pub struct RSv1ClientSideResponseSender {
    pub router_socket_id: String,
    pub bytes_sent: Arc<AtomicI64>,
    pub is_removed: Arc<AtomicBool>,
    pub has_response: Arc<AtomicBool>,

    pub tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    pub tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,

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
        tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    ) -> Self {
        Self {
            router_socket_id,
            bytes_sent,

            is_removed,
            has_response,

            tgt_res_hdr_tx,
            tgt_res_bdy_tx,
            is_tgt_res_hdr_received: AtomicBool::new(false), // 1
            is_tgt_res_hdr_sent: AtomicBool::new(false),     // 2
            is_tgt_res_bdy_received: AtomicBool::new(false), // 3
            is_wss_closed: AtomicBool::new(false),           // 4
        }
    }

    #[inline]
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
        self.tgt_res_hdr_tx.close(); // close it immediately
        self.is_tgt_res_hdr_sent.store(true, SeqCst);
        return res;
    }

    #[inline]
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
        if let Err(_) = self.is_tgt_res_bdy_received.compare_exchange(false, true, AcqRel, Relaxed) {
            trace!("continuous binary to res body to cli res chan");
        }

        let bin_len = bin.len() as i64;
        self.tgt_res_bdy_tx.send(Ok(bin)).await
            .map(|ok| {
                self.bytes_sent.fetch_add(bin_len, SeqCst);
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
        self.cli_side_res_sender.send_target_response_header_text_to_client(txt).await
    }

    async fn on_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        // slightly different from mu cranker router that it will judge the current state of websocket / has_response first
        self.binary_frame_received.fetch_add(1, SeqCst);
        let mut opt_bin_clone_for_listeners = None;

        if self.really_need_on_response_body_chunk_received_from_target {
            // FIXME: Copy vec<u8> is expensive!
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
        // TODO: Handle the reason carefully like mu cranker router
        if self.has_response.load(SeqCst)
            && self.bytes_received.load(Acquire) == 0
        {
            trace!("43");
            if code == 1011 {
                trace!("44");
                // 1011 indicates that a server is terminating the connection because
                // it encountered an unexpected condition that prevented it from
                // fulfilling the request.
                let ex = CrankerRouterException::new("ws code 1011. should res to cli 502".to_string());
                // TODO: Handle carefully in on_error
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            } else if code == 1008 {
                trace!("45");
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
            trace!("46");
            // let mut is_tgt_chan_close_as_expected = true;
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
                let _ = self.tgt_res_hdr_tx.send(Err(ex.clone())).await;
                let _ = self.tgt_res_bdy_tx.send(Err(ex.clone())).await;
                let may_ex = self.on_error(ex); // ?Necessary
                total_err = exceptions::compose_ex(total_err, may_ex);
            }
            // I think here same as asyncHandle.complete(null) in mu cranker router
            self.tgt_res_hdr_rx.close();
            self.tgt_res_bdy_rx.close();
        }
        trace!("49");

        let may_ex = self.raise_completion_event();
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
                let _ = self.raise_completion_event();
                ok
            })
            .map_err(|se| CrankerRouterException::new(format!(
                "rare error that failed to send error to err chan: {:?}", se
            )))

        // the version below MAY causing recursive ex creation, test it later
        // let err_clone = err.clone();
        // return match self.err_chan_tx.send_blocking(err) {
        //     Ok(_) => Err(err_clone),
        //     Err(se) => {
        //         Err(err_clone.plus_string(format!(
        //             "rare error that failed to send error to err chan: {:?}", se
        //         )))
        //     }
        // }
    }
}

impl ProxyInfo for RouterSocketV1 {
    fn is_catch_all(&self) -> bool {
        self.route.eq("*")
    }

    fn connector_instance_id(&self) -> String {
        self.connector_id.clone()
    }

    fn service_address(&self) -> SocketAddr {
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
    fn component_name(&self) -> String {
        self.component_name.clone()
    }

    fn connector_id(&self) -> String {
        self.connector_id.clone()
    }

    #[inline]
    fn is_removed(&self) -> bool {
        return self.is_removed.load(Acquire);
    }

    fn cranker_version(&self) -> &'static str {
        return CRANKER_V_1_0;
    }

    fn raise_completion_event(&self) -> Result<(), CrankerRouterException> {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            wsf.remove_websocket_in_background(
                self.route.clone(), self.router_socket_id.clone(), self.is_removed.clone(),
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

    fn is_dark_mode_on(&self, dark_hosts: &HashSet<DarkHost>) -> bool {
        let remote_ip_addr = self.remote_address.ip();
        dark_hosts.iter().any(|i| i.same_host(remote_ip_addr))
    }


    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           orig_uri: &OriginalUri,
                           cli_headers: &HeaderMap,
                           addr: &SocketAddr,
                           opt_body: Option<BodyDataStream>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        // 0. if is removed then should not run into this method (fast)
        if self.is_removed() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a is_removed router socket. router_socket_id={}", &self.router_socket_id
            )));
        }
        self.has_response_notify.notify_waiters();
        self.has_response.store(true, SeqCst);
        let current_time_millis = time_utils::current_time_millis();
        self.socket_wait_in_millis.fetch_add(current_time_millis, SeqCst);
        self.client_req_start_ts.store(current_time_millis, SeqCst);
        self.duration_millis.store(-current_time_millis, SeqCst);
        // 1. Cli header processing (fast)
        trace!("1");
        let mut hdr_to_tgt = HeaderMap::new();
        set_target_request_headers(cli_headers, &mut hdr_to_tgt, &app_state, http_version, addr, orig_uri);
        // 2. Build protocol request line / protocol header frame without endmarker (fast)
        trace!("2");
        let request_line = create_request_line(method, &orig_uri);
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

        // 4. Send protocol header frame (slow, blocking)
        trace!("4");
        self.wss_send_task_tx.send(Message::Text(cranker_req)).await
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send cli req hdr to tgt: {:?}", e
            )))?; // fast fail

        // 5. Pipe cli req body to underlying wss (slow, blocking)
        trace!("5");
        if let Some(mut body) = opt_body {
            let mut body = body.map_err(|e| CrankerRouterException::new(format!("{:?}", e)));
            trace!("5.5");
            while let Some(res_bdy_chunk) = body.next().await {
                let bytes = res_bdy_chunk?;
                // fast fail
                trace!("6");
                trace!("8");
                for i in self.proxy_listeners.iter() {
                    i.on_request_body_chunk_sent_to_target(self.as_ref(), &bytes)?; // fast fail
                }
                self.wss_send_task_tx.send(Message::Binary(bytes.to_vec())).await
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

        let mut cranker_res: Option<CrankerProtocolResponse> = None;
        // if the wss_recv_pipe chan is EMPTY AND CLOSED then err will come from this recv()
        trace!("22");
        if let hdr_res = self.tgt_res_hdr_rx.recv().await
            .map_err(|recv_err|
                CrankerRouterException::new(format!(
                    "rare ex seems nothing received from tgt res hdr chan and it closed: {:?}. router_socket_id={}",
                    recv_err, self.router_socket_id
                ))
            )??  // fast fail
        {
            let message_to_apply = hdr_res;
            cranker_res = Some(CrankerProtocolResponse::new(message_to_apply)?); // fast fail
        }

        if cranker_res.is_none() {
            // Should never run into this line
            return Err(CrankerRouterException::new(
                "rare ex failed to build response from protocol response".to_string()
            ));
        }

        let cranker_res = cranker_res.unwrap();
        let status_code = cranker_res.status;
        let res_builder = cranker_res.build()?; // fast fail

        for i in self.proxy_listeners.iter() {
            i.on_before_responding_to_client(self.as_ref())?;
            i.on_after_target_to_proxy_headers_received(
                self.as_ref(), status_code, res_builder.headers_ref(),
            )?;
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

fn create_request_line(method: &Method, orig_uri: &OriginalUri) -> String {
    // Example: GET /example/hello?nonce=1023 HTTP/1.1
    let mut res = String::new();
    res.push_str(method.as_str());
    res.push(' ');
    if let Some(paq) = orig_uri.path_and_query() {
        res.push_str(paq.as_str());
    }
    res.push_str(" HTTP/1.1");
    res
}


// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
pub async fn harvest_router_socket(
    wss: WebSocket,
    app_state: ACRState,
    connector_id: String,
    component_name: String,
    cranker_version: &'static str,
    domain: String, // V3 only
    route: String,
    addr: SocketAddr,
)
{
    if cranker_version == CRANKER_V_1_0 {
        let (wss_tx, wss_rx) = wss.split();
        let router_socket_id = Uuid::new_v4().to_string();
        let weak_wsf = Arc::downgrade(&app_state.websocket_farm);
        let rs = RouterSocketV1::new(
            route.clone(),
            component_name.clone(),
            router_socket_id.clone(),
            weak_wsf,
            connector_id.clone(),
            addr,
            wss_tx,
            wss_rx,
            app_state.config.proxy_listeners.clone(),
            app_state.config.idle_read_timeout_ms,
            app_state.config.ping_sent_after_no_write_for_ms,
        );
        info!("Connector registered! connector_id: {}, router_socket_id: {}", connector_id, router_socket_id);
        app_state
            .websocket_farm
            .clone()
            .add_socket_in_background(rs);
    } else {
        error!("not supported cranker version: {}", cranker_version);
        let _ = wss.close();
    }
    {
        // Prepare for some counter
        // let write_guard = state.clone();
        // let _ = write_guard.counter.fetch_add(1, SeqCst);
    }
}

trait MessageIdentifier {
    fn identifier(&self) -> &'static str;
    fn err_msg_for_uwss(&self) -> &'static str;
}

impl MessageIdentifier for Message {
    #[inline]
    fn identifier(&self) -> &'static str {
        match self {
            Message::Text(_) => "txt",
            Message::Binary(_) => "bin",
            Message::Ping(_) => "ping",
            Message::Pong(_) => "pong",
            Message::Close(_) => "close"
        }
    }

    fn err_msg_for_uwss(&self) -> &'static str {
        match self {
            Message::Text(_) => "err sending txt to uwss",
            Message::Binary(_) => "err sending bin to uwss",
            Message::Ping(_) => "err sending ping to uwss",
            Message::Pong(_) => "err sending pong to uwss",
            Message::Close(_) => "err sending close to uwss"
        }
    }
}
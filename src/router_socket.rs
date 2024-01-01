use std::borrow::Cow;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, SeqCst};

use async_channel::{Receiver, Sender};
use axum::{async_trait, Error};
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, Method, Response, StatusCode};
use axum::http::uri::PathAndQuery;
use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use futures::{Sink, SinkExt, StreamExt, TryFutureExt};
use futures::stream::{SplitSink, SplitStream};
use log::{debug, error, info, trace, warn};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{exceptions, REPRESSED_HEADERS, TSCRState};
use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::exceptions::CrankerRouterException;

const RESPONSE_HEADERS_TO_NOT_SEND_BACK: &[&str] = &["server"];
const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub trait WebSocketListener: Send + Sync {
    // this should be done in on_upgrade, so ignore it
    // fn on_connect(&self, wss_tx: SplitSink<WebSocket, Message>) -> Result<(), CrankerRouterException>;
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException>;
    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        let decoded = std::str::from_utf8(ping_msg.as_slice()).unwrap_or("INVALID PONG MSG");
        debug!("pinged: {}", decoded);
        Ok(())
    }
    async fn on_pong(&self, pong_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        let decoded = std::str::from_utf8(pong_msg.as_slice()).unwrap_or("INVALID PONG MSG");
        debug!("ponged: {}", decoded);
        Ok(())
    }
    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException>;

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        error!("error: {:?}", err); // FIXME: Swallow the error by default
        Ok(())
    }
}

#[async_trait]
pub trait RouterSocket: Send + Sync {
    fn should_remove(&self) -> bool;

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
    // pub web_socket_farm: Option<Weak<Mutex<WebSocketFarm>>>,
    pub connector_instance_id: String,
    // pub proxy_listeners: Vec<&'static dyn ProxyListener>,
    pub remote_address: SocketAddr,
    pub should_remove: Arc<AtomicBool>,
    // previously `is_removed`, but actually we can't remove ourself from the mpmc channel so this is just a marker
    pub has_response: Arc<AtomicBool>,
    // this should be shared because before wss acquired to handle, ping/pong/close should be handling somewhere
    pub bytes_received: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub binary_frame_received: AtomicI64,
    pub socket_wait_in_millis: AtomicI64,
    pub error: Mutex<Option<CrankerRouterException>>,
    pub duration_millis: AtomicI64,

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
    wss_recv_pipe_join_handle: JoinHandle<()>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_join_handle: JoinHandle<()>,
}

impl RouterSocketV1 {
    // err should use a separated channel to communicate between wss side and router socket side
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               connector_instance_id: String,
               remote_address: SocketAddr,
               underlying_wss_tx: SplitSink<WebSocket, Message>,
               underlying_wss_rx: SplitStream<WebSocket>,
    ) -> Self {
        let (tgt_res_hdr_tx, tgt_res_hdr_rx) = async_channel::unbounded();
        let (tgt_res_bdy_tx, tgt_res_bdy_rx) = async_channel::unbounded();
        let (wss_send_task_tx, mut wss_send_task_rx) = async_channel::unbounded();
        let (err_chan_tx, err_chan_rx) = async_channel::unbounded();

        let err_chan_tx_clone = err_chan_tx.clone();
        let tgt_res_hdr_tx_clone = tgt_res_hdr_tx.clone();
        let tgt_res_bdy_tx_clone = tgt_res_bdy_tx.clone();
        let router_socket_id_clone = router_socket_id.clone();
        let wss_send_task_join_handle = tokio::spawn(pipe_and_queue_the_wss_send_task_and_handle_err_chan(
            router_socket_id_clone,
            underlying_wss_tx, wss_send_task_rx, err_chan_tx_clone, err_chan_rx, tgt_res_hdr_tx_clone, tgt_res_bdy_tx_clone,
        ));

        let (wss_recv_pipe_tx, wss_recv_pipe_rx) = async_channel::unbounded();
        let err_chan_tx_clone = err_chan_tx.clone();
        let router_socket_id_clone = router_socket_id.clone();
        let should_remove = Arc::new(AtomicBool::new(false));
        let should_remove_clone = should_remove.clone();
        let has_response = Arc::new(AtomicBool::new(false));
        let has_response_clone = has_response.clone();
        let wss_send_task_tx_clone = wss_send_task_tx.clone();
        let wss_recv_pipe_join_handle = tokio::spawn(pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
            router_socket_id_clone, has_response_clone, should_remove_clone,
            wss_send_task_tx_clone, // use for ping/pong before socket handle cli req
            underlying_wss_rx, wss_recv_pipe_tx, err_chan_tx_clone,
        ));
        let router_socket_id_clone = router_socket_id.clone();

        Self {
            route,
            component_name,
            router_socket_id,
            connector_instance_id,
            remote_address,

            should_remove,
            has_response,
            bytes_received: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            binary_frame_received: AtomicI64::new(0),
            socket_wait_in_millis: AtomicI64::new(-1),
            error: Mutex::new(None),
            duration_millis: AtomicI64::new(-1),

            err_chan_tx,

            cli_side_res_sender: RSv1ClientSideResponseSender::new(router_socket_id_clone, tgt_res_hdr_tx.clone(), tgt_res_bdy_tx.clone()),

            tgt_res_hdr_tx,
            tgt_res_hdr_rx,
            tgt_res_bdy_tx,
            tgt_res_bdy_rx,

            wss_recv_pipe_rx,
            wss_recv_pipe_join_handle,

            wss_send_task_tx,
            wss_send_task_join_handle,
        }
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
        self.tgt_res_hdr_tx.close();
        self.tgt_res_hdr_rx.close();
        self.tgt_res_bdy_tx.close();
        self.tgt_res_bdy_rx.close();

        self.wss_recv_pipe_rx.close();
        self.wss_recv_pipe_join_handle.abort();

        self.wss_send_task_tx.close();
        self.wss_send_task_join_handle.abort();
    }
}

async fn pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
    router_socket_id: String,
    has_response: Arc<AtomicBool>,
    should_remove: Arc<AtomicBool>,
    wss_send_task_tx: Sender<Message>,
    mut underlying_wss_rx: SplitStream<WebSocket>,
    wss_recv_pipe_tx: Sender<Message>,
    err_chan_tx: Sender<CrankerRouterException>,
) {
    while let Some(res_msg) = underlying_wss_rx.next().await {
        match res_msg {
            Err(wss_recv_err) => {
                let _ = wss_recv_pipe_tx.close(); // close or drop?
                let _ = err_chan_tx.send_blocking(CrankerRouterException::new(format!(
                    "underlying wss recv err: {:?}. router_socket_id={}", wss_recv_err, router_socket_id
                )));
                break;
            }
            Ok(msg) => {
                if has_response.load(SeqCst) {
                    if let Err(e) = wss_recv_pipe_tx.send(msg).await {
                        let _ = err_chan_tx.send_blocking(CrankerRouterException::new(format!(
                            "rare ex that failed to pipe underlying wss rx to wss recv pipe: {:?}. router_socket_id={}", e, router_socket_id
                        )));
                        break;
                    }
                } else {
                    match msg {
                        Message::Text(_) | Message::Binary(_) => {
                            let failed_reason = format!("recv txt or bin before handle cli req. router_socket_id={}", router_socket_id);
                            error!("{}", failed_reason);
                            should_remove.store(true, SeqCst);
                            let _ = err_chan_tx.send_blocking(CrankerRouterException::new(failed_reason));
                            break;
                        }
                        Message::Ping(ping_hello) => {
                            let _ = wss_send_task_tx.send(Message::Pong(ping_hello));
                        }
                        Message::Pong(_) => {
                            // NOP
                        }
                        Message::Close(opt_clo_fra) => {
                            // wss closed prior to handle req
                            let failed_reason = format!("recv close before handle cli req. router_socket_id={}", router_socket_id);
                            error!("{}", failed_reason);
                            should_remove.store(true, SeqCst);
                            let _ = err_chan_tx.send_blocking(CrankerRouterException::new(failed_reason));
                            break;
                        }
                    }
                }
            }
        }
    }

    // recv nothing from underlying wss, implicating it already closed
    should_remove.store(true, SeqCst);
    let _ = wss_recv_pipe_tx.close(); // close or drop?
    let _ = err_chan_tx.send_blocking(CrankerRouterException::new(format!(
        "underlying wss already closed. router_socket_id={}", router_socket_id
    )));
}

async fn pipe_and_queue_the_wss_send_task_and_handle_err_chan(
    router_socket_id: String,
    mut underlying_wss_tx: SplitSink<WebSocket, Message>,
    wss_send_task_rx: Receiver<Message>,
    err_chan_tx: Sender<CrankerRouterException>,
    err_chan_rx: Receiver<CrankerRouterException>,

    // For end cli res early
    tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
    tgt_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
) {
    loop {
        tokio::select! {
            Ok(crex) = err_chan_rx.recv() => {
                error!("err_chan_rx received err: {:?}. router_socket_id={}", crex.reason.as_str(), router_socket_id);
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
                // 2. Close the res to cli
                let _ = tgt_res_hdr_tx.send(Err(crex.clone())).await;
                let _ = tgt_res_bdy_tx.send(Err(crex.clone())).await;
                tgt_res_hdr_tx.close();
                tgt_res_bdy_tx.close();
                break;
            }
            recv_res = wss_send_task_rx.recv() => {
                match recv_res {
                    Ok(msg) => {
                        if let Err(e) = underlying_wss_tx.send(msg).await {
                            let _ = err_chan_tx.send_blocking(CrankerRouterException::new(format!("{:?}", e)));
                        }
                    }
                    Err(recv_err) => { // Indicates the wss_send_task_rx is EMPTY AND CLOSED
                        debug!("the wss_send_task_rx is EMPTY AND CLOSED. router_socket_id={}", router_socket_id);
                        break;
                    }
                }
            }
        }
    }
}

pub struct RSv1ClientSideResponseSender {
    pub router_socket_id: String,

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
        tgt_res_hdr_tx: Sender<Result<String, CrankerRouterException>>,
        tgr_res_bdy_tx: Sender<Result<Vec<u8>, CrankerRouterException>>,
    ) -> Self {
        Self {
            router_socket_id,
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
            return Err(CrankerRouterException::new(format!(
                "res header already handled! router_socket_id={}", self.router_socket_id
            )));
        }

        if txt.len() > 64 * 1024 {
            return Err(CrankerRouterException::new(format!(
                "response header too large: over 64 * 1024 bytes. router_socket_id={}", self.router_socket_id
            )));
        }

        let res = self.tgt_res_hdr_tx.send(Ok(txt)).await
            .map_err(|e| {
                let failed_reason = format!(
                    "rare ex: failed to send txt to cli res chan: {:?}. router_socket_id={}", e, self.router_socket_id
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

        self.tgr_res_bdy_tx.send(Ok(bin)).await
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
    // NOT IN USE YET!
    async fn on_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        if let Err(e) = self.cli_side_res_sender.send_target_response_header_text_to_client(txt).await {
            return self.on_error(e);
        }
        Ok(())
    }

    // NOT IN USE YET!
    async fn on_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        if let Err(e) = self.cli_side_res_sender.send_target_response_body_binary_fragment_to_client(bin).await {
            return self.on_error(e);
        }
        Ok(())
    }

    // when receiving close frame (equals client close ?)
    // Theoretically, tungstenite should already reply a close frame to connector, but in practice chances are it wouldn't
    async fn on_close(&self, opt_close_frame: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        // TODO: ProxyListener stuff
        let mut code = 4000u16; // 4000-4999 is reserved
        let mut reason = "N/A".to_string();
        let mut total_err: Option<CrankerRouterException> = None;
        if let Some(clo_msg) = opt_close_frame {
            // WebSocket status code: https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1
            code = clo_msg.code;
            reason = clo_msg.reason.to_string();
        }

        // TODO: Handle the reason carefully like mu cranker router
        if self.has_response.load(SeqCst) {
            if code == 1011 {
                // 1011 indicates that a server is terminating the connection because
                // it encountered an unexpected condition that prevented it from
                // fulfilling the request.
                let ex = CrankerRouterException::new("ws code 1011. should res to cli 502".to_string());
                // TODO: Handle carefully in on_error
                let may_ex = self.on_error(ex);
                total_err = exceptions::compose_ex(total_err, may_ex);
            } else if code == 1008 {
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
            // let mut is_tgt_chan_close_as_expected = true;
            if code == 1000 {
                // 1000 indicates a normal closure, meaning that the purpose for
                // which the connection was established has been fulfilled.

                // I think here same as asyncHandle.complete(null) in mu cranker router
                self.tgt_res_hdr_rx.close();
                self.tgt_res_bdy_rx.close();

                // is_tgt_chan_close_as_expected = self.tgt_res_hdr_rx.close() && is_tgt_chan_close_as_expected;
                // is_tgt_chan_close_as_expected = self.tgt_res_bdy_rx.close() && is_tgt_chan_close_as_expected;
            } else {
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

        self.should_remove.store(true, SeqCst);

        if total_err.is_some() {
            return Err(total_err.unwrap());
        }
        Ok(())
    }

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        // The actual heavy burden is done in `pipe_and_queue_the_wss_send_task_and_handle_err_chan`
        self.err_chan_tx.send_blocking(err)
            .map_err(|se| CrankerRouterException::new(format!(
                "rare error that failed to send error to err chan: {:?}", se
            )))
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    fn should_remove(&self) -> bool {
        return self.should_remove.load(SeqCst);
    }

    async fn on_client_req(self: Arc<Self>,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<Receiver<Result<Bytes, CrankerRouterException>>>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        // 0. if should remove then should not run into this method
        if self.should_remove() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a should_remove router socket. router_socket_id={}", &self.router_socket_id
            )));
        }
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
            while let Ok(r) = body.recv().await {
                debug!("6");
                match r {
                    Ok(b) => {
                        debug!("8");
                        self.wss_send_task_tx.send(Message::Binary(b.to_vec())).await
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

        let mut cranker_res: Option<CrankerProtocolResponse> = None;

        // if the wss_recv_pipe chan is EMPTY AND CLOSED then err will come from this recv()
        self.has_response.store(true, SeqCst);
        let another_self = self.clone();
        tokio::spawn(listen_tgt_res_async(another_self));

        if let hdr_res = self.tgt_res_hdr_rx.recv().await
            .map_err(|recve|
                CrankerRouterException::new(format!(
                    "seems nothing received from tgt res hdr chan and it closed. router_socket_id={}", self.router_socket_id
                ))
            )?
            .map_err(|e| e.plus_str(self.router_socket_id.as_str()))?
        {
            cranker_res = Some(CrankerProtocolResponse::new(hdr_res)?);
        }

        if cranker_res.is_none() {
            return Ok(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(Body::new(format!(
                "failed to build response from protocol response. router_socket_id={}", self.router_socket_id
            ))).unwrap());
        }
        let res_builder = cranker_res.unwrap().build();
        let wrapped_stream = self.tgt_res_bdy_rx.clone();
        let stream_body = Body::from_stream(wrapped_stream);
        res_builder
            ?.body(stream_body)
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
) {
    let (wss_tx, wss_rx) = wss.split();
    let router_socket_id = Uuid::new_v4().to_string();
    let rs = Arc::new(RouterSocketV1::new(
        route.clone(),
        component_name.clone(),
        router_socket_id.clone(),
        connector_id.clone(),
        addr,
        wss_tx,
        wss_rx,
    ));
    info!("Connector registered! connector_id: {}, router_socket_id: {}", connector_id, router_socket_id);
    let _ = state
        .route_to_socket_chan
        .entry(route.clone())
        .or_insert(async_channel::unbounded())
        .value()
        .0
        .send(rs.clone())
        .await;
    if let Entry::Occupied(notifier) = state.route_notifier.entry(route.clone()) {
        notifier.get().notify_waiters();
    }
    {
        // Prepare for some counter
        let write_guard = state.clone();
        let _ = write_guard.counter.fetch_add(1, SeqCst);
    }
}

async fn websocket_exchange(
    connector_id: String,
    router_socket_id: String,
    mut from_rs_to_ws_rx: Receiver<Message>,
    from_ws_to_rs_tx: Sender<Message>,
    mut wss_tx: SplitSink<WebSocket, Message>,
    mut wss_rx: SplitStream<WebSocket>,
    mut err_chan_from_rs: Receiver<CrankerRouterException>,
)
{
    debug!("Listening on connector ws message!");
    let (notify_ch_close_tx, mut notify_ch_close_rx) = async_channel::unbounded::<()>();
    let notify_ch_close_tx_clone = notify_ch_close_tx.clone();
    let msg_counter_lib = AtomicUsize::new(0);

    // queue the task to wss_tx
    let (wss_send_task_tx, mut wss_send_task_rx) = async_channel::unbounded::<Message>();
    {
        let notify_ch_close_tx = notify_ch_close_tx.clone();
        let from_ws_to_rs_tx = from_ws_to_rs_tx.clone();
        let router_socket_id = router_socket_id.clone();
        tokio::spawn(async move {
            while let Ok(msg) = wss_send_task_rx.recv().await {
                // if let Message::Close(opt_something) = msg {
                //     debug!("++Sending close frame to connector");
                //     let _ = wss_tx.send(Message::Close(opt_something));
                // } else {
                let _ = wss_tx.send(msg).await.map_err(|e| async {
                    let _ = from_ws_to_rs_tx.send(Message::Close(None)).await;
                    let _ = notify_ch_close_tx.send(()).await;
                });
                // }
            }
            drop(notify_ch_close_tx);
            drop(from_ws_to_rs_tx);
            info!("end of wss_send_task for router_socket_id={}", router_socket_id);
        });
    }

    loop {
        tokio::select! {
                Ok(should_stop) = notify_ch_close_rx.recv() => {
                    info!("should stop looping now! router_socket_id: {}", router_socket_id);
                    break;
                }
                Ok(err) = err_chan_from_rs.recv() => {
                    error!(
                        "exception received in websocket_exchange: {:?}. connector_id: {}, router_socket_id: {}",
                        err, connector_id, router_socket_id
                    );
                    notify_ch_close_tx.send(()).await;
                }
                Ok(to_ws) = from_rs_to_ws_rx.recv() => {
                    match to_ws {
                        Message::Text(txt) => {
                            wss_send_task_tx.send(Message::Text(txt)).await;
                        }
                        Message::Binary(bin) => {
                            wss_send_task_tx.send(Message::Binary(bin)).await;
                        }
                        Message::Ping(msg) | Message::Pong(msg) => {
                            error!("unexpected message comes from rs_to_ws chan: {:?}. connector_id: {}, router_socket_id: {}",
                                    msg , connector_id, router_socket_id
                            )
                        }
                        Message::Close(msg) => {
                            error!("unexpected message comes from rs_to_ws chan: {:?}. connector_id: {}, router_socket_id: {}",
                                    msg , connector_id, router_socket_id
                            )
                        }
                    }
                }
                from_wss_nullable = wss_rx.next() => {
                    match from_wss_nullable {
                        None => {
                            warn!(
                                "Receive None from wss_rx, seems wss closed! connector_id: {}, router_socket_id: {}",
                                connector_id, router_socket_id
                            );
                            let _ = from_ws_to_rs_tx.send(Message::Close(None)).await;
                            let _ = notify_ch_close_tx_clone.send(()).await;
                        }
                        Some(Ok(Message::Ping(_))) => {
                            let _ = wss_send_task_tx.send(Message::Pong(Vec::new())).await;
                        }
                        Some(may_err) => {
                            msg_counter_lib.fetch_add(1,SeqCst);
                            // FIXME: Let's believe cloning these channels equals cloning their inner Arc, which is super cheap!
                            let from_ws_to_rs_tx = from_ws_to_rs_tx.clone();
                            let notify_ch_close_tx = notify_ch_close_tx.clone();
                            let wss_send_task_tx = wss_send_task_tx.clone();
                            // tokio::spawn(
                                pipe_msg_from_wss_to_router_socket(
                                    connector_id.clone(), router_socket_id.clone(),
                                    may_err, from_ws_to_rs_tx, notify_ch_close_tx, wss_send_task_tx
                                ).await;
                            // );
                        }
                    }
                }
            }
    }
    // stop receiving from router socket
    from_rs_to_ws_rx.close();
    // drop receiver from router socket
    drop(from_rs_to_ws_rx);
    // drop sender to router socket
    drop(from_ws_to_rs_tx);
    drop(notify_ch_close_tx_clone);
    debug!("END OF Listening on connector ws message! msg count lib: {}", msg_counter_lib.load(SeqCst));
}

async fn pipe_msg_from_wss_to_router_socket(
    connector_id: String,
    router_socket_id: String,
    may_err: Result<Message, Error>,
    from_ws_to_rs_tx: Sender<Message>,
    notify_ch_close_tx: Sender<()>,
    wss_send_task_tx: Sender<Message>,
)
{
    match may_err {
        Err(e) => {
            error!(
                "Error from wss: {:?}. connector_id: {}, router_socket_id: {}",
                e, connector_id, router_socket_id
            );
            notify_ch_close_tx.send(()).await;
        }
        Ok(msg) => {
            match msg {
                Message::Text(txt) => {
                    from_ws_to_rs_tx.send(Message::Text(txt)).await;
                }
                Message::Binary(bin) => {
                    from_ws_to_rs_tx.send(Message::Binary(bin)).await;
                }
                Message::Pong(_) => debug!("ponged!"),
                Message::Close(opt_close_frame) => {
                    if let Some(clo_fra) = opt_close_frame {
                        warn!(
                            "Closing from connector: code={}, {:?}. connector_id: {}, router_socket_id: {}",
                            clo_fra.code, clo_fra.reason, connector_id, router_socket_id
                        );
                    }
                    // let _ = wss_send_task_tx.send(Message::Close(None)).await;
                    debug!("Sending close to connector");
                    let _ = from_ws_to_rs_tx.send(Message::Close(None)).await;
                    let _ = notify_ch_close_tx.send(()).await;
                }
                _ => {}
            }
        }
    }
}

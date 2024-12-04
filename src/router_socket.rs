use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use async_channel::Receiver;
use async_stream::__private::AsyncStream;
use axum::async_trait;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method, Response, Version};
use axum_core::body::BodyDataStream;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, trace};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::dark_host::DarkHost;
use crate::exceptions::{CrankerRouterException, CrexKind};
use crate::route_identify::RouteIdentify;
use crate::router_socket_v1::RouterSocketV1;
use crate::router_socket_v3::RouterSocketV3;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};
use crate::websocket_listener::WebSocketListener;
use crate::{ACRState, CRANKER_V_1_0, CRANKER_V_3_0};

pub const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

/// To do exchange on a WebSocket and proxy requests
/// from client to connector on the target service side
#[async_trait]
pub trait RouterSocket: Send + Sync + RouteIdentify {
    fn component_name(&self) -> String;
    fn connector_id(&self) -> String;
    fn is_removed(&self) -> bool;
    fn get_is_removed_arc_atomic_bool(&self) -> Arc<AtomicBool>;

    fn cranker_version(&self) -> &'static str;

    fn raise_completion_event(&self, _: Option<ClientRequestIdentifier>) -> Result<(), CrankerRouterException>;

    /// Not used in V3
    fn is_dark_mode_on(&self, _: &HashSet<DarkHost>) -> bool;

    /// For V3. V1 socket can only serve one request in the whole lifecycle.
    fn inflight_count(&self) ->i32;

    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           original_uri: &OriginalUri,
                           cli_headers: &HeaderMap,
                           addr: &SocketAddr,
                           opt_body: Option<BodyDataStream>,
    ) -> Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException>;

    async fn send_ws_msg_to_uwss(self: Arc<Self>, message: Message) -> Result<(), CrankerRouterException>;
    async fn terminate_all_conn(self: Arc<Self>, opt_crex: Option<CrankerRouterException>) -> Result<(), CrankerRouterException>;

    fn inc_bytes_received_from_cli(&self, byte_count: i32);

    fn try_provide_general_error(&self, opt_crex: Option<CrankerRouterException>) -> Result<(), CrankerRouterException>;

    fn get_opt_arc_websocket_farm(&self) -> Option<Arc<WebSocketFarm>>;
}

/// For V3 multiplexed request contexts
#[derive(Debug, Clone)]
pub struct ClientRequestIdentifier {
    pub request_id: i32,
}

pub(crate) async fn pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
    rs: Arc<dyn RouterSocket>,
    wss_listener: Arc<dyn WebSocketListener>,
    mut underlying_wss_rx: SplitStream<WebSocket>,
)
{
    let mut may_ex: Option<CrankerRouterException> = None;
    let idle_read_timeout_ms = wss_listener.get_idle_read_timeout_ms();
    let read_notifier = Arc::new(Notify::new());
    let read_notifier_clone = read_notifier.clone();
    let rs_weak = Arc::downgrade(&rs);
    let wss_listener_clone = wss_listener.clone();
    let read_timeout_handle = tokio::spawn(async move {
        loop { // @outer
            match tokio::time::timeout(
                Duration::from_millis(idle_read_timeout_ms as u64),
                read_notifier_clone.notified(),
            ).await {
                Err(_) => {
                    // if strong arc(rs) already end of life then no need to send
                    if let Some(rs) = rs_weak.upgrade() {
                        let _ = rs.send_ws_msg_to_uwss(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: Cow::from("timeout"),
                        }))).await;
                        let _ = wss_listener_clone.on_error(
                            CrankerRouterException::new(
                                "uwss read idle timeout".to_string()
                            ).with_err_kind(CrexKind::Timeout_0001)
                        );
                    }
                    break; // @outer
                }
                _ => continue
            }
        }
    });
    // @outer
    while let Some(res_msg) = underlying_wss_rx.next().await {
        match res_msg {
            Err(wss_recv_err) => {
                may_ex = Some(CrankerRouterException::new(format!(
                    "underlying wss recv err: {:?}.", wss_recv_err
                )));
                break; // @outer
            }
            Ok(msg) => {
                // mimic mu base websocket that on no matter what message is
                // received the timeout on read will be reset
                read_notifier.notify_waiters();
                match msg {
                    Message::Text(txt) => {
                        trace!("31");
                        may_ex = wss_listener.on_text(txt).await.err();
                        trace!("32")
                    }
                    Message::Binary(bin) => {
                        may_ex = wss_listener.on_binary(bin).await.err();
                    }
                    Message::Ping(ping_hello) => {
                        may_ex = wss_listener.on_ping(ping_hello).await.err();
                    }
                    Message::Pong(pong_msg) => {
                        may_ex = wss_listener.on_pong(pong_msg).await.err();
                    }
                    Message::Close(opt_clo_fra) => {
                        may_ex = wss_listener.on_close(opt_clo_fra).await.err();
                    }
                }
                if let Some(ex) = may_ex {
                    may_ex = None;
                    let _ = wss_listener.on_error(ex);
                }
            }
        }
    }

    read_timeout_handle.abort();

    if let Some(ex) = may_ex {
        let _ = wss_listener.on_error(ex);
    }

    // recv nothing from underlying wss, implicating it already closed
    debug!("seems router socket already closed. router_socket_id={}", rs.router_socket_id());
    if !rs.is_removed() {
        // means the is_removed is still false, which is not expected
        let _ = wss_listener.on_error(CrankerRouterException::new(
            "underlying wss already closed but still is_removed = false".to_string()
        ));
    }
}

pub(crate) async fn pipe_and_queue_the_wss_send_task_and_handle_err_chan(
    rs: Arc<dyn RouterSocket>,
    wss_listener: Arc<dyn WebSocketListener>,
    mut underlying_wss_tx: SplitSink<WebSocket, Message>,
    wss_send_task_rx: Receiver<Message>,
    err_chan_rx: Receiver<CrankerRouterException>,
)
{
    let ping_sent_after_no_write_for_ms = wss_listener.get_ping_sent_after_no_write_for_ms();
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
                        let _ = rs.send_ws_msg_to_uwss(Message::Ping("ping".into())).await;
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
                    error!("err_chan_rx received err: {:?}. router_socket_id={}", crex.reason.as_str(), rs.router_socket_id());
                    let _ = rs.try_provide_general_error(Some(crex.clone()));
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
                    if let Some(wsf) = rs.get_opt_arc_websocket_farm() {
                        wsf.clone().remove_router_socket_in_background(rs.route(), rs.router_socket_id(), rs.get_is_removed_arc_atomic_bool().clone());
                    }
                }
                // 3. Close the res to cli
                let _ = rs.terminate_all_conn(Some(crex.clone())).await;
                break;
            }
            recv_res = wss_send_task_rx.recv() => {
                match recv_res {
                    Ok(msg) => {
                        write_notifier.notify_waiters();
                        let may_err_msg = msg.err_msg_for_uwss();
                        if let Message::Binary(bin) = msg {
                            rs.inc_bytes_received_from_cli(bin.len().try_into().unwrap());
                            if let Err(e) = underlying_wss_tx.send(Message::Binary(bin)).await {
                                let _ = wss_listener.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Message::Text(txt) = msg {
                            rs.inc_bytes_received_from_cli(txt.len().try_into().unwrap());
                            if let Err(e) = underlying_wss_tx.send(Message::Text(txt)).await {
                                let _ = wss_listener.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Message::Close(opt_clo_fra) = msg {
                            trace!("85");
                            if let Err(e) = underlying_wss_tx.send(Message::Close(opt_clo_fra)).await {
                                trace!("86");
                                // here failing is expected when is_removed
                                let _ = wss_listener.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                            trace!("87");
                        } else if let Err(e) = underlying_wss_tx.send(msg).await {
                            // ping / pong
                            if !rs.is_removed() {
                                trace!("88");
                                let _ = wss_listener.on_error(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            } else {
                                trace!("89");
                                let _ = underlying_wss_tx.close().await;
                                break;
                            }
                        }
                    }
                    Err(_recv_err) => { // Indicates the wss_send_task_rx is EMPTY AND CLOSED
                        debug!("the wss_send_task_rx is EMPTY AND CLOSED. router_socket_id={}", rs.router_socket_id());
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

pub(crate) fn create_request_line(method: &Method, orig_uri: &OriginalUri) -> String {
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

/// This function deals with a single websocket connection, i.e., a single
/// connected client / user, for which we will spawn two independent tasks (for
/// receiving / sending chat messages).
pub(crate) async fn harvest_router_socket(
    wss: WebSocket,
    app_state: ACRState,
    connector_id: String,
    component_name: String,
    cranker_version: &'static str,
    domain: String, // V3 only
    route: String,
    addr: SocketAddr,
) {
    let router_socket_id = Uuid::new_v4().to_string();
    let weak_wsf = Arc::downgrade(&app_state.websocket_farm);
    if cranker_version == CRANKER_V_1_0 {
        let (wss_tx, wss_rx) = wss.split();
        let rs = RouterSocketV1::new_arc(
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
        info!("Connector (v1) registered! route: {} , connector_id: {} , router_socket_id: {}",route, connector_id, router_socket_id);
        app_state
            .websocket_farm
            .clone()
            .add_router_socket_in_background(rs);
    } else if cranker_version == CRANKER_V_3_0 {
        let (wss_tx, wss_rx) = wss.split();
        let rs = RouterSocketV3::new_arc(
            route.clone(),
            domain.clone(),
            component_name.clone(),
            router_socket_id.clone(),
            weak_wsf,
            connector_id.clone(),
            addr,
            wss_tx,
            wss_rx,
            app_state.config.proxy_listeners.clone(),
            app_state.config.discard_client_forwarded_headers,
            app_state.config.send_legacy_forwarded_headers,
            app_state.config.via_name.clone(),
            app_state.config.idle_read_timeout_ms,
            app_state.config.ping_sent_after_no_write_for_ms,
        );
        info!("Connector (v3) registered! route: {} , connector_id: {} , router_socket_id: {}", route,  connector_id, router_socket_id);
        app_state
            .websocket_farm
            .clone()
            .add_router_socket_in_background(rs);
    } else {
        error!("not supported cranker version: {}", cranker_version);
        let _ = wss.close();
    }
}

trait MessageIdentifier {
    #[allow(dead_code)]
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

pub(crate) struct TcpConnClosureGuard {
    pub(crate) notify: Arc<Notify>,
}

impl Drop for TcpConnClosureGuard {
    fn drop(&mut self) {
        error!("dropping TcpConnClosureGuard");
        self.notify.notify_waiters();
        error!("TcpConnClosureGuard dropped");
    }
}

pub(crate) fn wrap_async_stream_with_guard(wrapped_stream: Receiver<Result<Vec<u8>, CrankerRouterException>>, stream_close_notify_clone: Arc<Notify>) -> AsyncStream<Result<Vec<u8>, CrankerRouterException>, impl Future<Output=()> + Sized> {
    async_stream::stream! {
            let notify_when_close = stream_close_notify_clone.clone();
            let _guard = TcpConnClosureGuard {
                notify: stream_close_notify_clone,
            };
            loop {
                match wrapped_stream.recv().await {
                    Ok(res) => {
                        yield res;
                    }
                    Err(_) => {
                        error!("dropping notify when err received");
                        notify_when_close.notify_waiters();
                        break;
                    }
                }
            }
            error!("dropping notify when outside loop");
            drop(notify_when_close);
            error!("manually dropping _guard");
            drop(_guard);
            error!("done manually dropping _guard");
            // The guard should be dropped here, then the notification will be sent
        }
}
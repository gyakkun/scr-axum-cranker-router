use std::borrow::Cow;
use std::cmp::max;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, SeqCst};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use axum::{async_trait, http};
use axum::body::Body;
use axum::extract::OriginalUri;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::handler::Handler;
use axum::http::{HeaderMap, Method, Response, StatusCode, Version};
use axum::http::header::AsHeaderName;
use bytes::Bytes;
use futures::{Sink, SinkExt, StreamExt, TryFutureExt};
use futures::stream::{SplitSink, SplitStream};
use log::{debug, error, info, trace};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{CRANKER_V_1_0, exceptions, get_custom_hop_by_hop_headers, LOCAL_IP, time_utils, ACRState};
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

    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           original_uri: &OriginalUri,
                           headers: &HeaderMap,
                           addr: &SocketAddr,
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
    pub has_response: Arc<AtomicBool>,
    pub bytes_received: Arc<AtomicI64>,
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

    wss_recv_pipe_rx: Receiver<Message>,
    wss_recv_pipe_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    wss_send_task_tx: Sender<Message>,
    wss_send_task_join_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    has_response_notify: Arc<Notify>,
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

        let (wss_recv_pipe_tx, wss_recv_pipe_rx) = async_channel::unbounded();
        let has_response = Arc::new(AtomicBool::new(false));

        let router_socket_id_clone = router_socket_id.clone();
        let has_response_notify = Arc::new(Notify::new());

        let arc_rs = Arc::new(Self {
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

            wss_recv_pipe_rx,
            wss_send_task_join_handle: Arc::new(Mutex::new(None)),

            wss_send_task_tx,
            wss_recv_pipe_join_handle: Arc::new(Mutex::new(None)),

            has_response_notify,
        });
        // Abort these two handles in Drop
        let wss_recv_pipe_join_handle = tokio::spawn(
            pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
                arc_rs.clone(),
                underlying_wss_rx, wss_recv_pipe_tx,
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

fn set_target_request_headers(
    cli_hdr: &HeaderMap,
    mut hdr_to_tgt: &mut HeaderMap,
    app_state: &ACRState,
    http_version: &Version,
    cli_remote_addr: &SocketAddr,
    orig_uri: &OriginalUri,
) -> bool {
    let custom_hop_by_hop = get_custom_hop_by_hop_headers(cli_hdr.get(http::header::CONNECTION));
    let mut has_content_length_or_transfer_encoding = false;

    cli_hdr.iter().for_each(|(k, v)| {
        let lowercase_key = k.to_string().to_lowercase();
        if app_state.config.do_not_proxy_headers.contains(lowercase_key.as_str())
            || custom_hop_by_hop.contains(&lowercase_key) {
            return; // @for_each
        }
        has_content_length_or_transfer_encoding |=
            lowercase_key.eq(http::header::CONTENT_LENGTH.as_str())
                || lowercase_key.eq(http::header::TRANSFER_ENCODING.as_str());
        hdr_to_tgt.insert(k, v.clone());
    });
    let cli_all_via = header_map_get_all_ok_to_string(cli_hdr, http::header::VIA);


    let new_via_val = get_new_via_value(
        format!("{:?} {}", http_version, app_state.config.via_name),
        cli_all_via,
    );
    if let Ok(p) = new_via_val.parse() {
        hdr_to_tgt.append(http::header::VIA, p);
    }

    set_forwarded_headers(
        cli_hdr,
        hdr_to_tgt,
        app_state.config.discard_client_forwarded_headers,
        app_state.config.send_legacy_forwarded_headers,
        cli_remote_addr,
        orig_uri,
    );

    return has_content_length_or_transfer_encoding;
}

fn set_forwarded_headers(
    cli_hdr: &HeaderMap,
    hdr_to_tgt: &mut HeaderMap,
    discard_client_forwarded_headers: bool,
    send_legacy_forwarded_headers: bool,
    cli_remote_addr: &SocketAddr,
    cli_req_uri: &OriginalUri,
) {
    let mut forwarded_headers = Vec::new();
    if !discard_client_forwarded_headers {
        forwarded_headers = get_existing_forwarded_headers(cli_hdr);
        for j in forwarded_headers.iter() {
            if let Ok(parsed) = j.to_string().parse() {
                hdr_to_tgt.append(http::header::FORWARDED, parsed);
            }
        }
    }

    let new_forwarded_hdr = create_forwarded_header_to_tgt(cli_hdr, cli_remote_addr, cli_req_uri);
    if let Ok(parsed) = new_forwarded_hdr.to_string().parse() {
        hdr_to_tgt.append(http::header::FORWARDED, parsed);
    }

    if send_legacy_forwarded_headers {
        let first = if forwarded_headers.is_empty() { &new_forwarded_hdr } else { forwarded_headers.get(0).unwrap() };
        set_x_forwarded_headers(hdr_to_tgt, first);
    }
}

fn set_x_forwarded_headers(hdr_to_hdr: &mut HeaderMap, fh: &ForwardedHeader) {
    let l = [fh.proto.as_ref(), fh.host.as_ref(), fh.for_value.as_ref()];
    for v in l {
        if let Some(Some(p)) = v.map(|s| s.parse().ok()) {
            hdr_to_hdr.append(http::header::FORWARDED, p);
        }
    }
}

fn create_forwarded_header_to_tgt(
    cli_hdr: &HeaderMap,
    cli_remote_addr: &SocketAddr,
    cli_req_uri: &OriginalUri,
) -> ForwardedHeader {
    let server_self_ip = LOCAL_IP.to_string();
    ForwardedHeader {
        by: Some(server_self_ip),
        for_value: Some(cli_remote_addr.ip().to_string()),
        host: cli_hdr.get(http::header::HOST)
            .and_then(|i| i.to_str().ok())
            .map(|j| j.to_string()),
        proto: cli_req_uri.scheme_str().map(|i| i.to_string()),
        extensions: None,
    }
}

fn get_existing_forwarded_headers(hdr: &HeaderMap) -> Vec<ForwardedHeader> {
    let all: Vec<String> = header_map_get_all_ok_to_string(hdr, http::header::FORWARDED);
    let mut res = Vec::new();
    if all.is_empty() {
        let hosts = get_x_forwarded_value(hdr, "x-forwarded-host");
        let ports = get_x_forwarded_value(hdr, "x-forwarded-port");
        let protos = get_x_forwarded_value(hdr, "x-forwarded-proto");
        let fors = get_x_forwarded_value(hdr, "x-forwarded-for");
        let max = max(max(max(hosts.len(), protos.len()), fors.len()), ports.len());
        if max == 0 {
            return res;
        }
        let include_host = hosts.len() == max;
        let include_port = ports.len() == max;
        let include_proto = protos.len() == max;
        let include_for = fors.len() == max;

        let cur_host = if include_port && !include_host {
            hdr.get(http::header::HOST)
                .and_then(|j| j.to_str().ok())
                .and_then(|k| Some(k.to_string()))
        } else {
            None
        };

        for i in 0..max {
            let host = if include_host { hosts.get(i).and_then(|i| Some(i.clone())) } else { None };
            let port = if include_port { ports.get(i).and_then(|i| Some(i.clone())) } else { None };
            let proto = if include_proto { protos.get(i).and_then(|i| Some(i.clone())) } else { None };
            let for_value = if include_for { fors.get(i).and_then(|i| Some(i.clone())) } else { None };
            let use_default_port = port.is_none() ||
                if port.is_some() { // and port.is_some()
                    let po = port.clone().unwrap();
                    let pr = port.clone().unwrap();
                    (pr.eq_ignore_ascii_case("http") && po.eq("80"))
                        || (pr.eq_ignore_ascii_case("https") && po.eq("443"))
                } else {
                    false
                };
            let host_to_use = if include_host { host } else if include_port { cur_host.clone() } else { None };
            res.push(ForwardedHeader {
                by: None,
                for_value,
                host: host_to_use,
                proto,
                extensions: None,
            });
        }
    } else {
        for s in all {
            for t in rfc7239::parse(s.as_str()) {
                if let Ok(u) = t {
                    res.push(u.into());
                }
            }
        }
    }
    res
}

#[derive(Default)]
struct ForwardedHeader {
    by: Option<String>,
    for_value: Option<String>,
    host: Option<String>,
    proto: Option<String>,
    extensions: Option<HashMap<String, String>>,
}

impl Display for ForwardedHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut sb = String::new();
        append_string_for_forwarded_header(&mut sb, "by", self.by.as_ref());
        append_string_for_forwarded_header(&mut sb, "for", self.for_value.as_ref());
        append_string_for_forwarded_header(&mut sb, "host", self.host.as_ref());
        append_string_for_forwarded_header(&mut sb, "proto", self.proto.as_ref());
        self.extensions.as_ref()
            .map(|m| {
                m.iter().for_each(|(k, v)| {
                    append_string_for_forwarded_header(&mut sb, k.as_str(), Some(v));
                });
            });
        write!(f, "{}", sb)
    }
}

fn append_string_for_forwarded_header(sb: &mut String, k: &str, v: Option<&String>) {
    if v.is_none() {
        return;
    }
    if !sb.is_empty() {
        sb.push(';');
    }
    let unwrapped = v.unwrap();
    sb.push_str(k);
    sb.push('=');
    sb.push_str(quote_if_needed(unwrapped).as_str());
}

fn quote_if_needed(s: &String) -> String {
    return if s.chars().any(is_t_char) {
        let mut res = String::new();
        let replaced = s.replace('"', "\\\"");
        res.push('"');
        res.push_str(replaced.as_str());
        res.push('"');
        res
    } else {
        s.clone()
    };
}

fn is_t_char(c: char) -> bool {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9' || c == '!' ||
        c == '#' || c == '$' || c == '%' || c == '&' || c == '\'' || c == '*' || c == '+' ||
        c == '-' || c == '.' || c == '^' || c == '_' || c == '`' || c == '|' || c == '~');
}

impl From<rfc7239::Forwarded<'_>> for ForwardedHeader {
    fn from(rf: rfc7239::Forwarded) -> Self {
        ForwardedHeader {
            by: rf.forwarded_by.map(|i| i.to_string()),
            for_value: rf.forwarded_for.map(|i| i.to_string()),
            host: rf.host.map(|i| i.to_string()),
            proto: rf.protocol.map(|i| i.to_string()),
            extensions: None,
        }
    }
}

fn header_map_get_all_ok_to_string<K>(hdr: &HeaderMap, hdr_key: K)
                                      -> Vec<String>
    where K: AsHeaderName
{
    hdr.get_all(hdr_key).iter()
        .map(|f| f.to_str())
        .filter(|r| r.is_ok())
        .map(|f| f.unwrap())
        .map(|f| f.to_string())
        .collect()
}

fn get_x_forwarded_value(hdr: &HeaderMap, xf_key: &str) -> Vec<String> {
    let mut res = Vec::new();
    let vals = header_map_get_all_ok_to_string(hdr, xf_key);
    if !vals.is_empty() {
        vals.iter()
            .for_each(|i| {
                i.split(",")
                    .map(|j| j.trim())
                    .map(|k| k.to_string())
                    .for_each(|o| res.push(o));
            });
    }
    res
}

fn get_new_via_value(this_server_via: String, prev_via_list: Vec<String>) -> String {
    let mut res = prev_via_list.join(", ").to_string();
    if !prev_via_list.is_empty() {
        res.push_str(", ");
    }
    res.push_str(this_server_via.as_str());
    return res;
}

async fn listen_tgt_res_async(another_self: Arc<RouterSocketV1>) -> Result<(), CrankerRouterException> {
    let msg_counter = AtomicUsize::new(0);
    // 11. handling what we received from wss_recv_pipe
    trace!("11");
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
    trace!("12");
    debug!("Totally received {} messages after starting listen to response in router_socket_id={}", msg_counter.load(SeqCst), another_self.router_socket_id);
    Ok(())
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
        trace!("72");
    }
}

async fn pipe_underlying_wss_recv_and_send_err_to_err_chan_if_necessary(
    rs: Arc<RouterSocketV1>,
    mut underlying_wss_rx: SplitStream<WebSocket>,
    wss_recv_pipe_tx: Sender<Message>,
) {
    let mut local_has_response = rs.has_response.load(SeqCst);
    let mut may_ex: Option<CrankerRouterException> = None;
    let idle_read_timeout_ms = rs.idle_read_timeout_ms;
    let read_notifier = Arc::new(Notify::new());
    let read_notifier_clone = read_notifier.clone();
    let rs_weak = Arc::downgrade(&rs);
    tokio::spawn(async move {
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
                                let _ = wss_recv_pipe_tx.close(); // close or drop?
                                may_ex = Some(CrankerRouterException::new(format!(
                                    "underlying wss recv err: {:?}.", wss_recv_err
                                )));
                                break; // @outer
                            }
                            Ok(msg) => {
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
                                        break; // @outer
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
    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(
                Duration::from_millis(ping_sent_after_no_write_for_ms as u64),
                write_notifier_clone.notified(),
            ).await {
                Err(_) => {
                    if let Some(rs) = rs_weak.upgrade() {
                        let _ = rs.wss_send_task_tx.send(Message::Ping("ping".as_bytes().to_vec())).await;
                    } else {
                        break;
                    }
                }
                _ => continue
            }
        }
    });
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
                                let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Message::Text(txt) = msg {
                            rs.bytes_received.fetch_add(txt.len().try_into().unwrap(),SeqCst);
                            if let Err(e) = underlying_wss_tx.send(Message::Text(txt)).await {
                                let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
                            }
                        } else if let Err(e) = underlying_wss_tx.send(msg).await {
                            let _ = rs.err_chan_tx.send_blocking(CrankerRouterException::new(format!("{may_err_msg} : {:?}", e)));
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

        if !self.proxy_listeners.is_empty()
            && self.proxy_listeners.iter().any(|i| i.really_need_on_response_body_chunk_received_from_target()) {
            // FIXME: Copy vec<u8> is expensive!
            // it's inevitable to make a clone since the
            // terminate method of wss_tx.send moves the whole Vec<u8>
            opt_bin_clone_for_listeners = Some(bin.clone());
        }

        if let Err(e) = self.cli_side_res_sender.send_target_response_body_binary_fragment_to_client(bin).await {
            res = self.on_error(e);
        }

        if opt_bin_clone_for_listeners.is_some() {
            let bin_clone = Bytes::from(opt_bin_clone_for_listeners.unwrap());
            for i in
            self.proxy_listeners
                .iter()
                .filter(|i| i.really_need_on_response_body_chunk_received_from_target())
            {
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
        trace!("40");
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
        if self.has_response.load(SeqCst) {
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
                trace!("47");
                // 1000 indicates a normal closure, meaning that the purpose for
                // which the connection was established has been fulfilled.

                // I think here same as asyncHandle.complete(null) in mu cranker router
                self.tgt_res_hdr_rx.close();
                self.tgt_res_bdy_rx.close();

                // is_tgt_chan_close_as_expected = self.tgt_res_hdr_rx.close() && is_tgt_chan_close_as_expected;
                // is_tgt_chan_close_as_expected = self.tgt_res_bdy_rx.close() && is_tgt_chan_close_as_expected;
            } else {
                trace!("48");
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

    async fn on_client_req(self: Arc<Self>,
                           app_state: ACRState,
                           http_version: &Version,
                           method: &Method,
                           orig_uri: &OriginalUri,
                           cli_headers: &HeaderMap,
                           addr: &SocketAddr,
                           opt_body: Option<Receiver<Result<Bytes, CrankerRouterException>>>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        // 0. if is removed then should not run into this method
        if self.is_removed() {
            return Err(CrankerRouterException::new(format!(
                "try to handle cli req in a is_removed router socket. router_socket_id={}", &self.router_socket_id
            )));
        }

        let current_time_millis = time_utils::current_time_millis();
        self.socket_wait_in_millis.fetch_add(current_time_millis, SeqCst);
        self.client_req_start_ts.store(current_time_millis, SeqCst);
        self.duration_millis.store(-current_time_millis, SeqCst);
        // 1. Cli header processing
        trace!("1");
        let mut hdr_to_tgt = HeaderMap::new();
        set_target_request_headers(cli_headers, &mut hdr_to_tgt, &app_state, http_version, addr, orig_uri);
        // 2. Build protocol request line / protocol header frame without endmarker
        trace!("2");
        let request_line = create_request_line(method, &orig_uri);
        let cranker_req_bdr = CrankerProtocolRequestBuilder::new()
            .with_request_line(request_line)
            .with_request_headers(&hdr_to_tgt);

        for i in self.proxy_listeners.iter() {
            if let Err(e) = i.on_before_proxy_to_target(self.as_ref(), cli_headers) {
                let ec = e.clone();
                return if let Err(e) = self.on_error(e) {
                    Err(ec.plus(e))
                } else {
                    Err(ec)
                };
            }
        }

        // 3. Choose endmarker based on has body or not
        trace!("3");
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
        trace!("4");
        self.wss_send_task_tx.send(Message::Text(cranker_req)).await
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send cli req hdr to tgt: {:?}", e
            )))?;

        // 5. Pipe cli req body to underlying wss
        trace!("5");
        if let Some(mut body) = opt_body {
            trace!("5.5");
            while let Ok(res_bdy_chunk) = body.recv().await {
                trace!("6");
                match res_bdy_chunk {
                    Ok(bytes) => {
                        trace!("8");

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
                        trace!("9");
                        error!("error when receiving req body from cli: {:?}", &e.reason);
                        return Err(e);
                    }
                }
            }
            // 10. Send body end marker
            trace!("10");
            let end_of_body = CrankerProtocolRequestBuilder::new().with_request_body_ended().build()?;
            self.wss_send_task_tx.send(Message::Text(end_of_body)).await.map_err(|e| CrankerRouterException::new(format!(
                "failed to send end of body end marker to tgt: {:?}", e
            )))?;
        }
        trace!("20");
        for i in self.proxy_listeners.iter() {
            i.on_request_body_sent_to_target(self.as_ref())?;
        }
        trace!("21");

        let mut cranker_res: Option<CrankerProtocolResponse> = None;

        // if the wss_recv_pipe chan is EMPTY AND CLOSED then err will come from this recv()
        self.has_response.store(true, SeqCst);
        self.has_response_notify.notify_waiters();
        let another_self = self.clone();
        // tokio::spawn(listen_tgt_res_async(another_self));
        trace!("22");

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
pub async fn take_wss_and_create_router_socket(
    wss: WebSocket,
    app_state: ACRState,
    connector_id: String,
    component_name: String,
    cranker_version: &'static str,
    domain: String,
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
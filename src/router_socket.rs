
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::atomic::Ordering::SeqCst;
use async_channel::{Receiver, Sender};

use axum::{async_trait, BoxError, Error};
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message};
use axum::http::{HeaderMap, Method, Response, StatusCode};
use axum::http::uri::PathAndQuery;
use bytes::Bytes;
use futures::{SinkExt, TryFutureExt};
use log::{debug, error, info};
use tokio::sync::{mpsc, Mutex};

use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::cranker_protocol_response::CrankerProtocolResponse;
use crate::exceptions::CrankerRouterException;
use crate::REPRESSED_HEADERS;

const RESPONSE_HEADERS_TO_NOT_SEND_BACK: &[&str] = &["server"];
const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub trait WssMessageListener: Send + Sync {
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
    // accept a client req
    async fn on_client_req(&self,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<Receiver<Result<Bytes, Error>>>,
    ) -> Result<Response<Body>, CrankerRouterException>;

}


pub struct RSv1WssExchange {
    pub from_ws_to_rs_rx: Receiver<Message>,
    pub from_rs_to_ws_tx: Sender<Message>,
    pub err_chan_to_ws: Sender<CrankerRouterException>,

    pub target_res_header_received: AtomicBool,
    pub header_string_buf: tokio::sync::RwLock<String>,
    pub target_res_header_sent: AtomicBool,
    pub target_res_body_received: AtomicBool,
    pub websocket_closed: AtomicBool,
}


pub struct RouterSocketV1 {
    pub route: String,
    pub component_name: String,
    pub router_socket_id: String,
    // pub web_socket_farm: Option<Weak<Mutex<WebSocketFarm>>>,
    pub connector_instance_id: String,
    // pub proxy_listeners: Vec<&'static dyn ProxyListener>,
    // on_ready_for_action: &'static dyn Fn() -> (),
    pub remote_address: SocketAddr,
    pub is_removed: AtomicBool,
    pub has_response: AtomicBool,
    pub bytes_received: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub binary_frame_received: AtomicI64,
    pub socket_wait_in_millis: AtomicI64,
    pub error: Mutex<Option<CrankerRouterException>>,
    pub duration_millis: AtomicI64,
    pub wss_exchange: RSv1WssExchange,

    // below should be private for inner routine

}

impl RouterSocketV1 {
    // err should use a separated channel to communicate between wss side and router socket side
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               connector_instance_id: String,
               from_web_socket: Receiver<Message>,
               to_websocket: Sender<Message>,
               err_chan_to_websocket: Sender<CrankerRouterException>,
               remote_address: SocketAddr,
    ) -> Self {
        Self {
            route,
            component_name,
            router_socket_id,
            connector_instance_id,
            remote_address,

            is_removed: AtomicBool::new(false),
            has_response: AtomicBool::new(false),
            bytes_received: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            binary_frame_received: AtomicI64::new(0),
            socket_wait_in_millis: AtomicI64::new(-1),
            error: Mutex::new(None),
            duration_millis: AtomicI64::new(-1),
            wss_exchange: RSv1WssExchange::new(from_web_socket, to_websocket, err_chan_to_websocket),
        }
    }



}

impl RSv1WssExchange {
    fn new(
        from_ws_to_rs_rx: Receiver<Message>,
        from_rs_to_ws_tx: Sender<Message>,
        err_chan_to_ws: Sender<CrankerRouterException>,
    ) -> Self {
        Self {
            from_ws_to_rs_rx,
            from_rs_to_ws_tx,
            err_chan_to_ws,
            target_res_header_received: AtomicBool::new(false),
            header_string_buf: tokio::sync::RwLock::new(String::new()),
            target_res_header_sent: AtomicBool::new(false),
            target_res_body_received: AtomicBool::new(false),
            websocket_closed: AtomicBool::new(false),
        }
    }

    async fn send_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        self.from_rs_to_ws_tx.send(Message::Text(txt)).await
            .map_err(|e| {
                let failed_reason = format!(
                    "failed to send txt to wss: {:?}", e
                );
                error!("{}",failed_reason);
                CrankerRouterException::new(failed_reason)
            })
    }

    async fn send_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        self.from_rs_to_ws_tx.send(Message::Binary(bin)).await
            .map_err(|e| {
                let failed_reason = format!(
                    "failed to send binary to wss: {:?}", e
                );
                error!("{}",failed_reason);
                CrankerRouterException::new(failed_reason)
            })
    }
}

#[async_trait]
impl WssMessageListener for RSv1WssExchange {
    // NOT IN USE YET!
    async fn on_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        debug!("Text coming! {}", txt);
        self.target_res_header_received.store(true, SeqCst);
        if self.target_res_body_received.load(SeqCst) {
            let failed_reason = "res body already received but still receiving text message which is not expected!".to_string();
            self.on_error(CrankerRouterException::new(failed_reason.clone()));
            return Err(CrankerRouterException::new(failed_reason));
            // self.on_target_res_error(failed_reason.clone());
            // return Err(CrankerRouterException::new(failed_reason));
        }
        let text_len = txt.len();
        if text_len + self.header_string_buf.read().await.len() > HEADER_MAX_SIZE {
            let failed_reason = format!("Header too large after appending: before {} bytes, after {} bytes, max {} bytes",
                                        text_len, self.header_string_buf.read().await.len(), HEADER_MAX_SIZE);
            self.on_error(CrankerRouterException::new(failed_reason.clone()).into());
            // self.on_target_res_error(failed_reason.clone());
            return Err(CrankerRouterException::new(failed_reason));
        }
        self.header_string_buf.write().await.push_str(txt.as_str());
        Ok(())
    }

    // NOT IN USE YET!
    async fn on_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        debug!("binary coming! {}", bin.len());
        if !self.target_res_header_received.load(SeqCst) {
            let failed_reason = "res header not received yet but binary comes first which is not expected!".to_string();
            let _ = self.on_error(CrankerRouterException::new(failed_reason.clone()).into());
            return Err(CrankerRouterException::new(failed_reason));
        }
        // FIXME: Seems the text message always comes with one frame / one message
        // consider simplify it to on_text()
        if let Ok(_) = self.target_res_body_received.compare_exchange_weak(false, true, SeqCst, SeqCst) {
            self.from_rs_to_ws_tx.send(Message::Text(self.header_string_buf.read().await.clone()))
                .await
                .map_err(|e| CrankerRouterException::new(format!(
                    "failed to send header from target to client: {:?}", e
                )))?
        }
        self.from_rs_to_ws_tx.send(Message::Binary(bin)).await;
        // TODO
        Ok(())
    }

    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        // TODO: Gracefully close the router socket
        Ok(())
    }

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        self.err_chan_to_ws.send_blocking(err)
            .map_err(|se| CrankerRouterException::new(format!(
                "failed to send error to ws: {:?}", se
            )))
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    async fn on_client_req(&self,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<Receiver<Result<Bytes, Error>>>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        debug!("1");
        let mut client_request_headers = HeaderMap::new();
        headers.iter().for_each(|(k, v)| {
            if REPRESSED_HEADERS.contains(k.as_str().to_ascii_lowercase().as_str()) {
                // NOP
            } else {
                client_request_headers.insert(k.to_owned(), v.to_owned());
            }
        });
        debug!("2");

        let request_line = build_request_line(method, path_and_query);
        let cranker_req_bdr = CrankerProtocolRequestBuilder::new()
            .with_request_line(request_line)
            .with_request_headers(&client_request_headers);
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
        debug!("4");

        self.wss_exchange.send_text(cranker_req.clone()).await?;
        debug!("5");

        match opt_body { // req body
            None => { /*no body*/ }
            Some(mut body) => {
                while let Ok(r) = body.recv().await {
                    debug!("7");
                    match r {
                        Ok(b) => {
                            debug!("8");
                            self.wss_exchange.send_binary(b.to_vec()).await?
                        }
                        Err(e) => {
                            debug!("9");
                            let failed_reason = format!("error when sending body to target: {:?}", e);
                            error!("{}", failed_reason.clone());
                            return Err(CrankerRouterException::new(failed_reason));
                        }
                    }
                }
            }
        }
        debug!("10");

        let (mut res_body_chan_tx, res_body_chan_rx)
            = mpsc::unbounded_channel::<Result<Bytes, CrankerRouterException>>();
        // Should i spawn it somewhere earlier? yes you should
        let mut cranker_res: Option<CrankerProtocolResponse> = None;
        let msg_counter = AtomicUsize::new(0);
        while let Ok(msg) = self.wss_exchange.from_ws_to_rs_rx.recv().await {
            debug!("11");
            msg_counter.fetch_add(1, SeqCst);
            match msg {
                Message::Text(txt) => {
                    // FIXME: LARGE header handling, chunking handling
                    // self.wss_exchange.on_text(txt);
                    debug!("12");
                    cranker_res = cranker_res.or(CrankerProtocolResponse::new(txt).ok());
                }
                Message::Binary(bin) => {
                    debug!("13");
                    if cranker_res.is_none() {
                        debug!("14");
                        let failed_reason = "receiving binary from wss before header arrives!".to_string();
                        error!("{}", failed_reason);
                        let err = CrankerRouterException::new(failed_reason);
                        let send_err_err = self.wss_exchange.on_error(err.clone());
                        let _ = res_body_chan_tx.send(Err(err));
                        if send_err_err.is_err() {
                            let _ = res_body_chan_tx.send(Err(send_err_err.err().unwrap()));
                        }
                        let _ = res_body_chan_tx.closed();
                        cranker_res = Some(CrankerProtocolResponse::default_failed());
                        // res_builder = cranker_res.unwrap().build().ok();
                        break;
                    } else {
                        debug!("15");
                        let _ = res_body_chan_tx.send(Ok(Bytes::from(bin)));
                    }
                }
                Message::Close(_) => {
                    debug!("16");
                    drop(res_body_chan_tx);
                    break;
                }
                _ => {}
            }
        }
        info!("Received {} messages in router_socket_id={}", msg_counter.load(SeqCst), self.router_socket_id);
        if cranker_res.is_none() {
            return Ok(Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR).body(Body::new(
                "failed to build response from protocol response".to_string()
            )).unwrap());
        }
        let res_builder =  cranker_res.unwrap().build();
        let wrapped_stream = tokio_stream::wrappers::UnboundedReceiverStream::from(res_body_chan_rx);
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
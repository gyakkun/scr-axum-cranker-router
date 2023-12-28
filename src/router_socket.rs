use std::fmt::{Display, format};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc::Receiver;
use std::sync::RwLock;

use axum::{async_trait, BoxError, Error};
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message};
use axum::http::{HeaderMap, Method, Response, StatusCode};
use axum::http::uri::PathAndQuery;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::{SinkExt, Stream, StreamExt, TryFutureExt};
use log::{debug, error, info};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::cranker_protocol_request_builder::CrankerProtocolRequestBuilder;
use crate::exceptions::CrankerRouterException;
use crate::REPRESSED_HEADERS;

const RESPONSE_HEADERS_TO_NOT_SEND_BACK: &[&str] = &["server"];
const HEADER_MAX_SIZE: usize = 64 * 1024; // 64KBytes

#[async_trait]
pub trait WssMessageListener: Send + Sync {
    // this should be done in on_upgrade, so ignore it
    // fn on_connect(&self, wss_tx: SplitSink<WebSocket, Message>) -> Result<(), CrankerRouterException>;
    async fn on_text(&mut self, text_msg: String) -> Result<(), CrankerRouterException>;
    async fn on_binary(&mut self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
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

    async fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        error!("error: {:?}", err); // FIXME: Swallow the error by default
        Ok(())
    }
}

#[async_trait]
pub trait RouterSocket: Send + Sync {
    // accept a client req
    async fn on_client_req(&mut self,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<UnboundedReceiver<Result<Bytes, Error>>>,
    ) -> Result<Response<Body>, CrankerRouterException>;
    async fn on_client_req_error(&self, reason: CrankerRouterException);

    // accept response from target
    async fn on_target_res(&self,
                           headers: &HeaderMap,
                           opt_body: Option<mpsc::Receiver<Bytes>>,
    ) -> Result<(), CrankerRouterException>;

    async fn on_target_res_error(&self, reason: CrankerRouterException);
}


pub struct RSv1WssMessageListener {
    pub from_ws_to_rs_rx: UnboundedReceiver<Message>,
    pub from_rs_to_ws_tx: UnboundedSender<Message>,

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
    pub is_removed: bool,
    pub has_response: bool,
    pub bytes_received: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub binary_frame_received: AtomicI64,
    pub socket_wait_in_millis: i64,
    pub error: Option<BoxError>,
    pub duration_millis: i64,
    pub router_socket_wss_message_listener: RSv1WssMessageListener,
    pub err_chan_to_wss: UnboundedSender<CrankerRouterException>,
    // TODO: seems axum receive websocket message in a Message level rather than a Frame level
    // so maybe no need to create buffer for frame of TEXT message
    // on_text_buffer: Vec<char>,

    // below should be private for inner routine
    // async_handle: Box<dyn Future<Output=hyper::body::Body>>,
    client_request_headers: HeaderMap,
    client_response_headers: HeaderMap,

}

impl RouterSocketV1 {
    // err should use a separated channel to communicate between wss side and router socket side
    pub fn new(route: String,
               component_name: String,
               router_socket_id: String,
               connector_instance_id: String,
               from_web_socket: UnboundedReceiver<Message>,
               to_websocket: UnboundedSender<Message>,
               err_chan_to_websocket: UnboundedSender<CrankerRouterException>,
               remote_address: SocketAddr,
    ) -> Self {
        Self {
            route,
            component_name,
            router_socket_id,
            connector_instance_id,
            remote_address,

            is_removed: false,
            has_response: false,
            bytes_received: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            binary_frame_received: AtomicI64::new(0),
            // response: None,
            // client_request: None,
            socket_wait_in_millis: -1,
            error: None,
            duration_millis: -1,
            router_socket_wss_message_listener: RSv1WssMessageListener::new(from_web_socket, to_websocket),
            err_chan_to_wss: err_chan_to_websocket,
            // websocket_listener: Arc::new(tokio::sync::RwLock::new(WebSocketListenerV1::new(
            //     ts_wss_tx.clone(), ts_wss_rx.clone(),
            //     client_err_tx, target_err_tx,
            // ))),

            client_request_headers: HeaderMap::new(),
            client_response_headers: HeaderMap::new(),
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
}

impl RSv1WssMessageListener {
    fn new(
        from_ws_to_rs_rx: UnboundedReceiver<Message>,
        from_rs_to_ws_tx: UnboundedSender<Message>,
    ) -> Self {
        Self {
            from_ws_to_rs_rx,
            from_rs_to_ws_tx,
            target_res_header_received: AtomicBool::new(false),
            header_string_buf: tokio::sync::RwLock::new(String::new()),
            target_res_header_sent: AtomicBool::new(false),
            target_res_body_received: AtomicBool::new(false),
            websocket_closed: AtomicBool::new(false),
        }
    }

    async fn send_text(&self, txt: String) -> Result<(), CrankerRouterException> {
        self.from_rs_to_ws_tx.send(Message::Text(txt))
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send txt to wss: {:?}", e
            )))
    }

    async fn send_binary(&self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        self.from_rs_to_ws_tx.send(Message::Binary(bin))
            .map_err(|e| CrankerRouterException::new(format!(
                "failed to send binary to wss: {:?}", e
            )))
    }
}

#[async_trait]
impl WssMessageListener for RSv1WssMessageListener {
    async fn on_text(&mut self, txt: String) -> Result<(), CrankerRouterException> {
        debug!("Text coming! {}", txt);
        self.target_res_header_received.store(true, SeqCst);
        if self.target_res_body_received.load(SeqCst) {
            let failed_reason = "res body already received but still receiving text message which is not expected!".to_string();
            self.on_error(CrankerRouterException::new(failed_reason.clone())).await;
            return Err(CrankerRouterException::new(failed_reason));
            // self.on_target_res_error(failed_reason.clone());
            // return Err(CrankerRouterException::new(failed_reason));
        }
        let text_len = txt.len();
        if text_len + self.header_string_buf.read().await.len() > HEADER_MAX_SIZE {
            let failed_reason = format!("Header too large after appending: before {} bytes, after {} bytes, max {} bytes",
                                        text_len, self.header_string_buf.read().await.len(), HEADER_MAX_SIZE);
            self.on_error(CrankerRouterException::new(failed_reason.clone()).into()).await;
            // self.on_target_res_error(failed_reason.clone());
            return Err(CrankerRouterException::new(failed_reason));
        }
        self.header_string_buf.write().await.push_str(txt.as_str());
        Ok(())
    }

    async fn on_binary(&mut self, bin: Vec<u8>) -> Result<(), CrankerRouterException> {
        debug!("binary coming! {}", bin.len());
        if !self.target_res_header_received.load(SeqCst) {
            let failed_reason = "res header not received yet but binary comes first which is not expected!".to_string();
            self.on_error(CrankerRouterException::new(failed_reason.clone()).into()).await;
            return Err(CrankerRouterException::new(failed_reason));
        }
        // FIXME: Seems the text message always comes with one frame / one message
        // consider simplify it to on_text()
        if let Ok(_) = self.target_res_body_received.compare_exchange_weak(false, true, SeqCst, SeqCst) {
            self.from_rs_to_ws_tx.send(Message::Text(self.header_string_buf.read().await.clone()))
                .map_err(|e| CrankerRouterException::new(format!(
                    "failed to send header from target to client: {:?}", e
                )))?
        }
        self.from_rs_to_ws_tx.send(Message::Binary(bin));
        // TODO
        Ok(())
    }

    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException> {
        // TODO: Gracefully close the router socket
        Ok(())
    }
}

#[async_trait]
impl RouterSocket for RouterSocketV1 {
    async fn on_client_req(&mut self,
                           method: Method,
                           path_and_query: Option<&PathAndQuery>,
                           headers: &HeaderMap,
                           opt_body: Option<UnboundedReceiver<Result<Bytes, Error>>>,
    ) -> Result<Response<Body>, CrankerRouterException> {
        debug!("9");
        headers.iter().for_each(|(k, v)| {
            debug!("Checking header {}", k.as_str());
            if REPRESSED_HEADERS.contains(k.as_str().to_ascii_lowercase().as_str()) {
                debug!("Repressed header: {}", k.as_str());
                // NOP
            } else {
                self.client_request_headers.insert(k.to_owned(), v.to_owned());
            }
        });
        debug!("After filtering: ");
        self.client_request_headers.iter().for_each(|(k, v)| debug!("{}", k.as_str()));
        let request_line = Self::build_request_line(method, path_and_query);
        let cranker_req_bdr = CrankerProtocolRequestBuilder::new()
            .with_request_line(request_line)
            .with_request_headers(&self.client_request_headers);
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
        debug!("10");
        self.router_socket_wss_message_listener.send_text(cranker_req.clone()).await?;


        match opt_body { // req body
            None => { /*no body*/ }
            Some(mut body) => {
                info!("Sending req body to target");
                while let Some(r) =  body.recv().await {
                    match r {
                        Ok(b) => {
                            self.router_socket_wss_message_listener.send_binary(b.to_vec()).await?
                        }
                        Err(e) => {
                            let failed_reason = format!("error when sending body to target: {:?}", e);
                            error!("{}", failed_reason.clone());
                            return Err(CrankerRouterException::new(failed_reason));
                        }
                    }
                }
            }
        }

        // while let Some(msg) = self.router_socket_wss_message_listener.from_ws_to_rs_rx.recv()


        // let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        // let mut streamrx = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        // while let Some(u) = streamrx.next().await {}

        //todo
        Ok(Response::builder().status(StatusCode::OK).body(Body::new("OK!".to_string())).unwrap())
    }

    // async fn on_client_req_error(&self, reason: String) {
    //    self.ts_wss_tx.lock().await.send(Message::Close(None));
    // }

    async fn on_target_res(&self,
                           headers: &HeaderMap,
                           opt_body: Option<mpsc::Receiver<Bytes>>,
    ) -> Result<(), CrankerRouterException> {
        // todo!()
        Ok(())
    }

    // fn on_target_res_error(&self, reason: String) {
    //    todo!()
    // }

    async fn on_target_res_error(&self, err: CrankerRouterException) {
        let _ = self.err_chan_to_wss.send(err);
    }
    async fn on_client_req_error(&self, err: CrankerRouterException) {
        let _ = self.err_chan_to_wss.send(err);
    }
}
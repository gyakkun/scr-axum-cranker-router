use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use axum::{BoxError, Error, Extension, http, Router, ServiceExt};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::extract::ws::{Message, WebSocket};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{SinkExt, Stream, StreamExt, TryStreamExt};
use futures::stream::{BoxStream, SplitSink, SplitStream};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use log::LevelFilter::Debug;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;
use tower::Service;
use tower_http::limit;
use uuid::Uuid;

use crate::exceptions::{CrankerProtocolVersionNotFoundException, CrankerProtocolVersionNotSupportedException, CrankerRouterException};
use crate::route_resolver::{DEFAULT_ROUTE_RESOLVER, RouteResolver};
use crate::router_socket::{RouterSocket, RouterSocketV1};

pub mod cranker_protocol;
pub mod router_socket;
pub mod time_utils;
pub mod exceptions;
pub mod cranker_protocol_response;
pub mod cranker_protocol_request_builder;
pub mod route_resolver;

pub(crate) const CRANKER_PROTOCOL_HEADER_KEY: &str = "crankerprotocol";
// should be CrankerProtocol, but axum all lower cased
pub const _VER_1_0: &str = "1.0";
pub const _VER_3_0: &str = "3.0";

pub const CRANKER_V_1_0: &str = "cranker_1.0";
pub const CRANKER_V_3_0: &str = "cranker_3.0";

#[derive(Clone, Debug)]
struct VersionNegotiate {
    dealt: &'static str,
}

lazy_static! {
    static ref SUPPORTED_CRANKER_VERSION: HashMap<&'static str, &'static str> =  {
        let mut s = HashMap::new();
        s.insert(_VER_1_0, CRANKER_V_1_0);
        s.insert(_VER_3_0, CRANKER_V_3_0);
        s
    };

    static ref HOP_BY_HOP_HEADERS: HashSet<&'static str> =  {
        let mut s = HashSet::new();
        [
            "keep-alive",
            "transfer-encoding",
            "te",
            "connection",
            "trailer",
            "upgrade",
            "proxy-authorization",
            "proxy-authenticate"
        ].iter().for_each(|h| {s.insert(*h);});
        s
    };

    static ref REPRESSED_HEADERS: HashSet<&'static str> = {
        let mut s = HOP_BY_HOP_HEADERS.clone();
        [
            // expect is already handled by mu server, so if it's forwarded it will break stuff
            "expect",

            // Headers that mucranker will overwrite
            "forwarded", "x-forwarded-by", "x-forwarded-for", "x-forwarded-host",
            "x-forwarded-proto", "x-forwarded-port", "x-forwarded-server", "via"
        ].iter().for_each(|h|{s.insert(*h);});
        s
    };
}

// Our shared state
struct WorkaroundRustcAsyncTraitBug(tokio::sync::mpsc::Sender<Arc<RwLock<dyn RouterSocket>>>);

pub struct CrankerRouterState {
    counter: AtomicU64,
    route_to_socket: RwLock<DashMap<String, VecDeque<Arc<RwLock<dyn RouterSocket>>>>>,

    waiting_socket_key_generator: AtomicU64,
    waiting_socket_hashmap: RwLock<DashMap<u64, WorkaroundRustcAsyncTraitBug>>,
}

pub type TSCRState = Arc<CrankerRouterState>;

pub struct CrankerRouter {
    state: TSCRState,
}


#[tokio::test]
async fn main() {
    SimpleLogger::new()
        .with_local_timestamps()
        .with_level(Debug)
        .init()
        .unwrap();


    let cranker_router = CrankerRouter::new();

    let reg_listener = TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    let visit_listener = TcpListener::bind("127.0.0.1:3002")
        .await
        .unwrap();

    let reg_router = cranker_router.registration_axum_router();
    let visit_router = cranker_router.visit_portal_axum_router();

    tokio::spawn(async {
        axum::serve(reg_listener, reg_router).await.unwrap()
    });

    axum::serve(visit_listener, visit_router).await.unwrap();
}


impl CrankerRouter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(CrankerRouterState {
                counter: AtomicU64::new(0),
                route_to_socket: RwLock::new(DashMap::new()),
                waiting_socket_key_generator: AtomicU64::new(0),
                waiting_socket_hashmap: RwLock::new(DashMap::new()),
            })
        }
    }

    pub fn registration_axum_router(&self) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
        let res = Router::new()
            .route("/register", any(CrankerRouter::register_handler)
                .layer(from_fn_with_state(self.state(), validate_route_domain_and_cranker_version)),
            )
            .route("/register/", any(CrankerRouter::register_handler)
                .layer(from_fn_with_state(self.state(), validate_route_domain_and_cranker_version)),
            )
            .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
            .with_state(self.state())
            .into_make_service_with_connect_info::<SocketAddr>();
        return res;
    }

    pub fn visit_portal_axum_router(&self) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
        let res = Router::new()
            .route("/*any", any(CrankerRouter::visit_portal))
            .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
            .with_state(self.state())
            .into_make_service_with_connect_info::<SocketAddr>();
        return res;
    }
    pub fn state(&self) -> TSCRState { self.state.clone() }

    // #[debug_handler]
    pub async fn visit_portal(
        State(app_state): State<TSCRState>,
        method: Method,
        original_uri: OriginalUri,
        headers: HeaderMap,
        req: axum::http::Request<Body>,
    ) -> Response
    {
        // FIXME: How to judge if has body or not
        // Get should have no body but not required
        let http_ver = req.version();
        debug!("Http version {:?}", http_ver);
        let body = req.into_body();
        let has_body = judge_has_body_from_header_http_1(&headers) || !body.is_end_stream();

        let mut body_data_stream = body.into_data_stream();
        let mut boxed_stream = Box::pin(body_data_stream) as BoxStream<Result<Bytes, Error>>;

        let path = original_uri.path().to_string();
        let route = DEFAULT_ROUTE_RESOLVER.resolve(&*(app_state.route_to_socket.read().await), path.clone());
        let expected_err = CrankerRouterException::new("No router socket available".to_string());
        match app_state.route_to_socket.write().await.get_mut(&route) {
            None => {
                debug!("No socket vecdeq available!");
                // return expected_err.clone().into_response();
                // should_wait = true;
            }
            Some(mut v) => {
                match v.value_mut().pop_front() {
                    None => {
                        debug!("No socket available!");
                        // should_wait = true;
                        // return expected_err.clone().into_response();
                    }
                    Some(mut am_rs) => {
                        debug!("Get a socket");
                        let mut opt_body = None;
                        if has_body {
                            let (mut pipe_tx, mut pipe_rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, Error>>();
                            opt_body = Some(pipe_rx);
                            tokio::spawn(pipe_body_data_stream_to_a_mpsc_sender(boxed_stream, pipe_tx));
                        }
                        debug!("Has body ? {:?}", opt_body);
                        debug!("Ready to lock on router socket");
                        match am_rs.write().await.on_client_req(
                            method,
                            original_uri.path_and_query(),
                            &headers,
                            opt_body,
                        ).await {
                            Ok(res) => {
                                debug!("received body!");
                                return res;
                            }
                            Err(e) => {
                                debug!("received error! {:?}", e);
                                return e.into_response();
                            }
                        }
                    }
                }
            }
        };
        // return expected_err.into_response();

        debug!("Waiting for socket from waiting_socket_hashmap");

        // No socket queue / socket available immediately
        let (mut waiting_task_tx, mut waiting_task_rx) = tokio::sync::mpsc::channel::<Arc<RwLock<dyn RouterSocket>>>(1);
        let waiting_task_id = app_state.waiting_socket_key_generator.fetch_add(1, SeqCst);
        {
            app_state.waiting_socket_hashmap.write().await.insert(waiting_task_id, WorkaroundRustcAsyncTraitBug(waiting_task_tx));
        }
        debug!("After inserting task");
        let timeout = tokio::time::timeout(Duration::from_secs(5), waiting_task_rx.recv()).await;
        match timeout {
            Ok(Some(socket)) => {

                waiting_task_rx.close();
                {
                    app_state.waiting_socket_hashmap.write().await.remove(&waiting_task_id);
                }

                let mut opt_body = None;
                if has_body {
                    let (mut pipe_tx, mut pipe_rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, Error>>();
                    opt_body = Some(pipe_rx);
                    tokio::spawn(pipe_body_data_stream_to_a_mpsc_sender(boxed_stream, pipe_tx));
                }
                {
                    let res = socket.write().await
                        .on_client_req(method, original_uri.path_and_query(), &headers, opt_body)
                        .await;
                    match res {
                        Ok(res) => {
                            return res;
                        }
                        Err(e) => {
                            return e.into_response();
                        }
                    }
                }
            }
            _ => {
                debug!("Desperate but nothing get from waiting.");
                waiting_task_rx.close();
                {
                    app_state.waiting_socket_hashmap.write().await.remove(&waiting_task_id);
                }
                debug!("After removing the task id");
                return expected_err.into_response();
            }
        };
    }

    // #[debug_handler]
    pub async fn register_handler(
        State(app_state): State<TSCRState>,
        ws: WebSocketUpgrade,
        Extension(ext_map): Extension<HashMap<String, String>>,
        Extension(ver_neg): Extension<VersionNegotiate>,
        Query(params): Query<HashMap<String, String>>,
        headers: HeaderMap,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ) -> impl IntoResponse
    {
        let route = ext_map.get(&"route".to_string()).unwrap();
        let domain = ext_map.get(&"domain".to_string()).unwrap();
        debug!("ext map: {:?}", ext_map);
        debug!("ver neg: {:?}", ver_neg);
        // Extract component name and connector id
        let connector_id = params.get("connectorInstanceID")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let uuid = Uuid::new_v4().to_string();
                // TODO: Maybe we need to check if the connector id already exists here or not
                warn!("Connector id is not specified. Generating a random uuid for it: {}", uuid);
                uuid
            });

        let component_name = params.get("componentName")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let sub_uuid = connector_id.chars().take(5).collect::<String>();
                warn!("Component name is not set. Name it as unknown-{}.",sub_uuid);
                // TODO: What's the best practice of a SAFE and NEVER-FAIL substring function in rust?
                "unknown-".to_string() + sub_uuid.as_str()
            });

        // TODO: Error handling - what if all these fields not exists?
        let domain = ext_map.get("domain").unwrap().to_string();
        let route = ext_map.get("route").unwrap().to_owned();

        debug!("connector id : {:?}", connector_id);
        debug!("component name : {:?}", component_name);

        ws
            .protocols([ver_neg.dealt])
            .on_upgrade(move |socket| take_and_store_websocket(
                socket, app_state.clone(), connector_id, component_name, ver_neg.dealt.to_string(), domain, route,
                addr,
            ))
    }
}


fn judge_has_body_from_header_http_1(headers: &HeaderMap) -> bool {
    if let Some(content_length) = headers.get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| i64::from_str(v).ok())
    {
        return content_length > 0;
    }
    headers.contains_key(http::header::TRANSFER_ENCODING)
}

async fn pipe_body_data_stream_to_a_mpsc_sender(mut stream: BoxStream<'_, Result<Bytes, Error>>,
                                                mut sender: UnboundedSender<Result<Bytes, Error>>,
) -> Result<(), CrankerRouterException>
{
    while let Some(frag) = stream.next().await {
        match sender.send(frag) {
            Err(e) => {
                let failed_reason = format!("error when piping body data stream to mpsc sender, pipe will break: {:?}", e);
                error!("{failed_reason}");
                return Err(CrankerRouterException::new(failed_reason));
            }
            _ => {}
        }
    }
    drop(sender);
    Ok(())
}


// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
async fn take_and_store_websocket(wss: WebSocket,
                                  state: TSCRState,
                                  connector_id: String,
                                  component_name: String,
                                  cranker_version: String,
                                  domain: String,
                                  route: String,
                                  addr: SocketAddr,
) {
    let (mut from_ws_to_rs_tx, mut from_ws_to_rs_rx): (UnboundedSender<Message>, UnboundedReceiver<Message>) = unbounded_channel::<Message>();
    let (mut from_rs_to_ws_tx, mut from_rs_to_ws_rx) = unbounded_channel::<Message>();
    let (mut wss_tx, mut wss_rx) = wss.split();
    let router_socket_id = Uuid::new_v4().to_string();
    let (mut err_chan_tx, mut err_chan_rx) = unbounded_channel::<CrankerRouterException>();
    let rs = Arc::new(RwLock::new(RouterSocketV1::new(
        route.clone(),
        component_name.clone(),
        router_socket_id.clone(),
        connector_id.clone(),
        from_ws_to_rs_rx, // from websocket
        from_rs_to_ws_tx, // to websocket
        err_chan_tx,
        addr,
    )));
    info!("Connector registered! connector_id: {}, router_socket_id: {}", connector_id, router_socket_id);
    debug!("Before Spawned websocket_exchange");
    // tokio::spawn(
    //     websocket_exchange(
    //         connector_id.clone(), router_socket_id.clone(),
    //         from_rs_to_ws_rx, from_ws_to_rs_tx, wss_tx, wss_rx, err_chan_rx,
    //     )
    // );
    debug!("After Spawned websocket_exchange");

    //     if let Some(task_id) = state.read().await.waiting_socket_hashmap.into_read_only().keys().next() {
    //         if let Some((_, task)) = state.read().await.waiting_socket_hashmap.remove(task_id) {
    //             task.send(rs.clone()).await;
    //         }
    //     }

    if let Some(task) = state.waiting_socket_hashmap.read().await.iter().next() {
        debug!("Found task waiting for socket");
        let id = task.key();
        debug!("before sending to receiver");
        let _ = task.value().0.send(rs.clone()).await;
        debug!("after sending to receiver");
        // {
        //     state.waiting_socket_hashmap.write().await.remove(id);
        // }
        // debug!("Removed task id after sending to receiver");
    } else {
        debug!("No task waiting. Sending to vecdeq");
        {
            state
                .route_to_socket
                .write()
                .await
                .entry(route.clone())
                .or_insert(VecDeque::new())
                .push_back(rs.clone());
        }
        debug!("After sending to vecdeq. lock should released");
    }
    {
        // Prepare for some counter
        let write_guard = state.clone();
        let _ = write_guard.counter.fetch_add(1, SeqCst);
    }
    websocket_exchange(
        connector_id.clone(), router_socket_id.clone(),
        from_rs_to_ws_rx, from_ws_to_rs_tx, wss_tx, wss_rx, err_chan_rx,
    ).await;
}

async fn websocket_exchange(
    connector_id: String,
    router_socket_id: String,
    mut from_rs_to_ws_rx: UnboundedReceiver<Message>,
    mut from_ws_to_rs_tx: UnboundedSender<Message>,
    mut wss_tx: SplitSink<WebSocket, Message>,
    mut wss_rx: SplitStream<WebSocket>,
    mut err_chan_from_rs: UnboundedReceiver<CrankerRouterException>,
)
{
    debug!("Listening on connector ws message!");
    let (mut notify_ch_close_tx, mut notify_ch_close_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut notify_ch_close_tx_clone = notify_ch_close_tx.clone();
    let msg_counter_lib = AtomicUsize::new(0);

    // queue the task to wss_tx
    let (mut wss_send_task_tx, mut wss_send_task_rx) = unbounded_channel::<Message>();
    {
        let notify_ch_close_tx = notify_ch_close_tx.clone();
        let from_ws_to_rs_tx = from_ws_to_rs_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = wss_send_task_rx.recv().await {
                let _ = wss_tx.send(msg).await.map_err(|e| {
                    let _ = from_ws_to_rs_tx.send(Message::Close(None));
                    let _ = notify_ch_close_tx.send(());
                });
            }
            drop(notify_ch_close_tx);
            drop(from_ws_to_rs_tx);
        });
    }

    loop {
        tokio::select! {
                Some(should_stop) = notify_ch_close_rx.recv() => {
                    info!("should stop looping now! router_socket_id: {}", router_socket_id);
                    break;
                }
                Some(err) = err_chan_from_rs.recv() => {
                    error!(
                        "exception received in websocket_exchange: {:?}. connector_id: {}, router_socket_id: {}",
                        err, connector_id, router_socket_id
                    );
                    notify_ch_close_tx.send(());
                }
                Some(to_ws) = from_rs_to_ws_rx.recv() => {
                    match to_ws {
                        Message::Text(txt) => {
                            wss_send_task_tx.send(Message::Text(txt));
                        }
                        Message::Binary(bin) => {
                            wss_send_task_tx.send(Message::Binary(bin));
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
                            let _ = from_ws_to_rs_tx.send(Message::Close(None));
                            let _ = notify_ch_close_tx_clone.send(());
                        }
                        Some(Ok(Message::Ping(_))) => {
                            let _ = wss_send_task_tx.send(Message::Pong(Vec::new()));
                        }
                        Some(may_err) => {
                            msg_counter_lib.fetch_add(1,SeqCst);
                            // FIXME: Let's believe cloning these channels equals cloning their inner Arc, which is super cheap!
                            let mut from_ws_to_rs_tx = from_ws_to_rs_tx.clone();
                            let mut notify_ch_close_tx = notify_ch_close_tx.clone();
                            // tokio::spawn(
                                pipe_msg_from_wss_to_router_socket(
                                    connector_id.clone(), router_socket_id.clone(),
                                    may_err, from_ws_to_rs_tx, notify_ch_close_tx
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
    from_ws_to_rs_tx: UnboundedSender<Message>,
    notify_ch_close_tx: UnboundedSender<()>,
)
{
    match may_err {
        Err(e) => {
            error!(
                "Error from wss: {:?}. connector_id: {}, router_socket_id: {}",
                e, connector_id, router_socket_id
            );
            notify_ch_close_tx.send(());
        }
        Ok(msg) => {
            match msg {
                Message::Text(txt) => {
                    from_ws_to_rs_tx.send(Message::Text(txt));
                }
                Message::Binary(bin) => {
                    from_ws_to_rs_tx.send(Message::Binary(bin));
                }
                Message::Pong(_) => debug!("ponged!"),
                Message::Close(opt_close_frame) => {
                    if let Some(clo_fra) = opt_close_frame {
                        warn!(
                            "Closing from connector: code={}, {:?}. connector_id: {}, router_socket_id: {}",
                            clo_fra.code, clo_fra.reason, connector_id, router_socket_id
                        );
                    }
                    from_ws_to_rs_tx.send(Message::Close(None));
                    notify_ch_close_tx.send(());
                }
                _ => {}
            }
        }
    }
}

/// Domain and route is mandatory so add a middleware to check.
/// Return BAD_REQUEST if illegal
async fn validate_route_domain_and_cranker_version(
    State(appstate): State<TSCRState>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> Response
{
    info!("We got someone registering. Let's examine its info.");
    // TODO: Put in the catch all method
    let resolved_route = resolve_route(appstate.clone());
    // TODO: Put in the actual ws handler


    let route = match headers.get("Route")
        .map(|r| r.to_str())
        .unwrap_or_else(|| {
            warn!("No route specified. Fallback to \"*\"");
            Ok("*")
        }) {
        Ok(v) => v,
        Err(to_str_err) => {
            error!("Failed to convert Route header to str: {:?}",to_str_err);
            return (StatusCode::BAD_REQUEST, format!("Failed to convert Route header to str: {:?}", to_str_err)).into_response();
        }
    };


    let domain = match headers.get("Domain")
        .map(|r| r.to_str())
        .unwrap_or_else(|| {
            warn!("No domain specified. Fallback to \"*\"");
            Ok("*")
        }) {
        Ok(v) => v,
        Err(to_str_err) => {
            error!("Failed to convert Domain header to str: {:?}",to_str_err);
            return (StatusCode::BAD_REQUEST, format!("Failed to convert Domain header to str: {:?}", to_str_err)).into_response();
        }
    };

    // let mut supported_cranker_version_set = HashSet::new();
    // SUPPORTED_CRANKER_VERSION.iter().for_each(|i| { supported_cranker_version_set.insert(*i); });
    let dealt_version: &'static str;
    match extract_cranker_version(&SUPPORTED_CRANKER_VERSION, &headers) {
        Ok(v) => {
            dealt_version = v;
        }
        Err(err) => {
            error!("Not able to negotiate cranker version during handshake: {}", err);
            return (StatusCode::BAD_REQUEST,
                    format!("Failed to negotiate cranker version: {}", err)
            ).into_response();
        }
    }

    // Extract to
    let mut ext_hashmap = HashMap::<String, String>::new();
    ext_hashmap.insert("route".to_string(), route.to_string());
    ext_hashmap.insert("domain".to_string(), domain.to_string());
    let ext_ver_neg = VersionNegotiate { dealt: dealt_version };
    request.extensions_mut().insert(ext_ver_neg);

    request.extensions_mut().insert(ext_hashmap);

    let response = next.run(request).await;

    // do something with `response`...
    response
}


// Return "cranker_1.0" or "cranker_3.0", different from mu-cranker ("1.0" / "3.0")
// because the "protocols()" method requires `&'static str`
fn extract_cranker_version(
    supported_cranker_version_set: &HashMap<&'static str, &'static str>,
    headers: &HeaderMap,
) -> Result<&'static str, BoxError>
{
    let sub_protocols = headers.get(SEC_WEBSOCKET_PROTOCOL);
    let legacy_protocol_header = headers.get(CRANKER_PROTOCOL_HEADER_KEY);
    if sub_protocols.is_none() && legacy_protocol_header.is_none() {
        error!("No cranker version specified in headers: {:?}", headers);
        return Err(CrankerProtocolVersionNotFoundException::new().into());
    }

    match sub_protocols {
        Some(sp) => {
            let split = sp.to_str().ok().unwrap().split(",");
            for v in split.into_iter() {
                let trimmed = v.trim().replace("cranker_", "");
                let trimmed = trimmed.as_str();
                if supported_cranker_version_set.contains_key(trimmed) {
                    let res: &'static str = *supported_cranker_version_set.get(trimmed).unwrap();
                    return Ok(res);
                }
            }
        }
        None => {}
    }

    match legacy_protocol_header {
        Some(lph) => {
            let lp = lph.to_str().ok().unwrap();
            if supported_cranker_version_set.contains_key(lp) {
                let res: &'static str = *supported_cranker_version_set.get(lp).unwrap();
                return Ok(res);
            }
        }
        None => {}
    }
    error!("{:?}",CrankerProtocolVersionNotSupportedException::new(
        format!("(sub protocols: {:?}, legacy protocols: {:?}", sub_protocols, legacy_protocol_header)
    ));

    return Err(CrankerProtocolVersionNotSupportedException::new(
        format!("(sub protocols: {:?}, legacy protocols: {:?}", sub_protocols, legacy_protocol_header)
    ).into());
}


fn resolve_route(app_state: TSCRState) -> String {
    // TODO
    "example".into()
}
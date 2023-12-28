use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::SeqCst;

use axum::{BoxError, Error, Extension, Router, ServiceExt};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::ws::WebSocket;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum_macros::debug_handler;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{pin_mut, Stream, StreamExt, TryStreamExt};
use futures::stream::BoxStream;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use log::LevelFilter::Debug;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
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

#[derive(Clone)]
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
struct AppState {
    counter: AtomicI64,
    route_to_socket: DashMap<String, VecDeque<Arc<Mutex<dyn RouterSocket>>>>,
}

type TSState = Arc<RwLock<AppState>>;

#[tokio::test]
async fn main() {
    SimpleLogger::new()
        .with_local_timestamps()
        .with_level(Debug)
        .init()
        .unwrap();


    let app_state = Arc::new(RwLock::new(AppState {
        counter: AtomicI64::new(0),
        route_to_socket: DashMap::new(),
    }));
    let registration_portal = Router::new()
        .route("/register", any(cranker_register_handler)
            .layer(from_fn_with_state(app_state.clone(), validate_route_domain_and_cranker_version)),
        )
        .route("/register/", any(cranker_register_handler)
            .layer(from_fn_with_state(app_state.clone(), validate_route_domain_and_cranker_version)),
        )
        .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .with_state(app_state.clone())
        .into_make_service_with_connect_info::<SocketAddr>();

    let visit_portal = Router::new()
        // .route("/*any", get(|| async {}));
        .route("/*any", any(portal))
        .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
        .with_state(app_state.clone());

    let reg_listener = TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    let visit_listener = TcpListener::bind("127.0.0.1:3002")
        .await
        .unwrap();

    debug!("listening on {}", reg_listener.local_addr().unwrap());
    tokio::spawn(async {
        axum::serve(reg_listener, registration_portal).await.unwrap()
    });

    axum::serve(visit_listener, visit_portal)
        .await
        .unwrap();
}

struct Hi<'t> {
    body_data_stream: BoxStream<'t, Result<Bytes, Error>>,
}

#[debug_handler]
async fn portal(
    State(app_state): State<TSState>,
    method: Method,
    original_uri: OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // FIXME: How to judge if has body or not
    // Get should have no body but not required
    info!("size lower bound: {}", body.size_hint().lower());
    let has_body = body.size_hint().lower() > 0;
    info!("Has body? {}", has_body);
    let mut body_data_stream = body.into_data_stream();
    let mut boxed_stream = Box::pin(body_data_stream) as BoxStream<Result<Bytes, Error>>;

    let path = original_uri.path().to_string();
    let route = DEFAULT_ROUTE_RESOLVER.resolve(&app_state.read().await.route_to_socket, path.clone());
    let expected_err = CrankerRouterException::new("No router socket available".to_string());
    debug!("1");
    match app_state.read().await.route_to_socket.get_mut(&route) {
        None => {
            debug!("2");
            return expected_err.clone().into_response();
        }
        Some(mut v) => {
            match v.value_mut().pop_front() {
                None => {
                    debug!("3");
                    return expected_err.clone().into_response();
                }
                Some(mut am_rs) => {
                    debug!("4");
                    let mut opt_body = None;
                    if has_body {
                        debug!("5");
                        let (pipe_tx, pipe_rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, Error>>();
                        opt_body = Some(pipe_rx);
                        tokio::spawn(pipe_body_data_stream_to_a_mpsc_sender(boxed_stream, pipe_tx));
                    } else {
                        debug!("6");
                    }

                    match am_rs.lock().await.on_client_req(
                        method,
                        original_uri.path_and_query(),
                        &headers,
                        opt_body,
                    ).await {
                        Ok(res) => {
                            debug!("7");
                            return res;
                        }
                        Err(e) => {
                            debug!("8");
                            return e.into_response();
                        }
                    }
                }
            }
        }
    };
    Response::new(Body::new("hi there".to_string()))
}

async fn pipe_body_data_stream_to_a_mpsc_sender(mut stream: BoxStream<'_, Result<Bytes, Error>>, sender: tokio::sync::mpsc::UnboundedSender<Result<Bytes, Error>>) {
    while let Some(frag) = stream.next().await {
        match sender.send(frag) {
            Err(e) => {
                error!("error when piping body data stream to mpsc sender, pipe will break: {:?}", e);
                break;
            }
            _ => {}
        }
    }
    drop(sender);
}

#[debug_handler]
async fn cranker_register_handler(
    State(app_state): State<TSState>,
    ws: WebSocketUpgrade,
    Extension(ext_map): Extension<HashMap<String, String>>,
    Extension(ver_neg): Extension<VersionNegotiate>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    // ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    info!("ws : {:?}", ws);
    info!("req : {:?}", req);
    info!("params : {:?}", params);
    info!("headers : {:?}", headers);
    info!("ext_map : {:?}", ext_map);
    let route = ext_map.get(&"route".to_string()).unwrap();
    let domain = ext_map.get(&"domain".to_string()).unwrap();
    info!("domain from ext: {:?}", domain);
    info!("route from ext: {:?}", route);

    // let connector_id_set = app_state.route_to_connector_id_map.get(&"route".to_string());
    //
    // assert!(connector_id_set.is_some());
    //
    // for i in connector_id_set.unwrap().iter() {
    //     info!("connector id from state: {:?}",i.to_string())
    // }


    // Extract component name and connector id
    let connector_id = params.get("connectorId")
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

    info!("connector id : {:?}", connector_id);
    info!("component name : {:?}", component_name);

    ws
        .protocols([ver_neg.dealt])
        .on_upgrade(move |socket| take_and_store_websocket(
            socket, app_state.clone(), connector_id, component_name, ver_neg.dealt.to_string(), domain, route,
            addr,
        ))
}

// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
async fn take_and_store_websocket(wss: WebSocket,
                                  state: TSState,
                                  connector_id: String,
                                  component_name: String,
                                  cranker_version: String,
                                  domain: String,
                                  route: String,
                                  addr: SocketAddr,
) {
    info!("Connector registered! connector_id: {}", connector_id.clone());

    // tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    // let a: Arc<Box<u64>> = Arc::new(Box::new(64));
    debug!("Going to split wss");
    let (mut tx, mut rx) = wss.split();
    debug!("going to gen router socket id");
    let router_socket_id = Uuid::new_v4().to_string();
    match state.try_read() {
        Ok(_) => debug!("state is not locking"),
        Err(_) => debug!("!!! state now is locking !!!")
    }
    {
        info!("before registration, state route size: {}", state.read().await.route_to_socket.len());
    }
    {
        state.write().await
            .route_to_socket
            .entry(route.clone())
            .or_insert(VecDeque::new())
            .push_back(Arc::new(Mutex::new(RouterSocketV1::new(
                route.clone(),
                component_name.clone(),
                router_socket_id.clone(),
                connector_id.clone(),
                tx,
                rx,
                addr,
            ))));
    }
    {
        match state.try_read() {
            Ok(_) => {}
            Err(_) => debug!("!!! state is still locking after inserting something !!!")
        }
    }
    {
        info!("after registration, state route size: {}", state.read().await.route_to_socket.len());
    }

    {
        let write_guard = state.write().await;
        write_guard.counter.compare_exchange_weak(write_guard.counter.load(SeqCst), write_guard.counter.load(SeqCst) + 1, SeqCst, SeqCst);
    }
}

/// Domain and route is mandatory so add a middleware to check.
/// Return BAD_REQUEST if illegal
async fn validate_route_domain_and_cranker_version(
    State(appstate): State<TSState>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> Response {
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
            debug!("Version negotiated: {}", v);
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
    // extension_test.insert("version".to_string(), dealt_version);
    let ext_ver_neg = VersionNegotiate { dealt: dealt_version };
    request.extensions_mut().insert(ext_ver_neg);

    // app_state.route_to_connector_id_map
    //     .entry("route".to_string())
    //     .or_insert(DashSet::<String>::new())
    //     .insert(Uuid::new_v4().to_string());
    request.extensions_mut().insert(ext_hashmap);
    info!("Domain: {:?}",domain);
    info!("Route: {:?}",route);

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
                    debug!("selected in sub_protocols: {}", trimmed);
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
                debug!("selected in legacy protocol header: {}", lp);
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


fn resolve_route(app_state: TSState) -> String {
    // TODO
    "example".into()
}
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::SeqCst;

use axum::{BoxError, Extension, extract, http, Router};
use axum::body::Body;
use axum::extract::{OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::ws::WebSocket;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum_macros::debug_handler;
use dashmap::DashMap;
use futures::{pin_mut, StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use log::LevelFilter::Info;
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

pub(crate) const CRANKER_PROTOCOL_HEADER_KEY: &str = "CrankerProtocol";
pub const CRANKER_PROTOCOL_VERSION_1_0: &str = "1.0";
pub const CRANKER_PROTOCOL_VERSION_2_0: &str = "2.0";
pub const CRANKER_PROTOCOL_VERSION_3_0: &str = "3.0";
pub const SUPPORTING_HTTP_VERSION_1_1: &str = "HTTP/1.1";

lazy_static! {
    static ref SUPPORTED_CRANKER_VERSION: HashSet<&'static str> =  {
        let mut s = HashSet::new();
        s.insert(CRANKER_PROTOCOL_VERSION_1_0);
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
        .with_level(Info)
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
        .with_state(app_state.clone());

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

#[debug_handler]
async fn portal(
    State(app_state): State<TSState>,
    method: Method,
    original_uri: OriginalUri,
    headers: HeaderMap,
    axum_req: extract::Request,
) -> Response {
    // axum_req
    let mut body_data_stream = axum_req.into_body().into_data_stream()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        .peekable();
    pin_mut!(body_data_stream);
    let mut has_body = body_data_stream.peek().await.is_some();
    let route = DEFAULT_ROUTE_RESOLVER.resolve(&app_state.read().await.route_to_socket, original_uri.path().to_string());
    let expected_err = CrankerRouterException::new("No router socket available".to_string());
    match app_state.read().await.route_to_socket.get_mut(&route) {
        None => { return expected_err.clone().into_response(); }
        Some(mut v) => {
            match v.value_mut().pop_back() {
                None => { return expected_err.clone().into_response(); }
                Some(mut am_rs) => {
                    async {
                        // do something
                    }.await;
                    //match am_rs.lock().unwrap().on_client_req(
                    //    method,
                    //    original_uri.path_and_query(),
                    //    &headers,
                    //    None,
                    //).await {
                    //    Ok(res) => {return res;}
                    //    Err(e) => {return e.into_response();}
                    //}
                }
            }
        }
    };
    todo!("
           resolve path and get a router socket\\
            send the req body (if any)
    ");
    // if has_body {} else {
    // while let sth = map_err.next().await {
    //     has_body = true;
    // }
    // }
    Response::new(Body::new("hi there".to_string()))
}

async fn cranker_register_handler(
    State(app_state): State<TSState>,
    ws: WebSocketUpgrade,
    Extension(ext_map): Extension<HashMap<String, String>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
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
    let cranker_version = ext_map.get("version").unwrap().to_string();
    let domain = ext_map.get("domain").unwrap().to_string();
    let route = ext_map.get("route").unwrap().to_owned();

    info!("connector id : {:?}", connector_id);
    info!("component name : {:?}", component_name);

    ws.on_upgrade(move |socket| take_and_store_websocket(
        socket, app_state, connector_id, component_name, cranker_version, domain, route,
        // addr.clone(),
    ))
}

// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
async fn take_and_store_websocket(wss: WebSocket, state: TSState,
                                  connector_id: String,
                                  component_name: String,
                                  cranker_version: String,
                                  domain: String,
                                  route: String,
                                  // addr: SocketAddr,
) {
    info!("Connector registered! connector_id: {}", connector_id.clone());

    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    let a: Arc<Box<u64>> = Arc::new(Box::new(64));
    let (mut tx, mut rx) = wss.split();
    let router_socket_id = Uuid::new_v4().to_string();
    let (rs_tx, rs_rx) = tokio::sync::mpsc::unbounded_channel::<RouterSocketV1>();
    state.write().await
        .route_to_socket
        .entry(route.clone())
        .or_insert(VecDeque::new())
        .push_back(Arc::new(Mutex::new(RouterSocketV1::new(
            route.clone(),
            component_name.clone(),
            /*router_socket_id*/ router_socket_id.clone(),
            connector_id.clone(),
            tx,
            rx,
            SocketAddr::from_str("127.0.0.1:1024").unwrap(),
            // addr,
        ))));
    state.write().await.counter.compare_exchange(state.read().await.counter.load(SeqCst), state.write().await.counter.load(SeqCst) + 1, SeqCst, SeqCst)
        .expect("Failed to add counter");
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
    let dealt_version: String;
    match extract_cranker_version(&SUPPORTED_CRANKER_VERSION, &headers) {
        Ok(v) => { dealt_version = v; }
        Err(err) => {
            error!("Not able to negotiate cranker version during handshake: {}", err);
            return (StatusCode::BAD_REQUEST,
                    format!("Failed to negotiate cranker version: {}", err)
            ).into_response();
        }
    }

    // Extract to
    let mut extension_test = HashMap::<String, String>::new();
    extension_test.insert("route".to_string(), route.to_string());
    extension_test.insert("domain".to_string(), domain.to_string());
    extension_test.insert("version".to_string(), dealt_version);

    // app_state.route_to_connector_id_map
    //     .entry("route".to_string())
    //     .or_insert(DashSet::<String>::new())
    //     .insert(Uuid::new_v4().to_string());
    request.extensions_mut().insert(extension_test);
    info!("Domain: {:?}",domain);
    info!("Route: {:?}",route);

    let response = next.run(request).await;

    // do something with `response`...

    response
}

fn extract_cranker_version(
    supported_cranker_version_set: &HashSet<&str>,
    headers: &HeaderMap,
) -> Result<String, BoxError>
{
    let sub_protocols = headers.get(http::header::SEC_WEBSOCKET_PROTOCOL);
    let legacy_protocol_header = headers.get(CRANKER_PROTOCOL_HEADER_KEY);
    if sub_protocols.is_none() && legacy_protocol_header.is_none() {
        error!("No cranker version specified in headers: {:?}", headers);
        return Err(CrankerProtocolVersionNotFoundException::new().into());
    }

    match sub_protocols {
        Some(sp) => {
            let split = sp.to_str().ok().unwrap().split(",");
            for v in split.into_iter() {
                if supported_cranker_version_set.contains(v) {
                    return Ok(v.to_string());
                }
            }
        }
        None => {}
    }

    match legacy_protocol_header {
        Some(lph) => {
            let lp = lph.to_str().ok().unwrap();
            if supported_cranker_version_set.contains(lp) {
                return Ok(lp.to_string());
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
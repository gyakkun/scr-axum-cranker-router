use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::SeqCst;

use axum::{BoxError, Extension, http, Router};
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State, WebSocketUpgrade};
use axum::extract::ws::WebSocket;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use dashmap::DashMap;
use futures::StreamExt;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use log::LevelFilter::Info;
use simple_logger::SimpleLogger;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::cranker_protocol::{CrankerProtocolVersionNotFoundException, CrankerProtocolVersionNotSupportedException};
use crate::router_socket::RouterSocketV1;

pub mod cranker_protocol;
pub mod router_socket;
pub mod time_utils;
pub mod exceptions;
mod cranker_protocol_response;

pub(crate) const CRANKER_PROTOCOL_HEADER_KEY: &str = "CrankerProtocol";
lazy_static! {
    static ref SUPPORTED_CRANKER_VERSION: HashSet<&'static str> =  {
        let mut s = HashSet::new();
        s.insert("1.0");
        s
    };
}

// Our shared state
struct AppState {
    counter: AtomicI64,
    route_to_socket: DashMap<String, VecDeque<RouterSocketV1>>,
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
        .with_state(app_state);

    let visit_portal = Router::new()
        // .route("/*any", get(|| async {}));
        .route("/*any", any(move || async {
            info!("hit any");
            Response::new(Body::from("<h1>Hi there</h1>"))
        }));

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
    let a: Arc<dyn Deref<Target=u64>> = Arc::new(Box::new(64));
    let (mut tx, mut rx) = wss.split();
    let router_socket_id = Uuid::new_v4().to_string();
    let (rs_tx, rs_rx) = tokio::sync::mpsc::unbounded_channel::<RouterSocketV1>();
    state.write().unwrap()
        .route_to_socket
        .entry(route.clone())
        .or_insert(VecDeque::new())
        .push_back(RouterSocketV1::new(
            route.clone(),
            component_name.clone(),
            /*router_socket_id*/ router_socket_id.clone(),
            connector_id.clone(),
            tx,
            rx,
            SocketAddr::from_str("127.0.0.1:1024").unwrap()
            // addr,
        ));
    state.write().unwrap().counter.compare_exchange(state.read().unwrap().counter.load(SeqCst), state.write().unwrap().counter.load(SeqCst) + 1, SeqCst, SeqCst)
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
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use axum::{BoxError, Error, Extension, http, Router, ServiceExt};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt, TryStreamExt};
use futures::stream::BoxStream;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use tokio::sync::Notify;
use tokio_util::io::SinkWriter;
use tower_http::limit;
use uuid::Uuid;

use crate::exceptions::{CrankerProtocolVersionNotFoundException, CrankerProtocolVersionNotSupportedException, CrankerRouterException};
use crate::route_resolver::{DEFAULT_ROUTE_RESOLVER, RouteResolver};
use crate::router_socket::RouterSocket;

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
pub struct CrankerRouterState {
    counter: AtomicU64,
    route_to_socket_chan: DashMap<String, (
        Sender<Arc<dyn RouterSocket>>,
        Receiver<Arc<dyn RouterSocket>>
    )>,
    route_notifier: DashMap<String, Notify>,
}

pub type TSCRState = Arc<CrankerRouterState>;

pub struct CrankerRouter {
    state: TSCRState,
}

impl CrankerRouter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(CrankerRouterState {
                counter: AtomicU64::new(0),
                route_to_socket_chan: DashMap::new(),
                route_notifier: DashMap::new(),
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

        let body_data_stream = body.into_data_stream()
            .map_err(|e| CrankerRouterException::new(format!(
                "axum ex when piping cli res body: {:?}", e
            )));
        // The full qualified name is  Pin<Box<MapErr<BodyDataStream, fn(Error) -> CrankerRouterException>>> which is too long
        let boxed_stream = Box::pin(body_data_stream) as BoxStream<Result<Bytes, CrankerRouterException>>;

        let path = original_uri.path().to_string();
        let route = DEFAULT_ROUTE_RESOLVER.resolve(&app_state.route_to_socket_chan, path.clone());
        let expected_err = CrankerRouterException::new("No router socket available".to_string());
        let opt_router_socket_receiver = app_state.route_to_socket_chan.get(&route);
        // mu-cranker-router here responses with 404 immediately
        if opt_router_socket_receiver.is_none() {
            let notifier = app_state.route_notifier
                .entry(route.clone())
                .or_insert(Notify::new());
            let route_timeout = tokio::time::timeout(Duration::from_secs(5), notifier.value().notified()).await;
            match route_timeout {
                Err(_) => {
                    return expected_err.clone().into_response();
                }
                _ => {}
            }
        }
        let router_socket_receiver = opt_router_socket_receiver.unwrap().clone().1;
        let timeout = tokio::time::timeout(Duration::from_secs(5), async {
             while let Ok(rs) = router_socket_receiver.recv().await {
                 // skip socket that should be removed
                 if rs.should_remove() {
                     continue;
                 }
                 return Ok(rs);
             }
            Err(())
        }).await;
        match timeout {
            Ok(Ok(rs)) => {
                debug!("Get a socket");
                let mut opt_body = None;
                if has_body {
                    let (pipe_tx, pipe_rx) = async_channel::unbounded::<Result<Bytes, CrankerRouterException>>();
                    opt_body = Some(pipe_rx);
                    tokio::spawn(pipe_body_data_stream_to_channel_sender(boxed_stream, pipe_tx));
                }
                debug!("Has body ? {:?}", opt_body);
                debug!("Ready to lock on router socket");
                match rs.on_client_req(
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
            _ => {
                debug!("No socket available!");
                return expected_err.clone().into_response();
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
            .on_upgrade(move |socket| router_socket::take_wss_and_create_router_socket(
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

async fn pipe_body_data_stream_to_channel_sender(
    mut stream: BoxStream<'_, Result<Bytes, CrankerRouterException>>,
    sender: Sender<Result<Bytes, CrankerRouterException>>,
) -> Result<(), CrankerRouterException>
{
    while let Some(frag) = stream.next().await {
        if let Err(e) = sender.send(frag).await {
            let failed_reason = format!("error when piping body data stream to mpsc sender, pipe will break: {:?}", e);
            error!("{failed_reason}");
            return Err(CrankerRouterException::new(failed_reason));
        }
    }
    drop(sender);
    Ok(())
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

    // Chain the layer
    let response = next.run(request).await;
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

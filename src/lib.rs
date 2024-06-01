use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::{BoxError, Extension, http, Json, Router};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use lazy_static::lazy_static;
use log::{debug, error, warn};
use tower_http::limit;
use uuid::Uuid;

use crate::dark_mode_manager::DarkModeManager;
use crate::exceptions::{CrankerRouterException, CrexKind};
use crate::ip_validator::{AllowAll, IPValidator};
use crate::proxy_listener::ProxyListener;
use crate::route_resolver::{DefaultRouteResolver, RouteResolver};
use crate::router_info::RouterInfo;
use crate::router_socket_filter::{DefaultRouterSocketFilter, RouterSocketFilter};
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};

pub mod router_socket;
pub mod time_utils;
pub mod exceptions;
pub mod cranker_protocol_response;
pub mod cranker_protocol_request_builder;
pub mod route_resolver;
pub mod proxy_info;
pub mod websocket_listener;
pub mod proxy_listener;
pub mod websocket_farm;
pub mod ip_validator;
pub mod route_identify;
pub mod router_socket_filter;
mod connector_connection;
mod connector_instance;
mod connector_service;
mod dark_mode_manager;
mod dark_host;
mod router_info;
mod router_socket_v3;
mod http_utils;
mod router_socket_v1;

pub(crate) const CRANKER_PROTOCOL_HEADER_KEY: &'static str = "CrankerProtocol";
// should be CrankerProtocol, but axum convert all header key to lowercase when reading req from client and sending res
// e.g. cli req with header[("hi","l"), ("HI","U"), ("Hi","Ca")], then you need header_map.get_all()
// then you can iterate and get "l", "U" and "Ca"
// Same as the header_map in res
pub const _VER_1_0: &'static str = "1.0";
pub const _VER_3_0: &'static str = "3.0";

pub const CRANKER_V_1_0: &'static str = "cranker_1.0";
pub const CRANKER_V_3_0: &'static str = "cranker_3.0";

lazy_static! {
    // Runtime evaluated, so it should be the actual serving server local ip
    // rather than compile machine ip
    pub(crate) static ref LOCAL_IP: IpAddr = local_ip_address::local_ip().unwrap();

    static ref SUPPORTED_CRANKER_VERSION: HashMap<&'static str, &'static str> =  {
        let mut s = HashMap::new();
        s.insert(_VER_1_0, CRANKER_V_1_0);
        s.insert(_VER_3_0, CRANKER_V_3_0);
        s
    };

    static ref RESPONSE_HEADERS_TO_NOT_SEND_BACK: HashSet<&'static str> = {
        let mut s = HashSet::new();
        s.insert("server");
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
            // expect is already handled by hyper under axum, so if it's forwarded it will break stuff
            // Check https://github.com/hyperium/hyper/pull/377
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
    pub websocket_farm: Arc<WebSocketFarm>,
    pub dark_mode_manager: Arc<DarkModeManager>,
    pub config: CrankerRouterConfig,
}

pub(crate) type ACRState = Arc<CrankerRouterState>;

/// This class creates a router function in axum for receiving HTTP requests
/// from clients (`visit_portal()`), and a router function for receiving
/// websocket registrations from Cranker connectors.
/// You are responsible for creating axum server instance(s) that the
/// router functions (handler) are added to.
/// When shutting down, the stop() method should be called after stopping your Mu Server(s).
/// This class is created by using the `CrankerRouterBuilder::new().build()`
/// builder.
pub struct CrankerRouter {
    /// An `Arc` of all states needed by the Cranker
    /// router instance
    pub state: ACRState,
}

impl CrankerRouter {
    /// Create a Cranker router from builder / config
    pub fn new(
        config: CrankerRouterConfig
    ) -> Self {
        let websocket_farm = WebSocketFarm::new(
            config.route_resolver.clone(),
            config.connector_max_wait_time_millis,
            config.proxy_listeners.clone(),
        );
        let websocket_farm_clone = websocket_farm.clone();
        let dark_mode_manager = Arc::new(DarkModeManager {
            websocket_farm: websocket_farm.clone()
        });
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(
                    config.routes_keep_time_millis as u64
                )
                ).await;
                websocket_farm_clone.clean_routes_in_background(config.routes_keep_time_millis);
            }
        });
        Self {
            state: Arc::new(CrankerRouterState {
                websocket_farm,
                dark_mode_manager,
                config,
            }),
        }
    }

    pub(crate) fn state(&self) -> ACRState { self.state.clone() }

    /// The registration axum router function.
    /// It registers multiple register and deregister paths
    /// for compatibility of node.js connector implementations
    /// Library users can control finer granularity by applying
    /// the `handler` functions to their own `Router`
    pub fn registration_axum_router(&self) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
        let res = Router::new()
            .route("/register", any(Self::register_handler)
                .layer(from_fn_with_state(self.state(), Self::reg_check_and_extract)),
            )
            .route("/register/", any(Self::register_handler)
                .layer(from_fn_with_state(self.state(), Self::reg_check_and_extract)),
            )
            .route("/deregister", any(Self::de_register_handler)
                .layer(from_fn_with_state(self.state(), Self::de_reg_check)),
            )
            .route("/deregister/", any(Self::de_register_handler)
                .layer(from_fn_with_state(self.state(), Self::de_reg_check)),
            )
            // .with_state(self.state())
            .route("/health/connectors", get(Self::connector_info_handler))
            // .with_state(self.state())
            // .route("/health/connections", get(???)) // how to get all conn of the server
            .route("/health", get(Self::health_root))
            .with_state(self.state())
            .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
            .into_make_service_with_connect_info::<SocketAddr>();
        return res;
    }

    /// Health endpoint root handler function.
    /// Currently only `isAvailable` field available.
    /// To be done...
    // TODO: Add all info
    pub async fn health_root(
        State(_): State<ACRState>
    ) -> Response {
        (StatusCode::OK, "{\"isAvailable\":true}").into_response()
    }

    /// `/health/connectors` endpoint handler
    /// It collects information by calling `collect_info(&ACRState)`
    /// static method of `CrankerRouter`
    pub async fn connector_info_handler(
        State(app_state): State<ACRState>
    ) -> Response {
        let mut res_map_inner = HashMap::new();
        let col = CrankerRouter::collect_info_by_state(
            app_state.clone()
        );
        col.services.iter().for_each(|cs| {
            res_map_inner.insert(cs.route.clone(), cs.clone());
        });
        let mut res_map = HashMap::new();
        res_map.insert("services", res_map_inner);
        (StatusCode::OK, Json(res_map)).into_response()
    }

    /// The client oriented visit portal router function
    /// Basically it removes the default request body size
    /// limitation from axum and/or tower crates.
    /// Library users can apply further control by adding
    /// the `visit_portal` handler function to their own
    /// router.
    pub fn visit_portal_axum_router(&self) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
        let res = Router::new()
            .route("/*any", any(CrankerRouter::visit_portal))
            .layer(limit::RequestBodyLimitLayer::new(usize::MAX - 1))
            .with_state(self.state())
            .into_make_service_with_connect_info::<SocketAddr>();
        return res;
    }

    /// The visit portal and main logic of the Cranker router
    /// implementation.
    /// It will receive client requests, find valid router
    /// socket and "pipe" them together, and proxy the request
    /// to the connector on target services' side.
    // #[debug_handler]
    pub async fn visit_portal(
        State(app_state): State<ACRState>,
        method: Method,
        original_uri: OriginalUri,
        headers: HeaderMap,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        req: Request<Body>,
    ) -> Response
    {
        // Get should have no body but not required
        let http_version = req.version();
        debug!("Http version {:?}", http_version);
        let body = req.into_body();
        let has_body = match http_version {
            http::version::Version::HTTP_09 |
            http::version::Version::HTTP_10 |
            http::version::Version::HTTP_11 =>
                judge_has_body_from_header_http_1(&headers),
            _ => !body.is_end_stream()
        };


        let path = original_uri.path().to_string();
        let socket_fut = app_state.websocket_farm.clone().get_router_socket_by_target_path_and_apply_filter(
            path.clone(),
            method.clone(),
            original_uri.clone(),
            headers.clone(),
            addr.clone(),
            app_state.config.clone(),
            app_state.config.router_socket_filter.clone()
        ).await;
        return match socket_fut {
            Ok(rs) => {
                debug!("Get a socket router_socket_id={}, cranker_ver={}", rs.router_socket_id(), rs.cranker_version());

                // FIXME: Put back v3 socket for reuse. Any better idea?
                if rs.cranker_version() == CRANKER_V_3_0 {
                    app_state.websocket_farm.clone().add_router_socket_in_background(rs.clone());
                }

                let mut opt_body = None;
                if has_body {
                    let boxed_body_byte_stream
                        = body.into_data_stream();
                    opt_body = Some(boxed_body_byte_stream);
                }
                debug!("Has body ? {:?}", opt_body.is_some());
                let rs_clone = rs.clone();
                match rs.on_client_req(
                    app_state.clone(),
                    &http_version,
                    &method,
                    &original_uri,
                    &headers,
                    &addr,
                    opt_body,
                ).await {
                    Ok((res, _)) => {
                        debug!("recv tgt res bdy from rs!");
                        // FIXME : This part of mu can't be simulated
                        //  No "response complete listener" can be set in axum
                        //  if !res.status().is_success() && !rs_clone.is_removed() {
                        //    let _ = rs_clone.raise_completion_event(opt_cli_req_id);
                        //  }
                        //  Check https://github.com/tokio-rs/axum/discussions/2490
                        res
                    }
                    Err(e) => {
                        debug!("recv err from rs, expect tgt res bdy: {:?}", e);
                        if !rs_clone.is_removed() {
                            let _ = rs_clone.raise_completion_event(None);
                        }
                        e.into_response()
                    }
                }
            }
            Err(e) => {
                debug!("No socket available!");
                e.clone().into_response()
            }
        };
    }

    /// The function needed to be called before
    /// a connector is de-registering.
    pub async fn de_reg_check(
        State(app_state): State<ACRState>,
        Query(params): Query<HashMap<String, String>>, // Not expecting multiple val for a key so hashmap is fine
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        mut request: Request<Body>,
        next: Next,
    ) -> Response {
        debug!("We got someone DE registering. Let's examine its info: headers={:?}", &headers);
        let route = headers.get("Route")
            .and_then(|r| r.to_str().ok())
            .and_then(|s| Some(s.to_string()))
            .unwrap_or_else(|| {
                warn!("[DeReg] No route specified. Fallback to \"*\"");
                "*".to_string()
            });

        let connector_id = params.get("connectorInstanceID")
            .map(|s| s.to_string());

        if !app_state.config.registration_ip_validator.allow(addr.ip()) {
            return failed_at_ip_validation(
                addr,
                route.as_str(),
                connector_id
                    .clone()
                    .unwrap_or("N/A".to_string())
                    .as_str(),
            ).into_response();
        }
        if connector_id.is_none() {
            request.extensions_mut().insert("unknown-".to_string().push_str(addr.to_string().as_str()));
            warn!("the service route={} using unsupported zero down time connector, will not deregister socket", route);
        } else {
            let connector_id = connector_id.unwrap();
            request.extensions_mut().insert(connector_id.clone());
            app_state
                .websocket_farm
                .de_register_router_socket_in_background(route, addr, connector_id);
        }
        next.run(request).await
    }

    /// The de-register handler for terminating
    /// the WebSocket connection between router
    /// and connector.
    pub async fn de_register_handler(
        Extension(connector_id): Extension<String>,
        wsu: WebSocketUpgrade,
    ) -> impl IntoResponse {
        wsu.on_upgrade(move |ws| {
            debug!("Sending goodbye to connector_id={}", connector_id);
            Self::send_goodbye(ws)
        })
    }

    async fn send_goodbye(mut ws: WebSocket) {
        let _ = ws.send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: Cow::from("Deregister complete"),
        }))).await;
        let _ = ws.close().await;
    }

    /// The register handler for connectors.
    /// When a Websocket connection is successfully
    /// created via `upgrade` mechanism of HTTP,
    /// the ws connection will be harvested and
    /// stored in the `WebSocketFarm` waiting for
    /// being consumed by client requests.
    // #[debug_handler]
    pub async fn register_handler(
        State(app_state): State<ACRState>,
        wsu: WebSocketUpgrade,
        Extension(cif): Extension<CrankerConnectorInfo>,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ) -> impl IntoResponse
    {
        wsu
            .protocols([cif.negotiated_cranker_version])
            .on_upgrade(move |socket| router_socket::harvest_router_socket(
                socket,
                app_state.clone(),
                cif.connector_id,
                cif.component_name,
                cif.negotiated_cranker_version,
                cif.domain,
                cif.route,
                addr,
            ))
    }

    /// The function needed to be applied before connector
    /// registration.
    /// Domain and route is mandatory so add a middleware to
    /// check.
    /// Return BAD_REQUEST if illegal
    // #[debug_handler]
    pub async fn reg_check_and_extract(
        State(app_state): State<ACRState>,
        headers: HeaderMap,
        // Not expecting multiple val for a key so hashmap is fine
        Query(params): Query<HashMap<String, String>>,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        mut request: Request<Body>,
        next: Next,
    ) -> Response
    {
        if request.method() == Method::TRACE {
            return (StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.").into_response();
        }
        debug!("We got someone registering. Let's examine its info: headers={:?}", &headers);

        // Extract component name and connector id
        let connector_id = params.get("connectorInstanceID")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let uuid = Uuid::new_v4().to_string();
                warn!("Connector id is not specified. Generating a random uuid for it: {}", uuid);
                uuid
            });

        let route = headers.get("Route")
            .and_then(|r| r.to_str().ok())
            .and_then(|s| Some(s.to_string()))
            .unwrap_or_else(|| {
                warn!("No route specified for connector_id={}. Fallback to \"*\"", connector_id);
                "*".to_string()
            });

        if !app_state.config.registration_ip_validator.allow(addr.ip()) {
            return failed_at_ip_validation(
                addr,
                route.as_str(),
                connector_id.as_str(),
            ).into_response();
        }

        let component_name = params.get("componentName")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let sub_uuid = connector_id.chars().take(5).collect::<String>();
                warn!(
                    "Component name is not set. Name it as unknown-{}. Route={}. Connector Id={}",
                    sub_uuid, route, connector_id
                );
                "unknown-".to_string() + sub_uuid.as_str()
            });

        // v3 only parameter
        let domain = headers.get("Domain")
            .and_then(|d| d.to_str().ok())
            .and_then(|s| Some(s.to_string()))
            .or({
                // warn!("No domain specified. Fallback to \"*\"");
                Some("*".to_string())
            })
            .unwrap();

        let negotiated_cranker_version = match extract_cranker_version(&app_state.config.supported_cranker_protocols, &headers) {
            Ok(v) => {
                v
            }
            Err(err) => {
                error!("Not able to negotiate cranker version during handshake: {}", err);
                return (StatusCode::BAD_REQUEST,
                        format!("Failed to negotiate cranker version: {}", err)
                ).into_response();
            }
        };
        let connector_info = CrankerConnectorInfo {
            connector_id,
            route,
            component_name,
            domain,
            negotiated_cranker_version,
        };

        request.extensions_mut().insert(connector_info);

        // Chain the layer
        let mut response = next.run(request).await;
        // mu cranker router will set this, cranker connector doesn't seem to read this header
        // add here for compatibility. Not sure axum will actually add this or not since the
        // ws.on_upgrade will be executed next and take the stream and set the negotiated protocol
        // as sub protocol header
        if let Ok(cph) = HeaderValue::from_str(negotiated_cranker_version) {
            response.headers_mut().insert(CRANKER_PROTOCOL_HEADER_KEY, cph);
        }
        response
    }

    pub(crate) fn collect_info_by_state(acr_state: ACRState) -> RouterInfo {
        let websocket_farm = acr_state.websocket_farm.clone();
        let services = router_info::get_connector_service_list(
            websocket_farm.clone().get_sockets(),
            websocket_farm.clone().get_dark_hosts(),
        );
        RouterInfo {
            services,
            dark_hosts: websocket_farm.clone().get_dark_hosts(),
            waiting_tasks: websocket_farm.clone().get_waiting_tasks(),
        }
    }

    /// Gets metadata about the connected services.
    /// The function to collect all information defined in info
    /// structs.
    pub fn collect_info(&self) -> RouterInfo {
        Self::collect_info_by_state(self.state.clone())
    }

    /// Return the currently idle WebSocket connection count.
    /// It's not accurate if V3 support is enabled since in
    /// V1 each client request consumes an idle ws but in V3
    /// it's multiplexing so the ws is serving request but
    /// still be counted idle.
    pub fn idle_connection_count(&self) -> i32 {
        self.state.clone().websocket_farm.idle_count()
    }

    /// A manager that allows you to stop or start requests
    /// going to specific hosts.
    ///
    /// Currently only applies to V1 connectors. V3 in
    /// mu-cranker-router implementation has abandoned this
    /// feature.
    pub fn dark_mode_manager(&self) -> Arc<DarkModeManager> {
        self.state.clone().dark_mode_manager.clone()
    }

    /// Disconnects all sockets and cleans up.
    pub fn stop(&self) {
        self.state.clone().websocket_farm.clone().terminate_all()
    }
}

#[derive(Clone)]
pub struct CrankerConnectorInfo {
    pub connector_id: String,
    pub route: String,
    pub component_name: String,
    pub domain: String,
    pub negotiated_cranker_version: &'static str,
}

fn check_supported_cranker_version(versions: HashSet<String>) -> HashMap<&'static str, &'static str> {
    if versions.is_empty() {
        panic!("No supported cranker version provided.");
    }
    let mut res = HashMap::new();
    versions.iter().for_each(|ver| {
        SUPPORTED_CRANKER_VERSION.iter().for_each(|(&k, &v)| {
            if k.eq_ignore_ascii_case(ver) {
                res.insert(k, v);
            } else if v.eq_ignore_ascii_case(ver) {
                res.insert(k, v);
            } else {
                panic!("Not supported cranker version: {}", v);
            }
        })
    });
    if res.is_empty() {
        panic!("No supported cranker version provided.");
    }
    res
}

fn check_via_name(via_name: &String) {
    if via_name.len() > 1023 {
        panic!("The via name you set is too long!");
    }
    let allowed = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!#$%&'*+,-.^_`|~:";
    via_name.chars().for_each(|c| {
        if !c.is_ascii() {
            panic!("non ascii letter in via header: \"{}\"", c)
        }
    });
    // From Dan: hyphen should only be at the beginning or the end
    let mut idx = 0;
    let len = via_name.len(); // given all ascii chars, string length == vec length
    via_name.chars().for_each(|v| {
        if !allowed.chars().any(|a| a == v) {
            panic!("not allowed character in via header: \"{}\"", char::from(v.clone()))
        }
        if v == '-' {
            if idx != 0 || idx != len - 1 {
                panic!("hyphen should only be at the beginning or the end of the via header!");
            }
        }
        idx += 1;
    })
}

fn check_proxy_listeners(proxy_listeners: &Vec<Arc<dyn ProxyListener>>) {
    for i in proxy_listeners {
        if i.really_need_on_response_body_chunk_received_from_target() {
            error!("Detected one proxy listener really needs on_response_body_chunk_received_from_target hook. This hook is expensive.");
        }
    }

    for i in proxy_listeners {
        if i.really_need_on_request_body_chunk_sent_to_target() {
            error!("Detected one proxy listener really needs on_request_body_chunk_sent_to_target hook. This hook is expensive.");
        }
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

fn failed_at_ip_validation(addr: SocketAddr, route: &str, connector_id: &str) -> (StatusCode, String) {
    let failed_reason = format!(
        "Fail to establish websocket connection to cranker connector because of not supported ip address={}. Route={}. Connector Id={}",
        addr.ip(), route, connector_id
    );
    warn!("{}", failed_reason);
    (StatusCode::FORBIDDEN, failed_reason)
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
        return Err(CrankerRouterException::new(
            "Version is null. Please set header Sec-WebSocket-Protocol for cranker protocol negotiation".to_string()
        ).with_err_kind(CrexKind::CrankerProtocolVersionNotFound_0003).into());
    }

    match sub_protocols {
        Some(sp) => {
            debug!("sub_protocols len {}",sp.len());
            let sp_str = sp.to_str().ok().unwrap();
            debug!("sub_protocols = {}", sp_str);
            let split = sp_str.split(",");
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

    let crex = CrankerRouterException::new(format!(
        "Cranker version not supported! sub protocols = {:?}, legacy protocols = {:?}",
        sub_protocols, legacy_protocol_header
    )).with_err_kind(CrexKind::CrankerProtocolVersionNotSupported_0004);

    error!("{:?}",crex);
    return Err(crex.into());
}

/// The builder / config of a CrankerRouter
pub type CrankerRouterBuilder = CrankerRouterConfig;

/// The config / builder of a CrankerRouter
#[derive(Clone)]
pub struct CrankerRouterConfig {
    pub proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    pub router_socket_filter: Arc<dyn RouterSocketFilter>,

    // config
    pub discard_client_forwarded_headers: bool,
    pub send_legacy_forwarded_headers: bool,
    pub via_name: String,
    pub routes_keep_time_millis: i64,
    pub connector_max_wait_time_millis: i64,
    pub ping_sent_after_no_write_for_ms: i64,
    pub idle_read_timeout_ms: i64,
    pub allow_catch_all: bool,

    pub do_not_proxy_headers: HashSet<&'static str>,
    pub registration_ip_validator: Arc<dyn IPValidator>,
    pub route_resolver: Arc<dyn RouteResolver>,
    pub supported_cranker_protocols: HashMap<&'static str, &'static str>,
}

const DEF_IDLE_READ_TIMEOUT_MS: i64 = 60_000;
const DEF_ROUTES_KEEP_TIME_MILLIS: i64 = 2 * 60 * 60 * 1_000; // 2 hours
const DEF_PING_SENT_AFTER_NO_WRITE_FOR_MS: i64 = 10_000;
const DEF_CONNECTOR_MAX_WAIT_TIME_MILLIS: i64 = 5_000;

impl CrankerRouterBuilder {
    /// Create a builder
    pub fn new() -> CrankerRouterBuilder {
        Self {
            proxy_listeners: vec![],
            router_socket_filter: Arc::new(DefaultRouterSocketFilter::new()),

            discard_client_forwarded_headers: false,
            send_legacy_forwarded_headers: false,
            via_name: "scr-axum".to_string(),

            routes_keep_time_millis: DEF_ROUTES_KEEP_TIME_MILLIS,
            connector_max_wait_time_millis: DEF_CONNECTOR_MAX_WAIT_TIME_MILLIS,
            ping_sent_after_no_write_for_ms: DEF_PING_SENT_AFTER_NO_WRITE_FOR_MS,
            idle_read_timeout_ms: DEF_IDLE_READ_TIMEOUT_MS,
            allow_catch_all: true,

            do_not_proxy_headers: REPRESSED_HEADERS.clone(),
            registration_ip_validator: Arc::new(AllowAll::new()),
            route_resolver: Arc::new(DefaultRouteResolver::new()),
            supported_cranker_protocols: SUPPORTED_CRANKER_VERSION.clone(),
        }
    }

    /// Build and get a CrankerRouter instance
    pub fn build(&self) -> CrankerRouter {
        check_proxy_listeners(&self.proxy_listeners);
        CrankerRouter::new(self.clone())
    }

    /// If true, then any Forwarded or X-Forwarded-* headers that are sent
    /// from the client to this reverse proxy will be dropped
    /// (defaults to false).
    pub fn with_discard_client_forwarded_headers(self, discard_client_forwarded_headers: bool) -> Self {
        let mut c = self.clone();
        c.discard_client_forwarded_headers = discard_client_forwarded_headers;
        c
    }

    /// mu-cranker-router always sends Forwarded headers, however by
    /// default does not send the non-standard X-Forwarded-* headers.
    ///
    /// Set this to true to enable these legacy headers for older clients
    /// that rely on them.
    pub fn with_send_legacy_forwarded_headers(self, send_legacy_forwarded_headers: bool) -> Self {
        let mut c = self.clone();
        c.send_legacy_forwarded_headers = send_legacy_forwarded_headers;
        c
    }

    /// The name to add as the Via header, which defaults to "scr-axum".
    ///
    /// Note that limited ASCII characters can be used for via name.
    /// They are
    ///
    /// "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!#$%&'*+,-.^_`|~:"
    pub fn with_via_name(self, via_name: String) -> Self {
        let mut c = self.clone();
        check_via_name(&via_name);
        c.via_name = via_name;
        c
    }

    /// Sets the idle timeout of a WebSocket connection.
    ///
    /// If nothing read from the client target side connector
    /// via the ws connection more than this timeout milliseconds,
    /// including `Ping` and `Pong` message, the RouterSocket
    /// will be terminated along with the underlying websocket.
    ///
    /// Default is 5 min.
    pub fn with_idle_read_timeout_ms(self, idle_read_timeout_ms: i64) -> Self {
        panic_if_less_than_zero("idle_read_timeout_ms", idle_read_timeout_ms);
        let mut c = self.clone();
        c.idle_read_timeout_ms = idle_read_timeout_ms;
        c
    }

    /// Sets the routes keep time if no more connector registered.
    /// Within the time, client will receive 503 (no cranker available).
    /// After that, the route info will be cleaned up, and client will
    /// receive 404 if requesting against this route.
    ///
    /// The default keep time is 2 hours.
    ///
    /// The route will be kept for this amount of milliseconds until
    /// being removed from the internal state and the Cranker router
    /// will respond with 404 Not Found immediately without waiting
    /// if subsequent client requests match the route.
    pub fn with_routes_keep_time_millis(self, routes_keep_time_millis: i64) -> Self {
        panic_if_less_than_zero("routes_keep_time_millis", routes_keep_time_millis);
        let mut c = self.clone();
        c.routes_keep_time_millis = routes_keep_time_millis;
        c
    }

    /// Sets the amount of time to wait before sending a ping message if no
    /// messages having been sent.
    pub fn with_ping_sent_after_no_write_for_ms(self, ping_sent_after_no_write_for_ms: i64) -> Self {
        panic_if_less_than_zero("ping_sent_after_no_write_for_ms", ping_sent_after_no_write_for_ms);
        let mut c = self.clone();
        c.ping_sent_after_no_write_for_ms = ping_sent_after_no_write_for_ms;
        c
    }

    /// When a request is made for a route that has no connectors connected
    /// currently, the router will wait for a period to see if a connector
    /// will connect that can service the request.
    ///
    /// This is important because if there are a burst of requests for a
    /// route, there might be just a few milliseconds gap where there is no
    /// connector available, so there is no point sending an error back to
    /// the client if it would be found after a short period.
    ///
    /// This setting controls how long it waits before returning a 503
    /// Service Unavailable to the client.
    pub fn with_connector_max_wait_time_millis(self, connector_max_wait_time_millis: i64) -> Self {
        panic_if_less_than_zero("connector_max_wait_time_millis", connector_max_wait_time_millis);
        let mut c = self.clone();
        c.connector_max_wait_time_millis = connector_max_wait_time_millis;
        c
    }

    /// Sets if allow to register catch all / wildcard ("*") route
    /// as a fallback for all no-route-matched client requests
    pub fn should_allow_catch_all(self, allow_catch_all: bool) -> Self {
        let mut c = self.clone();
        c.allow_catch_all = allow_catch_all;
        c
    }


    /// Specifies whether to send the original `Host` header to the
    /// target server.
    ///
    /// Reverse proxies are generally supposed to forward the original Host
    /// header to target servers, however there are cases (particularly where
    /// you are proxying to HTTPS servers) that the Host needs to match the
    /// Host of the SSL certificate (in which case you may see SNI-related
    /// errors).
    pub fn should_proxy_host_header(self, should_proxy_host_header: bool) -> Self {
        let mut c = self.clone();
        if should_proxy_host_header {
            c.do_not_proxy_headers.remove("host");
        } else {
            c.do_not_proxy_headers.insert("host");
        }
        c
    }

    /// Sets the IP validator for service registration requests.
    /// Defaults to `ip_validator::AllowAll`
    pub fn with_registration_ip_validator(self, ip_validator: Arc<dyn IPValidator>) -> Self {
        let mut c = self.clone();
        c.registration_ip_validator = ip_validator;
        c
    }

    /// Registers proxy listeners to be called before, during and after
    /// requests are processed.
    pub fn with_proxy_listeners(self, proxy_listeners: Vec<Arc<dyn ProxyListener>>) -> Self {
        let mut c = self.clone();
        c.proxy_listeners = proxy_listeners;
        c
    }

    /// Provide a custom route resolver. If it's not specified, will use the
    /// default implementation in RouteResolver.resolve(&DashSet, &String)
    ///
    /// LongestFirstRouteResolver is also provided in the library.
    pub fn with_route_resolver(self, route_resolver: Arc<dyn RouteResolver>) -> Self {
        let mut c = self.clone();
        c.route_resolver = route_resolver;
        c
    }

    /// Provide a custom RouterSocketFilter.
    ///
    /// By default, when the RouteResolver resolves the route
    /// from the client request path, if there's matched router
    /// socket available, then it will be used to serve the
    /// client request. Further filtering can be applied in
    /// terms of whether the router socket should be used
    /// based on its detail along with client request detail.
    ///
    /// By default, no filtering will be added that should_use()
    /// will always return true.
    pub fn with_router_socket_filter(self, router_socket_filter: Arc<dyn RouterSocketFilter>) -> Self {
        let mut c = self.clone();
        c.router_socket_filter = router_socket_filter;
        c
    }

    /// Set cranker protocols.
    ///
    /// Default supporting both ["cranker_1.0", "cranker_3.0"].
    pub fn with_supported_cranker_version(self, supported_cranker_version: HashSet<String>) -> Self {
        let scv = check_supported_cranker_version(supported_cranker_version);
        let mut c = self.clone();
        c.supported_cranker_protocols = scv;
        c
    }
}

fn panic_if_less_than_zero(name: &'static str, val: i64) {
    if val >= 0 {
        return;
    }
    panic!("{} must be greater or equals to 0", name);
}

#[cfg(test)]
mod lib_tests {
    use crate::check_via_name;

    #[test]
    #[should_panic]
    fn test_non_ascii_char_in_via() {
        check_via_name(&"🏦".to_string());
    }

    #[test]
    #[should_panic]
    fn test_invalid_ascii_char_in_via() {
        check_via_name(&"[]".to_string());
    }

    #[test]
    #[should_panic]
    fn test_hyphen_in_via() {
        check_via_name(&"a-b".to_string());
    }
}
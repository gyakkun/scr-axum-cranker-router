use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use async_channel::Sender;
use axum::{BoxError, Extension, http, Router, ServiceExt};
use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, OriginalUri, Query, State, WebSocketUpgrade};
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt};
use futures::stream::BoxStream;
use lazy_static::lazy_static;
use log::{debug, error, warn};
use tower_http::limit;
use uuid::Uuid;
use crate::cranker_protocol_request_builder::EndMarker::RequestBodyEnded;

use crate::exceptions::{CrankerProtocolVersionNotFoundException, CrankerProtocolVersionNotSupportedException, CrankerRouterException};
use crate::ip_validator::{AllowAll, IPValidator};
use crate::proxy_listener::ProxyListener;
use crate::route_resolver::{DefaultRouteResolver, RouteResolver};
use crate::router_socket::RouterSocket;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};

pub mod cranker_protocol;
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

pub(crate) const CRANKER_PROTOCOL_HEADER_KEY: &'static str = "CrankerProtocol";
// should be CrankerProtocol, but axum convert all header key to lowercase when reading req from client and sending res
// e.g. cli req with header[("hi","l"), ("HI","U"), ("Hi","Ca")], then you need header_map.get_all()
// then you can iterate and get "l", "U" and "Ca"
// Same as the header_map in res
pub const _VER_1_0: &'static str = "1.0";
pub const _VER_3_0: &'static str = "3.0";

pub const CRANKER_V_1_0: &'static str = "cranker_1.0";
pub const CRANKER_V_3_0: &'static str = "cranker_3.0";

#[derive(Clone, Debug)]
struct VersionNegotiate {
    dealt: &'static str,
}

lazy_static! {
    // Runtime evaluated, so it should be the actual serving server local ip
    // rather than compile machine ip
    pub static ref LOCAL_IP: IpAddr = local_ip_address::local_ip().unwrap();

    static ref SUPPORTED_CRANKER_VERSION: HashMap<&'static str, &'static str> =  {
        let mut s = HashMap::new();
        s.insert(_VER_1_0, CRANKER_V_1_0);
        // V3 not implemented yet
        // s.insert(_VER_3_0, CRANKER_V_3_0);
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
    websocket_farm: Arc<WebSocketFarm>,
    // route_to_socket_chan: DashMap<String, (
    //     Sender<Arc<dyn RouterSocket>>,
    //     Receiver<Arc<dyn RouterSocket>>
    // )>,

    config: CrankerRouterConfig,
    // config
    // proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    // discard_client_forwarded_headers: bool,
    // send_legacy_forwarded_headers: bool,
    // via_name: String,
    // routes_keep_time_millis: i64,
    // max_wait_time_millis: i64,
    // ping_sent_after_no_write_for_ms: i64,
    // idle_read_timeout_ms: i64,
    // do_not_proxy_headers: HashSet<&'static str>,
    // ip_validator: Arc<dyn IPValidator>,
    // route_resolver: Arc<dyn RouteResolver>,
    // supported_cranker_version: HashSet<&'static str>,
}

pub type TSCRState = Arc<CrankerRouterState>;

pub struct CrankerRouter {
    state: TSCRState,
}

impl CrankerRouter {
    pub fn new(
        config: CrankerRouterConfig
        // proxy_listeners: Vec<Arc<dyn ProxyListener>>,
        // discard_client_forwarded_headers: bool,
        // send_legacy_forwarded_headers: bool,
        // via_name: String,
        // routes_keep_time_millis: i64,
        // max_wait_time_millis: i64,
        // ping_sent_after_no_write_for_ms: i64,
        // idle_read_timeout_ms: i64,
        // send_host_header_to_target: bool,
        // ip_validator: Arc<dyn IPValidator>,
        // route_resolver: Arc<dyn RouteResolver>,
        // supported_cranker_version: HashSet<String>,
    ) -> Self {
        // FIXME: Move these checks to builder
        // check_proxy_listeners(&proxy_listeners);
        // check_via_name(&via_name);
        // let supported_cranker_version = check_supported_cranker_version(supported_cranker_version);
        // let mut do_not_proxy_headers: HashSet<&'static str> = REPRESSED_HEADERS.clone();
        // if !send_host_header_to_target {
        //     do_not_proxy_headers.insert("host");
        // } else {
        //     do_not_proxy_headers.remove("host");
        // }

        let websocket_farm = WebSocketFarm::new(
            Arc::new(DefaultRouteResolver::new()),
            config.connector_max_wait_time_millis,
            config.proxy_listeners.clone()
        );
        let websocket_farm_clone = websocket_farm.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(
                    config.routes_keep_time_millis as u64)
                ).await;
                websocket_farm_clone.clone()
                    .clean_routes_in_background(config.routes_keep_time_millis);
            }
        });
        Self {
            state: Arc::new(CrankerRouterState {
                counter: AtomicU64::new(0),
                websocket_farm,
                config,
                // proxy_listeners,
                // discard_client_forwarded_headers,
                // send_legacy_forwarded_headers,
                // via_name,
                // routes_keep_time_millis,
                // max_wait_time_millis,
                // ping_sent_after_no_write_for_ms,
                // idle_read_timeout_ms,
                // do_not_proxy_headers,
                // ip_validator,
                // route_resolver,
                // supported_cranker_version,
            }),
        }
    }

    pub fn registration_axum_router(&self) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
        let res = Router::new()
            .route("/register", any(CrankerRouter::register_handler)
                .layer(from_fn_with_state(self.state(), CrankerRouter::validate_route_domain_and_cranker_version)),
            )
            .route("/register/", any(CrankerRouter::register_handler)
                .layer(from_fn_with_state(self.state(), CrankerRouter::validate_route_domain_and_cranker_version)),
            )
            .route("/deregister", any(CrankerRouter::de_register_handler)
                .layer(from_fn_with_state(self.state(), CrankerRouter::de_reg_check))
            )
            .route("/deregister/", any(CrankerRouter::de_register_handler)
                .layer(from_fn_with_state(self.state(), CrankerRouter::de_reg_check))
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
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        req: Request<Body>,
    ) -> Response
    {
        // FIXME: How to judge if has body or not
        // Get should have no body but not required
        let http_version = req.version();
        debug!("Http version {:?}", http_version);
        let body = req.into_body();
        let has_body = judge_has_body_from_header_http_1(&headers) || !body.is_end_stream();

        let body_data_stream = body.into_data_stream()
            .map_err(|e| CrankerRouterException::new(format!(
                "axum ex when piping cli res body: {:?}", e
            )));
        // The full qualified name is  Pin<Box<MapErr<BodyDataStream, fn(Error) -> CrankerRouterException>>> which is too long
        let boxed_stream = Box::pin(body_data_stream) as BoxStream<Result<Bytes, CrankerRouterException>>;

        let path = original_uri.path().to_string();
        let socket_fut = app_state.websocket_farm.clone().get_router_socket_by_target_path(path.clone()).await;
        match socket_fut {
            Ok(rs) => {
                debug!("Get a socket");
                let mut opt_body = None;
                if has_body {
                    let (pipe_tx, pipe_rx) = async_channel::unbounded::<Result<Bytes, CrankerRouterException>>();
                    opt_body = Some(pipe_rx);
                    tokio::spawn(pipe_body_data_stream_to_channel_sender(boxed_stream, pipe_tx));
                }
                debug!("Has body ? {:?}", opt_body);
                debug!("Ready to lock on router socket");
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
                    Ok(res) => {
                        debug!("recv tgt res bdy from rs!");
                        if !res.status().is_success() && !rs_clone.is_removed() {
                            let _ = rs_clone.raise_completion_event();
                        }
                        return res;
                    }
                    Err(e) => {
                        debug!("recv err from rs, expect tgt res bdy: {:?}", e);
                        if !rs_clone.is_removed() {
                            let _ = rs_clone.raise_completion_event();
                        }
                        return e.into_response();
                    }
                }
            }
            Err(e) => {
                debug!("No socket available!");
                return e.clone().into_response();
            }
        };
    }

    pub async fn de_reg_check(
        State(app_state): State<TSCRState>,
        // wsu: WebSocketUpgrade,
        Query(params): Query<HashMap<String, String>>, // Not expecting multiple val for a key so hashmap is fine
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        mut request: Request<Body>,
        next: Next,
    ) -> Response {
        debug!("We got someone DE registering. Let's examine its info: headers={:?}", &headers);
        let route = match headers.get("Route")
            .map(|r| r.to_str())
            .unwrap_or_else(|| {
                warn!("No route specified. Fallback to \"*\"");
                Ok("*")
            }) {
            Ok(v) => v.to_string(),
            Err(to_str_err) => {
                error!("Failed to convert Route header to str: {:?}",to_str_err);
                return (StatusCode::BAD_REQUEST, format!("Failed to convert Route header to str: {:?}", to_str_err)).into_response();
            }
        };
        if !app_state.config.registration_ip_validator.allow(addr.ip()) {
            return failed_at_ip_validation(addr, route.as_str()).into_response();
        }

        // let mut ext_map = HashMap::new();
        // ext_map.insert("route", Some(route.clone()));
        let connector_instance_id = params.get("connectorInstanceID")
            .map(|s| s.to_string());
        if connector_instance_id.is_none() {
            warn!("the service route={} using unsupported zero down time connector, will not deregister socket", route);
            // ext_map.insert("connectorInstanceId", None);
        } else {
            let connector_instance_id = connector_instance_id.unwrap();
            // ext_map.insert("connectorInstanceId", Some(connector_instance_id.clone()));
            app_state.websocket_farm.clone().de_register_socket_in_background(route, addr, connector_instance_id);
        }
        // request.extensions_mut().insert(ext_map);
        let res = next.run(request).await;
        res
    }
    pub async fn de_register_handler(
        // State(app_state): State<TSCRState>,
        wsu: WebSocketUpgrade,
        // Query(params): Query<HashMap<String, String>>, // Not expecting multiple val for a key so hashmap is fine
        // ConnectInfo(addr): ConnectInfo<SocketAddr>,
        // headers: HeaderMap,
    ) -> impl IntoResponse {
        wsu.on_upgrade(move |ws| Self::send_goodbye(ws))
    }

    async fn send_goodbye(mut ws: WebSocket) {
        let _ = ws.send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: Cow::from("Deregister complete"),
        }))).await;
        let _ = ws.close().await;
    }

    // #[debug_handler]
    pub async fn register_handler(
        State(app_state): State<TSCRState>,
        wsu: WebSocketUpgrade,
        Extension(ext_map): Extension<HashMap<String, String>>,
        Extension(ver_neg): Extension<VersionNegotiate>,
        Query(params): Query<HashMap<String, String>>, // Not expecting multiple val for a key so hashmap is fine
        headers: HeaderMap,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ) -> impl IntoResponse
    {
        let route = ext_map.get(&"route".to_string()).unwrap();
        let domain = ext_map.get(&"domain".to_string()).unwrap();
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

        wsu
            .protocols([ver_neg.dealt])
            .on_upgrade(move |socket| router_socket::take_wss_and_create_router_socket(
                socket, app_state.clone(), connector_id, component_name, ver_neg.dealt.to_string(), domain, route,
                addr,
            ))
    }

    /// Domain and route is mandatory so add a middleware to check.
    /// Return BAD_REQUEST if illegal
    // #[debug_handler]
    async fn validate_route_domain_and_cranker_version(
        State(app_state): State<TSCRState>,
        headers: HeaderMap,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
        mut request: Request<Body>,
        next: Next,
    ) -> Response
    {
        // if method == Method::TRACE {
        //     return (StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.").into_response();
        // }
        debug!("We got someone registering. Let's examine its info: headers={:?}", &headers);

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

        if !app_state.config.registration_ip_validator.allow(addr.ip()) {
            return failed_at_ip_validation(addr, route).into_response();
        }

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
        let mut response = next.run(request).await;
        // mu cranker router will set this, cranker connector doesn't seem to read this header
        // add here for compatibility. Not sure axum will actually add this or not since the
        // ws.on_upgrade will be executed next and take the stream and set the negotiated protocol
        // as sub protocol header
        if let Ok(cph) = HeaderValue::from_str(dealt_version) {
            response.headers_mut().insert(CRANKER_PROTOCOL_HEADER_KEY, cph);
        }
        response
    }
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
            error!("Detect a proxy listener really needs on_response_body_chunk_received_from_target hook. This hook is expensive.");
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




fn failed_at_ip_validation(addr: SocketAddr, route: &str) -> (StatusCode, String) {
    let failed_reason = format!(
        "Fail to establish websocket connection to cranker connector because of not supported ip address={}. Route={}",
        addr.ip(), route
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

fn get_custom_hop_by_hop_headers(opt_conn_hdr: Option<&HeaderValue>) -> HashSet<String> {
    let mut res = HashSet::new();
    if let Some(conn_hdr) = opt_conn_hdr {
        if let Ok(conn_hdr_str) = conn_hdr.to_str() {
            if !conn_hdr_str.is_empty() {
                let split = conn_hdr_str.split(",");
                for s in split.into_iter() {
                    res.insert(s.trim().to_string());
                }
            }
        }
    }
    return res;
}

pub type CrankerRouterBuilder = CrankerRouterConfig;

#[derive(Clone)]
pub struct CrankerRouterConfig {
    proxy_listeners: Vec<Arc<dyn ProxyListener>>,

    // config
    discard_client_forwarded_headers: bool,
    send_legacy_forwarded_headers: bool,
    via_name: String,

    routes_keep_time_millis: i64,
    connector_max_wait_time_millis: i64,
    ping_sent_after_no_write_for_ms: i64,
    idle_read_timeout_ms: i64,

    // proxy_host_header: bool,
    do_not_proxy_headers: HashSet<&'static str>,
    registration_ip_validator: Arc<dyn IPValidator>,
    route_resolver: Arc<dyn RouteResolver>,
    supported_cranker_protocols: HashMap<&'static str, &'static str>,
}

impl CrankerRouterBuilder {
    pub fn new() -> CrankerRouterBuilder {
        Self {
            discard_client_forwarded_headers: false,
            send_legacy_forwarded_headers: false,
            via_name: "scr-axum".to_string(),
            idle_read_timeout_ms: 60_000,
            routes_keep_time_millis: 2 * 60 * 60 * 1_000, // 2hours
            ping_sent_after_no_write_for_ms: 10_000,
            connector_max_wait_time_millis: 5_000,
            do_not_proxy_headers: REPRESSED_HEADERS.clone(),
            registration_ip_validator: Arc::new(AllowAll::new()),
            proxy_listeners: vec![],
            route_resolver: Arc::new(DefaultRouteResolver::new()),
            supported_cranker_protocols: SUPPORTED_CRANKER_VERSION.clone(),
        }
    }

    pub fn build(&self) -> CrankerRouter {
        CrankerRouter::new(self.clone())
    }

    pub fn with_discard_client_forwarded_headers(self, discard_client_forwarded_headers: bool) -> Self {
        let mut c = self.clone();
        c.discard_client_forwarded_headers = discard_client_forwarded_headers;
        c
    }
    pub fn with_send_legacy_forwarded_headers(self, send_legacy_forwarded_headers: bool) -> Self {
        let mut c = self.clone();
        c.send_legacy_forwarded_headers = send_legacy_forwarded_headers;
        c
    }
    pub fn with_via_name(self, via_name: String) -> Self {
        let mut c = self.clone();
        check_via_name(&via_name);
        c.via_name = via_name;
        c
    }

    pub fn with_idle_read_timeout_ms(self, idle_read_timeout_ms: i64) -> Self {
        panic_if_less_than_zero("idle_read_timeout_ms", idle_read_timeout_ms);
        let mut c = self.clone();
        c.idle_read_timeout_ms = idle_read_timeout_ms;
        c
    }

    pub fn with_routes_keep_time_millis(self, routes_keep_time_millis: i64) -> Self {
        panic_if_less_than_zero("routes_keep_time_millis", routes_keep_time_millis);
        let mut c = self.clone();
        c.routes_keep_time_millis = routes_keep_time_millis;
        c
    }
    pub fn with_ping_sent_after_no_write_for_ms(self, ping_sent_after_no_write_for_ms: i64) -> Self {
        panic_if_less_than_zero("ping_sent_after_no_write_for_ms", ping_sent_after_no_write_for_ms);
        let mut c = self.clone();
        c.ping_sent_after_no_write_for_ms = ping_sent_after_no_write_for_ms;
        c
    }

    pub fn with_connector_max_wait_time_millis(self, connector_max_wait_time_millis: i64) -> Self {
        panic_if_less_than_zero("connector_max_wait_time_millis", connector_max_wait_time_millis);
        let mut c = self.clone();
        c.connector_max_wait_time_millis = connector_max_wait_time_millis;
        c
    }

    pub fn should_proxy_host_header(self, should_proxy_host_header: bool) -> Self {
        let mut c = self.clone();
        if should_proxy_host_header {
            c.do_not_proxy_headers.remove("host");
        } else {
            c.do_not_proxy_headers.insert("host");
        }
        c
    }

    pub fn with_registration_ip_validator(self, ip_validator: Arc<dyn IPValidator>) -> Self {
        let mut c = self.clone();
        c.registration_ip_validator = ip_validator;
        c
    }

    pub fn with_proxy_listeners(self, proxy_listeners: Vec<Arc<dyn ProxyListener>>) -> Self {
        let mut c = self.clone();
        c.proxy_listeners = proxy_listeners;
        c
    }

    pub fn with_route_resolver(self, route_resolver: Arc<dyn RouteResolver>) -> Self {
        let mut c = self.clone();
        c.route_resolver = route_resolver;
        c
    }
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
mod tests {
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
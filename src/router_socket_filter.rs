use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method};

use crate::CrankerRouterConfig;
use crate::router_socket::RouterSocket;

/// With this trait, library users can implement their own logic to filter out
/// whether a target-path-matched router socket should be used or not.
/// For example, we can filter by the RouterSocket's domain.
pub trait RouterSocketFilter: Sync + Send {
    fn should_use(
        &self,
        target_path: String,
        method: Method,
        original_uri: OriginalUri,
        headers: HeaderMap,
        addr: SocketAddr,
        cranker_router_config: CrankerRouterConfig,
        router_socket: Arc<dyn RouterSocket>,
    ) -> bool;

    /// If no exact ones matched, should fall back to the first
    /// one which matches the target path by `RouteResolver`
    /// Return false by default
    fn should_fallback_to_first_path_matched(&self) -> bool {
        false
    }

    /// If no any path matched (even partial), should fall back
    /// to the catch-all connector socket
    /// Return true by default
    fn should_fallback_to_catch_all(&self) -> bool { true }
}

/// Always return true for should_use()
pub struct DefaultRouterSocketFilter;

impl DefaultRouterSocketFilter {
    pub const fn new() -> Self {
        DefaultRouterSocketFilter {}
    }
}

impl RouterSocketFilter for DefaultRouterSocketFilter {
    fn should_use(
        &self,
        _: String, _: Method, _: OriginalUri, _: HeaderMap, _: SocketAddr, _: CrankerRouterConfig, _: Arc<dyn RouterSocket>,
    ) -> bool { true }
}

pub struct DomainRouterSocketFilter;

impl DomainRouterSocketFilter {
    pub const fn new() -> Self {
        DomainRouterSocketFilter {}
    }
}

impl RouterSocketFilter for DomainRouterSocketFilter {
    fn should_use(
        &self, _: String, _: Method, original_uri: OriginalUri, _: HeaderMap, _: SocketAddr, _: CrankerRouterConfig, rs: Arc<dyn RouterSocket>,
    ) -> bool {
        original_uri.host().map(|host| {
            rs.domain() == host
        }).unwrap_or(false)
    }

    fn should_fallback_to_first_path_matched(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod router_socket_filter_test {
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use axum::extract::OriginalUri;
    use axum::extract::ws::Message;
    use axum::http::{HeaderMap, Method, Response, Version};
    use axum_core::body::{Body, BodyDataStream};

    use crate::{ACRState, CRANKER_V_1_0};
    use crate::dark_host::DarkHost;
    use crate::exceptions::CrankerRouterException;
    use crate::route_identify::RouteIdentify;
    use crate::router_socket::{ClientRequestIdentifier, RouterSocket};
    use crate::router_socket_filter::RouterSocketFilter;
    use crate::websocket_farm::WebSocketFarm;

    struct MockRouterSocket;

    impl RouteIdentify for MockRouterSocket {
        fn router_socket_id(&self) -> String {
            "*".to_string()
        }

        fn route(&self) -> String {
            "*".to_string()
        }

        fn service_address(&self) -> SocketAddr {
            SocketAddr::new([u8::MAX, u8::MAX, u8::MAX, u8::MAX].into(), u16::MAX)
        }

        fn domain(&self) -> String {
            "nyamori.moe".to_string()
        }
    }

    #[async_trait]
    impl RouterSocket for MockRouterSocket {
        fn component_name(&self) -> String {
            "mock".to_string()
        }

        fn connector_id(&self) -> String {
            "mock".to_string()
        }

        fn is_removed(&self) -> bool {
            false
        }

        fn get_is_removed_arc_atomic_bool(&self) -> Arc<AtomicBool> {
            Arc::new(AtomicBool::new(false))
        }

        fn cranker_version(&self) -> &'static str {
            CRANKER_V_1_0
        }

        fn raise_completion_event(&self, _: Option<ClientRequestIdentifier>) -> Result<(), CrankerRouterException> {
            Ok(())
        }

        fn is_dark_mode_on(&self, _: &HashSet<DarkHost>) -> bool {
            false
        }

        fn inflight_count(&self) -> i32 {
            -1
        }

        async fn on_client_req(self: Arc<Self>, _: ACRState, _: &Version, _: &Method, _: &OriginalUri, _: &HeaderMap, _: &SocketAddr, _: Option<BodyDataStream>) -> Result<(Response<Body>, Option<ClientRequestIdentifier>), CrankerRouterException> {
            Err(CrankerRouterException::new(
                "mock".to_string()
            ))
        }

        async fn send_ws_msg_to_uwss(self: Arc<Self>, _: Message) -> Result<(), CrankerRouterException> {
            Err(CrankerRouterException::new(
                "mock".to_string()
            ))
        }

        async fn terminate_all_conn(self: Arc<Self>, _: Option<CrankerRouterException>) -> Result<(), CrankerRouterException> {
            Err(CrankerRouterException::new(
                "mock".to_string()
            ))
        }

        fn inc_bytes_received_from_cli(&self, _: i32) {}

        fn try_provide_general_error(&self, _: Option<CrankerRouterException>) -> Result<(), CrankerRouterException> {
            Err(CrankerRouterException::new(
                "mock".to_string()
            ))
        }

        fn get_opt_arc_websocket_farm(&self) -> Option<Arc<WebSocketFarm>> {
            None
        }
    }

    #[test]
    fn test_domain_router_socket_filter() {
        let filter = super::DomainRouterSocketFilter::new();
        let rs = Arc::new(MockRouterSocket);
        assert_eq!(filter.should_use(
            "/".to_string(),
            Method::GET,
            OriginalUri("https://nyamori.moe/hi/there".try_into().unwrap()),
            HeaderMap::new(),
            SocketAddr::new([0, 0, 0, 0].into(), 0),
            crate::CrankerRouterConfig::new(),
            rs.clone(),
        ), true);
        assert_eq!(filter.should_use(
            "/".to_string(),
            Method::GET,
            OriginalUri("https://github.com/hi/there".try_into().unwrap()),
            HeaderMap::new(),
            SocketAddr::new([0, 0, 0, 0].into(), 0),
            crate::CrankerRouterConfig::new(),
            rs.clone(),
        ), false);
    }
}
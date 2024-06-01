use std::net::SocketAddr;

/// We define these common methods for selecting route in websocket farm
pub trait RouteIdentify {
    fn router_socket_id(&self) -> String;
    fn route(&self) -> String;
    /// Remote socket address
    fn service_address(&self) -> SocketAddr;
    /// V3 domain. Return "*" (catch all) by default
    /// for backward compatibility
    fn domain(&self) -> String { "*".to_string() }
}

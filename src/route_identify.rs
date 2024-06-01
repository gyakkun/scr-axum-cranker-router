use std::net::SocketAddr;

/// We define these common methods for selecting route in websocket farm
pub trait RouteIdentify {
    fn router_socket_id(&self) -> String;
    fn route(&self) -> String;
    fn service_address(&self) -> SocketAddr;
    fn domain(&self) -> String { "*".to_string() }
}

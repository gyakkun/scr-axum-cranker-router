use std::net::SocketAddr;

/// We define these common methods for selecting route in websocket farm
pub trait RouteIdentify {
    /// A unique id of the underlying router socket.
    fn router_socket_id(&self) -> String;
    fn route(&self) -> String;
    /// The address of the service connector that this request is
    /// being proxied to.
    fn service_address(&self) -> SocketAddr;
    /// V3 domain. Return "*" (catch all) by default
    /// for backward compatibility
    fn domain(&self) -> String { "*".to_string() }
}

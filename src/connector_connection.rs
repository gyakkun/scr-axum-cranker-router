#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ConnectorConnection {
    domain: String,
    port: i32,
    router_socket_id: String,
    protocol: String,
    inflight: i32,
}
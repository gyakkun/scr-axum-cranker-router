#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ConnectorConnection {
    pub domain: String,
    pub port: i32,
    pub router_socket_id: String,
    pub protocol: String,
    pub inflight: i32,
}
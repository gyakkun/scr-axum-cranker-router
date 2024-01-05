use serde::Serialize;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorConnection {
    pub domain: String,
    pub port: i32,
    pub router_socket_id: String,
    pub protocol: String,
    pub inflight: i32,
}
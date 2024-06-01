use serde::Serialize;

/// Information about one of the connector sockets connected to this router.
#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorConnection {
    /// The domain field of a connector connection.
    /// Only available in V3 as an extension of the
    /// protocol.
    #[serde(skip_serializing_if = "_if_ser_domain")]
    pub domain: String,
    /// The port the socket is connected on
    pub port: i32,
    /// A unique ID of this socket
    pub router_socket_id: String,
    /// The protocol version, e.g. cranker_1.0, cranker_3.0
    pub protocol: String,
    /// How many inflight request being proxying. Only available
    /// in V3 since it's multiplexing the underlying WebSocket
    #[serde(skip_serializing_if = "_if_ser_inflight")]
    pub inflight: i32,
}

fn _if_ser_domain(domain: &String)-> bool {
    domain == "*"
}

fn _if_ser_inflight(ifl: &i32) -> bool {
    ifl < &0
}
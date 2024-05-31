use std::net::IpAddr;

use serde::Serialize;

use crate::connector_connection::ConnectorConnection;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorInstance {
    pub connection_count: usize,
    pub dark_mode: bool,
    pub connector_id: String,
    /// Return the current idle connections
    pub connections: Vec<ConnectorConnection>,
    pub ip: IpAddr,
}
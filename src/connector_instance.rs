use std::net::IpAddr;

use crate::connector_connection::ConnectorConnection;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ConnectorInstance {
    pub ip: IpAddr,
    pub connector_id: String,
    /// Return the current idle connections
    pub connections: Vec<ConnectorConnection>, // TODO
    pub dark_mode: bool,
}
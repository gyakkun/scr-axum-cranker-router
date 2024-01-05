use crate::connector_connection::ConnectorConnection;

#[derive(Clone,Debug, Hash, Eq, PartialEq)]
pub struct ConnectorInstance {
    ip: String,
    connector_instance_id: String,
    /// Return the current idle connections
    connections: Vec<ConnectorConnection>, // TOD,
    dark_mode: bool,
}
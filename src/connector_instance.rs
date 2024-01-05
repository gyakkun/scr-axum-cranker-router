use crate::connector_connection::ConnectorConnection;

pub struct ConnectorInstance {
    ip: String,
    connector_instance_id: String,
    /// Return the current idle connections
    connections: Vec<ConnectorConnection>, // TOD,
    dark_mode: bool,
}
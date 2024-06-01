use std::net::IpAddr;

use serde::Serialize;

use crate::connector_connection::ConnectorConnection;


/// Information about a connector instance that is connected to this router.
/// Note that one instance may have multiple connections.
/// Access by iterating the services returned by `CrankerRouter.collect_info()`
#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorInstance {
    /// How many connections are there
    pub connection_count: usize,
    /// Dark mode status
    pub dark_mode: bool,
    /// The unique ID of the connector.
    pub connector_id: String,
    /// The current idle connections that this
    /// connector has registered to the router.
    pub connections: Vec<ConnectorConnection>,
    /// The remote IP Address of the connector
    /// instance
    pub ip: IpAddr,
}
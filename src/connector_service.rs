use serde::Serialize;

use crate::connector_instance::ConnectorInstance;

/// Information about a service that is connected to this router.
/// A "service" is 1 or more connector instances that register the same route.
/// Get a copy of this data by calling `CrankerRouter.collect_info()`
#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorService {
    /// The path prefix of the service.
    #[serde(rename(serialize = "name"))]
    pub route: String,
    /// The component name that the connector registered
    pub component_name: String,
    /// The connectors that serve this route.
    pub connectors: Vec<ConnectorInstance>,
    /// If this connector serves from the root of the URL path,
    /// iff. "*" equals the registered route
    pub is_catch_all: bool,
}
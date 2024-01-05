use serde::Serialize;

use crate::connector_instance::ConnectorInstance;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorService {
    pub route: String,
    pub component_name: String,
    pub connectors: Vec<ConnectorInstance>,
    pub is_catch_all: bool,
}
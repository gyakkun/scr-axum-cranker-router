use crate::connector_instance::ConnectorInstance;

#[derive(Clone,Debug, Hash, Eq, PartialEq)]
pub struct ConnectorService {
    pub route: String,
    pub component_name: String,
    pub connectors:  Vec<ConnectorInstance>,
    pub is_catch_all: bool,
}
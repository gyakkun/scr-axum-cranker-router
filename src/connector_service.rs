use crate::connector_instance::ConnectorInstance;

pub struct ConnectorService {
    route: String,
    component_name: String,
    connectors:  Vec<ConnectorInstance>,
    is_catch_all: bool,
}
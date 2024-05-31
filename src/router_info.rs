use std::collections::{HashMap, HashSet};
use std::sync::Weak;

use serde::Serialize;

use crate::connector_connection::ConnectorConnection;
use crate::connector_instance::ConnectorInstance;
use crate::connector_service::ConnectorService;
use crate::dark_host::DarkHost;
use crate::router_socket::RouterSocket;
use crate::websocket_farm::WaitingSocketTask;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterInfo {
    pub services: Vec<ConnectorService>,
    pub dark_hosts: HashSet<DarkHost>,
    pub waiting_tasks: HashMap<String, Vec<WaitingSocketTask>>,
}

pub fn get_connector_service_list(
    sockets: HashMap<String, Vec<Weak<dyn RouterSocket>>>,
    dark_hosts: HashSet<DarkHost>,
) -> Vec<ConnectorService> {
    let mut connector_services = Vec::new();
    let uniq_routes = sockets.keys().map(|i| i.clone()).collect::<HashSet<String>>();
    uniq_routes.iter()
        .for_each(|route| {
            let mut instance_map: HashMap<String, ConnectorInstance> = HashMap::new();
            let mut component_name = None;

            if let Some(weak_router_socket_list) = sockets.get(route) {
                for ws in weak_router_socket_list {
                    if let Some(router_socket) = ws.upgrade() {
                        component_name = Some(router_socket.component_name());
                        let connector_id = router_socket.connector_id();
                        let connector_instance =
                            match instance_map.get_mut(&connector_id) {
                                None => {
                                    let _res = ConnectorInstance {
                                        ip: router_socket.service_address().ip(),
                                        connector_id: connector_id.clone(),
                                        connection_count: 0, // update it once connection pushed
                                        connections: Vec::new(),
                                        dark_mode: router_socket.is_dark_mode_on(&dark_hosts),
                                    };
                                    instance_map.insert(connector_id.clone(), _res);
                                    instance_map.get_mut(&connector_id).unwrap()
                                }
                                Some(instance) => {
                                    instance
                                }
                            };

                        connector_instance.connections.push(ConnectorConnection {
                            domain: "*".to_string(),
                            port: router_socket.service_address().port() as i32,
                            router_socket_id: router_socket.router_socket_id(),
                            protocol: router_socket.cranker_version().to_string(),
                            // to skip serialize , need to make this field negative
                            // this field is useful in v3
                            inflight: router_socket.inflight_count(),
                        });
                        connector_instance.connection_count += 1;
                    }
                }
            }

            // TODO: v3 support
            // for String domain in domain to socket v3 : key set

            connector_services.push(ConnectorService {
                route: route.clone(),
                component_name: component_name.unwrap_or("[UNKNOWN]".to_string()),
                connectors: instance_map.iter().map(|(_k, v)| v.clone()).collect(),
                is_catch_all: route == "*",
            })
        });

    connector_services
}

use std::collections::{HashMap, HashSet};
use std::sync::Weak;

use crate::connector_connection::ConnectorConnection;
use crate::connector_instance::ConnectorInstance;
use crate::connector_service::ConnectorService;
use crate::dark_host::DarkHost;
use crate::router_socket::RouterSocket;
use crate::websocket_farm::WaitingSocketTask;

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
            let mut instances = Vec::new();
            let mut component_name = None;

            if let Some(socket_list_weak) = sockets.get(route) {
                for ws in socket_list_weak {
                    if let Some(router_socket) = ws.upgrade() {
                        component_name = Some(router_socket.component_name());
                        let connector_id = router_socket.connector_id();
                        let mut instance =
                            match instance_map.get(&connector_id) {
                                None => {
                                    let instance = ConnectorInstance {
                                        ip: router_socket.service_address().ip(),
                                        connector_id: connector_id.clone(),
                                        connections: Vec::new(),
                                        dark_mode: router_socket.is_dark_mode_on(&dark_hosts),
                                    };
                                    instance_map.insert(connector_id, instance.clone());
                                    instances.push(instance.clone());
                                    instance
                                }
                                Some(instance) => {
                                    instance.clone()
                                }
                            };

                        instance.connections.push(ConnectorConnection {
                            domain: "*".to_string(),
                            port: router_socket.service_address().port() as i32,
                            router_socket_id: router_socket.router_socket_id(),
                            protocol: router_socket.cranker_version().to_string(),
                            inflight: 0,
                        })
                    }
                }
            }

            // TODO: v3 support
            // for String domain in domain to socket v3 : key set

            // TODO: Given one v3 router socket can serve multiple connector_connection s,
            // should change some methods in RouterSocket trait to return a vec of connector info

            connector_services.push(ConnectorService {
                route: route.clone(),
                component_name: component_name.unwrap_or("[UNKNOWN]".to_string()),
                connectors: instances,
                is_catch_all: route == "*",
            })
        });

    connector_services
}

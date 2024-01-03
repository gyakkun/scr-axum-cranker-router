use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::atomic::Ordering::{Acquire, SeqCst};
use std::time::Duration;

use async_channel::{Receiver, Sender, unbounded};
use axum::async_trait;
use dashmap::{DashMap, DashSet};
use log::{debug, error, trace, warn};

use crate::exceptions::CrankerRouterException;
use crate::route_resolver::RouteResolver;
use crate::router_socket::RouterSocket;
use crate::time_utils;

const MU_ID: &str = "muid";

#[async_trait]
pub trait WebSocketFarmInterface: Sync + Send {
    async fn get_router_socket_by_target_path(self: Arc<Self>, target_path: String) -> Result<Arc<dyn RouterSocket>, CrankerRouterException>;

    fn clean_routes_in_background(self: Arc<Self>, routes_keep_time_millis: i64);

    fn idle_count(&self) -> i32 { -1 }

    fn remove_websocket_in_background(
        self: Arc<Self>, route: String,
        router_socket_id: String,
        is_removed: Arc<AtomicBool>
    );

    fn add_socket_in_background(self: Arc<Self>, router_socket: Arc<dyn RouterSocket>);

    fn de_register_socket_in_background(self: Arc<Self>, route: String, remote_addr: SocketAddr, connector_instance_id: String);

    // TODO: For health info map
    fn get_sockets(self: Arc<Self>) {}
    fn get_waiting_tasks(self: Arc<Self>) {}

    // TODO: DarkHost feature
    fn enable_dark_mode(self: Arc<Self>) {}
    fn disable_dark_mode(self: Arc<Self>) {}
    fn get_dark_hosts(self: Arc<Self>) {}
}


pub struct WebSocketFarm {
    pub route_resolver: Arc<dyn RouteResolver>,
    pub route_to_socket_chan: DashMap<String, (
        Sender<Weak<dyn RouterSocket>>,
        Receiver<Weak<dyn RouterSocket>>
    )>,
    pub route_to_router_socket_id_to_arc_router_socket_map: DashMap<String, DashMap<String, Arc<dyn RouterSocket>>>,
    pub route_last_removal_times: DashMap<String, i64>,
    pub idle_count: AtomicI32,
    pub waiting_task_count: AtomicI32,
    pub dark_hosts: DashSet<String>,
    pub has_catch_all: AtomicBool,
    pub max_wait_in_millis: i64,
}

// unsafe impl<ANY> Send for WebSocketFarm<ANY>{}
// unsafe impl<ANY> Sync for WebSocketFarm<ANY>{}

impl WebSocketFarm {
    pub fn new(
        route_resolver: Arc<dyn RouteResolver>,
        max_wait_in_millis: i64,
    ) -> Arc<Self> {
        Arc::new(Self {
            route_resolver,
            // Weak<RS> passing the chan
            route_to_socket_chan: DashMap::new(),
            // The only place we store a real STRONG Arc
            route_to_router_socket_id_to_arc_router_socket_map: DashMap::new(),
            route_last_removal_times: DashMap::new(),
            idle_count: AtomicI32::new(0),
            waiting_task_count: AtomicI32::new(0),
            dark_hosts: DashSet::new(),
            has_catch_all: AtomicBool::new(false),
            max_wait_in_millis,
        })
    }
}

#[async_trait]
impl WebSocketFarmInterface for WebSocketFarm {
    async fn get_router_socket_by_target_path(self: Arc<Self>, target_path: String) -> Result<Arc<dyn RouterSocket>, CrankerRouterException> {
        // we have both maps storing <routes, xxx >, here we use route_to_chan map since it removes route earlier
        // a little bit ( check the trace!("82") in retain )
        let current_routes = self.route_to_socket_chan.iter().map(|e| e.key().clone()).collect::<DashSet<String>>();
        let resolved_route = self.route_resolver.resolve(&current_routes, &target_path);
        let opt_chan = self.route_to_socket_chan.get(&resolved_route);
        if opt_chan.is_none() {
            return Err(CrankerRouterException::new(format!(
                "no socket registered for this path yet (404): {}", target_path
            ))); // 404 immediately
        }
        let router_socket_receiver = opt_chan.unwrap().clone().1;
        let timeout = tokio::time::timeout(Duration::from_millis(self.max_wait_in_millis as u64), async {
            while let Ok(rs) = router_socket_receiver.recv().await {
                if let Some(arc_rs) = rs.upgrade() {
                    // skip socket that should be removed
                    if arc_rs.is_removed() {
                        // panic here?
                        error!("unexpected removed router socket received from receiver! router_socket_id={}", arc_rs.router_socket_id());
                        continue;
                    }
                    return Ok(arc_rs); // @.await
                } else {
                    continue;
                }
            }
            Err(())
        }).await;
        match timeout {
            Ok(Ok(arc_rs)) => {
                Ok(arc_rs)
            }
            _ => {
                Err(CrankerRouterException::new(format!(
                    "No cranker connector available in {}ms", self.max_wait_in_millis
                )))
            }
        }
    }

    fn clean_routes_in_background(self: Arc<Self>, routes_keep_time_millis: i64) {
        trace!("80: clean_routes_in_background ticked");
        let cut_off_time = time_utils::current_time_millis() - routes_keep_time_millis;
        // Remove may deadlock so put it in thread
        tokio::spawn(async move {
            // trace!("80.5 before clean routes: {:?}", self.router_socket_id_to_arc_router_socket_map);
            let _ = self.route_to_router_socket_id_to_arc_router_socket_map
                .retain(|k, v| {
                    trace!("81");
                    let should_remove = v.is_empty()
                        && self.route_last_removal_times.contains_key(k)
                        && *self.route_last_removal_times.get(k).unwrap().value() < cut_off_time;
                    if should_remove {
                        trace!("82");
                        warn!(
                            "removing registration info for {}, consequence requests to {} will be responded with 404.",
                            k,k
                        );
                        self.route_to_socket_chan.remove(k);
                    }
                    !should_remove
                });
            // trace!("82.5 after clean routes: {:?}", self.router_socket_id_to_arc_router_socket_map);
            trace!("83");
        });
    }

    fn idle_count(&self) -> i32 {
        self.idle_count.load(Acquire)
    }

    fn remove_websocket_in_background(
        self: Arc<Self>, route: String,
        router_socket_id: String,
        is_removed: Arc<AtomicBool>
    ) {
        trace!("90 remove_websocket_in_background");
        tokio::spawn(async move {
            let route_clone = route.clone();
            let mut success = true;
            self.route_last_removal_times.insert(route_clone, time_utils::current_time_millis());
            trace!("91");
            success = self.route_to_router_socket_id_to_arc_router_socket_map.get(&route)
                .map(|some| some.remove(&router_socket_id))
                .is_some() && success;
            trace!("92 success {}", success);
            if success {
                trace!("93");
                is_removed.store(true, SeqCst);
            }
            trace!("94");
        });
    }

    fn add_socket_in_background(self: Arc<Self>, router_socket: Arc<dyn RouterSocket>) {
        tokio::spawn(async move {
            let route = router_socket.route();
            let router_socket_id = router_socket.router_socket_id();
            let weak = Arc::downgrade(&router_socket);
            let route_clone = route.clone();
            self.route_to_router_socket_id_to_arc_router_socket_map
                .entry(route_clone)
                .or_insert_with(DashMap::new)
                .value()
                .insert(router_socket_id, router_socket);
            let _ = self.route_to_socket_chan
                .entry(route)
                .or_insert_with(unbounded)
                .value()
                .0
                .send(weak).await;
        });
    }

    fn de_register_socket_in_background(self: Arc<Self>, route: String, remote_addr: SocketAddr, connector_instance_id: String) {
        warn!(
            "Going to deregister targetName={} and the targetAddr={} and the connectorInstanceID={}",
            route, remote_addr, connector_instance_id
        );
        tokio::spawn(async move {
            self.route_to_router_socket_id_to_arc_router_socket_map
                .get(&route)
                .map(|some| {
                    // ( router socket id , router socket )
                    some.value()
                        .retain(|k, v| {
                            let should_remove = v.connector_instance_id().eq(&connector_instance_id);
                            if should_remove {
                                warn!("De-registering router_socket_id={}", v.router_socket_id());
                            }
                            !should_remove
                        });
                })
        });
    }
}

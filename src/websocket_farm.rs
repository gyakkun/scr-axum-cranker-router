use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_channel::{unbounded, Receiver, Sender};
use async_trait::async_trait;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, Method, StatusCode};
use dashmap::{DashMap, DashSet};
use log::{error, info, trace, warn};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::dark_host::DarkHost;
use crate::exceptions::{CrankerRouterException, CrexKind};
use crate::proxy_info::ProxyInfo;
use crate::proxy_listener::ProxyListener;
use crate::route_identify::RouteIdentify;
use crate::route_resolver::RouteResolver;
use crate::router_socket::RouterSocket;
use crate::router_socket_filter::RouterSocketFilter;
use crate::{time_utils, CrankerRouterConfig, LOCAL_IP, CRANKER_V_3_0};

/// Here we mimic the Java WebSocketFarm but allow adding
/// both V1 and V3 router socket to the same farm to reduce
/// complexity. The `RouterSocket` trait should define all
/// common behaviours needed for the farm to pick an eligible
/// router socket for a client side request
#[allow(unused_variables)]
#[async_trait]
pub trait WebSocketFarmInterface: Sync + Send {
    async fn get_router_socket_by_target_path_and_apply_filter(
        self: Arc<Self>,
        target_path: String,
        method: Method,
        original_uri: OriginalUri,
        headers: HeaderMap,
        addr: SocketAddr,
        cranker_router_config: CrankerRouterConfig,
        router_socket_filter: Arc<dyn RouterSocketFilter>
    ) -> Result<Arc<dyn RouterSocket>, CrankerRouterException>;

    fn clean_routes_in_background(self: Arc<Self>, routes_keep_time_millis: i64);

    fn idle_count(&self) -> i32 { -1 }

    fn remove_router_socket_in_background(
        self: Arc<Self>, route: String,
        router_socket_id: String,
        is_removed: Arc<AtomicBool>,
    );

    fn add_router_socket_in_background(self: Arc<Self>, router_socket: Arc<dyn RouterSocket>);

    fn de_register_router_socket_in_background(self: Arc<Self>, route: String, remote_addr: SocketAddr, connector_instance_id: String);

    fn get_sockets(&self) -> HashMap<String, Vec<Weak<dyn RouterSocket>>> {
        HashMap::new()
    }
    fn get_waiting_tasks(&self) -> HashMap<String, Vec<WaitingSocketTask>> {
        HashMap::new()
    }

    fn enable_dark_mode(&self, dark_host: DarkHost) {}
    fn disable_dark_mode(&self, dark_host: DarkHost) {}
    fn get_dark_hosts(&self) -> HashSet<DarkHost> {
        HashSet::new()
    }
}

/// Contains the target path waiting for matched router socket
/// available
#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WaitingSocketTask {
    pub target: String,
}

/// The implementation of WebSocketFarmInterface
pub struct WebSocketFarm {
    route_resolver: Arc<dyn RouteResolver>,
    /// Use an unbounded channel of `Weak<dyn RouterSocket>`,
    /// to pick connector/connection/cranker socket in FIFO
    /// style, implying round-robin like behaviour.
    route_to_socket_chan: DashMap<String, (
        Sender<Weak<dyn RouterSocket>>,
        Receiver<Weak<dyn RouterSocket>>
    )>,
    /// This is the only place we store our Strong Arc
    /// of dyn RouterSocket trait object
    route_to_router_socket_id_to_arc_router_socket_map: DashMap<String, DashMap<String, Arc<dyn RouterSocket>>>,
    route_last_removal_times: DashMap<String, i64>,
    idle_count: AtomicI32,
    waiting_tasks: DashMap<String, DashSet<WaitingSocketTask>>,
    waiting_task_count: AtomicI32,
    dark_hosts: DashSet<DarkHost>,
    max_wait_in_millis: i64,
    proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    new_socket_notifier: broadcast::Sender<String>,
}

// unsafe impl<ANY> Send for WebSocketFarm<ANY>{}
// unsafe impl<ANY> Sync for WebSocketFarm<ANY>{}

impl WebSocketFarm {
    pub fn new(
        route_resolver: Arc<dyn RouteResolver>,
        max_wait_in_millis: i64,
        proxy_listeners: Vec<Arc<dyn ProxyListener>>,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(128);
        Arc::new(Self {
            route_resolver,
            // Weak<RS> passing the chan
            route_to_socket_chan: DashMap::new(),
            // The only place we store a real STRONG Arc
            route_to_router_socket_id_to_arc_router_socket_map: DashMap::new(),
            route_last_removal_times: DashMap::new(),
            idle_count: AtomicI32::new(0),
            waiting_tasks: DashMap::new(),
            waiting_task_count: AtomicI32::new(0),
            dark_hosts: DashSet::new(),
            max_wait_in_millis,
            proxy_listeners,
            new_socket_notifier: tx,
        })
    }

    fn should_remove_from_web_socket_farm_if_is_removed(self: &Arc<Self>, arc_rs: &Arc<dyn RouterSocket>) -> bool {
        if arc_rs.is_removed() {
            // panic here?
            error!("unexpected removed router socket received from receiver! router_socket_id={}", arc_rs.router_socket_id());
            self.clone().remove_router_socket_in_background(arc_rs.route(), arc_rs.router_socket_id(), arc_rs.get_is_removed_arc_atomic_bool());
            return true;
        }
        false
    }

    pub(crate) fn terminate_all(self: Arc<Self>, force: bool) {
        if force {
            self.route_to_router_socket_id_to_arc_router_socket_map.iter().for_each(|route_dashmap_rsid_rs| {
                route_dashmap_rsid_rs.iter().for_each(|rsid_rs| {
                    let rs = rsid_rs.value().clone();
                    futures::executor::block_on(async move {
                        let _ = rs.terminate_all_conn(Some(
                            CrankerRouterException::new("process exiting".to_string()).with_status_code(500)
                        )).await;
                    });
                })
            });
        }
        self.route_to_router_socket_id_to_arc_router_socket_map.retain(|_, _| {
            false
        })
    }
}

#[async_trait]
impl WebSocketFarmInterface for WebSocketFarm {
    async fn get_router_socket_by_target_path_and_apply_filter(
        self: Arc<Self>,
        target_path: String,
        method: Method,
        original_uri: OriginalUri,
        headers: HeaderMap,
        addr: SocketAddr,
        cranker_router_config: CrankerRouterConfig,
        router_socket_filter: Arc<dyn RouterSocketFilter>
    ) -> Result<Arc<dyn RouterSocket>, CrankerRouterException> {
        let host_opt = original_uri.host()
            .map(|h| h.to_string())
            .or_else(|| {
                headers.get("host")
                    .and_then(|h| h.to_str().ok())
                    .map(|h| {
                        h.split(':').next().unwrap_or(h).to_string()
                    })
            });

        // Check if there is any socket registered on the precise domain `host`
        let mut has_precise_domain_sockets = false;
        if let Some(ref host) = host_opt {
            for entry in self.route_to_router_socket_id_to_arc_router_socket_map.iter() {
                for sub_entry in entry.value().iter() {
                    let rs = sub_entry.value();
                    if !self.should_remove_from_web_socket_farm_if_is_removed(rs) {
                        if rs.domain() == *host {
                            has_precise_domain_sockets = true;
                            break;
                        }
                    }
                }
                if has_precise_domain_sockets {
                    break;
                }
            }
        }

        // Collect routes. If there are precise domain sockets, only collect routes that have sockets on the precise domain!
        let current_routes = self.route_to_socket_chan.iter()
            .filter(|e| {
                let route = e.key();
                if has_precise_domain_sockets {
                    if let Some(ref host) = host_opt {
                        if let Some(sub_map) = self.route_to_router_socket_id_to_arc_router_socket_map.get(route) {
                            for sub_entry in sub_map.value().iter() {
                                let rs = sub_entry.value();
                                if !self.should_remove_from_web_socket_farm_if_is_removed(rs) {
                                    if rs.domain() == *host {
                                        return true;
                                    }
                                }
                            }
                        }
                        return false;
                    }
                }
                true
            })
            .map(|e| e.key().clone())
            .collect::<DashSet<String>>();

        // TODO: Catch all handling
        let resolved_route = self.route_resolver.resolve(&current_routes, &target_path);
        let opt_chan = self.route_to_socket_chan.get(&resolved_route);
        if opt_chan.is_none() {
            return Err(
                CrankerRouterException::new(format!(
                    "no router socket registered for this path yet: {}", target_path
                )).with_status_code(
                    StatusCode::NOT_FOUND.as_u16()
                ).with_err_kind(CrexKind::NoRouterSocketAvailable_0002)
            );
        }
        let chan_pair = opt_chan.unwrap().clone();
        let router_socket_sender = chan_pair.0;
        let router_socket_receiver = chan_pair.1;
        let router_socket_receiver_c = router_socket_receiver.clone();
        let router_socket_sender_c = router_socket_sender.clone();
        let cranker_router_config_c = cranker_router_config.clone();
        let router_socket_filter_c = router_socket_filter.clone();
        let start_ts = time_utils::current_time_millis();
        let mut new_socket_rx = self.new_socket_notifier.subscribe();
        self.waiting_task_count.fetch_add(1, AcqRel);
        let timeout = tokio::time::timeout(
            Duration::from_millis(self.max_wait_in_millis as u64),
            async {
                'main: loop {
                    let host_opt = original_uri.host()
                        .map(|h| h.to_string())
                        .or_else(|| {
                            headers.get("host")
                                .and_then(|h| h.to_str().ok())
                                .map(|h| {
                                    h.split(':').next().unwrap_or(h).to_string()
                                })
                        });
                    let mut has_precise_match = false;
                    if let Some(ref host) = host_opt {
                        if let Some(sub_map) = self.route_to_router_socket_id_to_arc_router_socket_map.get(&resolved_route) {
                            for entry in sub_map.value().iter() {
                                let rs = entry.value();
                                if !self.should_remove_from_web_socket_farm_if_is_removed(rs) {
                                    if rs.domain() == *host {
                                        has_precise_match = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    let mut visited = HashSet::new();
                    while let Ok(rs) = router_socket_receiver.recv().await // @filter loop await
                    {
                        if let Some(arc_rs) = rs.upgrade() {
                            let rsid = arc_rs.router_socket_id();
                            let is_visited = !visited.insert(rsid.clone());
                            if is_visited {
                                // put it back and break from this inner loop
                                let _ = router_socket_sender.send(Arc::downgrade(&arc_rs)).await;
                                break;
                            }
                            // skip socket that should be removed
                            if self.should_remove_from_web_socket_farm_if_is_removed(&arc_rs) {
                                visited.remove(&rsid);
                                continue;
                            }
                            // skip socket whose connector is in dark mode
                            {
                                let dark_hosts: HashSet<DarkHost> = self.dark_hosts.iter().map(|i| i.clone()).collect();
                                if arc_rs.is_dark_mode_on(&dark_hosts) {
                                    let _ = router_socket_sender.send(Arc::downgrade(&arc_rs)).await;
                                    continue;
                                }
                            }
                            if has_precise_match && arc_rs.domain() == "*" {
                                let _ = router_socket_sender.send(Arc::downgrade(&arc_rs)).await;
                                continue;
                            }
                            if router_socket_filter.should_use(
                                target_path.clone(),
                                method.clone(),
                                original_uri.clone(),
                                headers.clone(),
                                addr.clone(),
                                cranker_router_config.clone(),
                                arc_rs.clone(),
                            ) {
                                if arc_rs.cranker_version() == CRANKER_V_3_0 {
                                    let _ = router_socket_sender.send(Arc::downgrade(&arc_rs)).await;
                                }
                                return Ok(arc_rs); // @filter loop await
                            }
                            // put it back if not being chosen
                            let _ = router_socket_sender.send(Arc::downgrade(&arc_rs)).await;
                        }
                    }
                    // If we are here, it means we have iterated all sockets in the chan and none of them are suitable.
                    // We will wait for a new socket to be added.
                    loop {
                        match new_socket_rx.recv().await {
                            Ok(notified_route) => {
                                if notified_route == resolved_route || notified_route == "*" {
                                    // A new socket for our route has been added or dark mode changed. Continue the main loop to try again.
                                    continue 'main;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // Lagged, but we can continue listening.
                                continue 'main;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // The notifier is closed. No new sockets will ever be announced.
                                // We can't do anything else. The outer timeout will eventually fire.
                                // A sleep prevents a hot loop.
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            },
        )
        .await;

        match timeout {
            Ok(Ok(arc_rs)) => {
                self.waiting_task_count.fetch_add(-1, AcqRel);
                Ok(arc_rs)
            }
            Ok(Err(e)) => {
                self.waiting_task_count.fetch_add(-1, AcqRel);
                Err(e)
            }
            Err(_) => {
                self.waiting_task_count.fetch_add(-1, AcqRel);
                let dark_hosts: HashSet<DarkHost> = self.dark_hosts.iter().map(|i| i.clone()).collect();
                if router_socket_filter_c.should_fallback_to_first_path_matched() {
                    let mut skipped = Vec::new();
                    while let Ok(rs) = router_socket_receiver_c.try_recv() {
                        if let Some(arc_rs) = rs.upgrade() {
                            if self.should_remove_from_web_socket_farm_if_is_removed(&arc_rs) {
                                continue;
                            }
                            if arc_rs.is_dark_mode_on(&dark_hosts) {
                                skipped.push(Arc::downgrade(&arc_rs));
                                continue;
                            }
                            for s in skipped {
                                let _ = router_socket_sender_c.send(s).await;
                            }
                            return Ok(arc_rs);
                        }
                    }
                    for s in skipped {
                        let _ = router_socket_sender_c.send(s).await;
                    }
                }

                if cranker_router_config_c.allow_catch_all
                    && router_socket_filter_c.should_fallback_to_catch_all()
                {
                    if let Some(opt_catch_all_chan) = self.route_to_socket_chan.get("*") {
                        let (ca_tx, ca_rx) = opt_catch_all_chan.value().clone();
                        let mut skipped = Vec::new();
                        while let Ok(rs) = ca_rx.try_recv() {
                            if let Some(arc_rs) = rs.upgrade() {
                                if self.should_remove_from_web_socket_farm_if_is_removed(&arc_rs)
                                {
                                    continue;
                                }
                                if arc_rs.is_dark_mode_on(&dark_hosts) {
                                    skipped.push(Arc::downgrade(&arc_rs));
                                    continue;
                                }
                                for s in skipped {
                                    let _ = ca_tx.send(s).await;
                                }
                                return Ok(arc_rs);
                            }
                        }
                        for s in skipped {
                            let _ = ca_tx.send(s).await;
                        }
                    }
                }

                let elapsed = time_utils::current_time_millis() - start_ts;
                let epif = ErrorProxyInfo::new(resolved_route.clone(), elapsed);
                for i in self.proxy_listeners.iter() {
                    let _ = i.on_failure_to_acquire_proxy_socket(&epif);
                }
                Err(
                    CrankerRouterException::new(format!(
                        "No cranker connector available within {} ms. target path = {} ",
                        self.max_wait_in_millis, target_path
                    )).with_status_code(
                        StatusCode::SERVICE_UNAVAILABLE.as_u16()
                    ).with_err_kind(CrexKind::NoRouterSocketAvailable_0002)
                )
            }
        }
    }

    fn clean_routes_in_background(self: Arc<Self>, routes_keep_time_millis: i64) {
        trace!("80: clean_routes_in_background ticked");
        let another_self = self.clone();
        let cut_off_time = time_utils::current_time_millis() - routes_keep_time_millis;
        // Remove may deadlock so put it in thread
        tokio::spawn(async move {
            // trace!("80.5 before clean routes: {:?}", self.router_socket_id_to_arc_router_socket_map);
            let _ = another_self.route_to_router_socket_id_to_arc_router_socket_map
                .retain(|k, v| {
                    trace!("81");
                    let should_remove = v.is_empty()
                        && another_self.route_last_removal_times.contains_key(k)
                        && *another_self.route_last_removal_times.get(k).unwrap().value() < cut_off_time;
                    if should_remove {
                        trace!("82");
                        warn!(
                            "removing registration info for {}, consequence requests to {} will be responded with 404.",
                            k,k
                        );
                        another_self.route_to_socket_chan.remove(k);
                    }
                    !should_remove
                });
            // trace!("82.5 after clean routes: {:?}", another_self.router_socket_id_to_arc_router_socket_map);
            trace!("83");
        });
    }

    fn idle_count(&self) -> i32 {
        self.idle_count.load(Acquire)
    }

    fn remove_router_socket_in_background(
        self: Arc<Self>, route: String,
        router_socket_id: String,
        is_removed: Arc<AtomicBool>,
    ) {
        warn!("Removing websocket in background. router_socket_id={}", router_socket_id);
        trace!("90 remove_websocket_in_background");
        let another_self = self.clone();
        tokio::spawn(async move {
            let route_clone = route.clone();
            another_self.route_last_removal_times.insert(route_clone, time_utils::current_time_millis());
            trace!("91");
            let success = another_self.route_to_router_socket_id_to_arc_router_socket_map.get(&route)
                .and_then(|some| some.remove(&router_socket_id))
                .is_some();
            trace!("92 success {}", success);
            if success {
                trace!("93");
                self.idle_count.fetch_add(-1, AcqRel);
                is_removed.store(true, Release);
            }
            trace!("94");
        });
    }

    fn add_router_socket_in_background(self: Arc<Self>, router_socket: Arc<dyn RouterSocket>) {
        let another_self = self.clone();
        tokio::spawn(async move {
            let route = router_socket.route();
            let router_socket_id = router_socket.router_socket_id();
            let weak = Arc::downgrade(&router_socket);
            let route_clone = route.clone();
            another_self.route_to_router_socket_id_to_arc_router_socket_map
                .entry(route_clone)
                .or_insert_with(DashMap::new)
                .value()
                .insert(router_socket_id, router_socket);
            let res = another_self
                .route_to_socket_chan
                .entry(route.clone())
                .or_insert_with(unbounded)
                .value()
                .0
                .send(weak).await;
            if res.is_ok() {
                another_self.idle_count.fetch_add(1, AcqRel);
                if let Err(e) = another_self.new_socket_notifier.send(route) {
                    warn!("Could not send notification for new socket on route: {}", e);
                }
            }
        });
    }

    fn de_register_router_socket_in_background(self: Arc<Self>, route: String, remote_addr: SocketAddr, connector_id: String) {
        let another_self = self.clone();
        warn!(
            "Going to deregister route={} and the target addr={} and the Connector Id={}",
            route, remote_addr, connector_id
        );
        tokio::spawn(async move {
            let mut sockets_to_close = Vec::new();
            if let Some(some) = another_self.route_to_router_socket_id_to_arc_router_socket_map.get(&route) {
                for item in some.value().iter() {
                    let v = item.value();
                    if v.connector_id().eq(&connector_id) {
                        sockets_to_close.push(v.clone());
                    }
                }
            }
            // FIXME: Java implementation doesn't wait for `maxWaitInMillis` in V3.
            //  I think it's good to keep this behaviour in V3. The bellow commented
            //  code is to align the behaviour. Please uncommented it if necessary.
            //  Check `canHandle()` in WebSocketFarmV3.java and the test in
            //  CrankerRouterHandlerTest.java and CrankerRouterRetryTest.java
            // let mut is_v3 = false;
            // let mut is_empty = false;
            if let Some(some) = another_self.route_to_router_socket_id_to_arc_router_socket_map.get(&route) {
                let route_clone = route.clone();
                another_self.route_last_removal_times.insert(route_clone, time_utils::current_time_millis());
                // for entry in some.value().iter() {
                //     let rs = entry.value();
                //     if rs.connector_id().eq(&connector_id) && rs.cranker_version() == CRANKER_V_3_0 {
                //         is_v3 = true;
                //     }
                // }
                some.value().retain(|_k, v| {
                    !v.connector_id().eq(&connector_id)
                });
                // if some.value().is_empty() {
                //     is_empty = true;
                // }
            }
            // if is_empty && is_v3 {
            //     another_self.route_to_socket_chan.remove(&route);
            //     another_self.route_to_router_socket_id_to_arc_router_socket_map.remove(&route);
            // }
            for socket in sockets_to_close {
                another_self.idle_count.fetch_add(-1, AcqRel);
                socket.get_is_removed_arc_atomic_bool().store(true, Release);
                warn!("De-registering and draining/closing router_socket_id={}", socket.router_socket_id());
                tokio::spawn(async move {
                    let mut is_active = socket.is_active();
                    if is_active {
                        let start_time = std::time::Instant::now();
                        while is_active && start_time.elapsed() < std::time::Duration::from_secs(15) {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            is_active = socket.is_active();
                        }
                    }
                    warn!("Closing socket router_socket_id={}", socket.router_socket_id());
                    let _ = socket.clone().send_ws_msg_to_uwss(axum::extract::ws::Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1000,
                        reason: "De-registered".into(),
                    }))).await;
                    let _ = socket.terminate_all_conn(None).await;
                });
            }
        });
    }

    fn get_sockets(&self) -> HashMap<String, Vec<Weak<dyn RouterSocket>>> {
        let mut res = HashMap::new();
        self.route_to_router_socket_id_to_arc_router_socket_map.iter()
            .for_each(|i| {
                res.insert(i.key().clone(), Vec::new());
                i.value().iter().for_each(|j| { // ( router_socket_id, arc rs )
                    if let Some(k) = res.get_mut(i.key()) {
                        k.push(Arc::downgrade(j.value()));
                    }
                })
            });
        res
    }
    fn get_waiting_tasks(&self) -> HashMap<String, Vec<WaitingSocketTask>> {
        let mut res = HashMap::new();
        self.waiting_tasks.iter()
            .for_each(|i| {
                res.insert(i.key().clone(), Vec::new());
                i.value().iter().for_each(|j| {
                    if let Some(k) = res.get_mut(i.key()) {
                        k.push(j.clone());
                    }
                });
            });
        res
    }

    fn enable_dark_mode(&self, dark_host: DarkHost) {
        let dark_host_dbg = format!("{:?}", dark_host);
        let added = self.dark_hosts.insert(dark_host);
        if added {
            info!("Enabled dark mode for {:?}", dark_host_dbg);
        } else {
            info!("Requested dark mode for {:?} but it was already in dark mode, so doing nothing.", dark_host_dbg);
        }
    }

    fn disable_dark_mode(&self, dark_host: DarkHost) {
        let removed = self.dark_hosts.remove(&dark_host).is_some();
        if removed {
            info!("Disabled dark mode for {:?}", dark_host);
            let _ = self.new_socket_notifier.send("*".to_string());
        } else {
            info!("Requested to disable dark mode for {:?} but it was not in dark mode, so doing nothing.", dark_host);
        }
    }

    fn get_dark_hosts(&self) -> HashSet<DarkHost> {
        return self.dark_hosts.iter().map(|i|i.clone()).collect();
    }
}

struct ErrorProxyInfo {
    route: String,
    socket_wait_in_millis: i64,
}

impl ErrorProxyInfo {
    pub fn new(route: String, socket_wait_in_millis: i64) -> Self {
        Self { route, socket_wait_in_millis }
    }
}

impl RouteIdentify for ErrorProxyInfo {
    fn router_socket_id(&self) -> String {
        "N/A".to_string()
    }

    fn route(&self) -> String {
        self.route.clone()
    }

    fn service_address(&self) -> SocketAddr {
        SocketAddr::new(LOCAL_IP.clone(), 0)
    }
}

impl ProxyInfo for ErrorProxyInfo {
    fn is_catch_all(&self) -> bool {
        self.route == "*"
    }

    fn connector_id(&self) -> String {
        "N/A".to_string()
    }

    fn duration_millis(&self) -> i64 {
        0
    }

    fn bytes_received(&self) -> i64 {
        0
    }

    fn bytes_sent(&self) -> i64 {
        0
    }

    fn response_body_frames(&self) -> i64 {
        0
    }

    fn error_if_any(&self) -> Option<CrankerRouterException> {
        Some(CrankerRouterException::new(format!(
            "failed to acquire connector socket in {}ms", self.socket_wait_in_millis
        )))
    }

    fn socket_wait_in_millis(&self) -> i64 {
        self.socket_wait_in_millis
    }
}

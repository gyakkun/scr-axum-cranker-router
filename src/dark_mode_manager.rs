use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use crate::dark_host::DarkHost;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};

pub struct DarkModeManager {
    pub websocket_farm: Arc<WebSocketFarm>,
}

impl DarkModeManager {
    pub fn enable_dark_mode(&self, dark_host: DarkHost) {
        self.websocket_farm.enable_dark_mode(dark_host);
    }

    pub fn disable_dark_mode(&self, dark_host: DarkHost) {
        self.websocket_farm.disable_dark_mode(dark_host);
    }

    pub fn get_dark_hosts(&self) -> HashSet<DarkHost> {
        self.websocket_farm.get_dark_hosts()
    }

    pub fn find_host(&self, address: IpAddr) -> Option<DarkHost> {
        self.get_dark_hosts()
            .into_iter()
            .filter(|i| i.address == address)
            .next()
    }
}
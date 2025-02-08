use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Weak;

use crate::dark_host::DarkHost;
use crate::websocket_farm::{WebSocketFarm, WebSocketFarmInterface};

/// This class allows you to block certain hosts from receiving requests.
///
/// If a host is in "dark mode" then cranker connectors can still register
/// with it, however no requests will be forwarded to those connectors.
///
/// You can acquire an instance of this class by creating a cranker router
/// instance and then calling `CrankerRouter.dark_mode_manager()`
pub struct DarkModeManager {
    pub(crate) websocket_farm: Weak<WebSocketFarm>,
}

impl DarkModeManager {
    /// Specifies that the given target destination should not have any
    /// requests sent to it.
    ///
    /// Does nothing if the host was already in dark mode.
    pub fn enable_dark_mode(&self, dark_host: DarkHost) {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            wsf.enable_dark_mode(dark_host);
        }
    }

    /// Removes the target from the set of blocked hosts.
    ///
    /// Does nothing if the host was not already in dark mode.
    pub fn disable_dark_mode(&self, dark_host: DarkHost) {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            wsf.disable_dark_mode(dark_host);
        }
    }

    /// The current dark hosts.
    pub fn get_dark_hosts(&self) -> HashSet<DarkHost> {
        if let Some(wsf) = self.websocket_farm.upgrade() {
            return wsf.get_dark_hosts();
        }
        panic!("no websocket farm strong reference, already end of life");
    }

    /// Finds the host associated with the given address, if it is in dark mode.
    /// * `address` - The address of the host
    pub fn find_host(
        &self,
        address: IpAddr
    ) -> Option<DarkHost> {
        self.get_dark_hosts()
            .into_iter()
            .filter(|i| i.address == address)
            .next()
    }
}
use std::net::IpAddr;

use serde::Serialize;

/// A host that does not have requests forwarded to it.
///
/// Putting a host in dark mode is useful when needing to take a host out of an environment temporarily, e.g. for patching etc.
#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DarkHost {
    /// The address of the host
    pub address: IpAddr,
    /// The time that dark mode was turned on for this host
    ///
    /// Represent in unix epoch in millisecond
    pub date_enabled: i64,
    /// An optional description of why this host is in dark mode.
    pub reason: String,
}

impl DarkHost {
    /// Returns true if the given address matches this host
    pub fn same_host(&self, another: IpAddr) -> bool {
        self.address == another
    }
}
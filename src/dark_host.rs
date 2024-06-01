use std::net::IpAddr;

use serde::Serialize;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DarkHost {
    pub address: IpAddr,
    /// unix epoch in millisecond
    pub date_enabled: i64,
    pub reason: String,
}

impl DarkHost {
    pub fn same_host(&self, another: IpAddr) -> bool {
        self.address == another
    }
}
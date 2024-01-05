use std::net::IpAddr;
use serde::Serialize;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DarkHost {
    address: IpAddr,
    date_enabled: i64,
    // unix timestamp,
    reason: String,
}

impl DarkHost {
    pub fn same_host(&self, another: IpAddr) -> bool {
        self.address == another
    }
}
use std::net::IpAddr;
use serde::Serialize;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DarkHost {
    pub address: IpAddr,
    pub date_enabled: i64,
    // unix timestamp,
    pub reason: String,
}

impl DarkHost {
    pub fn same_host(&self, another: IpAddr) -> bool {
        self.address == another
    }
}
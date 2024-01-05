use std::net::IpAddr;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
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
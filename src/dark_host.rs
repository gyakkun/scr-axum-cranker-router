use std::net::IpAddr;


#[derive(Clone,Debug, Hash)]
pub struct DarkHost {
    address: IpAddr,
    date_enabled: i64, // unix timestamp,
    reason: String,
}
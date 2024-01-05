use serde::Serialize;
use crate::CRANKER_V_3_0;

#[derive(Serialize, Clone, Debug, Hash, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorConnection {
    #[serde(skip_serializing_if = "_if_ser_domain")]
    pub domain: String,
    pub port: i32,
    pub router_socket_id: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "_if_ser_inflight")]
    pub inflight: i32,
}

// TODO: Make all domain field optional
fn _if_ser_domain(domain: &String)-> bool {
    domain == "*"
}

fn _if_ser_inflight(ifl: &i32) -> bool {
    ifl < &0
}
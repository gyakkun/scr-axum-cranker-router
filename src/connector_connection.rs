use serde::Serialize;

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

fn _if_ser_domain(domain: &String)-> bool {
    domain == "*"
}

fn _if_ser_inflight(ifl: &i32) -> bool {
    ifl < &0
}
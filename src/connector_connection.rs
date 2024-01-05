use serde_json::Value;

pub struct ConnectorConnection {
    socket_id: String,
    port: i32,
    to_map: Value,
}
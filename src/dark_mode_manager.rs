use std::sync::{Arc, Weak};
use crate::websocket_farm::WebSocketFarm;

pub struct DarkModeManager {
    pub websocket_farm: Arc<WebSocketFarm>
}
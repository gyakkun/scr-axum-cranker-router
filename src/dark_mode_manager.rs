use std::sync::Weak;
use crate::websocket_farm::WebSocketFarm;

struct DarkModeManager {
    websocket_farm: Weak<WebSocketFarm>
}
use async_trait::async_trait;
use axum::extract::ws::CloseFrame;
use bytes::Bytes;
use log::debug;

use crate::exceptions::CrankerRouterException;

/// The common behaviours for handling WebSocket messages
#[async_trait]
pub(crate) trait WebSocketListener: Send + Sync {
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException>;
    async fn on_binary(&self, binary_msg: Bytes) -> Result<(), CrankerRouterException>;
    async fn on_ping(&self, ping_msg: Bytes) -> Result<(), CrankerRouterException> {
        // you should pong back here
        let decoded = std::str::from_utf8(ping_msg.as_ref()).unwrap_or("INVALID PING MSG");
        debug!("pinged: {}", decoded);
        Err(CrankerRouterException::new("PLEASE IMPLEMENT YOURSELF".to_string()))
    }
    async fn on_pong(&self, pong_msg: Bytes) -> Result<(), CrankerRouterException> {
        let decoded = std::str::from_utf8(pong_msg.as_ref()).unwrap_or("INVALID PONG MSG");
        debug!("ponged: {}", decoded);
        Ok(())
    }
    async fn on_close(&self, close_msg: Option<CloseFrame>) -> Result<(), CrankerRouterException>;

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException>;

    fn get_idle_read_timeout_ms(&self) -> i64;
    fn get_ping_sent_after_no_write_for_ms(&self) -> i64;
}
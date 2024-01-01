use axum::async_trait;
use axum::extract::ws::CloseFrame;
use log::{debug, error};
use crate::exceptions::CrankerRouterException;

#[async_trait]
pub trait WebSocketListener: Send + Sync {
    // this should be done in on_upgrade, so ignore it

    // fn on_connect(&self, wss_tx: SplitSink<WebSocket, Message>) -> Result<(), CrankerRouterException>;
    async fn on_text(&self, text_msg: String) -> Result<(), CrankerRouterException>;
    async fn on_binary(&self, binary_msg: Vec<u8>) -> Result<(), CrankerRouterException>;
    async fn on_ping(&self, ping_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        // you should pong back here
        let decoded = std::str::from_utf8(ping_msg.as_slice()).unwrap_or("INVALID PING MSG");
        debug!("pinged: {}", decoded);
        Err(CrankerRouterException::new("PLEASE IMPLEMENT YOURSELF".to_string()))
    }
    async fn on_pong(&self, pong_msg: Vec<u8>) -> Result<(), CrankerRouterException> {
        let decoded = std::str::from_utf8(pong_msg.as_slice()).unwrap_or("INVALID PONG MSG");
        debug!("ponged: {}", decoded);
        Ok(())
    }
    async fn on_close(&self, close_msg: Option<CloseFrame<'static>>) -> Result<(), CrankerRouterException>;

    fn on_error(&self, err: CrankerRouterException) -> Result<(), CrankerRouterException> {
        // FIXME: Swallow the error by default
        error!("error: {:?}", err);
        Ok(())
    }
}
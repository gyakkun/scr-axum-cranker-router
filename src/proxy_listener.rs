use std::fmt::Debug;
use axum::http::HeaderMap;
use bytes::Bytes;
use log::error;

use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;

pub trait ProxyListener: Sync + Send {
    fn on_before_proxy_to_target(&self, info: &dyn ProxyInfo, request_headers_to_target: &mut HeaderMap) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_before_responding_to_client(&self, info: &dyn ProxyInfo) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_failure_to_acquire_proxy_socket(&self, info: &dyn ProxyInfo) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_complete(&self, proxy_info: &dyn ProxyInfo) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_after_proxy_to_target_headers_sent(&self, proxy_info: &dyn ProxyInfo, headers: Option<&HeaderMap>) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_after_target_to_proxy_headers_received(&self, proxy_info: &dyn ProxyInfo, status: u16, headers: Option<&HeaderMap>) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_request_body_chunk_sent_to_target(&self, proxy_info: &dyn ProxyInfo, chunk: &Bytes) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_request_body_sent_to_target(&self, proxy_info: &dyn ProxyInfo) -> Result<(), CrankerRouterException> { Ok(()) }
    fn on_response_body_chunk_received_from_target(&self, proxy_info: &dyn ProxyInfo, chunk: &Bytes) -> Result<(), CrankerRouterException> { Ok(()) }
    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
        // `on_response_body_chunk_received_from_target` is expensive, we need you to tell us ahead
        error!("BOOM");
        panic!("Please ensure you implement this method! It's very important to us: do you `really_need_on_response_body_chunk_received_from_target`");
    }
    fn on_response_body_chunk_received(&self, proxy_info: &dyn ProxyInfo) -> Result<(), CrankerRouterException> { Ok(()) }
}

pub(crate) struct DefaultProxyListener;

impl ProxyListener for DefaultProxyListener {
    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
        false
    }
}

use axum::http::HeaderMap;
use bytes::Bytes;
use log::error;

use crate::exceptions::CrankerRouterException;
use crate::proxy_info::ProxyInfo;

/// Hooks to intercept, change and observe the proxying of
/// requests from a client to a target and back.
///
/// Register listeners when constructor the router with the
/// `CrankerRouterBuilder.with_proxy_listeners()` method.
///
/// Note: the default implementation of each method is a no-op
/// operation, so you can just override the events you are
/// interested in.
#[allow(unused_variables)]
pub trait ProxyListener: Sync + Send {

    /// This is called before sending a request to the target service
    ///
    /// * `info` -  Info about the request and response. Note that
    /// duration, bytes sent and bytes received will contain values
    /// as the current point in time.
    /// * `request_headers_to_target` -   The headers that will be
    /// sent to the client. Modify this object in order to change
    /// the headers sent to the target server.
    fn on_before_proxy_to_target(
        &self,
        info: &dyn ProxyInfo,

        request_headers_to_target: &mut HeaderMap
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// This is called before sending the response to the client.
    ///
    /// The response info contains the response objects with the
    /// HTTP status and headers that will be sent to the client.
    /// You are able to change these values at this point in the
    /// lifecycle.
    ///
    /// * `info` -  Info about the request and response. Note that
    /// duration, bytes sent and bytes received will contain values
    /// as the current point in time.
    fn on_before_responding_to_client(
        &self,
        info: &dyn ProxyInfo
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// This is called if a free socket could not be found for the target in which case:
    /// `ProxyInfo.bytes_received()` will be 0.
    /// `ProxyInfo.bytes_sent()` will be 0.
    /// `ProxyInfo.connector_id()` will be an invalid placeholder value.
    /// `ProxyInfo.service_address()` will be an invalid placeholder value.
    /// `ProxyInfo.error_if_any()` will be none.
    ///
    /// * `info` - Information about the response.
    fn on_failure_to_acquire_proxy_socket(
        &self,
        info: &dyn ProxyInfo
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// This is called after a response has been completed.
    /// Note that this is called even if the response was not completed
    /// successfully (for example if a browser was closed before a
    /// response was complete). If the proxying was not successful,
    /// then `ProxyInfo.error_if_any()` will not be None.
    ///
    /// * `proxy_info` - Information about the response.
    fn on_complete(
        &self,
        proxy_info: &dyn ProxyInfo
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// This is called if async method which used to send request headers
    /// has already called. Because the operation of send header is async
    /// so this callback does not mean headers has sent complete
    ///
    /// * `proxy_info` - Information about the response.
    /// * `headers` - The headers that has sent to the target
    fn on_after_proxy_to_target_headers_sent(
        &self,
        proxy_info: &dyn ProxyInfo,
        headers: Option<&HeaderMap>
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// This is called if response headers has already received from target
    ///
    /// * `proxy_info` - Information about the response.
    /// * `status` - Response status code
    /// * `headers` - The headers that has sent to the target
    fn on_after_target_to_proxy_headers_received(
        &self,
        proxy_info: &dyn ProxyInfo,
        status: u16,
        headers: Option<&HeaderMap>
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// Called before a chunk of request body data is sent to the target
    /// This will be called many times if the body has been fragmented
    ///
    /// * `proxy_info` - Information about the response.
    /// * `chunk` - Request body data which is going to be sent to target.
    fn on_before_request_body_chunk_sent_to_target(
        &self,
        proxy_info: &dyn ProxyInfo,
        chunk: &Bytes
    ) -> Result<(), CrankerRouterException> { Ok(())}

    /// Called when a chunk of request body data is sent to the target
    /// This will be called many times if the body has been fragmented
    ///
    /// * `proxy_info` - Information about the response.
    /// * `chunk` - Request body data which already been sent to target
    /// successfully.
    fn on_request_body_chunk_sent_to_target(
        &self,
        proxy_info: &dyn ProxyInfo,
        chunk: &Bytes
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// Called when the full request body has been received to the target
    ///
    /// * `proxy_info` - Information about the response.
    fn on_request_body_sent_to_target(
        &self,
        proxy_info: &dyn ProxyInfo
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// Called when a chunk of response body data is received from the
    /// target This will be called many times if the body has been
    /// fragmented
    ///
    /// * `proxy_info` - Information about the response.
    /// * `chunk` - Response body data received from the target.
    fn on_response_body_chunk_received_from_target(
        &self,
        proxy_info: &dyn ProxyInfo,
        chunk: &Bytes
    ) -> Result<(), CrankerRouterException> { Ok(()) }

    /// `on_response_body_chunk_received_from_target` is expensive, we need you to tell us ahead
    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
        error!("BOOM");
        panic!("Please ensure you implement this method! It's very important to us: do you `really_need_on_response_body_chunk_received_from_target`");
    }

    /// `on_request_body_chunk_sent_to_targe` is expensive in V3, we need you to tell us ahead
    fn really_need_on_request_body_chunk_sent_to_target(&self) -> bool {
        error!("BOOM");
        panic!("Please ensure you implement this method! It's very important to us: do you `really_need_on_request_body_chunk_sent_to_target` (V3)");
    }

    /// Called when the full response body has been received from the target
    /// * `proxy_info` - Info about the request and response. proxying the request.
    fn on_response_body_chunk_received(
        &self,
        proxy_info: &dyn ProxyInfo
    ) -> Result<(), CrankerRouterException> { Ok(()) }
}

pub(crate) struct DefaultProxyListener;

#[allow(dead_code)]
impl DefaultProxyListener {
    pub const fn new() -> Self {
        DefaultProxyListener {}
    }
}

impl ProxyListener for DefaultProxyListener {
    fn really_need_on_response_body_chunk_received_from_target(&self) -> bool {
        false
    }
    fn really_need_on_request_body_chunk_sent_to_target(&self) -> bool {
        false
    }
}

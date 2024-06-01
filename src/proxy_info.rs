use crate::exceptions::CrankerRouterException;
use crate::route_identify::RouteIdentify;

/// An info trait to tell the information during Cranker
/// proxying a request. It's applied to multiple proxy
/// listener hooks.
///
/// Information about a proxied request and response.
/// Use `CrankerRouterBuilder.with_proxy_listeners()` to
/// subscribe to events that exposes this data.
pub trait ProxyInfo : RouteIdentify {
    /// If the proxied request is using catch all
    /// fallback route and the connector/connection/service
    /// behind the scene.
    fn is_catch_all(&self) -> bool;
    /// A unique ID for the service connector.
    fn connector_id(&self) -> String;
    /// The time in millis from when the router received the
    /// request until it sent the last response byte.
    fn duration_millis(&self) -> i64;
    /// The number of bytes uploaded by the client in the request
    fn bytes_received(&self) -> i64;
    /// The number of bytes sent to the client on the response
    fn bytes_sent(&self) -> i64;
    /// Response bodies are sent from a connector to the router
    /// as a number of binary websocket frames. This is a count
    /// of the number of frames received on this socket.
    ///
    /// This can be used to understand how target services are
    /// streaming responses to clients, especially if used in
    /// conjunction with bytesSent() as it can give an idea of
    /// average response chunk size.
    fn response_body_frames(&self) -> i64;
    /// If the response was not proxied successfully, then this
    /// has the exception.
    fn error_if_any(&self) -> Option<CrankerRouterException>;
    /// Wait time in millis seconds to get a websocket (which is
    /// used for proxy requests)
    fn socket_wait_in_millis(&self) -> i64;
}
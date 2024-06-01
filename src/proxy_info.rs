use crate::exceptions::CrankerRouterException;
use crate::route_identify::RouteIdentify;

/// An info trait to tell the information during Cranker
/// proxying a request. It's applied to multiple proxy
/// listener hooks.
pub trait ProxyInfo : RouteIdentify {
    fn is_catch_all(&self) -> bool;
    fn connector_id(&self) -> String;
    fn duration_millis(&self) -> i64;
    fn bytes_received(&self) -> i64;
    fn bytes_sent(&self) -> i64;
    fn response_body_frames(&self) -> i64;
    fn error_if_any(&self) -> Option<CrankerRouterException>;
    fn socket_wait_in_millis(&self) -> i64;
}
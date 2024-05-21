use std::fmt::Debug;

use crate::exceptions::CrankerRouterException;
use crate::router_socket::RouteIdentify;

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
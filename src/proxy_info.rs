use std::net::{IpAddr, SocketAddr};

use crate::exceptions::CrankerRouterException;

pub trait ProxyInfo {
    fn is_catch_all(&self) -> bool;
    fn connector_instance_id(&self) -> String;
    fn server_address(&self) -> SocketAddr;
    fn route(&self) -> String;

    // fn request(&self) -> &Request<Body>;// TODO: Maybe arc mutex ?
    // fn response(&self) -> &Response;// TODO: Maybe arc mutex ?
    fn duration_millis(&self) -> i64;
    fn bytes_received(&self) -> i64;
    fn bytes_sent(&self) -> i64;
    fn response_body_frames(&self) -> i64;
    fn error_if_any(&self) -> Option<CrankerRouterException>;
    // TODO: What type?
    fn socket_wait_in_millis(&self) -> i64;
}
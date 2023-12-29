use std::collections::VecDeque;
use std::sync::{Arc};
use tokio::sync::{Mutex};

use dashmap::{DashMap, Map};
use log::info;

use crate::router_socket::RouterSocket;

pub trait RouteResolver: Sync + Send {
    fn resolve(&self, routes: &DashMap<String, VecDeque<Arc<tokio::sync::RwLock<dyn RouterSocket>>>>, target: String) -> String {
        let split: Vec<&str> = target.split("/").collect();

        return if split.len() >= 2 && routes.contains_key(&split[1].to_string()) {
            info!("route selected: {}", split[1]);
            split[1].to_string()
        } else {
            info!("route selected: **catch all**");
            "*".to_string()
        };
    }
}

pub struct DefaultRouteResolver;

impl DefaultRouteResolver {
    pub const fn new() -> Self { DefaultRouteResolver {} }
}

impl RouteResolver for DefaultRouteResolver {}

impl<F: Send + Sync + 'static> RouteResolver for F
    where F: Fn(&DashMap<String, VecDeque<Arc<tokio::sync::RwLock<dyn RouterSocket>>>>, String) -> String
{
    fn resolve(&self, routes: &DashMap<String, VecDeque<Arc<tokio::sync::RwLock<dyn RouterSocket>>>>, target: String) -> String {
        self(routes, target)
    }
}

pub static DEFAULT_ROUTE_RESOLVER: DefaultRouteResolver = DefaultRouteResolver::new();

// TODO : Longest Route Resolver

#[test]
fn test() {
    let mut s = DashMap::new();
    let resolver = DefaultRouteResolver::new();
    s.insert("route".to_string(), VecDeque::new());
    s.insert("router/v1/api".to_string(), VecDeque::new());
    let res = resolver.resolve(&s, "/route".to_string());
    assert_eq!(res, "route")
}

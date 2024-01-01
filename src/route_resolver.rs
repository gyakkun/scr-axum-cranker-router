use dashmap::DashMap;
use log::debug;

pub trait RouteResolver<T>: Sync + Send {
    fn resolve(&self, routes: &DashMap<String, T>, target: String) -> String {
        let split: Vec<&str> = target.split("/").collect();

        return if split.len() >= 2 && routes.contains_key(&split[1].to_string()) {
            debug!("route selected for {}: {}", target, split[1]);
            split[1].to_string()
        } else {
            debug!("route selected for {}: **catch all**", target);
            "*".to_string()
        };
    }
}

pub struct DefaultRouteResolver;

impl DefaultRouteResolver {
    pub const fn new() -> Self { DefaultRouteResolver {} }
}

impl<T> RouteResolver<T> for DefaultRouteResolver {}

impl<F: Send + Sync + 'static, T> RouteResolver<T> for F
    where F: Fn(&DashMap<String, T>, String) -> String
{
    fn resolve(&self, routes: &DashMap<String, T>, target: String) -> String {
        self(routes, target)
    }
}

pub static DEFAULT_ROUTE_RESOLVER: DefaultRouteResolver = DefaultRouteResolver::new();

// TODO : Longest Route Resolver

#[test]
fn test() {
    use std::collections::VecDeque;
    let mut s = DashMap::new();
    let resolver = DefaultRouteResolver::new();
    s.insert("route".to_string(), VecDeque::<u64>::new());
    s.insert("router/v1/api".to_string(), VecDeque::new());
    let res = resolver.resolve(&s, "/route".to_string());
    assert_eq!(res, "route")
}

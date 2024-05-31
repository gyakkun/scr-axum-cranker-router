use dashmap::DashSet;
use log::debug;

pub trait RouteResolver: Sync + Send {
    fn resolve(&self, routes: &DashSet<String>, target: &String) -> String {
        let split: Vec<&str> = target.split("/").collect();

        return if split.len() >= 2 && routes.contains(&split[1].to_string()) {
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

impl RouteResolver for DefaultRouteResolver {}

impl<F: Send + Sync + 'static> RouteResolver for F
    where F: Fn(&DashSet<String>, &String) -> String
{
    fn resolve(&self, routes: &DashSet<String>, target: &String) -> String {
        self(routes, target)
    }
}

pub static DEFAULT_ROUTE_RESOLVER: DefaultRouteResolver = DefaultRouteResolver::new();

// TODO : Longest Route Resolver

#[test]
fn test() {
    let s = DashSet::new();
    let resolver = DefaultRouteResolver::new();
    s.insert("route".to_string());
    s.insert("router/v1/api".to_string());
    let res = resolver.resolve(&s, &"/route".to_string());
    assert_eq!(res, "route")
}

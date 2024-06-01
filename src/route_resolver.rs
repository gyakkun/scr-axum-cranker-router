use dashmap::DashSet;
use log::debug;

/// To choose a route for an HTTP path. Can be prefix matching or exact matching.
/// See `DefaultRouteResolver` and `LongestRouteResolver`
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

pub struct LongestRouteResolver;

impl LongestRouteResolver {
    pub const fn new() -> Self { LongestRouteResolver {} }
}

impl RouteResolver for LongestRouteResolver {
    fn resolve(&self, routes: &DashSet<String>, target: &String) -> String {
        if routes.contains(target) {
            return target.clone();
        }
        let mut vec_char = target.clone().chars().collect::<Vec<char>>();
        if vec_char[0] == '/' {
            vec_char.remove(0);
        }

        let mut last_index;
        while {
            last_index = Self::last_index_of(&vec_char, '/');
            last_index
        } >= 0 {
            vec_char.drain((last_index as usize)..vec_char.len());
            let cur_slice = Self::vec_char_to_string(&vec_char);
            if routes.contains(&cur_slice) {
                return cur_slice;
            }
        }

        "*".to_string()
    }
}

impl LongestRouteResolver {
    fn last_index_of(str: &Vec<char>, pat: char) -> i32 {
        let len = str.len();
        let iter_rev = str.iter().rev();
        for (pos, ele) in iter_rev.enumerate() {
            if ele == &pat {
                return (len - pos -1) as i32;
            }
        }
        -1
    }

    fn vec_char_to_string(vec_char: &Vec<char>) -> String {
        vec_char.iter().collect::<String>()
    }
}

pub static LONGEST_ROUTE_RESOLVER: LongestRouteResolver = LongestRouteResolver::new();


#[test]
fn default_route_resolver_test() {
    let s = DashSet::new();
    let resolver = DefaultRouteResolver::new();
    s.insert("route".to_string());
    s.insert("router/v1/api".to_string());
    let res = resolver.resolve(&s, &"/route".to_string());
    assert_eq!(res, "route")
}

#[test]
fn test_vec_char_to_string() {
    let vec_char = vec!['a', 'b', 'c', 'd', 'e'];
    let res = LongestRouteResolver::vec_char_to_string(&vec_char);
    assert_eq!(res.as_str(), "abcde")
}

#[test]
fn longest_first_route_resolver_test() {
    let s = DashSet::new();
    let resolver = LongestRouteResolver::new();
    s.insert("my-service".to_string());
    s.insert("my-service/instance".to_string());
    s.insert("my-service/instance/api".to_string());

    assert_eq!(resolver.resolve(&s, &"/my-service/hello".to_string()), "my-service");
    assert_eq!(resolver.resolve(&s, &"my-service/hello".to_string()), "my-service");
    assert_eq!(resolver.resolve(&s, &"/my-service/instance/api/test".to_string()), "my-service/instance/api");
    assert_eq!(resolver.resolve(&s, &"/my-service/instance/test".to_string()), "my-service/instance");
    assert_eq!(resolver.resolve(&s, &"/non-exist/instance/test".to_string()), "*");
    assert_eq!(resolver.resolve(&s, &"/non-exist".to_string()), "*")
}

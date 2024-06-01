use std::net::IpAddr;

/// A validator that can be used to secure registration requests.
/// An IPValidator is registered when constructing the router, via
/// `CrankerRouterBuilder.with_registration_ip_validator(Arc< dyn IPValidator>)`
pub trait IPValidator: Sync + Send {
    fn allow(&self, ip: IpAddr) -> bool;
}

/// A validator that allows all IP addresses to connect
pub struct AllowAll;

impl AllowAll {
    pub const fn new() -> Self {
        Self {}
    }
}

#[allow(unused_variables)]
impl IPValidator for AllowAll {
    /// Called when a connector attempts to register a route to this router.
    fn allow(&self, ip: IpAddr) -> bool { true }
}
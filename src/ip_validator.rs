use std::net::IpAddr;

pub trait IPValidator: Sync + Send {
    fn allow(&self, ip: IpAddr) -> bool;
}

pub struct AllowAll;

impl AllowAll {
    pub fn new() -> Self {
        Self {}
    }
}

#[allow(unused_variables)]
impl IPValidator for AllowAll {
    fn allow(&self, ip: IpAddr) -> bool { true }
}
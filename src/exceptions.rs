use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct CrankerRouterException {
    reason: String
}

impl CrankerRouterException {
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl Display for CrankerRouterException {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cranker router exception: {}", self.reason)
    }
}

impl Error for CrankerRouterException {}

#[derive(Debug, Clone)]
pub(crate) struct CrankerProtocolVersionNotSupportedException {
    version: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CrankerProtocolVersionNotFoundException;

impl CrankerProtocolVersionNotSupportedException {
    pub(crate) fn new(version: String) -> CrankerProtocolVersionNotSupportedException {
        CrankerProtocolVersionNotSupportedException {
            version
        }
    }
}

impl CrankerProtocolVersionNotFoundException {
    pub(crate) fn new() -> CrankerProtocolVersionNotFoundException {
        CrankerProtocolVersionNotFoundException {}
    }
}

impl Display for CrankerProtocolVersionNotFoundException {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Version is null. Please set header Sec-WebSocket-Protocol for cranker protocol negotiation")
    }
}

impl Error for CrankerProtocolVersionNotFoundException {}

impl Display for CrankerProtocolVersionNotSupportedException {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cranker version {} not supported!", &self.version)
    }
}

impl Error for CrankerProtocolVersionNotSupportedException {}



#[test]
fn test_error() {
    let ex = CrankerProtocolVersionNotSupportedException::new(String::from("v1.0"));
    println!("{}", ex);
    assert_eq!(format!("{}", ex), "Cranker version v1.0 not supported!")
}

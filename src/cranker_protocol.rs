use std::error::Error;
use std::fmt::{Display, Formatter};

pub(crate) const CRANKER_PROTOCOL_VERSION_1_0: &str = "1.0";
pub(crate) const CRANKER_PROTOCOL_VERSION_2_0: &str = "2.0";
pub(crate) const CRANKER_PROTOCOL_VERSION_3_0: &str = "3.0";
pub(crate) const SUPPORTING_HTTP_VERSION_1_1: &str = "HTTP/1.1";

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

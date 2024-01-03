use std::error::Error;
use std::fmt::{Display, Formatter};

use axum::body::Body;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone)]
pub struct CrankerRouterException {
    pub reason: String,
}

impl CrankerRouterException {
    pub fn new(reason: String) -> Self {
        Self { reason }
    }

    pub fn plus(self, another: CrankerRouterException) -> Self {
        let mut reason = self.reason;
        reason.push_str(another.reason.as_str());
        Self {
            reason
        }
    }

    pub fn plus_string(self, further_reason: String) -> Self {
        let mut reason = self.reason;
        reason.push_str(further_reason.as_str());
        Self {
            reason
        }
    }

    pub fn plus_str(self, further_reason: &str) -> Self {
        let mut reason = self.reason;
        reason.push_str(further_reason);
        Self {
            reason
        }
    }
}

impl Display for CrankerRouterException {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cranker router exception: {}", self.reason)
    }
}

impl Error for CrankerRouterException {}

impl IntoResponse for CrankerRouterException {
    fn into_response(self) -> Response {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::new(self.reason))
            .unwrap()
    }
}


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

#[inline]
pub fn compose_ex<ANY>(
    opt_total_err: Option<CrankerRouterException>,
    this_may_err: Result<ANY, CrankerRouterException>,
) -> Option<CrankerRouterException> {
    if let Err(ex) = this_may_err {
        return Some(opt_total_err.map_or(ex.clone(), |some| some.plus(ex)));
    }
    return opt_total_err;
}

#[test]
fn test_error() {
    let ex = CrankerProtocolVersionNotSupportedException::new(String::from("v1.0"));
    println!("{}", ex);
    assert_eq!(format!("{}", ex), "Cranker version v1.0 not supported!")
}

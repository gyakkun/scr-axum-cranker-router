use std::error::Error;
use std::fmt::{Display, Formatter};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use log::error;
use uuid::Uuid;

pub(crate) type CrexKind = CrankerRouterExceptionErrorKind;

#[derive(Debug, Clone)]
pub struct CrankerRouterException {
    pub reason: String,
    pub opt_status_code: Option<u16>,
    pub opt_err_kind: Option<CrexKind>
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CrankerRouterExceptionErrorKind {
    Timeout_0001,

    NoRouterSocketAvailable_0002,

    CrankerProtocolVersionNotFound_0003,
    CrankerProtocolVersionNotSupported_0004,
    CrankerProtocolEndMarkerMissing_0005,
    CrankerProtocolZeroLengthHeaderLine_0006,
    CrankerProtocolHttpStatusCodeMissing_0007,

    ForwardedHeaderParserError_0008,

    ProxyListenerError_0009,

    ClientRequestBodyReadError_0010
}

impl CrankerRouterException {
    pub fn new(reason: String) -> Self {
        Self {
            reason,
            opt_status_code: None,
            opt_err_kind: None,
        }
    }

    pub fn with_status_code(self, status_code: u16) -> Self {
        Self {
            reason: self.reason,
            opt_status_code: Some(status_code),
            opt_err_kind: self.opt_err_kind,
        }
    }

    pub fn with_err_kind(self, err_kind: CrexKind) -> Self {
        Self {
            reason: self.reason,
            opt_status_code: self.opt_status_code,
            opt_err_kind: Some(err_kind),
        }
    }

    pub fn plus(self, another: CrankerRouterException) -> Self {
        let mut reason = self.reason;
        reason.push_str(another.reason.as_str());
        Self {
            reason,
            opt_err_kind: self.opt_err_kind.or(another.opt_err_kind),
            opt_status_code: self.opt_status_code.or(another.opt_status_code)
        }
    }

    pub fn append_string(self, further_reason: String) -> Self {
        let mut reason = self.reason;
        reason.push_str(further_reason.as_str());
        Self {
            reason,
            opt_err_kind: self.opt_err_kind,
            opt_status_code: self.opt_status_code
        }
    }

    pub fn append_str(self, further_reason: &str) -> Self {
        let mut reason = self.reason;
        reason.push_str(further_reason);
        Self {
            reason,
            opt_err_kind: self.opt_err_kind,
            opt_status_code: self.opt_status_code
        }
    }

    pub fn plus_str(self, further_reason: &str) -> Self {
        self.append_str(further_reason)
    }

    pub fn plus_string(self, further_reason: String) -> Self {
        self.append_string(further_reason)
    }

    pub fn prepend_str(self, prior_reason: &str) -> Self {
        let mut reason = prior_reason.to_string();
        reason.push(' ');
        reason.push_str(self.reason.as_str());
        Self {
            reason,
            opt_err_kind: self.opt_err_kind,
            opt_status_code: self.opt_status_code,
        }
    }

    pub fn prepend_string(self, prior_reason: String) -> Self {
        let mut reason = prior_reason.clone();
        reason.push(' ');
        reason.push_str(self.reason.as_str());
        Self {
            reason,
            opt_err_kind: self.opt_err_kind,
            opt_status_code: self.opt_status_code,
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
        let mut body_str = self.reason.clone();
        if let Some(err_kind) = self.opt_err_kind {
            body_str.push_str(
                format!(" (error kind = {:?})", err_kind).as_str()
            )
        }
        let err_id = Uuid::new_v4().to_string();
        body_str.push_str(format!(
            " (error id = {})", err_id
        ).as_str());
        let status_code = self.opt_status_code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
        error!("Respond with CrankerRouterException: code = {} , body = {}", status_code, body_str);
        Response::builder()
            .status(status_code)
            .body(Body::new(body_str))
            .unwrap()
    }
}

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
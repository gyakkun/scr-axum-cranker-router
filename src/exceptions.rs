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
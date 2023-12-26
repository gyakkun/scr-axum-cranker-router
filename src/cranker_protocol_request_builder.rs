use axum::http::HeaderMap;
use log::error;

use crate::cranker_protocol_request_builder::EndMarker::{RequestBodyEnded, RequestBodyPending, RequestHasNoBody};
use crate::exceptions::CrankerRouterException;

pub struct CrankerProtocolRequestBuilder {
    pub request_line: Option<String>,
    pub header_map_str: Option<String>,
    pub end_marker: Option<EndMarker>,
}

impl CrankerProtocolRequestBuilder {
    pub fn new() -> Self {
        Self { request_line: None, header_map_str: None, end_marker: None }
    }

    pub fn with_request_line(mut self, request_line: String) -> Self {
        self.request_line = Some(request_line);
        self
    }
    pub fn with_request_headers(mut self, header_map: &HeaderMap) -> Self {
        self.header_map_str = Some(Self::build_header_str(header_map));
        self
    }
    pub fn with_request_body_pending(mut self) -> Self {
        self.end_marker = Some(RequestBodyPending);
        self
    }
    pub fn with_request_has_no_body(mut self) -> Self {
        self.end_marker = Some(RequestHasNoBody);
        self
    }
    pub fn with_request_body_ended(mut self) -> Self {
        self.end_marker = Some(RequestBodyEnded);
        self
    }

    pub fn build(self) -> Result<String, CrankerRouterException> {
        if self.end_marker.is_none() {
            return Err(CrankerRouterException::new(
                "failed to build cranker protocol request. end marker missing".to_string()
            ));
        }
        return if self.request_line.is_some() && self.header_map_str.is_some() {
            Ok(Self::build_req_and_hdr(self))
        } else {
            match self.end_marker {
                Some(RequestBodyEnded) => Ok(self.end_marker.unwrap().as_str().to_string()),
                _ => Err(CrankerRouterException::new(
                    "failed to build cranker protocol request. end marker should be RequestBodyEnded \\
                     when no request line and header map str is present.".to_string()
                ))
            }
        };
    }

    fn build_req_and_hdr(builder: Self) -> String {
        let mut res = String::new();
        res.push_str(builder.request_line.unwrap().as_str());
        res.push('\n');
        res.push_str(builder.header_map_str.unwrap().as_str());
        res.push('\n');
        res.push_str(builder.end_marker.unwrap().as_str());
        res
    }

    fn build_header_str(header_map: &HeaderMap) -> String {
        let mut headers_str = String::new();
        let mut cookie_list: Vec<&str> = vec![];

        header_map.keys().for_each(|k| {
            header_map.get_all(k).iter().for_each(|v| {
                match v.to_str() {
                    Ok(s) => {
                        if k.as_str().eq_ignore_ascii_case("cookie") {
                            cookie_list.push(s)
                        } else {
                            headers_str.push_str(format!("{}:{}", k.as_str(), s).as_str());
                            headers_str.push('\n');
                        }
                    }
                    Err(e) => {
                        error!("failed to handle header {:?}: {:?}", v, e);
                    }
                }
            });
        });
        let cookie_cat = cookie_list.join("; ");
        headers_str.push_str(cookie_cat.as_str());
        headers_str.push('\n');
        headers_str
    }
}


pub enum EndMarker {
    RequestBodyPending,
    RequestHasNoBody,
    RequestBodyEnded,
}

impl EndMarker {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestBodyPending => "_1",
            RequestHasNoBody => "_2",
            RequestBodyEnded => "_3"
        }
    }
}
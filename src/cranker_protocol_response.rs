use std::str::FromStr;

use axum::http;
use axum::response::Response;

use crate::{HOP_BY_HOP_HEADERS, RESPONSE_HEADERS_TO_NOT_SEND_BACK};
use crate::exceptions::CrankerRouterException;
use crate::http_utils::get_custom_hop_by_hop_headers;

pub struct CrankerProtocolResponse {
    pub headers: Vec<String>,
    pub status: u16,
}

impl CrankerProtocolResponse {
    /**
     * CRANKER_PROTOCOL_VERSION_1_0
     * <p>
     * response msg format:
     * <p>
     * =====Part 1===================
     * ** HTTP/1.1 200 OK\n
     * ** [headers]\n
     * ** \n
     * ===== Part 2 (if msg with body)======
     * **Binary Content
     */
    pub fn new(message_to_apply: String) -> Result<Self, CrankerRouterException> {
        let lines: Vec<&str> = message_to_apply.split('\n').collect();
        let lines_len = lines.len();
        if lines_len <= 0 {
            return Err(CrankerRouterException::new(
                format!("failed to parse header from target response ({})", message_to_apply).to_string()
            ));
        }
        let bits: Vec<&str> = lines[0].split(' ').collect();
        if bits.len() < 2 {
            return Err(CrankerRouterException::new(
                format!("failed to parse status code from target response ({}): no status code found",
                        message_to_apply).to_string()
            ));
        }
        let status = u16::from_str(bits[1]).map_err(|pie| {
            CrankerRouterException::new(
                format!("failed to parse status code from target response header ({}): {:?}",
                        bits[1], pie).to_string())
        })?;
        let headers = lines.into_iter().rev()
            .take(lines_len - 1)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        Ok(Self { headers, status })
    }

    pub fn headers_map(&self) {}

    pub fn default_failed() -> Self {
        Self {
            headers: Vec::new(),
            status: 500,
        }
    }

    pub fn build(&self) -> Result<http::response::Builder, CrankerRouterException> {
        let mut res = Response::builder().status(self.status);
        for header_line in self.headers.iter() {
            let colon_idx = header_line.find(":");
            if colon_idx.is_none() {
                let failed_reason = format!("failed to parse protocol response, header seems corrupted: {:?}", self.headers);
                return Err(CrankerRouterException::new(failed_reason));
            }
            let colon_idx = colon_idx.unwrap();
            let key = *&header_line[..colon_idx].trim();
            let lowercase_key = key.to_ascii_lowercase();

            // looks like axum will not overwrite the date header if exists
            // if lowercaseKey.eq_ignore_ascii_case("date") {
            // }

            if !HOP_BY_HOP_HEADERS.contains(lowercase_key.as_str())
                && !RESPONSE_HEADERS_TO_NOT_SEND_BACK.contains(lowercase_key.as_str()) {
                let value = *&header_line[(colon_idx + 1)..].trim();
                res = res.header(key, value);
            }
        }

        // case insensitive
        if let Some(headers_ref) = res.headers_mut() {
            get_custom_hop_by_hop_headers(
                headers_ref.get(http::header::CONNECTION)
            )
                .iter()
                .for_each(|custom_hop_by_hop_header| {
                    headers_ref.remove(custom_hop_by_hop_header);
                });
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use crate::cranker_protocol_response::CrankerProtocolResponse;

    #[test]
    fn test_parse() {
        let msg = [
            "HTTP/1.1 200 OK",
            "User-Agent: curl/8.0"
        ].join("\n");
        let res = CrankerProtocolResponse::new(msg);
        assert!(res.is_ok());
        let res = res.unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.headers.len(), 1);
    }

    #[test]
    fn test_parse_long() {
        let msg = "HTTP/1.1 302 TODO \n
content-length:10 \n
content-type:text/plain \n
date:Fri, 05 Jan 2024 16:29:44 GMT \n
location:/history".to_string();
        let res = CrankerProtocolResponse::new(msg.clone());
        assert!(res.is_ok());
        let res = res.unwrap();
        assert_eq!(res.status, 302);
        assert_eq!(res.headers.len(), 4);
    }
}
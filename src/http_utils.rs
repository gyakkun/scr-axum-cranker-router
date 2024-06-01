use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;

use axum::extract::OriginalUri;
use axum::http;
use axum::http::{HeaderMap, HeaderValue, Version};
use axum::http::header::AsHeaderName;
use hashlink::LinkedHashMap;
use log::error;

use crate::{ACRState, LOCAL_IP};
use crate::exceptions::{CrankerRouterException, CrexKind};

// Mostly port from ParseUtils, Headtils from mu-server
pub(crate) fn set_target_request_headers(
    cli_hdr: &HeaderMap,
    hdr_to_tgt: &mut HeaderMap,
    app_state: &ACRState,
    http_version: &Version,
    cli_remote_addr: &SocketAddr,
    orig_uri: &OriginalUri,
) -> bool {
    let custom_hop_by_hop = get_custom_hop_by_hop_headers(cli_hdr.get(http::header::CONNECTION));
    let mut has_content_length_or_transfer_encoding = false;

    cli_hdr.iter().for_each(|(k, v)| {
        let lowercase_key = k.to_string().to_lowercase();
        if app_state.config.do_not_proxy_headers.contains(lowercase_key.as_str())
            || custom_hop_by_hop.contains(&lowercase_key) {
            return; // @for_each
        }
        has_content_length_or_transfer_encoding |=
            lowercase_key.eq(http::header::CONTENT_LENGTH.as_str())
                || lowercase_key.eq(http::header::TRANSFER_ENCODING.as_str());
        hdr_to_tgt.insert(k, v.clone());
    });
    let cli_all_via = header_map_get_all_ok_to_string(cli_hdr, http::header::VIA);


    let new_via_val = get_new_via_value(
        format!("{:?} {}", http_version, app_state.config.via_name),
        cli_all_via,
    );
    if let Ok(p) = new_via_val.parse() {
        hdr_to_tgt.append(http::header::VIA, p);
    }

    if let Err(e) = set_forwarded_headers(
        cli_hdr,
        hdr_to_tgt,
        app_state.config.discard_client_forwarded_headers,
        app_state.config.send_legacy_forwarded_headers,
        cli_remote_addr,
        orig_uri,
    ) {
        error!("Failed to set forwarded headers: {}", e);
    }

    return has_content_length_or_transfer_encoding;
}

pub(crate) fn get_custom_hop_by_hop_headers(opt_conn_hdr: Option<&HeaderValue>) -> HashSet<String> {
    let mut res = HashSet::new();
    if let Some(conn_hdr) = opt_conn_hdr {
        if let Ok(conn_hdr_str) = conn_hdr.to_str() {
            if !conn_hdr_str.is_empty() {
                let split = conn_hdr_str.split(",");
                for s in split.into_iter() {
                    res.insert(s.trim().to_string());
                }
            }
        }
    }
    return res;
}

pub(crate) fn set_forwarded_headers(
    cli_hdr: &HeaderMap,
    hdr_to_tgt: &mut HeaderMap,
    discard_client_forwarded_headers: bool,
    send_legacy_forwarded_headers: bool,
    cli_remote_addr: &SocketAddr,
    cli_req_uri: &OriginalUri,
) -> Result<(), CrankerRouterException> {
    let mut forwarded_headers = Vec::new();
    if !discard_client_forwarded_headers {
        forwarded_headers = get_existing_forwarded_headers(cli_hdr)?;
        for j in forwarded_headers.iter() {
            if let Ok(parsed) = j.to_string().parse() {
                hdr_to_tgt.append(http::header::FORWARDED, parsed);
            }
        }
    }

    let new_forwarded_hdr = create_forwarded_header_to_tgt(cli_hdr, cli_remote_addr, cli_req_uri);
    if let Ok(parsed) = new_forwarded_hdr.to_string().parse() {
        hdr_to_tgt.append(http::header::FORWARDED, parsed);
    }

    if send_legacy_forwarded_headers {
        let first = if forwarded_headers.is_empty() { &new_forwarded_hdr } else { forwarded_headers.get(0).unwrap() };
        set_x_forwarded_headers(hdr_to_tgt, first);
    }
    Ok(())
}

pub(crate) fn set_x_forwarded_headers(hdr_to_hdr: &mut HeaderMap, fh: &ForwardedHeader) {
    let l = [fh.proto.as_ref(), fh.host.as_ref(), fh.for_value.as_ref()];
    for v in l {
        if let Some(Some(p)) = v.map(|s| s.parse().ok()) {
            hdr_to_hdr.append(http::header::FORWARDED, p);
        }
    }
}

pub(crate) fn create_forwarded_header_to_tgt(
    cli_hdr: &HeaderMap,
    cli_remote_addr: &SocketAddr,
    cli_req_uri: &OriginalUri,
) -> ForwardedHeader {
    let server_self_ip = LOCAL_IP.to_string();
    ForwardedHeader {
        by: Some(server_self_ip),
        for_value: Some(cli_remote_addr.ip().to_string()),
        host: cli_hdr.get(http::header::HOST)
            .and_then(|i| i.to_str().ok())
            .map(|j| j.to_string()),
        proto: cli_req_uri.scheme_str().map(|i| i.to_string()),
        extensions: None,
    }
}

pub(crate) fn get_existing_forwarded_headers(hdr: &HeaderMap) -> Result<Vec<ForwardedHeader>, CrankerRouterException> {
    let all: Vec<String> = header_map_get_all_ok_to_string(hdr, http::header::FORWARDED);
    let mut res = Vec::new();
    if all.is_empty() {
        let hosts = get_x_forwarded_value(hdr, "x-forwarded-host");
        let ports = get_x_forwarded_value(hdr, "x-forwarded-port");
        let protos = get_x_forwarded_value(hdr, "x-forwarded-proto");
        let fors = get_x_forwarded_value(hdr, "x-forwarded-for");
        let max = max(max(max(hosts.len(), protos.len()), fors.len()), ports.len());
        if max == 0 {
            return Ok(res);
        }
        let include_host = hosts.len() == max;
        let include_port = ports.len() == max;
        let include_proto = protos.len() == max;
        let include_for = fors.len() == max;

        let cur_host = if include_port && !include_host {
            hdr.get(http::header::HOST)
                .and_then(|j| j.to_str().ok())
                .and_then(|k| Some(k.to_string()))
        } else {
            None
        };

        for i in 0..max {
            let host = if include_host { hosts.get(i).and_then(|i| Some(i.clone())) } else { None };
            let port = if include_port { ports.get(i).and_then(|i| Some(i.clone())) } else { None };
            let proto = if include_proto { protos.get(i).and_then(|i| Some(i.clone())) } else { None };
            let for_value = if include_for { fors.get(i).and_then(|i| Some(i.clone())) } else { None };
            let use_default_port = port.is_none() ||
                if proto.is_some() { // and port.is_some()
                    let po = port.clone().unwrap();
                    let pr = proto.clone().unwrap();
                    (pr.eq_ignore_ascii_case("http") && po.eq("80"))
                        || (pr.eq_ignore_ascii_case("https") && po.eq("443"))
                } else {
                    false
                };
            let mut host_to_use = if include_host { host } else if include_port { cur_host.clone() } else { None };
            if host_to_use.is_some() && !use_default_port {
                // replace the ":12345" to default port
                // without regex!
                let host_clone = host_to_use.clone().unwrap().chars().collect::<Vec<char>>();
                let mut idx = (host_clone.len() - 1) as i32;
                // We can use iter here, but keep align with mu for better readability
                while idx >= 0 {
                    let c = host_clone.get(idx as usize).unwrap();
                    if !c.is_ascii_digit() {
                        if *c != ':' {
                            return Err(
                                CrankerRouterException::new(
                                    "Failed to parse port".to_string()
                                ).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                            );
                        } else {
                            break;
                        }
                    } else {
                        // NOP
                    }
                    idx -= 1;
                }
                host_to_use = Some(String::from_iter(host_clone.as_slice()[0..(idx as usize)].iter()));
            }
            res.push(ForwardedHeader {
                by: None,
                for_value,
                host: host_to_use,
                proto,
                extensions: None,
            });
        }
    } else {
        for s in all {
            let mut parsed_from_string = ForwardedHeader::from_string(s.as_str())?;
            res.append(&mut parsed_from_string);
        }
    }
    Ok(res)
}

#[derive(Default)]
pub(crate) struct ForwardedHeader {
    by: Option<String>,
    for_value: Option<String>,
    host: Option<String>,
    proto: Option<String>,
    extensions: Option<LinkedHashMap<String, String>>,
}

#[derive(Debug, PartialOrd, PartialEq, Copy, Clone)]
enum FHParseState {
    ParamName,
    ParamValue,
}

impl ForwardedHeader {
    // port from mu-server ForwardedHeader.java : public static List<ForwardedHeader> fromString(String)
    pub fn from_string(input: &str) -> Result<Vec<ForwardedHeader>, CrankerRouterException> {
        let mut results = Vec::new();
        if input.trim().is_empty() {
            return Ok(results);
        }
        let input_chars = input.chars().collect::<Vec<char>>();

        let mut buffer: Vec<char> = Vec::new();
        let mut i = 0;
        'outer: while i < input_chars.len() {
            let mut opt_extensions: Option<LinkedHashMap<String, String>> = None;
            let mut extensions = LinkedHashMap::new();
            let mut state = FHParseState::ParamName;
            let mut param_name: Option<String> = None;
            let mut by: Option<String> = None;
            let mut for_value: Option<String> = None;
            let mut host: Option<String> = None;
            let mut proto: Option<String> = None;
            let mut is_quoted_string = false;

            'header_value_loop: while i < input_chars.len() {
                let c = input_chars[i];

                if state == FHParseState::ParamName {
                    if c == ',' && buffer.len() == 0 {
                        i += 1;
                        break 'header_value_loop;
                    } else if c == '=' {
                        param_name = Some(String::from_iter(buffer.iter()));
                        buffer = Vec::new();
                        state = FHParseState::ParamValue;
                    } else if is_t_char(c) {
                        buffer.push(c);
                    } else if is_ows(c) {
                        if buffer.len() > 0 {
                            return Err(
                                CrankerRouterException::new(format!(
                                    "Got whitespace in parameter name while in {:?} - header was {}",
                                    state, String::from_iter(buffer.iter())
                                )).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                            );
                        }
                    } else {
                        return Err(
                            CrankerRouterException::new(format!(
                                "Got ascii {} while in {:?}", (c as i32), state
                            )).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                        );
                    }
                } else { // state == FHParseState::ParamValue
                    let is_first = !is_quoted_string && buffer.len() == 0;
                    if is_first && is_ows(c) {
                        // ignore it
                    } else if is_first && c == '"' {
                        is_quoted_string = true
                    } else {
                        if is_quoted_string {
                            let last_char = input_chars[i - 1];
                            if c == '\\' {
                                // don't append
                            } else if last_char == '\\' {
                                buffer.push(c)
                            } else if c == '"' {
                                // this is the end, but we'll update on the next go
                                is_quoted_string = false;
                            } else {
                                buffer.push(c);
                            }
                        } else {
                            if c == ';' {
                                let val = String::from_iter(buffer.iter());
                                let opt_param_name = param_name.clone();
                                match opt_param_name {
                                    None => {
                                        return Err(
                                            CrankerRouterException::new(
                                                "Illegal state: should already parsing forwarded header value but param name still None".to_string()
                                            ).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                                        );
                                    },
                                    Some(pn) => {
                                        let pn = pn.to_lowercase();
                                        if pn == "by" {
                                            by = Some(val);
                                        } else if pn == "for" {
                                            for_value = Some(val);
                                        } else if pn == "host" {
                                            host = Some(val);
                                        } else if pn == "proto" {
                                            proto = Some(val);
                                        } else {
                                            extensions.insert(pn, val);
                                        }
                                    }
                                }
                                buffer = Vec::new();
                                param_name = None;
                                state = FHParseState::ParamName;
                            } else if c == ',' {
                                i += 1;
                                break 'header_value_loop;
                            } else if is_v_char(c) {
                                buffer.push(c);
                            } else if is_ows(c) {
                                // ignore it
                            } else {
                                return Err(
                                    CrankerRouterException::new(format!(
                                        "Got character code {} ({}) while parsing param value", (c as i32), c
                                    )).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                                );
                            }
                        }
                    }
                }
                i += 1;
            }

            match state {
                FHParseState::ParamValue => {
                    let val = String::from_iter(buffer.iter());
                    let opt_param_name = param_name.clone();
                    match opt_param_name {
                        None => {
                            return Err(
                                CrankerRouterException::new(
                                    "Illegal state: should already parsing forwarded header value but param name still None".to_string()
                                ).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                            );
                        }
                        Some(pn) => {
                            let pn = pn.to_lowercase();
                            if pn == "by" {
                                by = Some(val);
                            } else if pn == "for" {
                                for_value = Some(val);
                            } else if pn == "host" {
                                host = Some(val);
                            } else if pn == "proto" {
                                proto = Some(val);
                            } else {
                                extensions.insert(pn, val);
                            }
                        }
                    }

                    buffer = Vec::new();
                }
                _ => {
                    if buffer.len() > 0 {
                        return Err(
                            CrankerRouterException::new(format!(
                                "Unexpected ending point at state {:?} for {}", state, input
                            )).with_err_kind(CrexKind::ForwardedHeaderParserError_0008)
                        );
                    }
                }
            }
            if !extensions.is_empty() {
                opt_extensions = Some(extensions);
            }
            results.push(ForwardedHeader {
                by,
                for_value,
                host,
                proto,
                extensions: opt_extensions,
            });
        }
        return Ok(results);
    }
}

impl Display for ForwardedHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut sb = String::new();
        append_string_for_forwarded_header(&mut sb, "by", self.by.as_ref());
        append_string_for_forwarded_header(&mut sb, "for", self.for_value.as_ref());
        append_string_for_forwarded_header(&mut sb, "host", self.host.as_ref());
        append_string_for_forwarded_header(&mut sb, "proto", self.proto.as_ref());
        self.extensions.as_ref()
            .map(|m| {
                m.iter().for_each(|(k, v)| {
                    append_string_for_forwarded_header(&mut sb, k.as_str(), Some(v));
                });
            });
        write!(f, "{}", sb)
    }
}

fn append_string_for_forwarded_header(sb: &mut String, k: &str, v: Option<&String>) {
    if v.is_none() {
        return;
    }
    if !sb.is_empty() {
        sb.push(';');
    }
    let unwrapped = v.unwrap();
    sb.push_str(k);
    sb.push('=');
    sb.push_str(quote_if_needed(unwrapped).as_str());
}

fn quote_if_needed(s: &String) -> String {
    return if !s.chars().all(is_t_char) {
        let mut res = String::new();
        let replaced = s.replace('"', "\\\"");
        res.push('"');
        res.push_str(replaced.as_str());
        res.push('"');
        res
    } else {
        s.clone()
    };
}

#[inline]
fn is_t_char(c: char) -> bool {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9' || c == '!' ||
        c == '#' || c == '$' || c == '%' || c == '&' || c == '\'' || c == '*' || c == '+' ||
        c == '-' || c == '.' || c == '^' || c == '_' || c == '`' || c == '|' || c == '~');
}

#[inline]
fn is_ows(c: char) -> bool {
    return c == ' ' || c == '\t';
}

#[inline]
fn is_v_char(c: char) -> bool {
    return (c as i32) >= 0x21 && (c as i32) <= 0x7E;
}

fn header_map_get_all_ok_to_string<K>(hdr: &HeaderMap, hdr_key: K)
                                      -> Vec<String>
    where K: AsHeaderName
{
    hdr.get_all(hdr_key).iter()
        .map(|f| f.to_str())
        .filter(|r| r.is_ok())
        .map(|f| f.unwrap())
        .map(|f| f.to_string())
        .collect()
}

fn get_x_forwarded_value(hdr: &HeaderMap, xf_key: &str) -> Vec<String> {
    let mut res = Vec::new();
    let vals = header_map_get_all_ok_to_string(hdr, xf_key);
    if !vals.is_empty() {
        vals.iter()
            .for_each(|i| {
                i.split(",")
                    .map(|j| j.trim())
                    .map(|k| k.to_string())
                    .for_each(|o| res.push(o));
            });
    }
    res
}

fn get_new_via_value(this_server_via: String, prev_via_list: Vec<String>) -> String {
    let mut res = prev_via_list.join(", ").to_string();
    if !prev_via_list.is_empty() {
        res.push_str(", ");
    }
    res.push_str(this_server_via.as_str());
    return res;
}

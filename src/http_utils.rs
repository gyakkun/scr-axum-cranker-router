use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use axum::extract::OriginalUri;
use axum::http;
use axum::http::{HeaderMap, HeaderValue, Version};
use axum::http::header::AsHeaderName;
use crate::{ACRState, LOCAL_IP};


// Mostly port from ParseUtils, Headtils from mu-server
pub(super) fn set_target_request_headers(
    cli_hdr: &HeaderMap,
    mut hdr_to_tgt: &mut HeaderMap,
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

    set_forwarded_headers(
        cli_hdr,
        hdr_to_tgt,
        app_state.config.discard_client_forwarded_headers,
        app_state.config.send_legacy_forwarded_headers,
        cli_remote_addr,
        orig_uri,
    );

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

pub(super) fn set_forwarded_headers(
    cli_hdr: &HeaderMap,
    hdr_to_tgt: &mut HeaderMap,
    discard_client_forwarded_headers: bool,
    send_legacy_forwarded_headers: bool,
    cli_remote_addr: &SocketAddr,
    cli_req_uri: &OriginalUri,
) {
    let mut forwarded_headers = Vec::new();
    if !discard_client_forwarded_headers {
        forwarded_headers = get_existing_forwarded_headers(cli_hdr);
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
}

pub(super) fn set_x_forwarded_headers(hdr_to_hdr: &mut HeaderMap, fh: &ForwardedHeader) {
    let l = [fh.proto.as_ref(), fh.host.as_ref(), fh.for_value.as_ref()];
    for v in l {
        if let Some(Some(p)) = v.map(|s| s.parse().ok()) {
            hdr_to_hdr.append(http::header::FORWARDED, p);
        }
    }
}

pub(super) fn create_forwarded_header_to_tgt(
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

pub(super) fn get_existing_forwarded_headers(hdr: &HeaderMap) -> Vec<ForwardedHeader> {
    let all: Vec<String> = header_map_get_all_ok_to_string(hdr, http::header::FORWARDED);
    let mut res = Vec::new();
    if all.is_empty() {
        let hosts = get_x_forwarded_value(hdr, "x-forwarded-host");
        let ports = get_x_forwarded_value(hdr, "x-forwarded-port");
        let protos = get_x_forwarded_value(hdr, "x-forwarded-proto");
        let fors = get_x_forwarded_value(hdr, "x-forwarded-for");
        let max = max(max(max(hosts.len(), protos.len()), fors.len()), ports.len());
        if max == 0 {
            return res;
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
                if port.is_some() { // and port.is_some()
                    let po = port.clone().unwrap();
                    let pr = port.clone().unwrap();
                    (pr.eq_ignore_ascii_case("http") && po.eq("80"))
                        || (pr.eq_ignore_ascii_case("https") && po.eq("443"))
                } else {
                    false
                };
            let host_to_use = if include_host { host } else if include_port { cur_host.clone() } else { None };
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
            for t in rfc7239::parse(s.as_str()) {
                if let Ok(u) = t {
                    res.push(u.into());
                }
            }
        }
    }
    res
}

#[derive(Default)]
struct ForwardedHeader {
    by: Option<String>,
    for_value: Option<String>,
    host: Option<String>,
    proto: Option<String>,
    extensions: Option<HashMap<String, String>>,
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
    return if s.chars().any(is_t_char) {
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

impl From<rfc7239::Forwarded<'_>> for ForwardedHeader {
    fn from(rf: rfc7239::Forwarded) -> Self {
        ForwardedHeader {
            by: rf.forwarded_by.map(|i| i.to_string()),
            for_value: rf.forwarded_for.map(|i| i.to_string()),
            host: rf.host.map(|i| i.to_string()),
            proto: rf.protocol.map(|i| i.to_string()),
            extensions: None,
        }
    }
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

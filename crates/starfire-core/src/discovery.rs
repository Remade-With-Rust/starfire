// SPDX-License-Identifier: Apache-2.0
//! Discovery & host management — docs/protocol/01-discovery.md.
//! Derived from protocol observation against Sunshine 2026.516.143833. Clean-room.
//!
//! F1: manual host entry + a reachability/pair-status probe of `/serverinfo` over
//! plaintext HTTP (47989). mDNS `_nvstream._tcp` browse is still TODO (needs a
//! capture mechanism on Windows to freeze its fixture). The HTTP probe is a tiny
//! std-only client — the unauthenticated endpoint needs no TLS, so no HTTP/TLS
//! dependency enters the tree yet.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::serverinfo::ServerInfo;

/// Conventional Sunshine ports (defaults; the host may override). [CAPTURE-LOCKED]
pub const DEFAULT_HTTP_PORT: u16 = 47989;
pub const DEFAULT_HTTPS_PORT: u16 = 47984;

/// mDNS service type Sunshine advertises. [CAPTURE-LOCKED] — confirm against a
/// real mDNS capture before relying on the exact string.
pub const MDNS_SERVICE: &str = "_nvstream._tcp.local.";

/// A host to probe: an address plus the plaintext HTTP control port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    pub host: String,
    pub http_port: u16,
}

impl HostAddress {
    /// Parse manual entry: `host`, `host:port`, `[v6]`, or `[v6]:port`.
    pub fn parse(input: &str) -> crate::Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            return Err(crate::Error::Protocol("empty host".into()));
        }

        // Bracketed IPv6 literal, optionally with a port.
        if let Some(rest) = input.strip_prefix('[') {
            let (addr, after) = rest
                .split_once(']')
                .ok_or_else(|| crate::Error::Protocol("unterminated IPv6 literal".into()))?;
            let http_port = match after.strip_prefix(':') {
                Some(p) => parse_port(p)?,
                None if after.is_empty() => DEFAULT_HTTP_PORT,
                None => return Err(crate::Error::Protocol("junk after IPv6 literal".into())),
            };
            return Ok(Self {
                host: addr.to_string(),
                http_port,
            });
        }

        // `host:port` only when there is exactly one colon (else it's a bare v6).
        match input.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && !h.is_empty() => Ok(Self {
                host: h.to_string(),
                http_port: parse_port(p)?,
            }),
            _ => Ok(Self {
                host: input.to_string(),
                http_port: DEFAULT_HTTP_PORT,
            }),
        }
    }
}

fn parse_port(s: &str) -> crate::Result<u16> {
    s.parse()
        .map_err(|_| crate::Error::Protocol(format!("invalid port {s:?}")))
}

/// A minimal HTTP/1.1 response: status code + body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Probe a host: GET `/serverinfo` over HTTP and parse it. This is how we decide
/// pair vs connect and surface reachability (docs/protocol/01).
pub fn probe(address: &HostAddress) -> crate::Result<ServerInfo> {
    let resp = http_get(
        &address.host,
        address.http_port,
        "/serverinfo",
        Duration::from_secs(5),
    )?;
    if resp.status != 200 {
        return Err(crate::Error::Protocol(format!(
            "/serverinfo returned HTTP {}",
            resp.status
        )));
    }
    ServerInfo::parse(&resp.body)
}

/// Issue a one-shot `GET` and read the full response (server closes the
/// connection). Std-only; for the unauthenticated HTTP control port.
pub fn http_get(
    host: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> crate::Result<HttpResponse> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| crate::Error::Protocol(format!("could not resolve {host}:{port}")))?;

    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: starfire\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_http_response(&raw)
}

/// Parse a raw HTTP/1.1 response into status + body. Pure (no I/O), so it is unit
/// tested directly. Assumes `Connection: close` (body = everything after the
/// header terminator); good enough for `/serverinfo`.
pub fn parse_http_response(raw: &[u8]) -> crate::Result<HttpResponse> {
    let sep = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| crate::Error::Protocol("no header/body separator in response".into()))?;
    let header = &raw[..sep];
    let body = raw[sep + 4..].to_vec();

    let status_line = header
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(header);
    let status_line = std::str::from_utf8(status_line)
        .map_err(|_| crate::Error::Protocol("non-UTF8 status line".into()))?;
    // "HTTP/1.1 200 OK" -> the second whitespace-separated token is the code.
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| crate::Error::Protocol(format!("bad status line: {status_line:?}")))?;

    Ok(HttpResponse { status, body })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn host_address_parse_forms() {
        assert_eq!(
            HostAddress::parse("192.168.1.10").unwrap(),
            HostAddress {
                host: "192.168.1.10".into(),
                http_port: DEFAULT_HTTP_PORT
            }
        );
        assert_eq!(
            HostAddress::parse("192.168.1.10:48000").unwrap(),
            HostAddress {
                host: "192.168.1.10".into(),
                http_port: 48000
            }
        );
        assert_eq!(
            HostAddress::parse("[::1]:47989").unwrap(),
            HostAddress {
                host: "::1".into(),
                http_port: 47989
            }
        );
        assert_eq!(
            HostAddress::parse("[fe80::1]").unwrap(),
            HostAddress {
                host: "fe80::1".into(),
                http_port: DEFAULT_HTTP_PORT
            }
        );
        // Bare IPv6 (multiple colons, unbracketed) -> treated as host, default port.
        assert_eq!(HostAddress::parse("fe80::1").unwrap().host, "fe80::1");
        assert!(HostAddress::parse("host:notaport").is_err());
        assert!(HostAddress::parse("").is_err());
    }

    #[test]
    fn http_response_parsing() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: 5\r\n\r\nhello";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");

        let r404 = parse_http_response(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
        assert_eq!(r404.status, 404);
        assert!(r404.body.is_empty());

        assert!(parse_http_response(b"garbage").is_err());
    }

    /// Live probe against a locally-running Sunshine. Ignored by default (CI has
    /// no host); run with `cargo test -p starfire-core -- --ignored live_probe`
    /// while Sunshine is up on 47989.
    #[test]
    #[ignore = "requires a running Sunshine host on 127.0.0.1:47989"]
    fn live_probe_localhost() {
        let addr = HostAddress::parse("127.0.0.1").unwrap();
        let si = probe(&addr).expect("probe localhost serverinfo");
        assert_eq!(si.status_code, Some(200));
        assert!(si.https_port.is_some());
        println!(
            "live host: pair_status={:?} codec_mode={:?} state={:?}",
            si.pair_status, si.server_codec_mode_support, si.state
        );
    }
}

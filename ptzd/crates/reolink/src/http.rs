//! Hand-rolled HTTP/1.1 POST client behind a swappable `Transport`.
//! Plain HTTP only for now. Which transport the camera really offers to a
//! root process (nginx HTTPS on 127.0.0.1:443 vs FastCGI to cgiserver on
//! 127.0.0.1:9527) is UNPROVEN (T9, plan §9 "Transport to the API").

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// POSTs a JSON body to `path_and_query` and returns the response body.
pub trait Transport: Send + Sync {
    fn post(&self, path_and_query: &str, body: &str) -> Result<String, TransportError>;
}

/// Own choice for a local API, not a camera fact. UNPROVEN (T9): real
/// Login/sweep latency is unmeasured; override with `with_timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HttpTransport {
    /// "host:port"
    addr: String,
    timeout: Duration,
}

impl HttpTransport {
    pub fn new(addr: impl Into<String>) -> Self {
        HttpTransport { addr: addr.into(), timeout: DEFAULT_TIMEOUT }
    }
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

fn err<E: std::fmt::Display>(what: &str, e: E) -> TransportError {
    TransportError(format!("{what}: {e}"))
}

impl Transport for HttpTransport {
    fn post(&self, path_and_query: &str, body: &str) -> Result<String, TransportError> {
        let sa = self
            .addr
            .to_socket_addrs()
            .map_err(|e| err("resolve", e))?
            .next()
            .ok_or_else(|| TransportError("no address".into()))?;
        let mut s = TcpStream::connect_timeout(&sa, self.timeout).map_err(|e| err("connect", e))?;
        s.set_read_timeout(Some(self.timeout)).map_err(|e| err("timeout", e))?;
        s.set_write_timeout(Some(self.timeout)).map_err(|e| err("timeout", e))?;
        let req = format!(
            "POST {path_and_query} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        s.write_all(req.as_bytes()).map_err(|e| err("write", e))?;
        let mut raw = Vec::new();
        // Connection: close, so read to EOF. Cap at 1 MiB against runaway replies.
        s.take(1 << 20).read_to_end(&mut raw).map_err(|e| err("read", e))?;
        parse_response(&raw)
    }
}

/// Parses a full HTTP/1.1 response (Content-Length, chunked, or until EOF).
pub fn parse_response(raw: &[u8]) -> Result<String, TransportError> {
    let sep = find(raw, b"\r\n\r\n").ok_or_else(|| TransportError("truncated response".into()))?;
    let head = String::from_utf8_lossy(&raw[..sep]).to_string();
    let body = &raw[sep + 4..];
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| TransportError(format!("bad status line: {status_line}")))?;
    if (300..400).contains(&code) {
        // research/ptzd-hardware.md §8: HTTP may redirect to HTTPS (UNPROVEN, T9).
        return Err(TransportError(format!("HTTP {code} redirect (HTTPS-only camera? T9)")));
    }
    if code != 200 {
        return Err(TransportError(format!("HTTP {code}")));
    }
    let mut clen: Option<usize> = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                clen = v.parse().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let bytes = if chunked {
        dechunk(body)?
    } else if let Some(n) = clen {
        if body.len() < n {
            return Err(TransportError("short body".into()));
        }
        body[..n].to_vec()
    } else {
        body.to_vec()
    };
    String::from_utf8(bytes).map_err(|_| TransportError("non-utf8 body".into()))
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, TransportError> {
    let bad = || TransportError("bad chunked body".into());
    let mut out = Vec::new();
    loop {
        let eol = find(b, b"\r\n").ok_or_else(bad)?;
        let size_s = std::str::from_utf8(&b[..eol]).map_err(|_| bad())?;
        let size_s = size_s.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_s, 16).map_err(|_| bad())?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_variants() {
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhiXX").unwrap(), "hi");
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(),
            "abcde"
        );
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\nrest").unwrap(), "rest");
        assert!(parse_response(b"HTTP/1.1 302 Found\r\nLocation: x\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 500 X\r\n\r\n").is_err());
    }
}

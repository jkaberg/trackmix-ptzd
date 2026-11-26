//! Blocking HTTP/1.1: thread per connection, keep-alive, Content-Length.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth;
use crate::device::{self, Ctx, PATH_DEVICE, PATH_MEDIA, PATH_PTZ};
use crate::soap::*;
use crate::{media, ptz, xml, Shared};

/// Request line / header line cap and header block cap. Bounds hostile
/// input only; ONVIF clients send < 1 KB of headers.
const MAX_LINE: usize = 4096;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;

pub struct Reply {
    pub status: u16,
    pub ctype: &'static str,
    pub body: String,
}

pub fn accept_loop(listener: TcpListener, sh: Arc<Shared>) {
    for conn in listener.incoming() {
        if sh.stop.load(Ordering::SeqCst) {
            break;
        }
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        if sh.conns.fetch_add(1, Ordering::SeqCst) >= sh.cfg.max_connections {
            sh.conns.fetch_sub(1, Ordering::SeqCst);
            let _ = refuse(stream);
            continue;
        }
        // Per-source-IP cap (R5): one host cannot hold every slot.
        let ip = match stream.peer_addr() {
            Ok(a) => a.ip(),
            Err(_) => {
                sh.conns.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
        };
        if !ip_acquire(&sh, ip) {
            sh.conns.fetch_sub(1, Ordering::SeqCst);
            let _ = refuse(stream);
            continue;
        }
        let sh2 = sh.clone();
        let spawned = std::thread::Builder::new().name("onvif-conn".into()).spawn(move || {
            let _ = serve_conn(stream, &sh2);
            ip_release(&sh2, ip);
            sh2.conns.fetch_sub(1, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            ip_release(&sh, ip);
            sh.conns.fetch_sub(1, Ordering::SeqCst);
            log::warn!("thread spawn: {e}");
        }
    }
}

fn ip_acquire(sh: &Shared, ip: IpAddr) -> bool {
    let mut m = sh.per_ip.lock().unwrap_or_else(|e| e.into_inner());
    let n = m.entry(ip).or_insert(0);
    if *n >= sh.cfg.max_connections_per_ip {
        return false;
    }
    *n += 1;
    true
}

fn ip_release(sh: &Shared, ip: IpAddr) {
    let mut m = sh.per_ip.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = m.get_mut(&ip) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            m.remove(&ip);
        }
    }
}

/// Reader whose every `read` is bounded by an absolute deadline (R5). A
/// per-read timeout alone lets a peer reset the clock one byte at a time.
struct DeadlineReader {
    s: TcpStream,
    idle: Duration,
    deadline: Option<Instant>,
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let t = match self.deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(r) if !r.is_zero() => r,
                _ => return Err(io::ErrorKind::TimedOut.into()),
            },
            None => self.idle,
        };
        self.s.set_read_timeout(Some(t))?;
        self.s.read(buf)
    }
}

fn refuse(mut s: TcpStream) -> io::Result<()> {
    s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

#[derive(Debug)]
enum ReadErr {
    /// Peer closed or idle timeout before a new request started.
    Closed,
    Bad(u16, &'static str),
    Io(io::Error),
}

struct Request {
    method: String,
    path: String,
    http11: bool,
    host: Option<String>,
    connection: Option<String>,
    body: Vec<u8>,
}

fn serve_conn(stream: TcpStream, sh: &Shared) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut w = stream.try_clone()?;
    // A stalled reader must not pin the thread either.
    stream.set_write_timeout(Some(sh.cfg.request_timeout))?;
    let mut r = BufReader::new(DeadlineReader { s: stream, idle: sh.cfg.idle_timeout, deadline: None });
    loop {
        r.get_mut().deadline = None; // idle wait for the next request
        let req = match read_request(&mut r, sh, &mut w) {
            Ok(r) => r,
            Err(ReadErr::Closed) => return Ok(()),
            Err(ReadErr::Io(e)) => return Err(e),
            Err(ReadErr::Bad(code, msg)) => {
                write_reply(&mut w, &Reply { status: code, ctype: "text/plain", body: msg.into() }, false)?;
                return Ok(());
            }
        };
        let keep = match req.connection.as_deref() {
            Some(c) if c.eq_ignore_ascii_case("close") => false,
            Some(c) if c.eq_ignore_ascii_case("keep-alive") => true,
            _ => req.http11,
        } && !sh.stop.load(Ordering::SeqCst);
        let reply = route(sh, &req);
        write_reply(&mut w, &reply, keep)?;
        if !keep {
            return Ok(());
        }
    }
}

fn read_line_limited<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, deadline: Instant) -> Result<(), ReadErr> {
    buf.clear();
    let mut lim = r.by_ref().take(MAX_LINE as u64 + 1);
    match lim.read_until(b'\n', buf) {
        Ok(0) => Err(ReadErr::Closed),
        Ok(_) => {
            if !buf.ends_with(b"\n") {
                return Err(if buf.len() > MAX_LINE {
                    ReadErr::Bad(431, "header line too long")
                } else {
                    ReadErr::Closed
                });
            }
            if Instant::now() > deadline {
                return Err(ReadErr::Bad(408, "request timeout"));
            }
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            Ok(())
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            Err(ReadErr::Bad(408, "request timeout"))
        }
        Err(e) => Err(ReadErr::Io(e)),
    }
}

fn read_request(
    r: &mut BufReader<DeadlineReader>,
    sh: &Shared,
    w: &mut TcpStream,
) -> Result<Request, ReadErr> {
    // Wait for the first byte under the idle timeout.
    match r.fill_buf() {
        Ok([]) => return Err(ReadErr::Closed),
        Ok(_) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            return Err(ReadErr::Closed)
        }
        Err(e) => return Err(ReadErr::Io(e)),
    }
    // One absolute deadline for the whole request, headers and body.
    let deadline = Instant::now() + sh.cfg.request_timeout;
    r.get_mut().deadline = Some(deadline);
    let mut line = Vec::new();
    read_line_limited(r, &mut line, deadline)?;
    let rl = String::from_utf8_lossy(&line).into_owned();
    let mut parts = rl.split_ascii_whitespace();
    let (method, path, ver) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(p), Some(v)) => (m.to_string(), p.to_string(), v),
        _ => return Err(ReadErr::Bad(400, "bad request line")),
    };
    let http11 = match ver {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(ReadErr::Bad(505, "HTTP version not supported")),
    };
    let (mut host, mut connection, mut clen, mut expect) = (None, None, None::<usize>, false);
    let (mut n, mut total) = (0usize, 0usize);
    loop {
        read_line_limited(r, &mut line, deadline).map_err(|e| match e {
            ReadErr::Closed => ReadErr::Bad(400, "truncated headers"),
            e => e,
        })?;
        if line.is_empty() {
            break;
        }
        n += 1;
        total += line.len();
        if n > MAX_HEADERS || total > MAX_HEADER_BYTES {
            return Err(ReadErr::Bad(431, "headers too large"));
        }
        let s = String::from_utf8_lossy(&line);
        let (k, v) = match s.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => return Err(ReadErr::Bad(400, "bad header")),
        };
        match k.as_str() {
            "host" => host = Some(v),
            "connection" => connection = Some(v),
            "content-length" => {
                let l: usize = v.parse().map_err(|_| ReadErr::Bad(400, "bad Content-Length"))?;
                if clen.is_some_and(|p| p != l) {
                    return Err(ReadErr::Bad(400, "conflicting Content-Length"));
                }
                clen = Some(l);
            }
            "transfer-encoding" => return Err(ReadErr::Bad(501, "chunked bodies not supported")),
            "expect" => expect = v.eq_ignore_ascii_case("100-continue"),
            _ => {}
        }
    }
    let len = clen.unwrap_or(0);
    if len > sh.cfg.max_body {
        return Err(ReadErr::Bad(413, "body too large"));
    }
    if expect && len > 0 {
        w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").map_err(ReadErr::Io)?;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| {
        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
            ReadErr::Bad(408, "request timeout")
        } else {
            ReadErr::Io(e)
        }
    })?;
    Ok(Request { method, path, http11, host, connection, body })
}

fn write_reply(w: &mut TcpStream, r: &Reply, keep: bool) -> io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        505 => "HTTP Version Not Supported",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n{}\r\n",
        r.status,
        r.ctype,
        r.body.len(),
        if keep { "keep-alive" } else { "close" },
        if r.status == 405 { "Allow: POST\r\n" } else { "" },
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(r.body.as_bytes());
    w.write_all(&out)?;
    w.flush()
}

fn route(sh: &Shared, req: &Request) -> Reply {
    let path = req.path.split('?').next().unwrap_or("");
    if ![PATH_DEVICE, PATH_MEDIA, PATH_PTZ].contains(&path) {
        return Reply { status: 404, ctype: "text/plain", body: "not found".into() };
    }
    if req.method != "POST" {
        return Reply { status: 405, ctype: "text/plain", body: "POST only".into() };
    }
    let host = req
        .host
        .clone()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| sh.local.to_string());
    handle_soap(sh, &req.body, &host)
}

const SOAP_CT: &str = "application/soap+xml; charset=utf-8";

fn fault_reply(f: &Fault) -> Reply {
    Reply { status: f.http_status(), ctype: SOAP_CT, body: f.to_xml() }
}

/// Parse, authenticate, dispatch. Pure over `Shared`, so unit tests call it
/// without sockets. `host` is the request Host header (host[:port]).
pub fn handle_soap(sh: &Shared, body: &[u8], host: &str) -> Reply {
    let env = match xml::parse(body) {
        Ok(e) => e,
        Err(m) => return fault_reply(&Fault::sender(&["WellFormed"], format!("bad XML: {m}"))),
    };
    if env.name != "Envelope" {
        return fault_reply(&Fault::sender(&["WellFormed"], "not a SOAP Envelope"));
    }
    let op = match env.child("Body").and_then(|b| b.children.first()) {
        Some(o) => o,
        None => return fault_reply(&Fault::sender(&["WellFormed"], "empty SOAP Body")),
    };
    let creds = auth::extract(env.child("Header"));
    if let Err(f) = auth::authorize(&creds, &op.name, &*sh.auth) {
        log::info!("auth refused for {}: {}", op.name, f.reason);
        return fault_reply(&f);
    }
    let base = match &sh.cfg.advertised_host {
        // Advertised host with the actual listening port.
        Some(h) => format!("http://{}:{}", h, sh.local.port()),
        None => format!("http://{host}"),
    };
    let ctx = Ctx { sh, base };
    let res = match op.ns.as_str() {
        NS_TDS => device::handle(&ctx, op),
        NS_TRT => media::handle(&ctx, op),
        NS_TPTZ => ptz::handle(&ctx, op),
        other => Err(Fault::sender(
            &["ActionNotSupported"],
            format!("unknown service namespace {other:?}"),
        )),
    };
    match res {
        Ok(b) => Reply { status: 200, ctype: SOAP_CT, body: envelope(&b) },
        Err(f) => fault_reply(&f),
    }
}

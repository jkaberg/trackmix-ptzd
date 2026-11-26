use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ptzd_core::*;

use crate::http::handle_soap;
use crate::*;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    moving: AtomicBool,
}
impl PtzControl for Fake {
    fn relative_move(&self, l: Lens, x: f32, y: f32, s: Option<f32>) -> Result<(), PtzError> {
        self.calls.lock().unwrap().push(format!("rel {} {x} {y} {s:?}", l.token()));
        Ok(())
    }
    fn continuous_move(&self, x: f32, y: f32, t: Option<Duration>) -> Result<(), PtzError> {
        self.calls.lock().unwrap().push(format!("cont {x} {y} {t:?}"));
        Ok(())
    }
    fn absolute_move(&self, x: f32, y: f32, s: Option<f32>) -> Result<(), PtzError> {
        self.calls.lock().unwrap().push(format!("abs {x} {y} {s:?}"));
        Ok(())
    }
    fn stop(&self) -> Result<(), PtzError> {
        self.calls.lock().unwrap().push("stop".into());
        Ok(())
    }
    fn status(&self) -> PtzStatus {
        PtzStatus { pan: 0.25, tilt: -0.5, moving: self.moving.load(std::sync::atomic::Ordering::SeqCst) }
    }
    fn presets(&self) -> Vec<Preset> {
        vec![Preset { token: "1".into(), name: "home & <x>".into(), pan: 0.0, tilt: 0.0 }]
    }
    fn goto_preset(&self, t: &str) -> Result<(), PtzError> {
        if t == "1" {
            Ok(())
        } else {
            Err(PtzError::NoSuchPreset(t.into()))
        }
    }
    fn set_preset(&self, _n: Option<&str>, _t: Option<&str>) -> Result<String, PtzError> {
        Ok("2".into())
    }
    fn remove_preset(&self, _t: &str) -> Result<(), PtzError> {
        Ok(())
    }
}

struct Auth(AtomicUsize);
impl Authenticator for Auth {
    fn check(&self, u: &str, p: &str) -> AuthResult {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if (u, p) == ("admin", "test") {
            AuthResult::Ok
        } else {
            AuthResult::BadCredentials
        }
    }
}

fn shared(cfg: OnvifConfig) -> (Arc<Shared>, Arc<Fake>, Arc<Auth>) {
    let f = Arc::new(Fake::default());
    let a = Arc::new(Auth(AtomicUsize::new(0)));
    let sh = Arc::new(Shared {
        cfg,
        ptz: f.clone(),
        auth: a.clone(),
        local: "127.0.0.1:8899".parse().unwrap(),
        conns: AtomicUsize::new(0),
        per_ip: Default::default(),
        stop: AtomicBool::new(false),
    });
    (sh, f, a)
}

const WSSE: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0";

fn req(user: Option<(&str, &str, &str)>, body: &str) -> String {
    let hdr = match user {
        Some((u, p, ty)) => format!(
            "<s:Header><wsse:Security xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><wsse:UsernameToken><wsse:Username>{u}</wsse:Username><wsse:Password Type=\"{WSSE}#{ty}\">{p}</wsse:Password></wsse:UsernameToken></wsse:Security></s:Header>"
        ),
        None => String::new(),
    };
    format!(
        "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tds=\"{}\" xmlns:trt=\"{}\" xmlns:tptz=\"{}\" xmlns:tt=\"{}\">{hdr}<s:Body>{body}</s:Body></s:Envelope>",
        soap::NS_TDS, soap::NS_TRT, soap::NS_TPTZ, soap::NS_TT
    )
}

const OK: Option<(&str, &str, &str)> = Some(("admin", "test", "PasswordText"));

fn call(sh: &Shared, auth: Option<(&str, &str, &str)>, body: &str) -> http::Reply {
    handle_soap(sh, req(auth, body).as_bytes(), "10.0.0.5:8899")
}

#[test]
fn anonymous_only_for_two_ops() {
    let (sh, _, a) = shared(OnvifConfig::default());
    let r = call(&sh, None, "<tds:GetSystemDateAndTime/>");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("<tt:Year>"));
    let r = call(&sh, None, "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>");
    assert_eq!(r.status, 200);
    // XAddrs echo the Host header
    assert!(r.body.contains("http://10.0.0.5:8899/onvif/ptz_service"));
    assert!(r.body.contains("http://10.0.0.5:8899/onvif/media_service"));
    for body in [
        "<trt:GetProfiles/>",
        "<tds:GetServices/>",
        "<tds:GetDeviceInformation/>",
        "<tptz:GetServiceCapabilities/>",
        "<tptz:Stop><tptz:ProfileToken>wide</tptz:ProfileToken></tptz:Stop>",
    ] {
        let r = call(&sh, None, body);
        assert_eq!(r.status, 400, "{body}");
        assert!(r.body.contains("ter:NotAuthorized"));
    }
    assert_eq!(a.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn password_text_goes_to_authenticator_digest_is_refused() {
    let (sh, _, a) = shared(OnvifConfig::default());
    let r = call(&sh, OK, "<trt:GetProfiles/>");
    assert_eq!(r.status, 200);
    assert_eq!(a.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let r = call(&sh, Some(("admin", "nope", "PasswordText")), "<trt:GetProfiles/>");
    assert_eq!(r.status, 400);
    assert!(r.body.contains("bad username or password"));
    let r = call(&sh, Some(("admin", "AAAA", "PasswordDigest")), "<tds:GetCapabilities/>");
    assert_eq!(r.status, 400);
    assert!(r.body.contains("tls_insecure: true"));
    // digest never reaches the authenticator
    assert_eq!(a.0.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn profiles_wide_tele_fov_no_zoom() {
    let (sh, _, _) = shared(OnvifConfig::default());
    let r = call(&sh, OK, "<trt:GetProfiles/>");
    assert!(r.body.contains("token=\"wide\"") && r.body.contains("token=\"tele\""));
    assert!(r.body.contains("TranslationSpaceFov"));
    assert!(!r.body.contains("Zoom"));
    assert!(r.body.contains("<tt:VideoEncoderConfiguration"));
    let r = call(&sh, OK, "<tptz:GetConfigurationOptions><tptz:ConfigurationToken>ptzcfg_tele</tptz:ConfigurationToken></tptz:GetConfigurationOptions>");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("TranslationSpaceFov") && r.body.contains("VelocityGenericSpace"));
    assert!(!r.body.contains("Zoom"));
    let r = call(&sh, OK, "<tptz:GetConfigurationOptions><tptz:ConfigurationToken>x</tptz:ConfigurationToken></tptz:GetConfigurationOptions>");
    assert_eq!(r.status, 400);
    let r = call(&sh, OK, "<tptz:GetServiceCapabilities/>");
    assert!(r.body.contains("MoveStatus=\"true\""));
}

#[test]
fn status_always_has_position_pantilt() {
    let (sh, f, _) = shared(OnvifConfig::default());
    let body = "<tptz:GetStatus><tptz:ProfileToken>wide</tptz:ProfileToken></tptz:GetStatus>";
    let r = call(&sh, OK, body);
    assert!(r.body.contains("<tt:PanTilt x=\"0.25\" y=\"-0.5\""));
    assert!(r.body.contains("<tt:PanTilt>IDLE</tt:PanTilt>"));
    f.moving.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = call(&sh, OK, body);
    assert!(r.body.contains("<tt:PanTilt>MOVING</tt:PanTilt>"));
    let r = call(&sh, OK, "<tptz:GetStatus><tptz:ProfileToken>zzz</tptz:ProfileToken></tptz:GetStatus>");
    assert!(r.body.contains("ter:NoProfile"));
}

#[test]
fn moves_forward_args() {
    let (sh, f, _) = shared(OnvifConfig::default());
    let rel = |p: &str, x: &str, sp: &str, space: &str| {
        format!("<tptz:RelativeMove><tptz:ProfileToken>{p}</tptz:ProfileToken><tptz:Translation><tt:PanTilt x=\"{x}\" y=\"-0.5\" {space}/></tptz:Translation><tptz:Speed>{sp}</tptz:Speed></tptz:RelativeMove>")
    };
    let sp = "<tt:PanTilt x=\"1\" y=\"1\"/>";
    let r = call(&sh, OK, &rel("tele", "0.5", sp, "space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationSpaceFov\""));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(call(&sh, OK, &rel("wide", "0.5", "", "")).status, 200);
    // out of range, bad number, wrong space, bad profile
    assert_eq!(call(&sh, OK, &rel("wide", "1.5", sp, "")).status, 400);
    assert_eq!(call(&sh, OK, &rel("wide", "nan", sp, "")).status, 400);
    assert_eq!(call(&sh, OK, &rel("wide", "0", sp, "space=\"urn:other\"")).status, 400);
    assert_eq!(call(&sh, OK, &rel("nope", "0", sp, "")).status, 400);
    assert_eq!(call(&sh, OK, &rel("wide", "0", "<tt:PanTilt x=\"2\" y=\"0\"/>", "")).status, 400);

    let r = call(&sh, OK, "<tptz:ContinuousMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"-0.5\" y=\"0\"/></tptz:Velocity></tptz:ContinuousMove>");
    assert_eq!(r.status, 200);
    let r = call(&sh, OK, "<tptz:ContinuousMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"0\" y=\"0.5\"/></tptz:Velocity><tptz:Timeout>PT2.5S</tptz:Timeout></tptz:ContinuousMove>");
    assert_eq!(r.status, 200);
    // zoom-only velocity, bad timeout
    let r = call(&sh, OK, "<tptz:ContinuousMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Velocity><tt:Zoom x=\"1\"/></tptz:Velocity></tptz:ContinuousMove>");
    assert_eq!(r.status, 400);
    let r = call(&sh, OK, "<tptz:ContinuousMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"0\" y=\"0.5\"/></tptz:Velocity><tptz:Timeout>soon</tptz:Timeout></tptz:ContinuousMove>");
    assert_eq!(r.status, 400);

    // Frigate's Stop carries Zoom=true (onvif.py:550-557)
    let r = call(&sh, OK, "<tptz:Stop><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>");
    assert_eq!(r.status, 200);

    let r = call(&sh, OK, "<tptz:AbsoluteMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Position><tt:PanTilt x=\"0.1\" y=\"0.2\"/></tptz:Position></tptz:AbsoluteMove>");
    assert_eq!(r.status, 200);
    let r = call(&sh, OK, "<tptz:AbsoluteMove><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:Position><tt:Zoom x=\"0.1\"/></tptz:Position></tptz:AbsoluteMove>");
    assert_eq!(r.status, 400);

    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            "rel tele 0.5 -0.5 Some(1.0)",
            "rel wide 0.5 -0.5 None",
            "cont -0.5 0 None",
            "cont 0 0.5 Some(2.5s)",
            "stop",
            "abs 0.1 0.2 None",
        ]
    );
}

#[test]
fn tracker_profile_gates_relative_only() {
    let cfg = OnvifConfig { tracker_profile: Some(Lens::Wide), ..Default::default() };
    let (sh, f, _) = shared(cfg);
    let rel = |p: &str| format!("<tptz:RelativeMove><tptz:ProfileToken>{p}</tptz:ProfileToken><tptz:Translation><tt:PanTilt x=\"0.1\" y=\"0.1\"/></tptz:Translation></tptz:RelativeMove>");
    assert_eq!(call(&sh, OK, &rel("wide")).status, 200);
    assert_eq!(call(&sh, OK, &rel("tele")).status, 400);
    let r = call(&sh, OK, "<tptz:ContinuousMove><tptz:ProfileToken>tele</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"0.5\" y=\"0\"/></tptz:Velocity></tptz:ContinuousMove>");
    assert_eq!(r.status, 200);
    assert_eq!(f.calls.lock().unwrap().len(), 2);
}

#[test]
fn presets_and_errors() {
    let (sh, _, _) = shared(OnvifConfig::default());
    let r = call(&sh, OK, "<tptz:GetPresets><tptz:ProfileToken>wide</tptz:ProfileToken></tptz:GetPresets>");
    assert!(r.body.contains("<tptz:Preset token=\"1\"><tt:Name>home &amp; &lt;x&gt;</tt:Name>"));
    let g = |t: &str| format!("<tptz:GotoPreset><tptz:ProfileToken>wide</tptz:ProfileToken><tptz:PresetToken>{t}</tptz:PresetToken></tptz:GotoPreset>");
    assert_eq!(call(&sh, OK, &g("1")).status, 200);
    let r = call(&sh, OK, &g("9"));
    assert_eq!(r.status, 400);
    assert!(r.body.contains("ter:NoToken"));
    let r = call(&sh, OK, "<tptz:Bogus/>");
    assert!(r.body.contains("ter:ActionNotSupported"));
    assert_eq!(handle_soap(&sh, b"<not xml", "h").status, 400);
    assert_eq!(handle_soap(&sh, b"<a/>", "h").status, 400);
    let r = call(&sh, OK, "<trt:GetStreamUri><trt:StreamSetup/><trt:ProfileToken>tele</trt:ProfileToken></trt:GetStreamUri>");
    assert!(r.body.contains("h264Preview_02_main"));
}

#[test]
fn advertised_host_overrides_host_header() {
    let cfg = OnvifConfig { advertised_host: Some("cam.lan".into()), ..Default::default() };
    let (sh, _, _) = shared(cfg);
    let r = call(&sh, None, "<tds:GetCapabilities/>");
    assert!(r.body.contains("http://cam.lan:8899/onvif/media_service"));
}

// ---- socket level

fn start() -> (ServerHandle, std::net::SocketAddr) {
    start_with(|_| {})
}

fn start_with(tweak: impl FnOnce(&mut OnvifConfig)) -> (ServerHandle, std::net::SocketAddr) {
    let mut cfg = OnvifConfig {
        bind_addr: "127.0.0.1".parse().unwrap(),
        port: 0,
        idle_timeout: Duration::from_millis(400),
        request_timeout: Duration::from_millis(500),
        max_body: 4096,
        ..Default::default()
    };
    tweak(&mut cfg);
    let s = OnvifServer::bind(cfg, Arc::new(Fake::default()), Arc::new(Auth(AtomicUsize::new(0)))).unwrap();
    let a = s.local_addr();
    (s.spawn().unwrap(), a)
}

fn post(path: &str, body: &str, extra: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    )
}

/// Read one response (Content-Length framed); returns (status line, body).
fn read_resp(s: &mut TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        buf.push(b[0]);
    }
    let head = String::from_utf8(buf).unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).unwrap();
    (head.lines().next().unwrap().to_string(), String::from_utf8(body).unwrap())
}

#[test]
fn keep_alive_and_limits_over_sockets() {
    let (h, addr) = start();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let body = req(None, "<tds:GetSystemDateAndTime/>");
    for _ in 0..3 {
        s.write_all(post("/onvif/device_service", &body, "").as_bytes()).unwrap();
        let (st, b) = read_resp(&mut s);
        assert!(st.contains("200"), "{st}");
        assert!(b.contains("GetSystemDateAndTimeResponse"));
    }
    // a method/path error does not break framing
    s.write_all(b"GET /onvif/device_service HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    assert!(read_resp(&mut s).0.contains("405"));
    s.write_all(post("/nope", &body, "").as_bytes()).unwrap();
    assert!(read_resp(&mut s).0.contains("404"));
    drop(s);

    // oversize body
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: 999999\r\n\r\n").unwrap();
    assert!(read_resp(&mut s).0.contains("413"));

    // chunked refused
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    assert!(read_resp(&mut s).0.contains("501"));

    // Connection: close honoured
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(post("/onvif/device_service", &body, "Connection: close\r\n").as_bytes()).unwrap();
    let _ = read_resp(&mut s);
    let mut rest = Vec::new();
    assert_eq!(s.read_to_end(&mut rest).unwrap(), 0);

    // stalled body times out instead of hanging a thread
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"POST /onvif/device_service HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nabc").unwrap();
    assert!(read_resp(&mut s).0.contains("408"));

    // idle keep-alive connection is closed by the server
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut rest = Vec::new();
    assert_eq!(s.read_to_end(&mut rest).unwrap(), 0);
    h.shutdown();
}

// ---- review 1 R5

/// The reviewer's probe: one byte every few seconds (scaled down). Per-read
/// timeouts never fire; only an absolute request deadline ends it.
#[test]
fn slowloris_drip_is_cut_off_by_absolute_deadline() {
    let (h, addr) = start(); // request_timeout 500 ms
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let t0 = Instant::now();
    let mut ended = false;
    for b in b"POST /onvif/device_service HTTP/1.1\r\nX-A: ".iter().cycle().take(40) {
        if s.write_all(&[*b]).is_err() {
            ended = true;
            break;
        }
        let mut buf = [0u8; 256];
        // Ok = 408 reply or EOF; Err = our 100 ms read timeout, keep dripping.
        if s.read(&mut buf).is_ok() {
            ended = true;
            break;
        }
    }
    assert!(ended, "server still waiting after {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_millis(2000), "{:?}", t0.elapsed());
    h.shutdown();
}

#[test]
fn per_ip_connection_cap() {
    let (h, addr) = start_with(|c| c.max_connections_per_ip = 2);
    let _a = TcpStream::connect(addr).unwrap();
    let _b = TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert!(read_resp(&mut c).0.contains("503"));
    // a slot frees when a connection closes
    drop(_a);
    std::thread::sleep(Duration::from_millis(100));
    let mut d = TcpStream::connect(addr).unwrap();
    d.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let body = req(None, "<tds:GetSystemDateAndTime/>");
    d.write_all(post("/onvif/device_service", &body, "").as_bytes()).unwrap();
    assert!(read_resp(&mut d).0.contains("200"));
    h.shutdown();
}

#[test]
fn default_per_ip_cap_fits_two_frigate_cameras() {
    let c = OnvifConfig::default();
    // review 2 B1: 2 cameras x 3 services x (1 live + 1 keep-alive) = 12
    assert!(c.max_connections_per_ip >= 16);
    assert!(c.max_connections_per_ip < c.max_connections);
}

//! Fake Reolink API for tests and `ptzd --mock`. One account, the cmds the
//! daemon uses, toggleable self-movers and limits, expirable token lease.
//! Request shapes follow the web UI JS (see api.rs / state.rs); malformed
//! bodies get rspCode -4 like a real "wrong input". Not a model of the real
//! camera's lockout policy (UNPROVEN) beyond the optional `set_lockout`.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct MockPatrol {
    pub id: i64,
    pub enable: bool,
    pub running: bool,
}

struct M {
    user: String,
    password: String,
    lease: u64,
    next_token: u32,
    tokens: HashSet<String>,
    smart_track: bool,
    guard: bool,
    patrols: Vec<MockPatrol>,
    tattern: bool,
    limit_l: i64,
    limit_r: i64,
    logins: u32,
    failed_logins: u32,
    lockout: Option<(u32, u64)>,
    locked: bool,
    log: Vec<String>,
}

pub struct MockCamera {
    addr: SocketAddr,
    m: Arc<Mutex<M>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

fn lock(m: &Mutex<M>) -> MutexGuard<'_, M> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MockCamera {
    /// `bind` like "127.0.0.1:0".
    pub fn start(bind: &str, user: &str, password: &str) -> std::io::Result<MockCamera> {
        let l = TcpListener::bind(bind)?;
        let addr = l.local_addr()?;
        l.set_nonblocking(true)?;
        let m = Arc::new(Mutex::new(M {
            user: user.into(),
            password: password.into(),
            lease: 3600,
            next_token: 1,
            tokens: HashSet::new(),
            smart_track: false,
            guard: false,
            patrols: vec![],
            tattern: false,
            limit_l: -1,
            limit_r: -1,
            logins: 0,
            failed_logins: 0,
            lockout: None,
            locked: false,
            log: vec![],
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (m2, s2) = (m.clone(), stop.clone());
        let join = std::thread::Builder::new().name("mock-reolink".into()).spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                match l.accept() {
                    Ok((c, _)) => {
                        let m3 = m2.clone();
                        let _ = std::thread::spawn(move || {
                            let _ = serve(c, &m3);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        })?;
        Ok(MockCamera { addr, m, stop, join: Some(join) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn set_smart_track(&self, on: bool) {
        lock(&self.m).smart_track = on;
    }
    pub fn smart_track(&self) -> bool {
        lock(&self.m).smart_track
    }
    pub fn set_guard(&self, on: bool) {
        lock(&self.m).guard = on;
    }
    pub fn guard(&self) -> bool {
        lock(&self.m).guard
    }
    pub fn set_patrol(&self, id: i64, enable: bool, running: bool) {
        let mut m = lock(&self.m);
        m.patrols.retain(|p| p.id != id);
        m.patrols.push(MockPatrol { id, enable, running });
    }
    pub fn patrols(&self) -> Vec<MockPatrol> {
        lock(&self.m).patrols.clone()
    }
    /// Pattern flag: shown nowhere in the API (SetPtzTattern body unknown).
    pub fn set_tattern(&self, on: bool) {
        lock(&self.m).tattern = on;
    }
    pub fn set_limits(&self, left: i64, right: i64) {
        let mut m = lock(&self.m);
        m.limit_l = left;
        m.limit_r = right;
    }
    pub fn set_lease(&self, secs: u64) {
        lock(&self.m).lease = secs;
    }
    /// Invalidates every issued token (camera answers rspCode -6).
    pub fn expire_tokens(&self) {
        lock(&self.m).tokens.clear();
    }
    /// After `failures` consecutive bad logins every Login gets an
    /// `auth_warning_info.unlock_time` error. `clear_lockout` ends it.
    pub fn set_lockout(&self, failures: u32, unlock_secs: u64) {
        lock(&self.m).lockout = Some((failures, unlock_secs));
    }
    pub fn clear_lockout(&self) {
        let mut m = lock(&self.m);
        m.lockout = None;
        m.locked = false;
        m.failed_logins = 0;
    }
    /// Tokens currently valid (not logged out, not expired).
    pub fn live_tokens(&self) -> usize {
        lock(&self.m).tokens.len()
    }
    pub fn login_count(&self) -> u32 {
        lock(&self.m).logins
    }
    pub fn failed_login_count(&self) -> u32 {
        lock(&self.m).failed_logins
    }
    /// Commands received, in order, e.g. "SetAiCfg".
    pub fn calls(&self) -> Vec<String> {
        lock(&self.m).log.clone()
    }
    pub fn clear_calls(&self) {
        lock(&self.m).log.clear();
    }
}

impl Drop for MockCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn serve(mut c: TcpStream, m: &Mutex<M>) -> std::io::Result<()> {
    c.set_nonblocking(false)?;
    c.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let (head_end, clen) = loop {
        let n = c.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
            let cl = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                .unwrap_or(0);
            break (p + 4, cl);
        }
    };
    while buf.len() < head_end + clen {
        let n = c.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let target = head.split_whitespace().nth(1).unwrap_or("").to_string();
    let body = String::from_utf8_lossy(&buf[head_end..(head_end + clen).min(buf.len())]).to_string();
    let reply = handle(m, &target, &body);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    c.write_all(resp.as_bytes())
}

fn err(cmd: &str, code: i64, detail: &str) -> Value {
    json!({"cmd": cmd, "code": 1, "error": {"rspCode": code, "detail": detail}})
}

fn ok(cmd: &str, value: Value) -> Value {
    json!({"cmd": cmd, "code": 0, "value": value})
}

fn query(target: &str, key: &str) -> Option<String> {
    let q = target.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')).map(String::from))
}

fn handle(m: &Mutex<M>, target: &str, body: &str) -> String {
    let mut m = lock(m);
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(body) else {
        return json!([err("Unknown", -4, "bad json")]).to_string();
    };
    let Some(item) = items.first() else {
        return json!([err("Unknown", -4, "empty")]).to_string();
    };
    let cmd = item.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    let param = item.get("param").cloned().unwrap_or(Value::Null);
    if query(target, "cmd").as_deref() != Some(cmd.as_str()) {
        return json!([err(&cmd, -4, "cmd mismatch")]).to_string();
    }
    m.log.push(cmd.clone());
    let r = if cmd == "Login" {
        login(&mut m, &param)
    } else {
        let tok = query(target, "token").unwrap_or_default();
        if !m.tokens.contains(&tok) {
            err(&cmd, -6, "please login first")
        } else if cmd == "Logout" {
            m.tokens.remove(&tok);
            ok(&cmd, json!({"rspCode": 200}))
        } else {
            dispatch(&mut m, &cmd, &param)
        }
    };
    json!([r]).to_string()
}

fn login(m: &mut M, p: &Value) -> Value {
    let u = p.get("User");
    let name = u.and_then(|u| u.get("userName")).and_then(Value::as_str);
    let pw = u.and_then(|u| u.get("password")).and_then(Value::as_str);
    let (Some(name), Some(pw)) = (name, pw) else { return err("Login", -4, "wrong input") };
    if m.locked {
        let secs = m.lockout.map_or(0, |l| l.1);
        return json!({"cmd":"Login","code":1,"error":{"rspCode":-7,"detail":"locked",
            "auth_warning_info":{"unlock_time":secs,"remain_times":0}}});
    }
    if name == m.user && pw == m.password {
        m.failed_logins = 0;
        m.logins += 1;
        let t = format!("tok{:08x}", m.next_token);
        m.next_token += 1;
        m.tokens.insert(t.clone());
        ok("Login", json!({"Token": {"leaseTime": m.lease, "name": t}}))
    } else {
        m.failed_logins += 1;
        if let Some((n, _)) = m.lockout {
            if m.failed_logins >= n {
                m.locked = true;
            }
        }
        err("Login", -7, "password or user name error")
    }
}

fn dispatch(m: &mut M, cmd: &str, p: &Value) -> Value {
    let bad = || err(cmd, -4, "wrong input");
    let b = |v: Option<&Value>| v.and_then(Value::as_i64).map(|x| x != 0);
    match cmd {
        "GetAiCfg" => {
            ok(cmd, json!({"bSmartTrack": m.smart_track as i32, "aiTrack": 1, "aiStopBackTime": 5, "aiDisappearBackTime": 10,
                "trackType": {"people": 1}, "AiDetectType": {"people": 1}, "channel": 0}))
        }
        "SetAiCfg" => match (b(p.get("bSmartTrack")), p.get("channel")) {
            (Some(v), Some(_)) => {
                m.smart_track = v;
                ok(cmd, json!({"rspCode": 200}))
            }
            _ => bad(),
        },
        "GetPtzGuard" => ok(cmd, json!({"PtzGuard": {"channel": 0, "benable": m.guard as i32, "timeout": 60, "bexistPos": 1, "imgName": ""}})),
        "SetPtzGuard" => {
            let g = p.get("PtzGuard");
            let cs = g.and_then(|g| g.get("cmdStr")).and_then(Value::as_str);
            match (b(g.and_then(|g| g.get("benable"))), cs) {
                (Some(v), Some("setPos")) => {
                    m.guard = v;
                    ok(cmd, json!({"rspCode": 200}))
                }
                // Like the camera: accepted, no effect.
                (Some(_), Some("toPos")) => ok(cmd, json!({"rspCode": 200})),
                _ => bad(),
            }
        }
        "GetPtzPatrol" => {
            let a: Vec<Value> = m
                .patrols
                .iter()
                .map(|x| json!({"id": x.id, "enable": x.enable as i32, "running": x.running as i32, "name": format!("p{}", x.id),
                    "preset": [{"id": 1, "dwellTime": 5, "speed": 3}]}))
                .collect();
            ok(cmd, json!({"PtzPatrol": a}))
        }
        "SetPtzPatrol" => {
            let q = p.get("PtzPatrol");
            let id = q.and_then(|q| q.get("id")).and_then(Value::as_i64);
            let en = b(q.and_then(|q| q.get("enable")));
            let run = b(q.and_then(|q| q.get("running")));
            match (id, en, run) {
                (Some(id), Some(en), Some(run)) => {
                    if let Some(x) = m.patrols.iter_mut().find(|x| x.id == id) {
                        x.enable = en;
                        x.running = run;
                    }
                    ok(cmd, json!({"rspCode": 200}))
                }
                _ => bad(),
            }
        }
        "PtzCtrl" => match p.get("op").and_then(Value::as_str) {
            Some("StopPatrol") => {
                for x in &mut m.patrols {
                    x.running = false;
                }
                ok(cmd, json!({"rspCode": 200}))
            }
            _ => bad(),
        },
        "GetPtzTraceSection" => ok(
            cmd,
            json!({"PtzTraceSection": {"channel": 0, "LimitLeft": m.limit_l, "LimitRight": m.limit_r, "ImgLeftName": "", "ImgRightName": ""}}),
        ),
        // SetPtzTattern: body unknown (UNPROVEN), deliberately not implemented.
        _ => err(cmd, -3, "unsupported"),
    }
}

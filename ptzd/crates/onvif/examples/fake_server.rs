//! In-memory fake for conformance testing. Usage:
//!   fake_server [PORT] [--tracker wide|tele]
//! PORT defaults to 0 (ephemeral). Prints `LISTENING <addr>` then one
//! `CALL ...` line per PtzControl call (stdout, flushed). Auth: admin/test.
//! Move model: a move marks the head MOVING for MOVE_MS, then IDLE; a
//! preset goto does the same. Positions are clamped to -1..1.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ptzd_core::*;
use ptzd_onvif::{OnvifConfig, OnvifServer};

/// Fake dwell only, not a hardware value.
const MOVE_MS: u64 = 120;

struct State {
    pan: f32,
    tilt: f32,
    until: Option<Instant>,
    cont: bool,
    presets: Vec<Preset>,
}

struct Fake(Mutex<State>);

fn log(s: String) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "CALL {s}");
    let _ = o.flush();
}

fn lock(m: &Mutex<State>) -> Result<std::sync::MutexGuard<'_, State>, PtzError> {
    m.lock().map_err(|_| PtzError::NotReady("poisoned".into()))
}

impl PtzControl for Fake {
    fn relative_move(&self, l: Lens, x: f32, y: f32, s: Option<f32>) -> Result<(), PtzError> {
        log(format!("relative_move {} {x} {y} {s:?}", l.token()));
        let mut st = lock(&self.0)?;
        // 1.0 = half the FOV; the fake just uses 0.5 range units per 1.0.
        st.pan = (st.pan + x * 0.5).clamp(-1.0, 1.0);
        st.tilt = (st.tilt + y * 0.5).clamp(-1.0, 1.0);
        st.until = Some(Instant::now() + Duration::from_millis(MOVE_MS));
        Ok(())
    }
    fn continuous_move(&self, x: f32, y: f32, t: Option<Duration>) -> Result<(), PtzError> {
        log(format!("continuous_move {x} {y} {t:?}"));
        let mut st = lock(&self.0)?;
        st.cont = true;
        Ok(())
    }
    fn absolute_move(&self, x: f32, y: f32, s: Option<f32>) -> Result<(), PtzError> {
        log(format!("absolute_move {x} {y} {s:?}"));
        let mut st = lock(&self.0)?;
        st.pan = x;
        st.tilt = y;
        st.until = Some(Instant::now() + Duration::from_millis(MOVE_MS));
        Ok(())
    }
    fn stop(&self) -> Result<(), PtzError> {
        log("stop".into());
        let mut st = lock(&self.0)?;
        st.cont = false;
        st.until = None;
        Ok(())
    }
    fn status(&self) -> PtzStatus {
        match self.0.lock() {
            Ok(st) => PtzStatus {
                pan: st.pan,
                tilt: st.tilt,
                moving: st.cont || st.until.is_some_and(|u| Instant::now() < u),
            },
            Err(_) => PtzStatus { pan: 0.0, tilt: 0.0, moving: false },
        }
    }
    fn presets(&self) -> Vec<Preset> {
        self.0.lock().map(|s| s.presets.clone()).unwrap_or_default()
    }
    fn goto_preset(&self, token: &str) -> Result<(), PtzError> {
        log(format!("goto_preset {token}"));
        let mut st = lock(&self.0)?;
        let p = st
            .presets
            .iter()
            .find(|p| p.token == token)
            .cloned()
            .ok_or_else(|| PtzError::NoSuchPreset(token.into()))?;
        st.pan = p.pan;
        st.tilt = p.tilt;
        st.until = Some(Instant::now() + Duration::from_millis(MOVE_MS));
        Ok(())
    }
    fn set_preset(&self, name: Option<&str>, token: Option<&str>) -> Result<String, PtzError> {
        log(format!("set_preset {name:?} {token:?}"));
        let mut st = lock(&self.0)?;
        let (pan, tilt) = (st.pan, st.tilt);
        let tok = match token {
            Some(t) => t.to_string(),
            None => format!("{}", st.presets.len() + 1),
        };
        let name = name.map(str::to_string).unwrap_or_else(|| format!("preset {tok}"));
        st.presets.retain(|p| p.token != tok);
        st.presets.push(Preset { token: tok.clone(), name, pan, tilt });
        Ok(tok)
    }
    fn remove_preset(&self, token: &str) -> Result<(), PtzError> {
        log(format!("remove_preset {token}"));
        lock(&self.0)?.presets.retain(|p| p.token != token);
        Ok(())
    }
}

struct TestAuth;
impl Authenticator for TestAuth {
    fn check(&self, user: &str, password: &str) -> AuthResult {
        if user == "admin" && password == "test" {
            AuthResult::Ok
        } else {
            AuthResult::BadCredentials
        }
    }
}

fn main() {
    let mut cfg = OnvifConfig {
        bind_addr: std::net::IpAddr::from([127, 0, 0, 1]),
        port: 0,
        ..Default::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tracker" => cfg.tracker_profile = args.next().and_then(|t| Lens::from_token(&t)),
            p => match p.parse() {
                Ok(p) => cfg.port = p,
                Err(_) => {
                    eprintln!("usage: fake_server [PORT] [--tracker wide|tele]");
                    std::process::exit(2);
                }
            },
        }
    }
    let ptz = Arc::new(Fake(Mutex::new(State {
        pan: 0.0,
        tilt: 0.0,
        until: None,
        cont: false,
        // Plan §3: the daemon ships a `home` preset; Frigate's return_preset default is "home".
        presets: vec![Preset { token: "1".into(), name: "home".into(), pan: 0.0, tilt: 0.0 }],
    })));
    let server = match OnvifServer::bind(cfg, ptz, Arc::new(TestAuth)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bind failed: {e}");
            std::process::exit(1);
        }
    };
    println!("LISTENING {}", server.local_addr());
    let _ = std::io::stdout().flush();
    server.serve();
}

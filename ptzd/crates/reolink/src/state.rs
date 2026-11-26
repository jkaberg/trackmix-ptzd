//! `Reolink`: the `ReolinkState` implementation (plan §9 D1, D4).
//!
//! Cold start: read-only parse of 0_ptz.cfg / ai.cfg. After the first
//! successful login: disable every self-mover through the API and read the
//! pan limits; repeat every `recheck` (10 min, plan §9 D1). API state is
//! authoritative over the files.

use crate::api::{ApiError, ApiSession, LoginOk};
use crate::cfg::{self, CfgMap};
use ptzd_core::{PanLimits, ReolinkState, SelfMovers};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::api::ApiClient;

#[derive(Clone, Debug)]
pub struct ReolinkConfig {
    /// UNPROVEN layout, see cfg.rs. Configurable.
    pub ptz_cfg: PathBuf,
    pub ai_cfg: PathBuf,
    /// Reolink channel index (UNPROVEN, T9: that channel 0 is the PTZ channel).
    pub channel: u32,
    /// Plan §9 D1/D4: every 10 min.
    pub recheck: Duration,
    /// API snapshot older than this no longer overrides the files' "on" flags
    /// (own fail-closed choice: files OR stale API).
    pub stale_after: Duration,
}

impl Default for ReolinkConfig {
    fn default() -> Self {
        ReolinkConfig {
            ptz_cfg: PathBuf::from("/mnt/para/0_ptz.cfg"),
            ai_cfg: PathBuf::from("/mnt/para/ai.cfg"),
            channel: 0,
            recheck: Duration::from_secs(600),
            stale_after: Duration::from_secs(1800),
        }
    }
}

#[derive(Default)]
struct FileState {
    limits: PanLimits,
    movers: Vec<String>,
}

struct ApiSnap {
    limits: PanLimits,
    movers: Vec<String>,
    at: Instant,
}

#[derive(Default)]
struct State {
    files: FileState,
    api: Option<ApiSnap>,
    last_err: Option<String>,
}

pub struct Reolink {
    api: Arc<ApiClient>,
    cfg: ReolinkConfig,
    state: Mutex<State>,
    /// Credentials live here in RAM only, for re-login (plan §9 Credentials).
    session: Mutex<Option<ApiSession>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|x| x != 0),
        Some(Value::String(s)) => s.trim().parse::<i64>().is_ok_and(|x| x != 0),
        _ => false,
    }
}

fn limit_of(v: Option<&Value>) -> Option<u32> {
    // -1 = unset (research/ptzd-hardware.md §7).
    v.and_then(Value::as_i64).filter(|x| *x >= 0).and_then(|x| u32::try_from(x).ok())
}

impl Reolink {
    pub fn new(api: Arc<ApiClient>, cfg: ReolinkConfig) -> Reolink {
        let r = Reolink { api, cfg, state: Mutex::new(State::default()), session: Mutex::new(None) };
        r.reload_files();
        r
    }

    /// Cold-start read (also re-run on every recheck tick). Never writes.
    /// A missing file counts as "nothing configured"; any other read error
    /// fails closed with an "unreadable" entry that blocks moves.
    pub fn reload_files(&self) {
        let mut fs = FileState::default();
        match read_file(&self.cfg.ptz_cfg) {
            Ok(Some(t)) => {
                let m = CfgMap::parse(&t);
                fs.limits = cfg::pan_limits_from_map(&m);
                fs.movers.extend(cfg::ptz_movers(&m));
            }
            Ok(None) => log::warn!("reolink: {} not found, assuming no limits or self-movers", self.cfg.ptz_cfg.display()),
            Err(e) => {
                log::error!("reolink: {}: {e}", self.cfg.ptz_cfg.display());
                fs.movers.push("unreadable 0_ptz.cfg".into());
            }
        }
        match read_file(&self.cfg.ai_cfg) {
            Ok(Some(t)) => {
                if cfg::ai_smart_track_on(&CfgMap::parse(&t)) {
                    fs.movers.push("smart_track".into());
                }
            }
            Ok(None) => log::warn!("reolink: {} not found, assuming smart_track off", self.cfg.ai_cfg.display()),
            Err(e) => {
                log::error!("reolink: {}: {e}", self.cfg.ai_cfg.display());
                fs.movers.push("unreadable ai.cfg".into());
            }
        }
        if !fs.movers.is_empty() {
            log::warn!("reolink: self-movers on per cfg files: {:?}", fs.movers);
        }
        lock(&self.state).files = fs;
    }

    /// Called by `ReolinkAuth` after a fresh successful Login. Adopts the
    /// credentials only if there is no healthy session yet (so a second
    /// account does not replace a working one), then runs the D1/D4 sweep.
    pub fn on_authenticated(&self, user: &str, password: &str, login: &LoginOk) {
        self.adopt_login(user, password, login);
    }

    /// Like `on_authenticated`, but returns whether the session took
    /// ownership of `login.token`. When false the caller must log it out (R9).
    pub fn adopt_login(&self, user: &str, password: &str, login: &LoginOk) -> bool {
        {
            let mut s = lock(&self.session);
            let healthy = s.is_some() && lock(&self.state).last_err.is_none();
            if healthy {
                return false;
            }
            if let Some(old) = s.take() {
                old.logout(); // replaced session: do not leak its token (R9)
            }
            *s = Some(ApiSession::new(self.api.clone(), user, password, login));
        }
        if let Err(e) = self.refresh() {
            log::error!("reolink: initial sweep failed: {e}");
        }
        true
    }

    /// Disable self-movers and read limits now. Err if no session or API failure.
    pub fn refresh(&self) -> Result<(), String> {
        let mut guard = lock(&self.session);
        let sess = guard.as_mut().ok_or_else(|| "no session yet".to_string())?;
        match self.sweep(sess) {
            Ok(snap) => {
                let mut st = lock(&self.state);
                st.api = Some(snap);
                st.last_err = None;
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e}");
                if matches!(e, ApiError::BadCredentials) {
                    // Password changed on the camera: drop the credentials.
                    if let Some(old) = guard.take() {
                        old.logout();
                    }
                }
                lock(&self.state).last_err = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Logs the held session out. Call at shutdown, after the recheck thread
    /// stopped: the camera keeps a session for its whole lease (1 h), so each
    /// restart would leave one behind (measured 2026-11-25).
    pub fn end_session(&self) {
        if let Some(s) = lock(&self.session).take() {
            s.logout();
        }
    }

    /// One recheck tick: reload files, then refresh if a session exists.
    pub fn tick(&self) {
        self.reload_files();
        if lock(&self.session).is_some() {
            if let Err(e) = self.refresh() {
                log::warn!("reolink: recheck failed: {e}");
            }
        }
    }

    /// Starts the periodic recheck thread. Stops when the handle is dropped.
    pub fn spawn_recheck(self: &Arc<Self>) -> RecheckHandle {
        let (tx, rx) = mpsc::channel::<()>();
        let me = self.clone();
        let every = self.cfg.recheck;
        let h = std::thread::Builder::new().name("reolink-recheck".into()).spawn(move || {
            // recv_timeout: Err(Timeout) = tick, anything else = stop.
            while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(every) {
                me.tick();
            }
        });
        RecheckHandle { stop: Some(tx), join: h.ok() }
    }

    // ------------------------------------------------------------ API sweep

    fn sweep(&self, s: &mut ApiSession) -> Result<ApiSnap, ApiError> {
        let ch = self.cfg.channel;
        let mut on: Vec<String> = Vec::new();

        // Auto-tracking. JS setAiTrackCfg: SetAiCfg {bSmartTrack,aiTrack,trackType,
        // AiDetectType,aiDisappearBackTime,aiStopBackTime,channel} (accountLogin.js ~749675).
        // Read first, write only if on (spares flash on every recheck).
        let ai = s.call("GetAiCfg", 0, json!({"channel": ch}))?;
        if truthy(ai.get("bSmartTrack")) {
            let mut body = serde_json::Map::new();
            for k in ["aiTrack", "trackType", "AiDetectType", "aiDisappearBackTime", "aiStopBackTime"] {
                if let Some(v) = ai.get(k) {
                    body.insert(k.into(), v.clone());
                }
            }
            body.insert("bSmartTrack".into(), json!(0));
            body.insert("channel".into(), json!(ch));
            s.call("SetAiCfg", 0, Value::Object(body))?;
            let again = s.call("GetAiCfg", 0, json!({"channel": ch}))?;
            if truthy(again.get("bSmartTrack")) {
                on.push("smart_track".into());
            }
        }

        // Guard. JS setGuard: SetPtzGuard {PtzGuard:{channel,cmdStr,benable,timeout,
        // bSaveCurrentPos,bexistPos,imgName}}. cmdStr must be "setPos": the camera
        // answers 200 to "toPos" and ignores benable (measured 2026-11-25, T9).
        // bSaveCurrentPos 0 keeps the stored guard position.
        let g = s.call("GetPtzGuard", 0, json!({"channel": ch}))?;
        let gv = g.get("PtzGuard").cloned().unwrap_or(Value::Null);
        if truthy(gv.get("benable")) {
            let body = json!({"PtzGuard": {
                "channel": ch, "cmdStr": "setPos", "benable": 0,
                "timeout": gv.get("timeout").cloned().unwrap_or(json!(0)),
                "bSaveCurrentPos": 0,
                "bexistPos": gv.get("bexistPos").cloned().unwrap_or(json!(1)),
                "imgName": gv.get("imgName").cloned().unwrap_or(json!("")),
            }});
            s.call("SetPtzGuard", 0, body)?;
            let again = s.call("GetPtzGuard", 0, json!({"channel": ch}))?;
            if truthy(again.get("PtzGuard").and_then(|p| p.get("benable"))) {
                on.push("guard".into());
            }
        }

        // Patrols. JS GetPtzPatrol -> value.PtzPatrol[] {id,enable,running,name,preset[]};
        // setCruise: SetPtzPatrol {PtzPatrol:{channel,enable,id,name,running,preset}} (~739695);
        // stop: PtzCtrl {channel,op:"StopPatrol"} (~740511).
        let p = s.call("GetPtzPatrol", 0, json!({"channel": ch}))?;
        let patrols = p.get("PtzPatrol").and_then(Value::as_array).cloned().unwrap_or_default();
        let active: Vec<&Value> =
            patrols.iter().filter(|x| truthy(x.get("enable")) || truthy(x.get("running"))).collect();
        if !active.is_empty() {
            s.call("PtzCtrl", 0, json!({"channel": ch, "op": "StopPatrol"}))?;
            for pt in &active {
                let body = json!({"PtzPatrol": {
                    "channel": ch,
                    "enable": 0,
                    "id": pt.get("id").cloned().unwrap_or(json!(0)),
                    "name": pt.get("name").cloned().unwrap_or(json!("")),
                    "running": 0,
                    "preset": pt.get("preset").cloned().unwrap_or(json!([])),
                }});
                s.call("SetPtzPatrol", 0, body)?;
            }
            let again = s.call("GetPtzPatrol", 0, json!({"channel": ch}))?;
            for x in again.get("PtzPatrol").and_then(Value::as_array).into_iter().flatten() {
                if truthy(x.get("enable")) || truthy(x.get("running")) {
                    on.push(format!("patrol {}", x.get("id").and_then(Value::as_i64).unwrap_or(0)));
                }
            }
        }

        // SetPtzTattern: UNPROVEN, SKIPPED. The cmd exists in cgiserver (0xde335) but
        // its JSON body is not in the web UI JS (research/ptzd-hardware.md §7). Pattern
        // state therefore stays file-derived (see self_movers).

        // Limits. JS getSmartTrackLimit: GetPtzTraceSection {PtzTraceSection:{channel}}
        // -> value.PtzTraceSection.{LimitLeft,LimitRight}, -1 = unset (~750902).
        let t = s.call("GetPtzTraceSection", 0, json!({"PtzTraceSection": {"channel": ch}}))?;
        let ts = t.get("PtzTraceSection").cloned().unwrap_or(Value::Null);
        let limits = PanLimits { left: limit_of(ts.get("LimitLeft")), right: limit_of(ts.get("LimitRight")) };

        on.sort();
        on.dedup();
        Ok(ApiSnap { limits, movers: on, at: Instant::now() })
    }
}

fn read_file(p: &PathBuf) -> Result<Option<String>, std::io::Error> {
    match std::fs::read(p) {
        Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl ReolinkState for Reolink {
    fn pan_limits(&self) -> PanLimits {
        let st = lock(&self.state);
        match &st.api {
            Some(a) => a.limits,
            None => st.files.limits,
        }
    }

    fn self_movers(&self) -> SelfMovers {
        let st = lock(&self.state);
        let mut v = match &st.api {
            Some(a) => {
                let mut v = a.movers.clone();
                // Patterns cannot be read or cleared via API (UNPROVEN), keep the file view.
                v.extend(st.files.movers.iter().filter(|m| m.starts_with("pattern")).cloned());
                if a.at.elapsed() > self.cfg.stale_after {
                    v.extend(st.files.movers.iter().cloned());
                }
                v
            }
            None => st.files.movers.clone(),
        };
        v.sort();
        v.dedup();
        SelfMovers { enabled: v }
    }
}

pub struct RecheckHandle {
    stop: Option<Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for RecheckHandle {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

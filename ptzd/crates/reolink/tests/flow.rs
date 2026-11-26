use ptzd_core::{AuthResult, Authenticator, PanLimits, ReolinkState};
use ptzd_reolink::mock::MockCamera;
use ptzd_reolink::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn api(cam: &MockCamera) -> Arc<ApiClient> {
    Arc::new(ApiClient::new(Arc::new(HttpTransport::new(cam.addr().to_string())), 0))
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ptzd-reolink-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg_in(d: &Path) -> ReolinkConfig {
    ReolinkConfig { ptz_cfg: d.join("0_ptz.cfg"), ai_cfg: d.join("ai.cfg"), ..ReolinkConfig::default() }
}

fn fast_auth() -> AuthConfig {
    AuthConfig { backoff_base: Duration::from_millis(300), backoff_max: Duration::from_millis(600), ..AuthConfig::default() }
}

#[test]
fn login_cache_and_wrong_password() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let auth = ReolinkAuth::new(api(&cam), None, AuthConfig::default());
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    assert_eq!(cam.login_count(), 1);
    // cache hit: no second Login
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    assert_eq!(cam.login_count(), 1);
    assert_eq!(auth.check("admin", "nope"), AuthResult::BadCredentials);
    assert_eq!(auth.check("root", "pw1"), AuthResult::BadCredentials);
}

#[test]
fn unreachable_is_unavailable() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let a = api(&cam);
    drop(cam);
    let auth = ReolinkAuth::new(a, None, AuthConfig::default());
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Unavailable);
}

#[test]
fn rate_limit_after_failures_then_recovers() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let auth = ReolinkAuth::new(api(&cam), None, fast_auth());
    for _ in 0..3 {
        assert_eq!(auth.check("admin", "bad"), AuthResult::BadCredentials);
    }
    assert_eq!(cam.failed_login_count(), 3);
    // blocked: even the right password fails closed and the API is not touched
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Unavailable);
    assert_eq!(auth.check("admin", "bad"), AuthResult::Unavailable);
    assert_eq!(cam.failed_login_count(), 3);
    assert_eq!(cam.login_count(), 0);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
}

#[test]
fn camera_lockout_is_unavailable() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    cam.set_lockout(1, 60);
    let auth = ReolinkAuth::new(api(&cam), None, fast_auth());
    assert_eq!(auth.check("admin", "bad"), AuthResult::BadCredentials);
    assert_eq!(auth.check("admin2", "x"), AuthResult::Unavailable);
}

#[test]
fn token_expiry_triggers_relogin() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let a = api(&cam);
    let l = a.login("admin", "pw1").unwrap();
    let mut s = ApiSession::new(a.clone(), "admin", "pw1", &l);
    assert!(s.call("GetPtzTraceSection", 0, serde_json::json!({"PtzTraceSection": {"channel": 0}})).is_ok());
    assert_eq!(cam.login_count(), 1);
    cam.expire_tokens();
    assert!(s.call("GetPtzTraceSection", 0, serde_json::json!({"PtzTraceSection": {"channel": 0}})).is_ok());
    assert_eq!(cam.login_count(), 2);
    // raw client sees Expired with a stale token
    assert_eq!(a.call("stale", "GetAiCfg", 0, serde_json::json!({"channel": 0})), Err(ApiError::Expired));
}

#[test]
fn self_mover_disable_round_trip_and_limits() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    cam.set_smart_track(true);
    cam.set_guard(true);
    cam.set_patrol(1, true, true);
    cam.set_patrol(2, false, false);
    cam.set_limits(100, 2500);
    let d = tmpdir("rt");
    // assumed-format fixtures: cold start sees everything on
    std::fs::write(d.join("ai.cfg"), "<ai_cfg><smart_track>1</smart_track></ai_cfg>").unwrap();
    std::fs::write(
        d.join("0_ptz.cfg"),
        "<ptz_config_t><p_limit_l>90</p_limit_l><p_limit_r>-1</p_limit_r><grdenable>1</grdenable><cc1_bval>1</cc1_bval></ptz_config_t>",
    )
    .unwrap();
    let a = api(&cam);
    let st = Arc::new(Reolink::new(a.clone(), cfg_in(&d)));
    assert_eq!(st.pan_limits(), PanLimits { left: Some(90), right: None });
    assert_eq!(st.self_movers().enabled, vec!["guard", "patrol 1", "smart_track"]);

    let auth = ReolinkAuth::new(a, Some(st.clone()), AuthConfig::default());
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);

    assert!(!cam.smart_track());
    assert!(!cam.guard());
    assert!(cam.patrols().iter().all(|p| !p.enable && !p.running));
    let calls = cam.calls();
    for c in ["SetAiCfg", "SetPtzGuard", "SetPtzPatrol", "PtzCtrl", "GetPtzTraceSection"] {
        assert!(calls.iter().any(|x| x == c), "missing {c}: {calls:?}");
    }
    assert!(!calls.iter().any(|x| x == "SetPtzTattern"));
    // API is authoritative over the (stale) files
    assert!(st.self_movers().enabled.is_empty());
    assert_eq!(st.pan_limits(), PanLimits { left: Some(100), right: Some(2500) });

    // Nothing on: a recheck writes nothing.
    cam.clear_calls();
    st.refresh().unwrap();
    assert!(cam.calls().iter().all(|c| c.starts_with("Get")), "{:?}", cam.calls());

    // The user re-enables tracking in the app; the next tick turns it off again.
    cam.set_smart_track(true);
    cam.set_limits(-1, -1);
    st.tick();
    assert!(!cam.smart_track());
    assert_eq!(st.pan_limits(), PanLimits { left: None, right: None });
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn recheck_thread_runs() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let d = tmpdir("thr");
    let mut c = cfg_in(&d);
    c.recheck = Duration::from_millis(100);
    let a = api(&cam);
    let st = Arc::new(Reolink::new(a.clone(), c));
    let auth = ReolinkAuth::new(a, Some(st.clone()), AuthConfig::default());
    let h = st.spawn_recheck();
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    cam.set_guard(true);
    std::thread::sleep(Duration::from_millis(450));
    assert!(!cam.guard());
    drop(h);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn cold_start_files_missing_and_unreadable() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let d = tmpdir("cold");
    let st = Reolink::new(api(&cam), cfg_in(&d));
    assert!(st.self_movers().enabled.is_empty());
    assert_eq!(st.pan_limits(), PanLimits::default());
    // a directory where a file should be: fail closed
    std::fs::create_dir(d.join("0_ptz.cfg")).unwrap();
    st.reload_files();
    assert_eq!(st.self_movers().enabled, vec!["unreadable 0_ptz.cfg"]);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn pattern_stays_file_derived() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let d = tmpdir("pat");
    std::fs::write(d.join("0_ptz.cfg"), "<ptz_config_t><tc0_bstart>1</tc0_bstart></ptz_config_t>").unwrap();
    let a = api(&cam);
    let st = Arc::new(Reolink::new(a.clone(), cfg_in(&d)));
    let auth = ReolinkAuth::new(a, Some(st.clone()), AuthConfig::default());
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    assert_eq!(st.self_movers().enabled, vec!["pattern 0"]);
    let _ = std::fs::remove_dir_all(d);
}

// ---- review 1 R6: cached logins survive a bad-password flood, accounting is atomic

#[test]
fn cached_login_survives_bad_password_flood_and_ttl_refreshes() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let cfg = AuthConfig { cache_ttl: Duration::from_millis(600), ..AuthConfig::default() };
    let auth = ReolinkAuth::new(api(&cam), None, cfg);
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    for _ in 0..10 {
        let _ = auth.check("admin", "wrong");
    }
    // user and global are blocked now, but the cached pair still passes
    assert_eq!(auth.check("admin", "wrong"), AuthResult::Unavailable);
    assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    // each use refreshes the TTL: 4 x 300 ms > 600 ms, still no new Login
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
    }
    assert_eq!(cam.login_count(), 1);
}

#[test]
fn concurrent_bad_logins_are_bounded_by_the_limit() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let auth = Arc::new(ReolinkAuth::new(api(&cam), None, AuthConfig::default()));
    let hs: Vec<_> = (0..16)
        .map(|_| {
            let a = auth.clone();
            std::thread::spawn(move || a.check("admin", "bad"))
        })
        .collect();
    for h in hs {
        let _ = h.join();
    }
    // limit is 3 per user; without an atomic reservation all 16 reach the camera
    assert!(cam.failed_login_count() <= 3, "{} bad logins reached the camera", cam.failed_login_count());
}

// ---- review 1 R9: abandoned tokens are logged out, one live token per user

#[test]
fn verification_logins_leave_no_live_token() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let cfg = AuthConfig { cache_ttl: Duration::from_millis(30), ..AuthConfig::default() };
    let auth = ReolinkAuth::new(api(&cam), None, cfg);
    for _ in 0..4 {
        assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
        std::thread::sleep(Duration::from_millis(60));
    }
    assert!(cam.login_count() >= 3);
    assert_eq!(cam.live_tokens(), 0);
}

#[test]
fn session_holds_at_most_one_token() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let d = tmpdir("tok");
    let a = api(&cam);
    let st = Arc::new(Reolink::new(a.clone(), cfg_in(&d)));
    let cfg = AuthConfig { cache_ttl: Duration::from_millis(30), ..AuthConfig::default() };
    let auth = ReolinkAuth::new(a, Some(st.clone()), cfg);
    for _ in 0..4 {
        assert_eq!(auth.check("admin", "pw1"), AuthResult::Ok);
        std::thread::sleep(Duration::from_millis(60));
    }
    assert_eq!(cam.live_tokens(), 1);
    // lease relogin inside the session releases the old token too
    cam.set_lease(0);
    cam.expire_tokens();
    st.refresh().unwrap();
    assert!(cam.live_tokens() <= 1);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn replaced_session_logs_out_old_token() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let a = api(&cam);
    let d = tmpdir("repl");
    let st = Reolink::new(a.clone(), cfg_in(&d));
    let l1 = a.login("admin", "pw1").unwrap();
    assert!(st.adopt_login("admin", "pw1", &l1));
    cam.expire_tokens(); // next sweep fails -> session unhealthy
    let _ = st.refresh();
    let l2 = a.login("admin", "pw1").unwrap();
    st.adopt_login("admin", "pw1", &l2);
    assert!(cam.calls().iter().any(|c| c == "Logout"));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn end_session_logs_out() {
    let cam = MockCamera::start("127.0.0.1:0", "admin", "pw1").unwrap();
    let a = api(&cam);
    let d = tmpdir("end");
    let st = Reolink::new(a.clone(), cfg_in(&d));
    let l = a.login("admin", "pw1").unwrap();
    assert!(st.adopt_login("admin", "pw1", &l));
    assert_eq!(cam.live_tokens(), 1);
    st.end_session();
    assert_eq!(cam.live_tokens(), 0);
    // no way back in without a new client login
    let n = cam.login_count();
    assert!(st.refresh().is_err());
    assert_eq!(cam.login_count(), n);
    st.end_session(); // twice is harmless
    let _ = std::fs::remove_dir_all(d);
}

struct NoLease;
impl Transport for NoLease {
    fn post(&self, _: &str, _: &str) -> Result<String, TransportError> {
        Ok(r#"[{"cmd":"Login","code":0,"value":{"Token":{"name":"t"}}}]"#.into())
    }
}

#[test]
fn m8_default_lease_is_configurable() {
    let c = ApiClient::new(Arc::new(NoLease), 0);
    assert_eq!(c.login("u", "p").unwrap().lease, Duration::from_secs(3600));
    let c = ApiClient::new(Arc::new(NoLease), 0).with_default_lease(Duration::from_secs(90));
    assert_eq!(c.login("u", "p").unwrap().lease, Duration::from_secs(90));
}

#[test]
fn m8_http_timeout_is_configurable() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // accepts, never answers
    let t = HttpTransport::new(l.local_addr().unwrap().to_string()).with_timeout(Duration::from_millis(200));
    let t0 = std::time::Instant::now();
    assert!(t.post("/x", "{}").is_err());
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn concurrent_first_logins_of_one_user_all_succeed() {
    // Two Frigate cameras (wide + tele) authenticate with the same user at once.
    let cam = MockCamera::start("127.0.0.1:0", "admin", "test").unwrap();
    let auth = Arc::new(ReolinkAuth::new(api(&cam), None, fast_auth()));
    let hs: Vec<_> = (0..6)
        .map(|_| {
            let a = auth.clone();
            std::thread::spawn(move || a.check("admin", "test"))
        })
        .collect();
    for h in hs {
        assert_eq!(h.join().unwrap(), AuthResult::Ok);
    }
}

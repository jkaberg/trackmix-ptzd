//! `key = value` config, hand-parsed. Missing keys keep the crate defaults,
//! unknown keys are an error. Every UNPROVEN crate value stays a key.
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use ptzd_core::Lens;
use ptzd_motion::MotionConfig;
use ptzd_onvif::OnvifConfig;
use ptzd_reolink::{AuthConfig, ReolinkConfig};

pub const DEFAULT_PATH: &str = "/mnt/para/ptzd/ptzd.conf";

pub struct Conf {
    pub log_level: log::LevelFilter,
    pub runtime_dir: PathBuf,
    /// `None` in mock mode means "temp dir".
    pub state_dir: Option<PathBuf>,
    pub lens_dev: PathBuf,
    pub soft_margin: u32,
    pub reolink_api: String,
    pub reolink: ReolinkConfig,
    pub auth: AuthConfig,
    pub onvif: OnvifConfig,
    /// True when the config set onvif_bind / onvif_port (mock mode overrides otherwise).
    pub onvif_bind_set: bool,
    pub onvif_port_set: bool,
    pub motion: MotionConfig,
    pub mock_user: String,
    pub mock_password: String,
    pub mock_smart_track: bool,
    pub mock_limits: Option<(i64, i64)>,
    /// Mock only: the sim's `device` starts boot homing this long after start.
    pub mock_device_home_delay: Option<Duration>,
    /// Lease assumed when Login has no leaseTime (M8).
    pub api_default_lease: Duration,
    /// Reolink API transport timeout (M8).
    pub api_timeout: Duration,
}

/// Directories ptzd must never take over (it chmods its runtime dir 0700).
pub fn check_runtime_dir(p: &std::path::Path) -> Result<(), String> {
    let comps: Vec<_> = p.components().collect();
    let shared = ["/", "/tmp", "/var/tmp", "/dev", "/mnt", "/mnt/para", "/etc", "/run", "/var"];
    let norm: PathBuf = comps.iter().collect();
    if !p.is_absolute() || shared.iter().any(|s| norm == std::path::Path::new(s)) {
        return Err(format!("runtime_dir {} must be a dedicated absolute subdirectory such as /tmp/ptzd", p.display()));
    }
    Ok(())
}

struct Kv(HashMap<String, (String, usize)>);

impl Kv {
    fn take<T: FromStr>(&mut self, key: &str) -> Result<Option<T>, String> {
        match self.0.remove(key) {
            None => Ok(None),
            Some((v, line)) => v.parse().map(Some).map_err(|_| format!("line {line}: bad value for {key}: {v:?}")),
        }
    }
    fn set<T: FromStr>(&mut self, key: &str, dst: &mut T) -> Result<(), String> {
        if let Some(v) = self.take(key)? {
            *dst = v;
        }
        Ok(())
    }
    fn flag(&mut self, key: &str, dst: &mut bool) -> Result<(), String> {
        if let Some((v, line)) = self.0.remove(key) {
            *dst = match v.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => return Err(format!("line {line}: bad bool for {key}: {v:?}")),
            };
        }
        Ok(())
    }
    fn secs(&mut self, key: &str, dst: &mut Duration) -> Result<(), String> {
        if let Some(v) = self.take::<f64>(key)? {
            if !(v >= 0.0 && v.is_finite()) {
                return Err(format!("bad value for {key}: {v}"));
            }
            *dst = Duration::from_secs_f64(v);
        }
        Ok(())
    }
    fn string(&mut self, key: &str, dst: &mut String) {
        if let Some((v, _)) = self.0.remove(key) {
            *dst = v;
        }
    }
    fn path(&mut self, key: &str, dst: &mut PathBuf) {
        if let Some((v, _)) = self.0.remove(key) {
            *dst = PathBuf::from(v);
        }
    }
}

fn parse_lens(s: &str) -> Result<Option<Lens>, String> {
    match s {
        "none" | "both" | "" => Ok(None),
        t => Lens::from_token(t).map(Some).ok_or_else(|| format!("tracker_profile must be wide|tele|none, got {t:?}")),
    }
}

pub fn parse(text: &str) -> Result<Conf, String> {
    let mut kv = Kv(HashMap::new());
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let (k, v) = l.split_once('=').ok_or_else(|| format!("line {}: expected key = value", i + 1))?;
        let v = v.trim().trim_matches('"');
        if kv.0.insert(k.trim().to_string(), (v.to_string(), i + 1)).is_some() {
            return Err(format!("line {}: duplicate key {}", i + 1, k.trim()));
        }
    }
    build(kv)
}

fn build(mut kv: Kv) -> Result<Conf, String> {
    let mut c = Conf {
        log_level: log::LevelFilter::Info,
        runtime_dir: PathBuf::from("/tmp/ptzd"),
        state_dir: None,
        lens_dev: PathBuf::from("/dev/lens_zfi"),
        soft_margin: 20,
        // UNPROVEN (T9): which local endpoint serves the Reolink API on the camera.
        reolink_api: "127.0.0.1:80".into(),
        reolink: ReolinkConfig::default(),
        auth: AuthConfig::default(),
        onvif: OnvifConfig::default(),
        onvif_bind_set: false,
        onvif_port_set: false,
        motion: MotionConfig::new("/mnt/para/ptzd/state"),
        mock_user: "admin".into(),
        mock_password: "test".into(),
        mock_smart_track: false,
        mock_limits: None,
        // The sim stands in for `device`'s boot homing (review 2 B2).
        mock_device_home_delay: Some(Duration::from_millis(200)),
        api_default_lease: Duration::from_secs(ptzd_reolink::api::DEFAULT_LEASE_SECS),
        api_timeout: ptzd_reolink::http::DEFAULT_TIMEOUT,
    };
    if let Some(v) = kv.take::<String>("log_level")? {
        c.log_level = v.parse().map_err(|_| format!("bad log_level {v:?}"))?;
    }
    kv.path("runtime_dir", &mut c.runtime_dir);
    c.state_dir = kv.take::<String>("state_dir")?.map(PathBuf::from);
    kv.path("lens_dev", &mut c.lens_dev);
    kv.set("soft_margin", &mut c.soft_margin)?;
    c.motion.soft_margin = c.soft_margin;

    kv.string("reolink_api", &mut c.reolink_api);
    kv.set("reolink_channel", &mut c.reolink.channel)?;
    kv.path("ptz_cfg", &mut c.reolink.ptz_cfg);
    kv.path("ai_cfg", &mut c.reolink.ai_cfg);
    kv.secs("reolink_recheck_secs", &mut c.reolink.recheck)?;
    kv.secs("reolink_stale_secs", &mut c.reolink.stale_after)?;
    kv.secs("reolink_default_lease_secs", &mut c.api_default_lease)?;
    kv.secs("reolink_http_timeout_secs", &mut c.api_timeout)?;
    kv.secs("auth_cache_ttl_secs", &mut c.auth.cache_ttl)?;
    kv.set("auth_user_fail_limit", &mut c.auth.user_fail_limit)?;
    kv.set("auth_global_fail_limit", &mut c.auth.global_fail_limit)?;
    kv.secs("auth_backoff_base_secs", &mut c.auth.backoff_base)?;
    kv.secs("auth_backoff_max_secs", &mut c.auth.backoff_max)?;

    if let Some(v) = kv.take::<IpAddr>("onvif_bind")? {
        c.onvif.bind_addr = v;
        c.onvif_bind_set = true;
    }
    if let Some(v) = kv.take::<u16>("onvif_port")? {
        c.onvif.port = v;
        c.onvif_port_set = true;
    }
    c.onvif.advertised_host = kv.take("onvif_advertised_host")?.or(c.onvif.advertised_host);
    kv.string("wide_rtsp", &mut c.onvif.wide.rtsp_url);
    kv.string("tele_rtsp", &mut c.onvif.tele.rtsp_url);
    if let Some(v) = kv.take::<String>("tracker_profile")? {
        c.onvif.tracker_profile = parse_lens(&v)?;
    }
    kv.set("onvif_max_connections", &mut c.onvif.max_connections)?;
    kv.set("onvif_max_connections_per_ip", &mut c.onvif.max_connections_per_ip)?;
    kv.secs("onvif_idle_timeout_secs", &mut c.onvif.idle_timeout)?;
    kv.secs("onvif_request_timeout_secs", &mut c.onvif.request_timeout)?;

    let m = &mut c.motion;
    kv.set("wide_fov_h", &mut m.wide.h)?;
    kv.set("wide_fov_v", &mut m.wide.v)?;
    kv.set("tele_fov_h", &mut m.tele.h)?;
    kv.set("tele_fov_v", &mut m.tele.v)?;
    kv.set("pan_steps_per_deg", &mut m.pan_steps_per_deg)?;
    kv.set("tilt_steps_per_deg", &mut m.tilt_steps_per_deg)?;
    kv.set("tilt_level_step", &mut m.tilt_level_step)?;
    kv.flag("tilt_reversed", &mut m.tilt_reversed)?;
    kv.flag("pan_reversed", &mut m.pan_reversed)?;
    kv.set("fast_speed", &mut m.fast_speed)?;
    kv.set("mid_speed", &mut m.mid_speed)?;
    kv.flag("pan_left_is_low", &mut m.pan_left_is_low)?;
    if let Some(v) = kv.take::<String>("rehome_hour_utc")? {
        m.rehome_hour_utc = match v.as_str() {
            "off" | "none" => None,
            h => match h.parse::<u32>() {
                Ok(h) if h < 24 => Some(h),
                _ => return Err(format!("rehome_hour_utc must be 0..23 or off, got {h:?}")),
            },
        };
    }
    kv.secs("rehome_idle_secs", &mut m.rehome_idle)?;
    kv.set("backlash_pan", &mut m.backlash_pan)?;
    kv.set("backlash_tilt", &mut m.backlash_tilt)?;
    kv.secs("continuous_timeout_secs", &mut m.continuous_timeout)?;
    kv.secs("home_timeout_secs", &mut m.home_timeout)?;
    kv.secs("dwell_secs", &mut m.dwell)?;
    kv.secs("poll_secs", &mut m.poll)?;
    kv.secs("idle_tick_secs", &mut m.idle_tick)?;
    kv.secs("settle_secs", &mut m.settle)?;
    kv.set("move_slack", &mut m.move_slack)?;
    kv.secs("move_margin_secs", &mut m.move_margin)?;
    kv.secs("home_idle_timeout_secs", &mut m.home_idle_timeout)?;
    kv.secs("device_home_wait_secs", &mut m.device_home_wait)?;
    kv.set("adopt_stable_polls", &mut m.adopt_stable_polls)?;
    kv.secs("adopt_stable_interval_secs", &mut m.adopt_stable_interval)?;
    if let Some(v) = kv.take::<String>("step_rates")? {
        let r: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if r.len() != 4 || r.iter().any(|x| !(*x > 0.0 && x.is_finite())) {
            return Err(format!("step_rates needs 4 positive steps/s (classes <=15,16..31,32..47,>=48), got {v:?}"));
        }
        m.step_rates = [r[0], r[1], r[2], r[3]];
    }
    kv.secs("persist_debounce_secs", &mut m.persist_debounce)?;

    if let Some(v) = kv.take::<String>("mock_device_home_delay_secs")? {
        c.mock_device_home_delay = match v.as_str() {
            "off" | "none" => None,
            s => match s.parse::<f64>() {
                Ok(x) if x >= 0.0 && x.is_finite() => Some(Duration::from_secs_f64(x)),
                _ => return Err(format!("mock_device_home_delay_secs must be seconds or off, got {s:?}")),
            },
        };
    }
    kv.string("mock_user", &mut c.mock_user);
    kv.string("mock_password", &mut c.mock_password);
    kv.flag("mock_smart_track", &mut c.mock_smart_track)?;
    let (l, r) = (kv.take::<i64>("mock_limit_left")?, kv.take::<i64>("mock_limit_right")?);
    if l.is_some() || r.is_some() {
        c.mock_limits = Some((l.unwrap_or(-1), r.unwrap_or(-1)));
    }

    if let Some(k) = kv.0.keys().min() {
        let line = kv.0.get(k).map_or(0, |x| x.1);
        return Err(format!("line {line}: unknown key {k}"));
    }
    c.motion.validate()?;
    check_runtime_dir(&c.runtime_dir)?;
    if c.api_timeout.is_zero() {
        return Err("reolink_http_timeout_secs must be > 0".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_override() {
        let c = parse("# x\nfast_speed = 40\nrehome_hour_utc = 3\n").ok().unwrap();
        assert_eq!(c.motion.fast_speed, 40);
        assert_eq!(c.motion.rehome_hour_utc, Some(3));
        assert_eq!(c.onvif.port, 8899);
    }
    #[test]
    fn motion_keys() {
        let c = parse("settle_secs = 0.3\nstep_rates = 1,2,3,4\nmove_slack = 3\nmove_margin_secs = 1\nhome_idle_timeout_secs = 9\nonvif_max_connections_per_ip = 2\n")
            .ok()
            .unwrap();
        assert_eq!(c.motion.step_rates, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(c.motion.settle, Duration::from_millis(300));
        assert_eq!(c.motion.home_idle_timeout, Duration::from_secs(9));
        assert_eq!(c.onvif.max_connections_per_ip, 2);
        assert!(parse("step_rates = 1,2").is_err());
    }
    #[test]
    fn unknown_key_is_error() {
        assert!(parse("bogus = 1").err().unwrap_or_default().contains("unknown key bogus"));
        assert!(parse("fast_speed = x").is_err());
        assert!(parse("fast_speed = 1\nfast_speed = 2").is_err());
    }
    #[test]
    fn validation_and_new_keys() {
        for bad in [
            "wide_fov_h = nan",
            "tele_fov_v = 0",
            "wide_fov_v = -5",
            "pan_steps_per_deg = inf",
            "tilt_steps_per_deg = 0",
            "move_slack = -1",
            "backlash_tilt = 1600",
            "soft_margin = 0",
            "runtime_dir = /tmp",
            "runtime_dir = /tmp/",
            "runtime_dir = relative",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let c = parse("backlash_tilt = 64\nruntime_dir = /tmp/ptzd\ndevice_home_wait_secs = 7\nreolink_default_lease_secs = 90\nreolink_http_timeout_secs = 2\nmock_device_home_delay_secs = off\n").ok().unwrap();
        assert_eq!(c.motion.device_home_wait, Duration::from_secs(7));
        assert_eq!(c.api_default_lease, Duration::from_secs(90));
        assert_eq!(c.api_timeout, Duration::from_secs(2));
        assert!(c.mock_device_home_delay.is_none());
        assert_eq!(parse("").ok().unwrap().motion.device_home_wait, Duration::from_secs(180));
        assert_eq!(parse("").ok().unwrap().onvif.max_connections_per_ip, 16);
    }
}

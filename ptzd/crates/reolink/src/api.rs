//! Reolink JSON API client: `POST /cgi-bin/api.cgi?cmd=X[&token=T]`, body
//! `[{"cmd":X,"action":N,"param":{..}}]`.
//!
//! Evidence (web UI JS, decompressed `www/js_CGI-API.js`, `js_accountLogin.*.js`):
//! - URL and body shape: `js_CGI-API.js` sendCommand (`cgi-bin/api.cgi?cmd=`,
//!   `?token=`), accountLogin `sendCommandLogin` (`[{cmd:"Login",action:0,param}]`).
//! - Expired session: rspCode -6, -27, -502 (CGI-API.js "Login expired"), -21 (batch path).
//! - Login failure: rspCode -7, -105, -502 (CGI-API.js, cmd Login code 1);
//!   `error.auth_warning_info.{unlock_time,remain_times}` = lockout info.
//! - Response: `[{"cmd","code":0,"value":{..}}]` or `code:1,"error":{"rspCode","detail"}`.
//!
//! UNPROVEN (T9): the web UI itself logs in with a Version:1 Digest handshake and
//! then AES-transforms later bodies (`infoHandler.transform`). We use the plain
//! `{"User":{"userName","password"}}` Login from research/ptzd-hardware.md §8, which is
//! the public form other clients use; whether fw 5429 still accepts it is untested.
//! The plain form returns `{"Token":{"leaseTime":N,"name":TOKEN}}` (UNPROVEN, not in JS).

use crate::http::Transport;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Transport failed (connect refused, timeout, HTTP error).
    Unreachable(String),
    /// Login rejected: wrong user or password.
    BadCredentials,
    /// Camera-side lockout (auth_warning_info.unlock_time > 0).
    Locked { unlock_secs: u64 },
    /// Token missing or expired.
    Expired,
    /// Any other camera error.
    Rejected { code: i64, detail: String },
    /// Reply not in the expected shape.
    Protocol(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone)]
pub struct LoginOk {
    pub token: String,
    pub lease: Duration,
}

/// Lease used when the reply carries no leaseTime. UNPROVEN (T9): 3600 s is
/// what community clients report for the plain Login; not seen in fw JS.
/// Default of the `reolink_default_lease_secs` config key.
pub const DEFAULT_LEASE_SECS: u64 = 3600;

/// We treat a token as dead slightly before the camera does (own margin).
fn usable_lease(lease: Duration) -> Duration {
    lease.saturating_sub((lease / 10).min(Duration::from_secs(60)))
}

pub struct ApiClient {
    transport: Arc<dyn Transport>,
    channel: u32,
    default_lease: Duration,
}

impl ApiClient {
    pub fn new(transport: Arc<dyn Transport>, channel: u32) -> Self {
        ApiClient { transport, channel, default_lease: Duration::from_secs(DEFAULT_LEASE_SECS) }
    }

    /// Lease assumed when Login carries no leaseTime (config key, M8).
    pub fn with_default_lease(mut self, lease: Duration) -> Self {
        self.default_lease = lease;
        self
    }

    pub fn channel(&self) -> u32 {
        self.channel
    }

    pub fn login(&self, user: &str, password: &str) -> Result<LoginOk, ApiError> {
        let param = json!({"User": {"userName": user, "password": password}});
        let v = self.send("/cgi-bin/api.cgi?cmd=Login", "Login", 0, param, true)?;
        let tok = v.get("Token").ok_or_else(|| ApiError::Protocol("no Token".into()))?;
        let name = tok
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::Protocol("no token name".into()))?;
        let lease = tok.get("leaseTime").and_then(Value::as_u64).map_or(self.default_lease, Duration::from_secs);
        Ok(LoginOk { token: name.to_string(), lease })
    }

    /// Ends a session token. Evidence: accountLogin.1bbf1a46.js (web UI,
    /// logout handler) `sendCommand("Logout",{},0)`, sent with `?token=`.
    /// Whether the camera then frees the slot is UNPROVEN (T9).
    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        self.call(token, "Logout", 0, json!({})).map(|_| ())
    }

    /// Returns the reply's `value`. `action`: 0 normal, 1 = also return ranges
    /// (CGI-API.js third argument of sendCommand).
    pub fn call(&self, token: &str, cmd: &str, action: u8, param: Value) -> Result<Value, ApiError> {
        let path = format!("/cgi-bin/api.cgi?cmd={}&token={}", pct(cmd), pct(token));
        self.send(&path, cmd, action, param, false)
    }

    fn send(&self, path: &str, cmd: &str, action: u8, param: Value, login: bool) -> Result<Value, ApiError> {
        let body = json!([{"cmd": cmd, "action": action, "param": param}]).to_string();
        let text = self.transport.post(path, &body).map_err(|e| ApiError::Unreachable(e.0))?;
        parse_reply(&text, cmd, login)
    }
}

fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn parse_reply(text: &str, cmd: &str, login: bool) -> Result<Value, ApiError> {
    let v: Value = serde_json::from_str(text).map_err(|e| ApiError::Protocol(format!("json: {e}")))?;
    let first = v
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| ApiError::Protocol("reply not a non-empty array".into()))?;
    let code = first.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == 0 {
        if first.get("cmd").and_then(Value::as_str) != Some(cmd) {
            return Err(ApiError::Protocol("cmd mismatch".into()));
        }
        return Ok(first.get("value").cloned().unwrap_or(Value::Null));
    }
    let err = first.get("error");
    let rsp = err.and_then(|e| e.get("rspCode")).and_then(Value::as_i64).unwrap_or(0);
    let detail = err.and_then(|e| e.get("detail")).and_then(Value::as_str).unwrap_or("").to_string();
    if login {
        let unlock = err
            .and_then(|e| e.get("auth_warning_info"))
            .and_then(|w| w.get("unlock_time"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unlock > 0 {
            return Err(ApiError::Locked { unlock_secs: unlock });
        }
        if matches!(rsp, -7 | -105 | -502) {
            return Err(ApiError::BadCredentials);
        }
    } else if matches!(rsp, -6 | -27 | -502 | -21) {
        return Err(ApiError::Expired);
    }
    Err(ApiError::Rejected { code: rsp, detail })
}

/// A logged-in session. Holds the credentials in RAM only, so it can
/// re-login when the lease ends or the camera answers "expired".
pub struct ApiSession {
    api: Arc<ApiClient>,
    user: String,
    password: String,
    token: String,
    expires: Instant,
}

impl ApiSession {
    pub fn new(api: Arc<ApiClient>, user: &str, password: &str, login: &LoginOk) -> Self {
        ApiSession {
            api,
            user: user.to_string(),
            password: password.to_string(),
            token: login.token.clone(),
            expires: Instant::now() + usable_lease(login.lease),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// Best-effort Logout of the held token (R9). Errors only logged.
    pub fn logout(&self) {
        if let Err(e) = self.api.logout(&self.token) {
            log::debug!("reolink: logout failed: {e}");
        }
    }

    fn relogin(&mut self) -> Result<(), ApiError> {
        // Release the old token first so a user never holds two (R9).
        self.logout();
        let l = self.api.login(&self.user, &self.password)?;
        log::info!("reolink: re-login ok");
        self.token = l.token;
        self.expires = Instant::now() + usable_lease(l.lease);
        Ok(())
    }

    pub fn call(&mut self, cmd: &str, action: u8, param: Value) -> Result<Value, ApiError> {
        if Instant::now() >= self.expires {
            self.relogin()?;
        }
        match self.api.call(&self.token, cmd, action, param.clone()) {
            Err(ApiError::Expired) => {
                self.relogin()?;
                self.api.call(&self.token, cmd, action, param)
            }
            other => other,
        }
    }
}

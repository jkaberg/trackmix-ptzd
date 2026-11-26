//! `ReolinkAuth`: credential passthrough (plan §9 Credentials).
//! Authentication = a Reolink `Login` with the presented user/password.
//! Cache: (user, SHA-256(password)) in RAM only, never on disk.
//!
//! Policy (review 1 R6/R9):
//! - A valid cached (user, hash) always succeeds without touching the API, is
//!   never evicted by someone else's wrong password, and its TTL refreshes on
//!   use. It still ends at the token lease (hard cap), so a password changed on
//!   the camera is noticed within one lease.
//! - Only uncached attempts hit the per-user and global backoff. Failure
//!   accounting is done under one lock together with a per-user in-flight
//!   reservation: counts are taken before the API call (refunded if the
//!   camera was merely unreachable) and only one uncached login per user runs
//!   at a time, so there is no check-then-act race.
//! - Remaining DoS surface (LAN host, no credentials): wrong passwords for
//!   user X block X's *uncached* logins (backoff, up to `backoff_max`), and
//!   `global_fail_limit` wrong passwords block everyone's uncached logins.
//!   Frigate stays logged in through its cache, but when its cache entry ends
//!   (lease, restart, password change) and a block is active, it gets
//!   `Unavailable` until the block lapses. Per-IP attribution is not possible
//!   here (the Authenticator trait has no peer address).
//! - R9: a Login used only to verify credentials is logged out at once unless
//!   `Reolink::adopt_login` takes the token as the daemon session, so at most
//!   one live token per user is held (the session's).

use crate::api::{ApiClient, ApiError, LoginOk};
use crate::sha256::sha256;
use crate::state::Reolink;
use ptzd_core::{AuthResult, Authenticator};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Max age of a cache hit. Bounds how long a password changed on the camera
    /// keeps working here. Own choice.
    pub cache_ttl: Duration,
    /// Consecutive failures for one user before we stop trying. The Reolink
    /// lockout policy is UNPROVEN (the web UI shows `remain_times`/`unlock_time`,
    /// values unknown), so stay well below any plausible limit.
    pub user_fail_limit: u32,
    /// Failures across all users before a global stop (guards user-name guessing).
    pub global_fail_limit: u32,
    /// First backoff after hitting a limit; doubles per further failure.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            cache_ttl: Duration::from_secs(300),
            user_fail_limit: 3,
            global_fail_limit: 6,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(900),
        }
    }
}

type Key = (String, [u8; 32]);

struct Cached {
    /// TTL end, refreshed on each use.
    until: Instant,
    /// Hard end: the token lease of the Login that proved this password.
    hard_until: Instant,
}

#[derive(Default)]
struct Fails {
    count: u32,
    blocked_until: Option<Instant>,
    inflight: bool,
}

#[derive(Default)]
struct Inner {
    cache: HashMap<Key, Cached>,
    per_user: HashMap<String, Fails>,
    global: Fails,
}

pub struct ReolinkAuth {
    api: Arc<ApiClient>,
    state: Option<Arc<Reolink>>,
    cfg: AuthConfig,
    inner: Mutex<Inner>,
    /// Signalled when an in-flight login ends, so a concurrent caller with the
    /// same user waits for its result instead of failing (two Frigate cameras).
    done: Condvar,
}

/// Longest wait for another caller's in-flight login (about the API timeout).
const INFLIGHT_WAIT: Duration = Duration::from_secs(8);

#[derive(PartialEq)]
enum Refuse {
    Blocked,
    Busy,
}

const MAX_TRACKED: usize = 64;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the per-user in-flight slot; releases it on every exit path.
struct Slot<'a> {
    auth: &'a ReolinkAuth,
    user: &'a str,
    refund: bool,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut i = lock(&self.auth.inner);
        if self.refund {
            // Camera unreachable: not the caller's fault, do not count it.
            i.global.count = i.global.count.saturating_sub(1);
            if let Some(f) = i.per_user.get_mut(self.user) {
                f.count = f.count.saturating_sub(1);
            }
        }
        if let Some(f) = i.per_user.get_mut(self.user) {
            f.inflight = false;
        }
        drop(i);
        self.auth.done.notify_all();
    }
}

impl ReolinkAuth {
    pub fn new(api: Arc<ApiClient>, state: Option<Arc<Reolink>>, cfg: AuthConfig) -> Self {
        ReolinkAuth { api, state, cfg, inner: Mutex::new(Inner::default()), done: Condvar::new() }
    }

    fn backoff(&self, fails: u32, limit: u32) -> Duration {
        let exp = fails.saturating_sub(limit).min(16);
        self.cfg.backoff_base.saturating_mul(1u32 << exp).min(self.cfg.backoff_max)
    }

    fn blocked(inner: &Inner, user: &str, now: Instant) -> bool {
        let g = inner.global.blocked_until.is_some_and(|t| t > now);
        let u = inner.per_user.get(user).and_then(|f| f.blocked_until).is_some_and(|t| t > now);
        g || u
    }

    /// Cache lookup; a hit refreshes the TTL. Caller holds the lock.
    fn cache_hit(&self, i: &mut Inner, key: &Key, now: Instant) -> bool {
        match i.cache.get_mut(key) {
            Some(c) if c.until > now && c.hard_until > now => {
                c.until = (now + self.cfg.cache_ttl).min(c.hard_until);
                true
            }
            Some(_) => {
                i.cache.remove(key);
                false
            }
            None => false,
        }
    }

    /// Atomic check-and-reserve under one lock: counts the attempt as a
    /// failure up front (applying any resulting block) and takes the user's
    /// in-flight slot. `Err` = refuse (rate-limited, busy or table full).
    fn reserve(&self, user: &str, now: Instant) -> Result<(), Refuse> {
        let mut i = lock(&self.inner);
        if Self::blocked(&i, user, now) {
            return Err(Refuse::Blocked);
        }
        if i.per_user.get(user).is_some_and(|f| f.inflight) {
            return Err(Refuse::Busy); // one uncached login per user at a time
        }
        if i.per_user.len() >= MAX_TRACKED && !i.per_user.contains_key(user) {
            // Drop idle entries whose block has lapsed; else fail closed.
            i.per_user.retain(|_, f| f.inflight || f.blocked_until.is_some_and(|t| t > now));
            if i.per_user.len() >= MAX_TRACKED {
                return Err(Refuse::Blocked);
            }
        }
        i.global.count += 1;
        if i.global.count >= self.cfg.global_fail_limit {
            i.global.blocked_until = Some(now + self.backoff(i.global.count, self.cfg.global_fail_limit));
        }
        let limit = self.cfg.user_fail_limit;
        let f = i.per_user.entry(user.to_string()).or_default();
        f.count += 1;
        f.inflight = true;
        if f.count >= limit {
            f.blocked_until = Some(now + self.backoff(f.count, limit));
        }
        Ok(())
    }

    fn note_lock(&self, secs: u64) {
        // Camera says it locked us out: honour it, capped.
        let d = Duration::from_secs(secs).min(self.cfg.backoff_max);
        lock(&self.inner).global.blocked_until = Some(Instant::now() + d);
    }

    fn note_success(&self, user: &str, key: Key, ok: &LoginOk) {
        let mut i = lock(&self.inner);
        let now = Instant::now();
        // Keep the entry: `Slot` still owns its in-flight flag.
        if let Some(f) = i.per_user.get_mut(user) {
            f.count = 0;
            f.blocked_until = None;
        }
        i.global.count = 0;
        i.global.blocked_until = None;
        // A proven new password makes older hashes for this user stale.
        i.cache.retain(|k, _| k.0 != user);
        if i.cache.len() >= MAX_TRACKED {
            i.cache.clear();
        }
        let ttl = self.cfg.cache_ttl.min(ok.lease);
        i.cache.insert(key, Cached { until: now + ttl, hard_until: now + ok.lease });
    }
}

impl Authenticator for ReolinkAuth {
    fn check(&self, user: &str, password: &str) -> AuthResult {
        let key: Key = (user.to_string(), sha256(password.as_bytes()));
        let start = Instant::now();
        loop {
            let now = Instant::now();
            {
                let mut i = lock(&self.inner);
                if self.cache_hit(&mut i, &key, now) {
                    return AuthResult::Ok;
                }
            }
            match self.reserve(user, now) {
                Ok(()) => break,
                Err(Refuse::Busy) if now.duration_since(start) < INFLIGHT_WAIT => {
                    // Same user logging in right now (the other lens): wait
                    // for it, then the cache answers. No failure is counted.
                    let g = lock(&self.inner);
                    let _ = self.done.wait_timeout(g, Duration::from_millis(500));
                }
                Err(_) => {
                    log::warn!("reolink auth: rate-limited, failing closed");
                    return AuthResult::Unavailable;
                }
            }
        }
        let mut slot = Slot { auth: self, user, refund: false };
        match self.api.login(user, password) {
            Ok(ok) => {
                let adopted = match &self.state {
                    Some(r) => r.adopt_login(user, password, &ok),
                    None => false,
                };
                if !adopted {
                    // Verification only: do not leave a live token behind (R9).
                    if let Err(e) = self.api.logout(&ok.token) {
                        log::debug!("reolink auth: logout failed: {e}");
                    }
                }
                self.note_success(user, key, &ok);
                AuthResult::Ok
            }
            Err(ApiError::BadCredentials) => AuthResult::BadCredentials,
            Err(ApiError::Locked { unlock_secs }) => {
                log::warn!("reolink auth: camera lockout {unlock_secs}s");
                self.note_lock(unlock_secs);
                slot.refund = true;
                AuthResult::Unavailable
            }
            Err(e) => {
                log::warn!("reolink auth: API unavailable: {e}");
                slot.refund = true;
                AuthResult::Unavailable
            }
        }
    }
}

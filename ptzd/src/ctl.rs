//! Control socket: line protocol over a unix socket in `runtime_dir`.
//! Request: one line. Response: zero or more text lines, then `OK` or `ERR <msg>`.
//! Permissions: dir 0700, socket 0600 (root only).
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ptzd_core::{Axis, MotorDev, PtzControl, ReolinkState};
use ptzd_hw::{DeviceHandle, Guarded, GuardedConfig, RealClock};
use ptzd_motion::Motion;
use ptzd_reolink::Reolink;

pub fn sock_path(dir: &Path) -> PathBuf {
    dir.join("ctl.sock")
}

pub struct Ctx {
    pub motion: Arc<Motion>,
    pub reolink: Arc<Reolink>,
    /// Mock only: the sim's second writer, standing in for `device`/the app.
    pub sim_device: Option<DeviceHandle<RealClock>>,
}

/// Created inside the runtime dir so a later start can tell it is ours.
const OWN_MARK: &str = ".ptzd-runtime";

/// Never chmod a directory ptzd did not create (review 2 R7): `/tmp` would
/// break the stock daemons. Existing dirs must be ours: a real directory, owned
/// by this uid, carrying the marker.
fn claim_dir(dir: &Path) -> Result<(), String> {
    crate::conf::check_runtime_dir(dir)?;
    let e = |x: std::io::Error| format!("{}: {x}", dir.display());
    match fs::symlink_metadata(dir) {
        Ok(m) => {
            use std::os::unix::fs::MetadataExt;
            if !m.is_dir() || m.uid() != nix::unistd::geteuid().as_raw() || !dir.join(OWN_MARK).exists() {
                return Err(format!("{}: exists and was not created by ptzd, refusing to use it", dir.display()));
            }
        }
        Err(x) if x.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(e)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(e)?;
            fs::write(dir.join(OWN_MARK), b"").map_err(e)?;
        }
        Err(x) => return Err(e(x)),
    }
    Ok(())
}

pub fn bind(dir: &Path) -> Result<UnixListener, String> {
    claim_dir(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))?;
    let p = sock_path(dir);
    if UnixStream::connect(&p).is_ok() {
        return Err(format!("{}: another ptzd is running", p.display()));
    }
    let _ = fs::remove_file(&p); // stale socket
    let l = UnixListener::bind(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(l)
}

pub fn serve(l: UnixListener, ctx: Ctx) {
    for c in l.incoming() {
        let Ok(c) = c else { continue };
        let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
        handle(c, &ctx);
    }
}

fn handle(mut c: UnixStream, ctx: &Ctx) {
    let mut line = String::new();
    // Cap the request line so a bad client cannot grow memory.
    if BufReader::new((&c).take(4096)).read_line(&mut line).is_err() {
        return;
    }
    let out = match run(line.trim(), ctx) {
        Ok(mut body) => {
            body.push_str("OK\n");
            body
        }
        Err(e) => format!("ERR {e}\n"),
    };
    let _ = c.write_all(out.as_bytes());
}

fn run(line: &str, ctx: &Ctx) -> Result<String, String> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    let m = &ctx.motion;
    match cmd {
        "stop" => m.stop().map(|_| String::new()).map_err(|e| e.to_string()),
        "rehome" => m.rehome().map(|_| String::new()).map_err(|e| e.to_string()),
        "status" => {
            let s = m.status();
            let mut o = format!("ready {}\nmoving {}\npan {:.4}\ntilt {:.4}\n", m.is_ready(), s.moving, s.pan, s.tilt);
            o += &format!("self_movers {}\n", ctx.reolink.self_movers().enabled.join(","));
            let l = ctx.reolink.pan_limits();
            o += &format!("pan_limits {:?} {:?}\n", l.left, l.right);
            for p in m.presets() {
                o += &format!("preset {} {} {:.4} {:.4}\n", p.token, p.name, p.pan, p.tilt);
            }
            Ok(o)
        }
        "sim-device-jog" => {
            // Mock only: `sim-device-jog pan|tilt POS DIR` (DIR 1 = down). Slowest speed class.
            let dev = ctx.sim_device.as_ref().ok_or("sim-device-jog needs --mock")?;
            let a: Vec<&str> = rest.split_whitespace().collect();
            let [axis, pos, dir] = a[..] else { return Err("usage: sim-device-jog pan|tilt POS DIR".into()) };
            let motor = match axis {
                "pan" => Axis::Pan.motor_index(),
                "tilt" => Axis::Tilt.motor_index(),
                _ => return Err("axis must be pan|tilt".into()),
            };
            let pos: u32 = pos.parse().map_err(|_| "bad POS")?;
            let dir: u32 = dir.parse().map_err(|_| "bad DIR")?;
            dev.set_pos(motor, pos, dir, 8).map(|_| String::new()).map_err(|e| e.to_string())
        }
        "preset" => {
            let (sub, name) = rest.split_once(' ').unwrap_or((rest, ""));
            let name = name.trim();
            if name.is_empty() {
                return Err("preset needs a NAME".into());
            }
            // Frigate matches preset names lower-cased (onvif.py:453-462).
            let found = m.presets().into_iter().find(|p| p.name.eq_ignore_ascii_case(name));
            match sub {
                "set" => {
                    let tok = m.set_preset(Some(name), found.as_ref().map(|p| p.token.as_str())).map_err(|e| e.to_string())?;
                    Ok(format!("token {tok}\n"))
                }
                "rm" => {
                    let p = found.ok_or_else(|| format!("no preset named {name}"))?;
                    m.remove_preset(&p.token).map(|_| String::new()).map_err(|e| e.to_string())
                }
                _ => Err("usage: preset set|rm NAME".into()),
            }
        }
        _ => Err(format!("unknown command {cmd:?}")),
    }
}

/// Client side. Returns the process exit code.
pub fn client(dir: &Path, line: &str) -> Result<i32, std::io::Error> {
    let mut s = UnixStream::connect(sock_path(dir))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.write_all(format!("{line}\n").as_bytes())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let mut code = 1;
    for l in resp.lines() {
        if l == "OK" {
            code = 0;
        } else if let Some(e) = l.strip_prefix("ERR ") {
            eprintln!("ptzd: {e}");
        } else {
            // Not println!: it panics when the reader is gone (`status | head`).
            let _ = writeln!(std::io::stdout(), "{l}");
        }
    }
    Ok(code)
}

/// No daemon: STOP both axes through the guarded device (real hardware only).
pub fn direct_stop(dev: &Path, margin: u32) -> i32 {
    let raw = match ptzd_hw::LensZfiFd::open(dev) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ptzd: {}: {e}", dev.display());
            return 1;
        }
    };
    let mut g = Guarded::new(raw, GuardedConfig { margin });
    let mut code = 0;
    for a in Axis::ALL {
        match g.stop(a) {
            Ok(()) => println!("stopped {a:?}"),
            Err(e) => {
                eprintln!("ptzd: STOP {a:?}: {e}");
                code = 1;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ptzd-ctl-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn r7_refuses_shared_and_foreign_dirs_and_creates_its_own() {
        assert!(claim_dir(Path::new("/tmp")).is_err());
        let before = fs::metadata("/tmp").unwrap().permissions().mode();
        // an existing dir without our marker is left untouched
        let d = tmp("pre");
        fs::create_dir_all(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(claim_dir(&d).is_err());
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
        // a new subdir is created 0700 with the marker, and can be reused
        let n = tmp("new");
        claim_dir(&n).unwrap();
        assert_eq!(fs::metadata(&n).unwrap().permissions().mode() & 0o777, 0o700);
        claim_dir(&n).unwrap();
        assert_eq!(fs::metadata("/tmp").unwrap().permissions().mode(), before);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&n);
    }
}

//! `ptzd run`: wire hw -> motion -> onvif and the Reolink side.
use std::path::PathBuf;
use std::sync::Arc;

use nix::sys::signal::{SigSet, Signal};
use ptzd_core::{Axis, MotorDev, ReolinkState};
use ptzd_hw::{Guarded, GuardedConfig, KernelSim};
use ptzd_motion::Motion;
use ptzd_onvif::OnvifServer;
use ptzd_reolink::mock::MockCamera;
use ptzd_reolink::{ApiClient, HttpTransport, Reolink, ReolinkAuth};

use crate::conf::Conf;
use crate::ctl;

pub fn run(mut c: Conf, mock: bool) -> Result<(), String> {
    // Block before any thread exists so every thread inherits the mask and
    // only the sigwait thread sees SIGTERM/SIGINT.
    let mut sigs = SigSet::empty();
    sigs.add(Signal::SIGTERM);
    sigs.add(Signal::SIGINT);
    sigs.thread_block().map_err(|e| format!("sigmask: {e}"))?;

    let mut temp_state = None;
    let mut mock_cam = None;
    let mut sim_device = None;
    let dev: Box<dyn MotorDev>;
    let api_addr;
    if mock {
        let cam = MockCamera::start("127.0.0.1:0", &c.mock_user, &c.mock_password).map_err(|e| format!("mock camera: {e}"))?;
        cam.set_smart_track(c.mock_smart_track);
        if let Some((l, r)) = c.mock_limits {
            cam.set_limits(l, r);
        }
        api_addr = cam.addr().to_string();
        log::info!("mock reolink api on {api_addr}");
        mock_cam = Some(cam);
        if !c.onvif_bind_set {
            c.onvif.bind_addr = std::net::IpAddr::from([127, 0, 0, 1]);
        }
        if !c.onvif_port_set {
            c.onvif.port = 0;
        }
        if c.state_dir.is_none() {
            let d = std::env::temp_dir().join(format!("ptzd-mock-{}", std::process::id()));
            temp_state = Some(d.clone());
            c.state_dir = Some(d);
        }
        let sim = KernelSim::realtime();
        let device = sim.device();
        if let Some(d) = c.mock_device_home_delay {
            // Stand-in for `device`'s boot homing (review 2 B2, T14).
            for a in Axis::ALL {
                device.autotest_after(a.motor_index(), d);
            }
        }
        sim_device = Some(device);
        dev = Box::new(Guarded::new(sim, GuardedConfig { margin: c.soft_margin }));
    } else {
        api_addr = c.reolink_api.clone();
        let raw = ptzd_hw::LensZfiFd::open(&c.lens_dev).map_err(|e| format!("{}: {e}", c.lens_dev.display()))?;
        dev = Box::new(Guarded::new(raw, GuardedConfig { margin: c.soft_margin }));
    }
    let state_dir: PathBuf = c.state_dir.clone().unwrap_or_else(|| c.motion.state_dir.clone());
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    c.motion.state_dir = state_dir.clone();
    log::info!("state dir {}", state_dir.display());

    let transport = HttpTransport::new(api_addr).with_timeout(c.api_timeout);
    let api = Arc::new(ApiClient::new(Arc::new(transport), c.reolink.channel).with_default_lease(c.api_default_lease));
    let reolink = Arc::new(Reolink::new(api.clone(), c.reolink.clone()));
    let recheck = reolink.spawn_recheck();
    let auth = Arc::new(ReolinkAuth::new(api, Some(reolink.clone()), c.auth.clone()));
    let state: Arc<dyn ReolinkState> = reolink.clone();
    let motion = Arc::new(Motion::new(dev, state, c.motion.clone()));

    let server = OnvifServer::bind(c.onvif.clone(), motion.clone(), auth).map_err(|e| format!("onvif bind: {e}"))?;
    log::info!("onvif listening on {}", std::net::SocketAddr::new(c.onvif.bind_addr, server.local_addr().port()));
    let handle = server.spawn().map_err(|e| format!("onvif spawn: {e}"))?;

    let listener = ctl::bind(&c.runtime_dir)?;
    log::info!("ctl socket {}", ctl::sock_path(&c.runtime_dir).display());
    let ctx = ctl::Ctx { motion: motion.clone(), reolink: reolink.clone(), sim_device };
    std::thread::Builder::new()
        .name("ctl".into())
        .spawn(move || ctl::serve(listener, ctx))
        .map_err(|e| format!("ctl thread: {e}"))?;

    let sock = ctl::sock_path(&c.runtime_dir);
    {
        // Review 2 W1: homing failed for good. Exit non-zero so runit restarts us (the
        // run script backs off); NotReady alone would last until a manual restart.
        let (motion, reolink, sock) = (motion.clone(), reolink.clone(), sock.clone());
        std::thread::Builder::new()
            .name("fatal".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if let Some(e) = motion.fatal() {
                    log::error!("homing failed for good, exiting 1 so runit retries: {e}");
                    motion.shutdown();
                    reolink.end_session();
                    let _ = std::fs::remove_file(&sock);
                    std::process::exit(1);
                }
            })
            .map_err(|e| format!("fatal thread: {e}"))?;
    }
    let waiter = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let sig = sigs.wait();
            log::info!("got {sig:?}, shutting down");
            handle.shutdown();
            motion.shutdown();
            let _ = std::fs::remove_file(&sock);
            drop(recheck);
            reolink.end_session();
            drop(mock_cam);
            if let Some(d) = temp_state {
                let _ = std::fs::remove_dir_all(d);
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    let _ = waiter.join();
    log::info!("bye");
    Ok(())
}

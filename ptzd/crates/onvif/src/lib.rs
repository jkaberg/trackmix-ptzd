//! ONVIF device/media/PTZ SOAP server for Frigate 0.18.3 (plan §3).
//! Hand-rolled blocking HTTP/1.1 on std threads; sits on `PtzControl` and
//! `Authenticator` from ptzd-core. See ../../README.md.
#![forbid(unsafe_code)]

mod auth;
mod device;
mod http;
mod media;
mod ptz;
mod soap;
mod xml;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ptzd_core::{Authenticator, Lens, PtzControl};

pub use auth::{Credentials, DIGEST_HELP};

/// Per-lens stream description for GetProfiles/GetVideoSources/GetStreamUri.
/// Frigate reads none of the resolution/fps fields (it only needs the profile
/// to have a VideoEncoderConfiguration, onvif.py:214-222). The defaults are
/// cosmetic and UNPROVEN: set them from the camera's real stream settings.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    /// Returned by GetStreamUri. Plan §3: "the stock RTSP URLs".
    pub rtsp_url: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Debug)]
pub struct OnvifConfig {
    pub bind_addr: IpAddr,
    /// Plan §3: default 8899.
    pub port: u16,
    /// Host put in advertised XAddrs. `None` = echo the request's Host
    /// header (falls back to the local socket address).
    pub advertised_host: Option<String>,
    pub wide: StreamInfo,
    pub tele: StreamInfo,
    /// Plan §6: only this profile may send RelativeMove. `None` = both.
    pub tracker_profile: Option<Lens>,
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub hardware_id: String,
    /// Advertised in PTZNode only. UNPROVEN: the real cap is the motion
    /// crate's preset store; nothing in Frigate reads it.
    pub max_presets: u32,
    /// Advertised PTZTimeout max (required by the schema). The daemon passes
    /// any requested Timeout to `PtzControl::continuous_move` unchanged.
    /// Cosmetic, UNPROVEN: motion decides the real cap.
    pub max_continuous_timeout: Duration,
    /// Concurrent connection cap (one thread each). Not a protocol value.
    pub max_connections: usize,
    /// Concurrent connections per source IP (slowloris guard, R5). Own
    /// choice. Measured (review 2 B1): one Frigate camera holds 3 services
    /// with 4 s keep-alive sockets each, so two lenses need 6-12 at once; 16
    /// leaves room. Must stay below `max_connections`.
    pub max_connections_per_ip: usize,
    /// Idle keep-alive wait between requests.
    pub idle_timeout: Duration,
    /// Absolute time allowed to receive one full request (headers and body)
    /// once its first byte arrived. Not reset by progress.
    pub request_timeout: Duration,
    /// Request body cap in bytes (ONVIF requests are < 2 KB).
    pub max_body: usize,
}

impl Default for OnvifConfig {
    fn default() -> Self {
        let s = |u: &str| StreamInfo { rtsp_url: u.into(), width: 2560, height: 1440, fps: 15 };
        OnvifConfig {
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 8899,
            advertised_host: None,
            // UNPROVEN: stream paths and sizes are placeholders to be set by config.
            wide: s("rtsp://127.0.0.1:554/h264Preview_01_main"),
            tele: s("rtsp://127.0.0.1:554/h264Preview_02_main"),
            tracker_profile: None,
            manufacturer: "Reolink".into(),
            model: "TrackMix PoE".into(),
            firmware: "ptzd".into(),
            serial: "ptzd-0".into(),
            hardware_id: "ptzd".into(),
            max_presets: 16,
            max_continuous_timeout: Duration::from_secs(60),
            max_connections: 32,
            max_connections_per_ip: 16,
            idle_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(10),
            max_body: 64 * 1024,
        }
    }
}

impl OnvifConfig {
    pub(crate) fn stream(&self, l: Lens) -> &StreamInfo {
        match l {
            Lens::Wide => &self.wide,
            Lens::Tele => &self.tele,
        }
    }
}

pub(crate) struct Shared {
    pub cfg: OnvifConfig,
    pub ptz: Arc<dyn PtzControl>,
    pub auth: Arc<dyn Authenticator>,
    pub local: SocketAddr,
    pub conns: AtomicUsize,
    pub per_ip: std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>,
    pub stop: AtomicBool,
}

pub struct OnvifServer {
    listener: TcpListener,
    shared: Arc<Shared>,
}

/// Returned by `spawn`; `shutdown` stops the accept loop (open connections
/// finish their current request and close).
pub struct ServerHandle {
    shared: Arc<Shared>,
    join: JoinHandle<()>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local
    }
    pub fn shutdown(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept().
        let _ = std::net::TcpStream::connect_timeout(&self.shared.local, Duration::from_secs(1));
        let _ = self.join.join();
    }
}

impl OnvifServer {
    pub fn bind(
        cfg: OnvifConfig,
        ptz: Arc<dyn PtzControl>,
        auth: Arc<dyn Authenticator>,
    ) -> std::io::Result<OnvifServer> {
        let listener = TcpListener::bind(SocketAddr::new(cfg.bind_addr, cfg.port))?;
        let mut local = listener.local_addr()?;
        if local.ip().is_unspecified() {
            // A wake-up connect to 0.0.0.0 is not portable; use loopback.
            local.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        Ok(OnvifServer {
            listener,
            shared: Arc::new(Shared {
                cfg,
                ptz,
                auth,
                local,
                conns: AtomicUsize::new(0),
                per_ip: Default::default(),
                stop: AtomicBool::new(false),
            }),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local
    }

    /// Blocking accept loop; one thread per connection.
    pub fn serve(self) {
        http::accept_loop(self.listener, self.shared);
    }

    pub fn spawn(self) -> std::io::Result<ServerHandle> {
        let shared = self.shared.clone();
        let join = std::thread::Builder::new()
            .name("onvif-accept".into())
            .spawn(move || self.serve())?;
        Ok(ServerHandle { shared, join })
    }
}

#[cfg(test)]
mod tests;

//! trackmix-ptzd entry point: `ptzd run` (daemon) and `ptzd ctl` (control).
#![forbid(unsafe_code)]

mod conf;
mod ctl;
mod logger;
mod run;

use std::path::Path;

const USAGE: &str = "usage:
  ptzd run [--config PATH] [--mock]
  ptzd ctl [--config PATH] <stop|status|rehome|preset set NAME|preset rm NAME>";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("ptzd: {msg}");
    std::process::exit(code)
}

fn load(path: Option<String>) -> conf::Conf {
    let explicit = path.is_some();
    let p = path.unwrap_or_else(|| conf::DEFAULT_PATH.to_string());
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => die(&format!("{p}: {e}"), 2),
    };
    conf::parse(&text).unwrap_or_else(|e| die(&format!("{p}: {e}"), 2))
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        die(USAGE, 2);
    }
    let sub = args.remove(0);
    let mut config = None;
    let mut mock = false;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => config = Some(it.next().unwrap_or_else(|| die(USAGE, 2))),
            "--mock" if sub == "run" => mock = true,
            _ => rest.push(a),
        }
    }
    match sub.as_str() {
        "run" => {
            if !rest.is_empty() {
                die(USAGE, 2);
            }
            let c = load(config);
            logger::init(c.log_level);
            if let Err(e) = run::run(c, mock) {
                log::error!("{e}");
                std::process::exit(1);
            }
        }
        "ctl" => {
            if rest.is_empty() {
                die(USAGE, 2);
            }
            let c = load(config);
            let line = rest.join(" ");
            match ctl::client(&c.runtime_dir, &line) {
                Ok(code) => std::process::exit(code),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                    // No daemon. The finish script relies on STOP still working.
                    if line == "stop" {
                        eprintln!("ptzd: daemon not running, stopping motors directly");
                        std::process::exit(ctl::direct_stop(Path::new(&c.lens_dev), c.soft_margin));
                    }
                    die("daemon not running", 1);
                }
                Err(e) => die(&format!("ctl: {e}"), 1),
            }
        }
        _ => die(USAGE, 2),
    }
}

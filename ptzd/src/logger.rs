//! Minimal stderr logger; runit/svlogd captures stderr (plan §7a).
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let lvl = match r.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        // svlogd -tt adds timestamps, so none here.
        let _ = writeln!(std::io::stderr().lock(), "{lvl} {}: {}", r.target(), r.args());
    }
    fn flush(&self) {}
}

static LOGGER: Stderr = Stderr;

pub fn init(level: LevelFilter) {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}

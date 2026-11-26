//! Flat-file persistence under `state_dir`. Atomic writes (tmp + rename).
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredPreset {
    pub token: String,
    pub name: String,
    /// Raw steps, so limit changes do not move a preset.
    pub pan: u32,
    pub tilt: u32,
}

pub struct Store {
    dir: PathBuf,
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Store { dir: dir.to_path_buf() }
    }
    fn p(&self, n: &str) -> PathBuf {
        self.dir.join(n)
    }

    pub fn read_pos(&self) -> Option<[u32; 2]> {
        let v: Value = serde_json::from_slice(&fs::read(self.p("pos")).ok()?).ok()?;
        Some([v["pan"].as_u64()? as u32, v["tilt"].as_u64()? as u32])
    }
    pub fn write_pos(&self, pos: [u32; 2]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_atomic(&self.p("pos"), json!({"pan": pos[0], "tilt": pos[1]}).to_string().as_bytes())
    }

    pub fn marker_exists(&self) -> bool {
        self.p("clean").exists()
    }
    pub fn write_marker(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_atomic(&self.p("clean"), b"1")
    }
    pub fn remove_marker(&self) -> io::Result<()> {
        match fs::remove_file(self.p("clean")) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// None = no file (first run). An unparseable file is renamed to
    /// `presets.json.bad` first (M3) so it is never overwritten.
    pub fn read_presets(&self) -> Option<Vec<StoredPreset>> {
        let raw = fs::read(self.p("presets.json")).ok()?;
        let r = self.parse_presets(&raw);
        if r.is_none() {
            log::error!("presets.json unparseable, moved to presets.json.bad");
            let _ = fs::rename(self.p("presets.json"), self.p("presets.json.bad"));
        }
        r
    }
    fn parse_presets(&self, raw: &[u8]) -> Option<Vec<StoredPreset>> {
        let v: Value = serde_json::from_slice(raw).ok()?;
        let mut out = Vec::new();
        for p in v["presets"].as_array()? {
            out.push(StoredPreset {
                token: p["token"].as_str()?.to_string(),
                name: p["name"].as_str()?.to_string(),
                pan: p["pan"].as_u64()? as u32,
                tilt: p["tilt"].as_u64()? as u32,
            });
        }
        Some(out)
    }
    pub fn write_presets(&self, ps: &[StoredPreset]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let arr: Vec<Value> = ps
            .iter()
            .map(|p| json!({"token": p.token, "name": p.name, "pan": p.pan, "tilt": p.tilt}))
            .collect();
        write_atomic(&self.p("presets.json"), json!({"presets": arr}).to_string().as_bytes())
    }
}

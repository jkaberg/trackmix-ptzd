//! Tolerant read-only reader for `/mnt/para/0_ptz.cfg` and `/mnt/para/ai.cfg`.
//!
//! Known (C, research/ptzd-hardware.md §7): both are plain TinyXML at rest
//! (roots `ptz_config_t` / `ai_cfg`), loaded by the router with no decrypt.
//! UNPROVEN: the exact element/attribute layout. So this reader accepts any of
//!   `<key>val</key>`, `<key value="val"/>` (also val/v), `key="val"` as an
//!   attribute of any element, and `<x name="key" value="val"/>` (name/key).
//! The last occurrence of a key wins. Missing key = off / unset.
//! Check on camera: `head -c 400 /mnt/para/0_ptz.cfg` (T-list: cfg layout).

use ptzd_core::PanLimits;
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct CfgMap {
    map: HashMap<String, String>,
}

impl CfgMap {
    pub fn parse(text: &str) -> CfgMap {
        let mut map = HashMap::new();
        // Stack of (tag, text so far, saw child element).
        let mut stack: Vec<(String, String, bool)> = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            match rest.find('<') {
                None => {
                    if let Some(top) = stack.last_mut() {
                        top.1.push_str(rest);
                    }
                    break;
                }
                Some(i) => {
                    if let Some(top) = stack.last_mut() {
                        top.1.push_str(&rest[..i]);
                    }
                    rest = &rest[i..];
                }
            }
            if let Some(r) = rest.strip_prefix("<!--") {
                rest = skip_past(r, "-->");
            } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
                let end = r.find("]]>").unwrap_or(r.len());
                if let Some(top) = stack.last_mut() {
                    top.1.push_str(&r[..end]);
                }
                rest = skip_past(r, "]]>");
            } else if rest.starts_with("<?") || rest.starts_with("<!") {
                rest = skip_past(&rest[1..], ">");
            } else if let Some(r) = rest.strip_prefix("</") {
                let end = r.find('>').unwrap_or(r.len());
                let name = r[..end].trim();
                // Pop to the matching open tag (tolerates stray unclosed tags).
                if let Some(pos) = stack.iter().rposition(|s| s.0 == name) {
                    while stack.len() > pos + 1 {
                        stack.pop();
                    }
                    if let Some((n, t, child)) = stack.pop() {
                        let t = unescape(t.trim());
                        if !child && !t.is_empty() {
                            map.insert(n, t);
                        }
                    }
                }
                rest = skip_past(r, ">");
            } else {
                let r = &rest[1..];
                let end = tag_end(r);
                let inner = &r[..end];
                let selfclose = inner.ends_with('/');
                let inner = inner.trim_end_matches('/');
                let (name, attrs) = split_tag(inner);
                if let Some(top) = stack.last_mut() {
                    top.2 = true;
                }
                absorb_attrs(&mut map, name, &attrs);
                if !selfclose {
                    stack.push((name.to_string(), String::new(), false));
                }
                rest = if end < r.len() { &r[end + 1..] } else { "" };
            }
        }
        CfgMap { map }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// Integer value; "true"/"false" accepted. None if absent or unparsable.
    pub fn int(&self, key: &str) -> Option<i64> {
        let v = self.get(key)?.trim();
        match v {
            "true" => Some(1),
            "false" => Some(0),
            _ => v.parse().ok(),
        }
    }

    /// Indexes N for which `<prefix>N_<suffix>` is nonzero (e.g. "cc", "bstart").
    fn indexed_on(&self, prefix: &str, suffix: &str) -> Vec<u32> {
        let tail = format!("_{suffix}");
        let mut v: Vec<u32> = self
            .map
            .keys()
            .filter_map(|k| {
                let n = k.strip_prefix(prefix)?.strip_suffix(tail.as_str())?;
                n.parse::<u32>().ok().filter(|_| self.int(k).is_some_and(|x| x != 0))
            })
            .collect();
        v.sort_unstable();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn skip_past<'a>(s: &'a str, pat: &str) -> &'a str {
    match s.find(pat) {
        Some(i) => &s[i + pat.len()..],
        None => "",
    }
}

/// Index of the closing '>' of a start tag, ignoring '>' inside quotes.
fn tag_end(s: &str) -> usize {
    let mut q: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (q, c) {
            (None, '"') | (None, '\'') => q = Some(c),
            (Some(x), c) if c == x => q = None,
            (None, '>') => return i,
            _ => {}
        }
    }
    s.len()
}

fn split_tag(inner: &str) -> (&str, Vec<(String, String)>) {
    let inner = inner.trim();
    let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
    let name = &inner[..name_end];
    let mut attrs = Vec::new();
    let mut s = inner[name_end..].trim_start();
    while let Some(eq) = s.find('=') {
        let key = s[..eq].trim().to_string();
        let after = s[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let body = &after[1..];
        let Some(close) = body.find(q) else { break };
        attrs.push((key, unescape(&body[..close])));
        s = body[close + 1..].trim_start();
    }
    (name, attrs)
}

fn absorb_attrs(map: &mut HashMap<String, String>, tag: &str, attrs: &[(String, String)]) {
    let find = |names: &[&str]| attrs.iter().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.clone());
    let val = find(&["value", "val", "v"]);
    if let Some(v) = &val {
        map.insert(tag.to_string(), v.clone());
        if let Some(n) = find(&["name", "key"]) {
            map.insert(n, v.clone());
        }
    }
    for (k, v) in attrs {
        map.insert(k.clone(), v.clone());
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn limit(v: Option<i64>) -> Option<u32> {
    // -1 (or any negative) = unset (research/ptzd-hardware.md §7).
    v.filter(|x| *x >= 0).and_then(|x| u32::try_from(x).ok())
}

pub fn pan_limits_from_map(m: &CfgMap) -> PanLimits {
    PanLimits { left: limit(m.int("p_limit_l")), right: limit(m.int("p_limit_r")) }
}

/// Self-mover names found ON in 0_ptz.cfg: guard (`grdenable`), patrols
/// (`cc%d_bval|bstart`), patterns (`tc%d_bval|bstart`). Meaning of bval vs
/// bstart is UNPROVEN (valid/enabled vs running); either nonzero counts.
pub fn ptz_movers(m: &CfgMap) -> Vec<String> {
    let mut v = Vec::new();
    if m.int("grdenable").is_some_and(|x| x != 0) {
        v.push("guard".to_string());
    }
    for (pfx, name) in [("cc", "patrol"), ("tc", "pattern")] {
        let mut idx = m.indexed_on(pfx, "bval");
        idx.extend(m.indexed_on(pfx, "bstart"));
        idx.sort_unstable();
        idx.dedup();
        for i in &idx {
            // M7: say which key blocks moves (a file-derived pattern cannot be cleared via the API, UNPROVEN).
            log::debug!("reolink: {name} {i} blocks moves ({pfx}{i}_bval or {pfx}{i}_bstart nonzero)");
        }
        v.extend(idx.into_iter().map(|i| format!("{name} {i}")));
    }
    v
}

pub fn ai_smart_track_on(m: &CfgMap) -> bool {
    m.int("smart_track").is_some_and(|x| x != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ASSUMED FORMAT: synthetic fixtures, not copied from a camera.
    const PTZ_ELEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<ptz_config_t>
  <!-- assumed -->
  <p_limit_l>120</p_limit_l><p_limit_r>-1</p_limit_r>
  <grdenable>1</grdenable>
  <cc0_bval>0</cc0_bval><cc0_bstart>0</cc0_bstart>
  <cc2_bval>1</cc2_bval><cc2_bstart>0</cc2_bstart>
  <tc1_bval>0</tc1_bval><tc1_bstart>1</tc1_bstart>
</ptz_config_t>"#;

    const PTZ_ATTRS: &str = r#"<ptz_config_t p_limit_l="5" p_limit_r="2600" grdenable="0">
  <cc1_bval value="0"/><cc1_bstart value="0"/><tc0_bstart val="0"/>
</ptz_config_t>"#;

    const PTZ_NAMEVAL: &str = r#"<ptz_config_t><item name="p_limit_l" value="77"/><item key="grdenable" value="1"/></ptz_config_t>"#;

    #[test]
    fn elements_layout() {
        let m = CfgMap::parse(PTZ_ELEMENTS);
        assert_eq!(pan_limits_from_map(&m), PanLimits { left: Some(120), right: None });
        assert_eq!(ptz_movers(&m), vec!["guard", "patrol 2", "pattern 1"]);
    }

    #[test]
    fn attribute_layout() {
        let m = CfgMap::parse(PTZ_ATTRS);
        assert_eq!(pan_limits_from_map(&m), PanLimits { left: Some(5), right: Some(2600) });
        assert!(ptz_movers(&m).is_empty());
    }

    #[test]
    fn name_value_layout() {
        let m = CfgMap::parse(PTZ_NAMEVAL);
        assert_eq!(pan_limits_from_map(&m).left, Some(77));
        assert_eq!(ptz_movers(&m), vec!["guard"]);
    }

    #[test]
    fn ai_cfg() {
        assert!(ai_smart_track_on(&CfgMap::parse("<ai_cfg><smart_track>1</smart_track></ai_cfg>")));
        assert!(!ai_smart_track_on(&CfgMap::parse("<ai_cfg><smart_track>0</smart_track></ai_cfg>")));
        assert!(!ai_smart_track_on(&CfgMap::parse("<ai_cfg smart_track=\"0\"/>")));
        assert!(ai_smart_track_on(&CfgMap::parse("<ai_cfg smart_track=\"1\"/>")));
    }

    #[test]
    fn garbage_is_empty() {
        let _ = CfgMap::parse("\x00\x01 not xml <<<"); // must not panic
        let m = CfgMap::parse("");
        assert_eq!(pan_limits_from_map(&m), PanLimits::default());
    }
}

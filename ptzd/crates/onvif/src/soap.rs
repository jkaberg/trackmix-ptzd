//! SOAP 1.2 envelope, faults, value parsing, UTC time.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const NS_S: &str = "http://www.w3.org/2003/05/soap-envelope";
pub const NS_TT: &str = "http://www.onvif.org/ver10/schema";
pub const NS_TDS: &str = "http://www.onvif.org/ver10/device/wsdl";
pub const NS_TRT: &str = "http://www.onvif.org/ver10/media/wsdl";
pub const NS_TPTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
pub const NS_TER: &str = "http://www.onvif.org/ver10/error";

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

pub fn envelope(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"{NS_S}\" xmlns:tt=\"{NS_TT}\" xmlns:tds=\"{NS_TDS}\" \
xmlns:trt=\"{NS_TRT}\" xmlns:tptz=\"{NS_TPTZ}\" xmlns:ter=\"{NS_TER}\">\
<s:Body>{body}</s:Body></s:Envelope>"
    )
}

/// SOAP 1.2 Fault. `subcodes` are `ter:` local names, outermost first.
#[derive(Debug, Clone, PartialEq)]
pub struct Fault {
    pub sender: bool,
    pub subcodes: Vec<&'static str>,
    pub reason: String,
}

impl Fault {
    pub fn sender(sub: &[&'static str], reason: impl Into<String>) -> Fault {
        Fault { sender: true, subcodes: sub.to_vec(), reason: reason.into() }
    }
    pub fn receiver(sub: &[&'static str], reason: impl Into<String>) -> Fault {
        Fault { sender: false, subcodes: sub.to_vec(), reason: reason.into() }
    }
    pub fn invalid_arg(reason: impl Into<String>) -> Fault {
        Fault::sender(&["InvalidArgVal"], reason)
    }
    pub fn not_authorized(reason: impl Into<String>) -> Fault {
        Fault::sender(&["NotAuthorized"], reason)
    }
    pub fn unsupported(op: &str) -> Fault {
        Fault::sender(&["ActionNotSupported"], format!("operation not supported: {op}"))
    }
    /// ONVIF core spec 5.11.2: sender faults 400, receiver faults 500.
    pub fn http_status(&self) -> u16 {
        if self.sender {
            400
        } else {
            500
        }
    }
    pub fn to_xml(&self) -> String {
        let mut sub = String::new();
        for s in self.subcodes.iter().rev() {
            sub = format!("<s:Subcode><s:Value>ter:{s}</s:Value>{sub}</s:Subcode>");
        }
        let code = if self.sender { "s:Sender" } else { "s:Receiver" };
        envelope(&format!(
            "<s:Fault><s:Code><s:Value>{code}</s:Value>{sub}</s:Code>\
<s:Reason><s:Text xml:lang=\"en\">{}</s:Text></s:Reason></s:Fault>",
            esc(&self.reason)
        ))
    }
}

/// Finite float, as the PTZ args must be.
pub fn parse_f32(s: &str) -> Result<f32, Fault> {
    match s.trim().parse::<f32>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(Fault::invalid_arg(format!("bad number: {s:?}"))),
    }
}

/// xs:duration subset: `P[nD][T[nH][nM][n[.n]S]]`, non-negative. No Y/M date parts.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let mut it = s.strip_prefix('P')?;
    let mut total = 0.0f64;
    let mut in_time = false;
    let mut seen = false;
    while !it.is_empty() {
        if let Some(rest) = it.strip_prefix('T') {
            if in_time {
                return None;
            }
            in_time = true;
            it = rest;
            continue;
        }
        let end = it.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let num: f64 = it[..end].parse().ok()?;
        let unit = it[end..].chars().next()?;
        let mult = match (in_time, unit) {
            (false, 'D') => 86400.0,
            (true, 'H') => 3600.0,
            (true, 'M') => 60.0,
            (true, 'S') => 1.0,
            _ => return None,
        };
        total += num * mult;
        seen = true;
        it = &it[end + 1..];
    }
    if !seen || !total.is_finite() || total > 1.0e9 {
        return None;
    }
    Some(Duration::from_secs_f64(total))
}

/// (year, month, day, hour, min, sec) UTC from the system clock.
pub fn utc_now() -> (i64, u32, u32, u32, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    utc_from_unix(secs)
}

/// Days-to-civil algorithm (H. Hinnant), proleptic Gregorian.
pub fn utc_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn utc_iso() -> String {
    let (y, mo, d, h, mi, s) = utc_now();
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT0S"), Some(Duration::ZERO));
        assert_eq!(parse_duration("PT1.5S"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("PT1M30S"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("P1DT1H"), Some(Duration::from_secs(90000)));
        assert_eq!(parse_duration("P"), None);
        assert_eq!(parse_duration("PT"), None);
        assert_eq!(parse_duration("-PT1S"), None);
        assert_eq!(parse_duration("P1Y"), None);
        assert_eq!(parse_duration("PT5"), None);
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn civil_time() {
        assert_eq!(utc_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_from_unix(951782400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(utc_from_unix(1700000000), (2023, 11, 14, 22, 13, 20));
        assert_eq!(utc_from_unix(-1), (1969, 12, 31, 23, 59, 59));
    }

    #[test]
    fn fault_xml_shape() {
        let x = Fault::sender(&["InvalidArgVal", "NoProfile"], "a<b").to_xml();
        assert!(x.contains("<s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:InvalidArgVal</s:Value><s:Subcode><s:Value>ter:NoProfile</s:Value></s:Subcode></s:Subcode>"));
        assert!(x.contains("a&lt;b"));
        assert_eq!(Fault::receiver(&[], "x").http_status(), 500);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_f32(" 0.5 ").unwrap(), 0.5);
        assert!(parse_f32("NaN").is_err());
        assert!(parse_f32("inf").is_err());
        assert!(parse_f32("x").is_err());
    }
}

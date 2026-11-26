//! WS-Security UsernameToken handling (plan §3 hard requirements, §9 Credentials).

use crate::soap::Fault;
use crate::xml::Node;
use ptzd_core::{AuthResult, Authenticator};

pub const NS_WSSE_LOCAL: &str = "Security";
/// OASIS UsernameToken profile 1.0 password types. The library picks by
/// `use_digest=encrypt` (client.py:321); Frigate passes `encrypt=not tls_insecure`
/// (onvif.py:163).
const TYPE_TEXT: &str = "PasswordText";
const TYPE_DIGEST: &str = "PasswordDigest";

#[derive(Debug, PartialEq)]
pub enum Credentials {
    Absent,
    Text { user: String, password: String },
    Digest { user: String },
    Malformed(String),
}

/// `header` is the SOAP Header element, if any.
pub fn extract(header: Option<&Node>) -> Credentials {
    let sec = match header.and_then(|h| h.child(NS_WSSE_LOCAL)) {
        Some(s) => s,
        None => return Credentials::Absent,
    };
    let tok = match sec.child("UsernameToken") {
        Some(t) => t,
        None => return Credentials::Absent,
    };
    let user = match tok.child("Username") {
        Some(u) if !u.text_trim().is_empty() => u.text_trim().to_string(),
        _ => return Credentials::Malformed("UsernameToken without Username".into()),
    };
    let pw = match tok.child("Password") {
        Some(p) => p,
        None => return Credentials::Malformed("UsernameToken without Password".into()),
    };
    // Per the profile, a missing Type means PasswordText.
    let ty = pw.attr("Type").unwrap_or(TYPE_TEXT);
    let ty = ty.rsplit(['#', '/']).next().unwrap_or(ty);
    match ty {
        TYPE_TEXT => Credentials::Text { user, password: pw.text.clone() },
        TYPE_DIGEST => Credentials::Digest { user },
        other => Credentials::Malformed(format!("unknown Password Type {other:?}")),
    }
}

pub const DIGEST_HELP: &str = "PasswordDigest is not supported (the camera password cannot be \
recovered from a digest). Set `tls_insecure: true` in the Frigate onvif config so it sends \
PasswordText.";

/// Whether `op` may run with no credentials at all. Library evidence: only
/// `GetSystemDateAndTime` is ever sent through the authless client
/// (client.py:466, in update_xaddrs when adjust_time is on); every other
/// call, GetCapabilities included (client.py:486), goes through the wsse
/// client. GetCapabilities is allowed anonymous by choice (plan §3),
/// not because the library needs it.
pub fn anonymous_ok(op: &str) -> bool {
    matches!(op, "GetSystemDateAndTime" | "GetCapabilities")
}

/// Decide whether the request may proceed.
/// - GetSystemDateAndTime: never checked (the library retries with auth only
///   if the authless call faults, and the time is not secret).
/// - Other ops: credentials are always checked when present, so a wrong
///   password or a digest user fails at the first call with a clear Fault,
///   even on GetCapabilities.
pub fn authorize(
    creds: &Credentials,
    op: &str,
    auth: &dyn Authenticator,
) -> Result<(), Fault> {
    if op == "GetSystemDateAndTime" {
        return Ok(());
    }
    match creds {
        Credentials::Absent => {
            if anonymous_ok(op) {
                Ok(())
            } else {
                Err(Fault::not_authorized(
                    "authentication required: send a WS-Security UsernameToken (PasswordText)",
                ))
            }
        }
        Credentials::Digest { .. } => Err(Fault::not_authorized(DIGEST_HELP)),
        Credentials::Malformed(m) => Err(Fault::not_authorized(format!("bad UsernameToken: {m}"))),
        Credentials::Text { user, password } => match auth.check(user, password) {
            AuthResult::Ok => Ok(()),
            AuthResult::BadCredentials => Err(Fault::not_authorized("bad username or password")),
            AuthResult::Unavailable => Err(Fault::receiver(
                &["Action"],
                "authentication backend unavailable, try again later",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::parse;

    struct A(AuthResult);
    impl Authenticator for A {
        fn check(&self, u: &str, p: &str) -> AuthResult {
            if u == "admin" && p == "test" {
                AuthResult::Ok
            } else {
                self.0
            }
        }
    }

    fn hdr(pw_attr: &str, user: &str, pw: &str) -> Node {
        let x = format!(
            r#"<s:Header xmlns:s="x" xmlns:w="w"><w:Security><w:UsernameToken><w:Username>{user}</w:Username><w:Password {pw_attr}>{pw}</w:Password></w:UsernameToken></w:Security></s:Header>"#
        );
        parse(x.as_bytes()).unwrap()
    }

    #[test]
    fn extracts_text_and_digest() {
        let h = hdr(r#"Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText""#, "admin", "test");
        assert_eq!(extract(Some(&h)), Credentials::Text { user: "admin".into(), password: "test".into() });
        let h = hdr(r#"Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest""#, "admin", "abc=");
        assert_eq!(extract(Some(&h)), Credentials::Digest { user: "admin".into() });
        let h = hdr("", "admin", "test");
        assert!(matches!(extract(Some(&h)), Credentials::Text { .. }));
        let h = hdr(r##"Type="#Weird""##, "admin", "x");
        assert!(matches!(extract(Some(&h)), Credentials::Malformed(_)));
        assert_eq!(extract(None), Credentials::Absent);
    }

    #[test]
    fn password_text_is_not_trimmed() {
        let h = hdr("", "admin", " p w ");
        assert_eq!(extract(Some(&h)), Credentials::Text { user: "admin".into(), password: " p w ".into() });
    }

    #[test]
    fn policy() {
        let a = A(AuthResult::BadCredentials);
        let ok = Credentials::Text { user: "admin".into(), password: "test".into() };
        let bad = Credentials::Text { user: "admin".into(), password: "no".into() };
        assert!(authorize(&ok, "GetProfiles", &a).is_ok());
        assert!(authorize(&bad, "GetProfiles", &a).is_err());
        // bad password fails even on the anonymous-capable GetCapabilities
        assert!(authorize(&bad, "GetCapabilities", &a).is_err());
        // but never on GetSystemDateAndTime
        assert!(authorize(&bad, "GetSystemDateAndTime", &a).is_ok());
        assert!(authorize(&Credentials::Absent, "GetSystemDateAndTime", &a).is_ok());
        assert!(authorize(&Credentials::Absent, "GetCapabilities", &a).is_ok());
        for op in ["GetProfiles", "GetServices", "GetStatus", "RelativeMove", "GetDeviceInformation"] {
            let f = authorize(&Credentials::Absent, op, &a).unwrap_err();
            assert_eq!(f.subcodes, vec!["NotAuthorized"]);
        }
        let f = authorize(&Credentials::Digest { user: "admin".into() }, "GetCapabilities", &a).unwrap_err();
        assert!(f.reason.contains("tls_insecure: true"));
        let f = authorize(&bad, "GetStatus", &A(AuthResult::Unavailable)).unwrap_err();
        assert!(!f.sender);
    }
}

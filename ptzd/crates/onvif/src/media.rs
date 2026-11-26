//! Media service (ver10/media/wsdl). Plan §3 rows: GetProfiles,
//! GetVideoSources, GetStreamUri; plus GetProfile.

use crate::device::Ctx;
use crate::soap::*;
use crate::xml::Node;
use ptzd_core::Lens;

pub const LENSES: [Lens; 2] = [Lens::Wide, Lens::Tele];

/// Tokens are the lens tokens (plan §6: profiles are matched by token in
/// Frigate 0.18.3, onvif.py:244).
pub fn video_source_token(l: Lens) -> String {
    format!("vs_{}", l.token())
}
pub fn ptz_config_token(l: Lens) -> String {
    format!("ptzcfg_{}", l.token())
}
pub const PTZ_NODE_TOKEN: &str = "ptz_node";

pub fn handle(c: &Ctx, op: &Node) -> Result<String, Fault> {
    match op.name.as_str() {
        "GetProfiles" => {
            let mut o = String::from("<trt:GetProfilesResponse>");
            for l in LENSES {
                o += &profile_xml(c, l, "trt:Profiles");
            }
            Ok(o + "</trt:GetProfilesResponse>")
        }
        "GetProfile" => {
            let l = lens_arg(op)?;
            Ok(format!("<trt:GetProfileResponse>{}</trt:GetProfileResponse>", profile_xml(c, l, "trt:Profile")))
        }
        "GetVideoSources" => {
            let mut o = String::from("<trt:GetVideoSourcesResponse>");
            for l in LENSES {
                let s = c.sh.cfg.stream(l);
                o += &format!(
                    "<trt:VideoSources token=\"{}\"><tt:Framerate>{}</tt:Framerate>\
<tt:Resolution><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:Resolution></trt:VideoSources>",
                    video_source_token(l),
                    s.fps,
                    s.width,
                    s.height
                );
            }
            Ok(o + "</trt:GetVideoSourcesResponse>")
        }
        "GetStreamUri" => {
            let l = lens_arg(op)?;
            Ok(format!(
                "<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>{}</tt:Uri>\
<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>\
<tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout>\
</trt:MediaUri></trt:GetStreamUriResponse>",
                esc(&c.sh.cfg.stream(l).rtsp_url)
            ))
        }
        other => Err(Fault::unsupported(other)),
    }
}

/// ProfileToken -> lens, or the ONVIF NoProfile fault.
pub fn lens_arg(op: &Node) -> Result<Lens, Fault> {
    let t = op
        .child_text("ProfileToken")
        .ok_or_else(|| Fault::invalid_arg("ProfileToken missing"))?;
    Lens::from_token(t).ok_or_else(|| {
        Fault::sender(&["InvalidArgVal", "NoProfile"], format!("no such profile: {t:?} (use wide or tele)"))
    })
}

/// PTZConfiguration, shared by GetProfiles and the PTZ service.
/// Element order follows tt:PTZConfiguration. Absolute space is advertised
/// as generic position; no zoom spaces (plan §3, D2). `DefaultAbsolutePantTiltPositionSpace`
/// keeps the ONVIF schema's own spelling.
pub fn ptz_configuration(l: Lens) -> String {
    format!(
        "<tt:PTZConfiguration token=\"{tok}\"><tt:Name>{n}</tt:Name><tt:UseCount>1</tt:UseCount>\
<tt:NodeToken>{node}</tt:NodeToken>\
<tt:DefaultAbsolutePantTiltPositionSpace>{abs}</tt:DefaultAbsolutePantTiltPositionSpace>\
<tt:DefaultRelativePanTiltTranslationSpace>{rel}</tt:DefaultRelativePanTiltTranslationSpace>\
<tt:DefaultContinuousPanTiltVelocitySpace>{cont}</tt:DefaultContinuousPanTiltVelocitySpace>\
</tt:PTZConfiguration>",
        tok = ptz_config_token(l),
        n = ptz_config_token(l),
        node = PTZ_NODE_TOKEN,
        abs = crate::ptz::SPACE_ABS,
        rel = crate::ptz::SPACE_FOV,
        cont = crate::ptz::SPACE_VEL
    )
}

fn profile_xml(c: &Ctx, l: Lens, tag: &str) -> String {
    let s = c.sh.cfg.stream(l);
    let t = l.token();
    format!(
        "<{tag} token=\"{t}\" fixed=\"true\"><tt:Name>{t}</tt:Name>\
<tt:VideoSourceConfiguration token=\"vsc_{t}\"><tt:Name>vsc_{t}</tt:Name><tt:UseCount>1</tt:UseCount>\
<tt:SourceToken>{vs}</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/></tt:VideoSourceConfiguration>\
<tt:VideoEncoderConfiguration token=\"venc_{t}\"><tt:Name>venc_{t}</tt:Name><tt:UseCount>1</tt:UseCount>\
<tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
<tt:Quality>1</tt:Quality><tt:RateControl><tt:FrameRateLimit>{fps}</tt:FrameRateLimit>\
<tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>0</tt:BitrateLimit></tt:RateControl>\
<tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address>\
<tt:Port>0</tt:Port><tt:TTL>0</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast>\
<tt:SessionTimeout>PT0S</tt:SessionTimeout></tt:VideoEncoderConfiguration>\
{ptz}</{tag}>",
        vs = video_source_token(l),
        w = s.width,
        h = s.height,
        fps = s.fps,
        ptz = ptz_configuration(l)
    )
}

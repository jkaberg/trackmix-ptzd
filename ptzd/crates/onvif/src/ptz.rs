//! PTZ service (ver20/ptz/wsdl). Plan §3 rows: GetConfigurationOptions,
//! GetServiceCapabilities, GetStatus, GetPresets, GotoPreset, ContinuousMove,
//! Stop, RelativeMove (FOV), AbsoluteMove, GetNodes; plus SetPreset,
//! RemovePreset, GetNode.

use crate::device::Ctx;
use crate::media::{lens_arg, ptz_config_token, LENSES, PTZ_NODE_TOKEN};
use crate::soap::*;
use crate::xml::Node;
use ptzd_core::PtzError;

/// ONVIF core spec pre-defined PanTilt spaces. Frigate detects FOV support by
/// the substring "TranslationSpaceFov" (onvif.py:335).
pub const SPACE_FOV: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationSpaceFov";
pub const SPACE_VEL: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace";
pub const SPACE_ABS: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace";

fn map_err(e: PtzError) -> Fault {
    match e {
        PtzError::InvalidArg(s) => Fault::invalid_arg(s),
        PtzError::NoSuchPreset(s) => {
            Fault::sender(&["InvalidArgVal", "NoToken"], format!("no such preset: {s}"))
        }
        PtzError::NotReady(s) => Fault::receiver(&["Action"], format!("not ready: {s}")),
        PtzError::Hw(e) => Fault::receiver(&["Action"], format!("hardware: {e}")),
    }
}

pub fn handle(c: &Ctx, op: &Node) -> Result<String, Fault> {
    let ptz = &*c.sh.ptz;
    match op.name.as_str() {
        // Plan §3: MoveStatus="true" (onvif.py:975 reads it, autotrack.py:353-361 gates on it).
        "GetServiceCapabilities" => Ok("<tptz:GetServiceCapabilitiesResponse>\
<tptz:Capabilities MoveStatus=\"true\" StatusPosition=\"true\" EFlip=\"false\" \
Reverse=\"false\" GetCompatibleConfigurations=\"false\"/></tptz:GetServiceCapabilitiesResponse>"
            .to_string()),
        "GetNodes" => Ok(format!("<tptz:GetNodesResponse>{}</tptz:GetNodesResponse>", node_xml(c))),
        "GetNode" => Ok(format!("<tptz:GetNodeResponse>{}</tptz:GetNodeResponse>", node_xml(c))),
        "GetConfigurationOptions" => {
            // ConfigurationToken is optional in practice; unknown tokens fault.
            if let Some(t) = op.child_text("ConfigurationToken") {
                if !t.is_empty() && !LENSES.iter().any(|l| ptz_config_token(*l) == t) {
                    return Err(Fault::sender(
                        &["InvalidArgVal", "NoConfig"],
                        format!("no such PTZ configuration: {t:?}"),
                    ));
                }
            }
            Ok(config_options(c))
        }
        "GetStatus" => {
            lens_arg(op)?;
            let s = ptz.status();
            // Position.PanTilt must always be present: Frigate dereferences it
            // unguarded (onvif.py:385, plan §3 hard requirements).
            Ok(format!(
                "<tptz:GetStatusResponse><tptz:PTZStatus><tt:Position>\
<tt:PanTilt x=\"{}\" y=\"{}\" space=\"{SPACE_ABS}\"/></tt:Position>\
<tt:MoveStatus><tt:PanTilt>{}</tt:PanTilt></tt:MoveStatus>\
<tt:UtcTime>{}</tt:UtcTime></tptz:PTZStatus></tptz:GetStatusResponse>",
                fin(s.pan),
                fin(s.tilt),
                if s.moving { "MOVING" } else { "IDLE" },
                utc_iso()
            ))
        }
        "GetPresets" => {
            lens_arg(op)?;
            let mut o = String::from("<tptz:GetPresetsResponse>");
            for p in ptz.presets() {
                o += &format!(
                    "<tptz:Preset token=\"{}\"><tt:Name>{}</tt:Name><tt:PTZPosition>\
<tt:PanTilt x=\"{}\" y=\"{}\" space=\"{SPACE_ABS}\"/></tt:PTZPosition></tptz:Preset>",
                    esc(&p.token),
                    esc(&p.name),
                    fin(p.pan),
                    fin(p.tilt)
                );
            }
            Ok(o + "</tptz:GetPresetsResponse>")
        }
        "GotoPreset" => {
            lens_arg(op)?;
            let t = op
                .child_text("PresetToken")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| Fault::invalid_arg("PresetToken missing"))?;
            ptz.goto_preset(t).map_err(map_err)?;
            Ok("<tptz:GotoPresetResponse/>".into())
        }
        "SetPreset" => {
            lens_arg(op)?;
            let name = op.child_text("PresetName").filter(|s| !s.is_empty());
            let tok = op.child_text("PresetToken").filter(|s| !s.is_empty());
            let t = ptz.set_preset(name, tok).map_err(map_err)?;
            Ok(format!(
                "<tptz:SetPresetResponse><tptz:PresetToken>{}</tptz:PresetToken></tptz:SetPresetResponse>",
                esc(&t)
            ))
        }
        "RemovePreset" => {
            lens_arg(op)?;
            let t = op
                .child_text("PresetToken")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| Fault::invalid_arg("PresetToken missing"))?;
            ptz.remove_preset(t).map_err(map_err)?;
            Ok("<tptz:RemovePresetResponse/>".into())
        }
        "ContinuousMove" => {
            lens_arg(op)?;
            let pt = op
                .child("Velocity")
                .and_then(|v| v.child("PanTilt"))
                .ok_or_else(|| Fault::invalid_arg("Velocity/PanTilt missing (no zoom support)"))?;
            check_space(pt, "VelocityGenericSpace")?;
            let (x, y) = xy_unit(pt)?;
            // Frigate sends no Timeout for the UI MOVE_* (onvif.py:574-610).
            let timeout = match op.child_text("Timeout").filter(|t| !t.is_empty()) {
                None => None,
                Some(t) => Some(
                    parse_duration(t)
                        .ok_or_else(|| Fault::invalid_arg(format!("bad Timeout duration: {t:?}")))?,
                ),
            };
            ptz.continuous_move(x, y, timeout).map_err(map_err)?;
            Ok("<tptz:ContinuousMoveResponse/>".into())
        }
        "Stop" => {
            lens_arg(op)?;
            // Frigate sends PanTilt=true Zoom=true (onvif.py:550-557). Zoom is
            // ignored; any Stop stops both motors.
            ptz.stop().map_err(map_err)?;
            Ok("<tptz:StopResponse/>".into())
        }
        "RelativeMove" => {
            let lens = lens_arg(op)?;
            if let Some(t) = c.sh.cfg.tracker_profile {
                if t != lens {
                    return Err(Fault::sender(
                        &["InvalidArgVal"],
                        format!(
                            "RelativeMove is only accepted on profile {:?} (one head, one tracker)",
                            t.token()
                        ),
                    ));
                }
            }
            let pt = op
                .child("Translation")
                .and_then(|v| v.child("PanTilt"))
                .ok_or_else(|| Fault::invalid_arg("Translation/PanTilt missing"))?;
            // A missing space means the configured default, which is FOV.
            check_space(pt, "TranslationSpaceFov")?;
            let (x, y) = xy_unit(pt)?;
            let speed = speed_of(op)?;
            ptz.relative_move(lens, x, y, speed).map_err(map_err)?;
            Ok("<tptz:RelativeMoveResponse/>".into())
        }
        "AbsoluteMove" => {
            lens_arg(op)?;
            let pt = op
                .child("Position")
                .and_then(|v| v.child("PanTilt"))
                .ok_or_else(|| Fault::invalid_arg("Position/PanTilt missing (no zoom support)"))?;
            check_space(pt, "PositionGenericSpace")?;
            let (x, y) = xy_unit(pt)?;
            let speed = speed_of(op)?;
            ptz.absolute_move(x, y, speed).map_err(map_err)?;
            Ok("<tptz:AbsoluteMoveResponse/>".into())
        }
        other => Err(Fault::unsupported(other)),
    }
}

fn fin(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// If the request names a space it must be the supported one.
fn check_space(n: &Node, needle: &str) -> Result<(), Fault> {
    match n.attr("space") {
        Some(s) if !s.is_empty() && !s.contains(needle) => Err(Fault::sender(
            &["InvalidArgVal", "SpaceNotSupported"],
            format!("unsupported space {s:?}, expected *{needle}"),
        )),
        _ => Ok(()),
    }
}

/// x/y attributes, each finite and within -1..1 (the advertised ranges;
/// Frigate interpolates through them, onvif.py:644-657).
fn xy_unit(n: &Node) -> Result<(f32, f32), Fault> {
    let get = |k: &str| -> Result<f32, Fault> {
        let v = parse_f32(n.attr(k).ok_or_else(|| Fault::invalid_arg(format!("{k} missing")))?)?;
        if !(-1.0..=1.0).contains(&v) {
            return Err(Fault::sender(
                &["InvalidArgVal", "InvalidPosition"],
                format!("{k}={v} outside -1..1"),
            ));
        }
        Ok(v)
    };
    Ok((get("x")?, get("y")?))
}

/// Speed/PanTilt -> one 0..1 value (the larger axis magnitude). Frigate always
/// sends 1 (autotrack.py:570); a Zoom speed alone is ignored.
fn speed_of(op: &Node) -> Result<Option<f32>, Fault> {
    let pt = match op.child("Speed").and_then(|s| s.child("PanTilt")) {
        Some(p) => p,
        None => return Ok(None),
    };
    let x = parse_f32(pt.attr("x").unwrap_or("0"))?.abs();
    let y = parse_f32(pt.attr("y").unwrap_or(pt.attr("x").unwrap_or("0")))?.abs();
    let v = x.max(y);
    if v > 1.0 {
        return Err(Fault::invalid_arg(format!("speed {v} outside 0..1")));
    }
    Ok(Some(v))
}

/// tt:PTZSpaces. Ranges are finite -1..1 as Frigate needs (plan §3,
/// onvif.py:644). No zoom spaces (D2).
fn config_options(_c: &Ctx) -> String {
    let rng = "<tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange>\
<tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange>";
    format!(
        "<tptz:GetConfigurationOptionsResponse><tptz:PTZConfigurationOptions><tt:Spaces>\
<tt:AbsolutePanTiltPositionSpace><tt:URI>{SPACE_ABS}</tt:URI>{rng}</tt:AbsolutePanTiltPositionSpace>\
<tt:RelativePanTiltTranslationSpace><tt:URI>{SPACE_FOV}</tt:URI>{rng}</tt:RelativePanTiltTranslationSpace>\
<tt:ContinuousPanTiltVelocitySpace><tt:URI>{SPACE_VEL}</tt:URI>{rng}</tt:ContinuousPanTiltVelocitySpace>\
</tt:Spaces><tt:PTZTimeout><tt:Min>PT0S</tt:Min><tt:Max>PT{}S</tt:Max></tt:PTZTimeout>\
</tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>",
        c_timeout_secs(_c)
    )
}

fn c_timeout_secs(c: &Ctx) -> u64 {
    c.sh.cfg.max_continuous_timeout.as_secs()
}

fn node_xml(c: &Ctx) -> String {
    let rng = "<tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange>\
<tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange>";
    format!(
        "<tptz:PTZNode token=\"{PTZ_NODE_TOKEN}\" FixedHomePosition=\"false\"><tt:Name>{PTZ_NODE_TOKEN}</tt:Name>\
<tt:SupportedPTZSpaces>\
<tt:AbsolutePanTiltPositionSpace><tt:URI>{SPACE_ABS}</tt:URI>{rng}</tt:AbsolutePanTiltPositionSpace>\
<tt:RelativePanTiltTranslationSpace><tt:URI>{SPACE_FOV}</tt:URI>{rng}</tt:RelativePanTiltTranslationSpace>\
<tt:ContinuousPanTiltVelocitySpace><tt:URI>{SPACE_VEL}</tt:URI>{rng}</tt:ContinuousPanTiltVelocitySpace>\
</tt:SupportedPTZSpaces><tt:MaximumNumberOfPresets>{}</tt:MaximumNumberOfPresets>\
<tt:HomeSupported>false</tt:HomeSupported></tptz:PTZNode>",
        c.sh.cfg.max_presets
    )
}


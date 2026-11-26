//! Device management service (ver10/device/wsdl). Plan §3 rows:
//! GetSystemDateAndTime, GetCapabilities, GetServices, GetDeviceInformation.

use crate::soap::*;
use crate::xml::Node;
use crate::Shared;

pub const PATH_DEVICE: &str = "/onvif/device_service";
pub const PATH_MEDIA: &str = "/onvif/media_service";
pub const PATH_PTZ: &str = "/onvif/ptz_service";

pub struct Ctx<'a> {
    pub sh: &'a Shared,
    /// `http://host:port`
    pub base: String,
}

impl Ctx<'_> {
    pub fn xaddr(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

pub fn handle(c: &Ctx, op: &Node) -> Result<String, Fault> {
    match op.name.as_str() {
        "GetSystemDateAndTime" => Ok(system_time()),
        "GetCapabilities" => Ok(capabilities(c, op)),
        "GetServices" => Ok(services(c, op)),
        "GetDeviceInformation" => Ok(device_info(c)),
        other => Err(Fault::unsupported(other)),
    }
}

fn system_time() -> String {
    // Plan §3: "system UTC". DateTimeType Manual: we do not run NTP claims.
    let (y, mo, d, h, mi, s) = utc_now();
    format!(
        "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
<tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings>\
<tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>\
<tt:UTCDateTime><tt:Time><tt:Hour>{h}</tt:Hour><tt:Minute>{mi}</tt:Minute><tt:Second>{s}</tt:Second></tt:Time>\
<tt:Date><tt:Year>{y}</tt:Year><tt:Month>{mo}</tt:Month><tt:Day>{d}</tt:Day></tt:Date></tt:UTCDateTime>\
</tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>"
    )
}

/// Plan §3: Media/PTZ XAddrs pointing at us, Imaging omitted. The library
/// reads only `Device`/`Media`/`PTZ`/`Imaging` XAddr (client.py:486-496).
fn capabilities(c: &Ctx, op: &Node) -> String {
    let cat = op.child_text("Category").unwrap_or("All");
    let all = cat.is_empty() || cat == "All";
    let mut o = String::from("<tds:GetCapabilitiesResponse><tds:Capabilities>");
    if all || cat == "Device" {
        o += &format!(
            "<tt:Device><tt:XAddr>{}</tt:XAddr></tt:Device>",
            esc(&c.xaddr(PATH_DEVICE))
        );
    }
    if all || cat == "Media" {
        o += &format!(
            "<tt:Media><tt:XAddr>{}</tt:XAddr><tt:StreamingCapabilities>\
<tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP>\
<tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media>",
            esc(&c.xaddr(PATH_MEDIA))
        );
    }
    if all || cat == "PTZ" {
        o += &format!("<tt:PTZ><tt:XAddr>{}</tt:XAddr></tt:PTZ>", esc(&c.xaddr(PATH_PTZ)));
    }
    o + "</tds:Capabilities></tds:GetCapabilitiesResponse>"
}

/// Same XAddrs as GetCapabilities (plan §3, "future-proofing").
fn services(c: &Ctx, _op: &Node) -> String {
    let mut o = String::from("<tds:GetServicesResponse>");
    for (ns, path) in [(NS_TDS, PATH_DEVICE), (NS_TRT, PATH_MEDIA), (NS_TPTZ, PATH_PTZ)] {
        o += &format!(
            "<tds:Service><tds:Namespace>{ns}</tds:Namespace><tds:XAddr>{}</tds:XAddr>\
<tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service>",
            esc(&c.xaddr(path))
        );
    }
    o + "</tds:GetServicesResponse>"
}

fn device_info(c: &Ctx) -> String {
    let g = &c.sh.cfg;
    format!(
        "<tds:GetDeviceInformationResponse><tds:Manufacturer>{}</tds:Manufacturer>\
<tds:Model>{}</tds:Model><tds:FirmwareVersion>{}</tds:FirmwareVersion>\
<tds:SerialNumber>{}</tds:SerialNumber><tds:HardwareId>{}</tds:HardwareId>\
</tds:GetDeviceInformationResponse>",
        esc(&g.manufacturer),
        esc(&g.model),
        esc(&g.firmware),
        esc(&g.serial),
        esc(&g.hardware_id)
    )
}

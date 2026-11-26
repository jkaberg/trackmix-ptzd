#!/usr/bin/env python
"""Drive the REAL onvif-zeep-async 4.0.4 against examples/fake_server, with the
call sequence Frigate 0.18.3 uses. Run with a python that has it (ptzd/README.md):

  "$VENV_PY" crates/onvif/tests/conformance.py

Citations "onvif.py:N" / "autotrack.py:N" are lines in Frigate v0.18.3
frigate/ptz/. "client.py:N" is onvif/client.py of onvif-zeep-async 4.0.4.
Exit status 0 only if every check passes.
"""
import asyncio
import os
import subprocess
import sys
import threading
import time
from importlib.util import find_spec
from pathlib import Path

from onvif import ONVIFCamera
from zeep.exceptions import Fault

HERE = Path(__file__).resolve().parent
WS = HERE.parents[2]  # ptzd/
FAKE = WS / "target" / "debug" / "examples" / "fake_server"
# onvif.py:156-164 (wsdl_dir, adjust_time, encrypt)
WSDL = str(Path(find_spec("onvif").origin).parent / "wsdl")

RESULTS = []


def check(name, cond, detail=""):
    RESULTS.append((name, bool(cond)))
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))
    return bool(cond)


class Server:
    def __init__(self, *args):
        self.p = subprocess.Popen([str(FAKE), *args], stdout=subprocess.PIPE, text=True, bufsize=1)
        self.calls = []
        line = self.p.stdout.readline().strip()
        assert line.startswith("LISTENING "), line
        host, port = line.split()[1].rsplit(":", 1)
        self.host, self.port = host, int(port)
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.p.stdout:
            if line.startswith("CALL "):
                self.calls.append(line[5:].strip())

    def mark(self):
        return len(self.calls)

    def since(self, m):
        time.sleep(0.05)
        return self.calls[m:]

    def close(self):
        self.p.terminate()
        self.p.wait()


def camera(srv, user="admin", pw="test", tls_insecure=True, ignore_time_mismatch=False):
    # onvif.py:156-164: ONVIFCamera(host, port, user, pw, wsdl_dir, adjust_time, encrypt=not tls_insecure)
    return ONVIFCamera(
        srv.host, srv.port, user, pw,
        wsdl_dir=WSDL,
        adjust_time=ignore_time_mismatch,
        encrypt=not tls_insecure,
    )


def interp(x, xp, fp):  # numpy.interp for two points (onvif.py:643-658)
    x0, x1 = xp
    y0, y1 = fp
    x = min(max(x, x0), x1)
    return y0 + (y1 - y0) * (x - x0) / (x1 - x0)


class Frigate:
    """Port of the parts of OnvifController the autotracker uses."""

    def __init__(self, cam, profile):
        self.cam = cam
        self.profile_name = profile
        self.c = {"active": False, "presets": {}, "features": []}
        self.motor_stopped = True

    async def init_onvif(self):  # onvif.py:182-542
        onvif = self.cam
        c = self.c
        await onvif.update_xaddrs()  # onvif.py:185 (GetCapabilities, client.py:486)
        media = await onvif.create_media_service()  # :191
        onvif.get_definition("ptz")  # :196
        profiles = await media.GetProfiles()  # :205
        valid = [  # :214-222
            p for p in profiles
            if p.VideoEncoderConfiguration and p.PTZConfiguration
            and (p.PTZConfiguration.DefaultContinuousPanTiltVelocitySpace is not None
                 or p.PTZConfiguration.DefaultContinuousZoomVelocitySpace is not None)
        ]
        c["profiles"] = [{"name": getattr(p, "Name", None) or p.token, "token": p.token} for p in valid]
        profile = None
        for p in valid:  # :244 exact token first, then name
            if p.token == self.profile_name:
                profile = p
                break
        if profile is None:
            return False
        configs = profile.PTZConfiguration  # :288 (profile.PTZConfiguration)
        ptz = await onvif.create_ptz_service()  # :288
        c["ptz"] = ptz
        try:  # :292 imaging is unadvertised, must fail cleanly
            c["imaging"] = await onvif.create_imaging_service()
        except Exception as e:
            c["imaging"] = None
            c["imaging_err"] = str(e)
        try:  # :298
            vs = await media.GetVideoSources()
            c["video_source_token"] = vs[0].token if vs else None
        except Exception:
            c["video_source_token"] = None
        move_request = ptz.create_type("ContinuousMove")  # :306
        move_request.ProfileToken = profile.token
        c["move_request"] = move_request
        request = ptz.create_type("GetConfigurationOptions")  # :315-317
        request.ConfigurationToken = profile.PTZConfiguration.token
        ptz_config = await ptz.GetConfigurationOptions(request)
        c["ptz_config"] = ptz_config
        fov_space_id = next(  # :329-335
            (i for i, space in enumerate(ptz_config.Spaces.RelativePanTiltTranslationSpace)
             if "TranslationSpaceFov" in space["URI"]), None)
        c["fov_space_id"] = fov_space_id
        status_request = ptz.create_type("GetStatus")  # :350
        status_request.ProfileToken = profile.token
        c["status_request"] = status_request
        c["service_capabilities_request"] = ptz.create_type("GetServiceCapabilities")  # :354
        if fov_space_id is not None and configs.DefaultRelativePanTiltTranslationSpace is not None:  # :359-360
            one_off = ptz.create_type("GetStatus")  # :367
            one_off.ProfileToken = profile.token
            status = await ptz.GetStatus(one_off)  # :369
            rel = ptz.create_type("RelativeMove")  # :374
            rel.ProfileToken = profile.token
            fov_uri = ptz_config["Spaces"]["RelativePanTiltTranslationSpace"][fov_space_id]["URI"]  # :380
            if rel.Translation is None:  # :383
                rel.Translation = status.Position  # :385
                rel.Translation.PanTilt.space = fov_uri  # :388
            # zooming-disabled branch :420-432
            if rel["Translation"] is not None and "Zoom" in rel["Translation"]:
                del rel["Translation"]["Zoom"]
            if rel["Speed"] is not None and "Zoom" in rel["Speed"]:
                del rel["Speed"]["Zoom"]
            if rel.Speed is None:  # :434
                rel.Speed = configs.DefaultPTZSpeed if configs else None
            c["relative_move_request"] = rel
        c["absolute_move_request"] = ptz.create_type("AbsoluteMove")  # :442
        c["absolute_move_request"].ProfileToken = profile.token
        presets = await ptz.GetPresets({"ProfileToken": profile.token})  # :448
        for preset in presets:  # :453-462
            name = getattr(preset, "Name") or f"preset {preset['token']}"
            c["presets"][name.lower()] = preset["token"]
        feats = c["features"]  # :~465-
        if configs.DefaultContinuousPanTiltVelocitySpace:
            feats.append("pt")
        if configs.DefaultContinuousZoomVelocitySpace:
            feats.append("zoom")
        if configs.DefaultRelativePanTiltTranslationSpace:
            feats.append("pt-r")
        if configs.DefaultRelativeZoomTranslationSpace:
            feats.append("zoom-r")
        if configs.DefaultAbsoluteZoomPositionSpace:
            feats.append("zoom-a")
        if fov_space_id is not None and configs.DefaultRelativePanTiltTranslationSpace is not None:
            feats.append("pt-r-fov")  # :541
            c["relative_fov_range"] = ptz_config.Spaces.RelativePanTiltTranslationSpace[fov_space_id]  # :542
        c["init"] = True
        self.profile = profile
        return True

    async def stop(self):  # onvif.py:550
        mr = self.c["move_request"]
        await self.c["ptz"].Stop({"ProfileToken": mr.ProfileToken, "PanTilt": True, "Zoom": True})
        self.c["active"] = False

    async def move(self, x, y):  # onvif.py:574 (UI MOVE_*, velocity +-0.5, no Timeout)
        if self.c["active"]:
            await self.stop()
        self.c["active"] = True
        mr = self.c["move_request"]
        mr.Velocity = {"PanTilt": {"x": x, "y": y}}
        await self.c["ptz"].ContinuousMove(mr)

    async def move_relative(self, pan, tilt, speed=1):  # onvif.py:612-690 (zoom == 0)
        if self.c["active"]:
            return False
        self.c["active"] = True
        mr = self.c["relative_move_request"]
        r = self.c["relative_fov_range"]
        pan = interp(pan, [-1, 1], [r["XRange"]["Min"], r["XRange"]["Max"]])
        tilt = interp(tilt, [-1, 1], [r["YRange"]["Min"], r["YRange"]["Max"]])
        mr.Speed = {"PanTilt": {"x": speed, "y": speed}}
        mr.Translation.PanTilt.x = pan
        mr.Translation.PanTilt.y = tilt
        await self.c["ptz"].RelativeMove(mr)
        mr.Translation.PanTilt.x = 0
        mr.Translation.PanTilt.y = 0
        self.c["active"] = False
        return True

    async def move_to_preset(self, preset):  # onvif.py:692
        preset = preset.lower()
        if preset not in self.c["presets"]:
            return False
        self.c["active"] = True
        mr = self.c["move_request"]
        await self.c["ptz"].GotoPreset({"ProfileToken": mr.ProfileToken, "PresetToken": self.c["presets"][preset]})
        self.c["active"] = False
        return True

    async def service_capabilities(self):  # onvif.py:957-975
        sc = await self.c["ptz"].GetServiceCapabilities(self.c["service_capabilities_request"])
        return find_by_key(vars(sc), "MoveStatus")

    async def camera_status(self):  # onvif.py:982-1014; returns "IDLE"/"MOVING" or None
        status = await self.c["ptz"].GetStatus(self.c["status_request"])
        pan_tilt_status = getattr(status.MoveStatus, "PanTilt", None)  # :999
        zoom_status = getattr(status.MoveStatus, "Zoom", None)  # :1000
        if pan_tilt_status is None:
            return None
        self.last_status = status
        self.zoom_status = zoom_status
        if pan_tilt_status == "IDLE" and (zoom_status is None or zoom_status == "IDLE"):  # :1015
            self.c["active"] = False
            self.motor_stopped = True
        else:
            self.c["active"] = True
            self.motor_stopped = False
        return pan_tilt_status


def find_by_key(dictionary, target_key):  # frigate/util/builtin.py:361
    if target_key in dictionary:
        return dictionary[target_key]
    for value in dictionary.values():
        if isinstance(value, dict):
            result = find_by_key(value, target_key)
            if result is not None:
                return result
    return None


async def wait_idle(f, timeout=5.0):  # autotrack.py:546-548 style polling
    t0 = time.time()
    while time.time() - t0 < timeout:
        if await f.camera_status() == "IDLE":
            return True
    return False


async def run_profile(srv, profile, ignore_time_mismatch):
    tag = f"[{profile}{' adjust_time' if ignore_time_mismatch else ''}]"
    cam = camera(srv, ignore_time_mismatch=ignore_time_mismatch)
    f = Frigate(cam, profile)
    try:
        ok = await f.init_onvif()
        if not check(f"{tag} _init_onvif succeeds (encrypt=False)", ok):
            return
        c = f.c
        check(f"{tag} profiles wide+tele valid", sorted(p["token"] for p in c["profiles"]) == ["tele", "wide"], c["profiles"])
        check(f"{tag} imaging not advertised (create_imaging_service raises)", c["imaging"] is None, c.get("imaging_err"))
        check(f"{tag} video source token present", bool(c["video_source_token"]), c["video_source_token"])
        check(f"{tag} features == pt, pt-r, pt-r-fov (no zoom, no focus)", c["features"] == ["pt", "pt-r", "pt-r-fov"], c["features"])
        r = c["relative_fov_range"]
        check(f"{tag} FOV X/Y range finite -1..1 (onvif.py:643)",
              (r["XRange"]["Min"], r["XRange"]["Max"], r["YRange"]["Min"], r["YRange"]["Max"]) == (-1, 1, -1, 1))
        check(f"{tag} FOV URI contains TranslationSpaceFov", "TranslationSpaceFov" in r["URI"])
        check(f"{tag} presets include 'home' (return_preset default)", "home" in c["presets"], c["presets"])
        zr = c["ptz_config"].Spaces
        check(f"{tag} no zoom spaces advertised", not zr.RelativeZoomTranslationSpace and not zr.AbsoluteZoomPositionSpace
              and not zr.ContinuousZoomVelocitySpace)

        # autotrack.py:353-361 MoveStatus gate
        ms = await f.service_capabilities()
        check(f"{tag} GetServiceCapabilities MoveStatus == 'true' (onvif.py:975)", isinstance(ms, str) and ms.lower() == "true", ms)

        # GetStatus: Position.PanTilt always present (onvif.py:385 deref), MoveStatus.PanTilt IDLE/MOVING
        st = await f.camera_status()
        check(f"{tag} GetStatus MoveStatus.PanTilt IDLE", st == "IDLE", st)
        pos = f.last_status.Position
        check(f"{tag} GetStatus Position.PanTilt x/y present", pos.PanTilt.x is not None and pos.PanTilt.y is not None)
        check(f"{tag} GetStatus zoom status None (Zoom absent)", f.zoom_status is None)

        # RelativeMove, FOV space (autotrack.py:570 calls _move_relative(camera, pan, tilt, 0, 1))
        m = srv.mark()
        await f.move_relative(0.5, -0.25, 1)
        calls = srv.since(m)
        check(f"{tag} RelativeMove reaches PtzControl with lens + FOV args",
              calls == [f"relative_move {profile} 0.5 -0.25 Some(1.0)"], calls)
        check(f"{tag} status MOVING right after RelativeMove", await f.camera_status() == "MOVING")
        check(f"{tag} status returns IDLE", await wait_idle(f))
        check(f"{tag} Position moved (pan 0.25, tilt -0.125)",
              abs(f.last_status.Position.PanTilt.x - 0.25) < 1e-3 and abs(f.last_status.Position.PanTilt.y + 0.125) < 1e-3,
              (f.last_status.Position.PanTilt.x, f.last_status.Position.PanTilt.y))
        # request object reuse: Frigate resets x/y to 0 and re-sends; second call must work
        m = srv.mark()
        await f.move_relative(0, 0, 1)
        check(f"{tag} second RelativeMove on reused request (0,0)", srv.since(m) == [f"relative_move {profile} 0 0 Some(1.0)"])
        await wait_idle(f)

        # UI jog: _move then _stop (onvif.py:574-610, 550-566)
        for cmd, (x, y) in {"left": (-0.5, 0), "right": (0.5, 0), "up": (0, 0.5), "down": (0, -0.5)}.items():
            m = srv.mark()
            await f.move(x, y)
            calls = srv.since(m)
            check(f"{tag} ContinuousMove {cmd} no Timeout", len(calls) == 1 and calls[0].startswith("continuous_move") and calls[0].endswith(" None"), calls)
            check(f"{tag} MOVING during continuous {cmd}", await f.camera_status() == "MOVING")
            m = srv.mark()
            await f.stop()  # Stop with PanTilt=True, Zoom=True
            check(f"{tag} Stop(PanTilt,Zoom=true) accepted", srv.since(m) == ["stop"])
            check(f"{tag} IDLE after Stop ({cmd})", await wait_idle(f))

        # presets: GotoPreset home (autotrack.py:553-557, 586-589)
        m = srv.mark()
        check(f"{tag} GotoPreset home", await f.move_to_preset("HOME") and srv.since(m) == ["goto_preset 1"])
        check(f"{tag} IDLE after preset", await wait_idle(f))

        # autotrack.py:431-600 calibration loop (30 steps, pan=tilt=step in 0..1)
        steps = [i / 29 for i in range(30)]
        durations = []
        m = srv.mark()
        for s in steps:
            t0 = time.time()
            await f.move_relative(s, s, 1)
            await f.camera_status()
            while not f.motor_stopped:  # autotrack.py:546 wait for motor_stopped
                await f.camera_status()
            durations.append(time.time() - t0)
            await f.move_to_preset("home")  # :556
            await f.camera_status()
            while not f.motor_stopped:
                await f.camera_status()
        calls = srv.since(m)
        rel = [x for x in calls if x.startswith("relative_move")]
        check(f"{tag} calibration: 30 RelativeMoves + 30 GotoPreset round trips",
              len(rel) == 30 and sum(x.startswith("goto_preset") for x in calls) == 30, len(calls))
        check(f"{tag} calibration: last move was pan=tilt=1.0 (full FOV/2)", rel[-1].split()[2:4] == ["1", "1"], rel[-1])
        check(f"{tag} calibration: every move saw MOVING then IDLE (durations > 0)", all(d > 0.05 for d in durations), min(durations))

        # error paths, as Frigate would see them
        try:
            await c["ptz"].GotoPreset({"ProfileToken": profile, "PresetToken": "nope"})
            check(f"{tag} GotoPreset unknown token faults", False)
        except Fault as e:
            check(f"{tag} GotoPreset unknown token faults", "no such preset" in str(e), e)
        try:
            await c["ptz"].GetStatus({"ProfileToken": "bogus"})
            check(f"{tag} unknown profile faults", False)
        except Fault as e:
            check(f"{tag} unknown profile faults", "no such profile" in str(e), e)
    finally:
        await cam.close()


async def run_misc(srv):
    cam = camera(srv, ignore_time_mismatch=True)
    try:
        await cam.update_xaddrs()  # adjust_time: authless GetSystemDateAndTime (client.py:466), then authed GetCapabilities
        check("adjust_time path: dt_diff computed from authless GetSystemDateAndTime", cam.dt_diff is not None)
        check("xaddrs: media + ptz advertised, imaging absent",
              any("media" in k for k in cam.xaddrs) and any("ptz" in k for k in cam.xaddrs)
              and not any("imaging" in k for k in cam.xaddrs), cam.xaddrs)
        dev = await cam.create_devicemgmt_service()
        info = await dev.GetDeviceInformation()
        check("GetDeviceInformation", info.Manufacturer == "Reolink", info)
        svcs = await dev.GetServices({"IncludeCapability": False})
        check("GetServices lists device/media/ptz", len(svcs) == 3, svcs)
        media = await cam.create_media_service()
        for lens in ("wide", "tele"):
            su = await media.GetStreamUri({"StreamSetup": {"Stream": "RTP-Unicast", "Transport": {"Protocol": "RTSP"}}, "ProfileToken": lens})
            check(f"GetStreamUri {lens}", su.Uri.startswith("rtsp://"), su)
        ptz = await cam.create_ptz_service()
        nodes = await ptz.GetNodes()
        check("GetNodes returns one node with FOV space",
              len(nodes) == 1 and "TranslationSpaceFov" in nodes[0].SupportedPTZSpaces.RelativePanTiltTranslationSpace[0].URI, nodes)
        # AbsoluteMove pan/tilt ok, zoom-only refused (plan §3)
        m = srv.mark()
        await ptz.AbsoluteMove({"ProfileToken": "wide", "Position": {"PanTilt": {"x": 0.2, "y": -0.2}}})
        check("AbsoluteMove pan/tilt", srv.since(m) == ["absolute_move 0.2 -0.2 None"], srv.calls[m:])
        try:
            await ptz.AbsoluteMove({"ProfileToken": "wide", "Position": {"Zoom": {"x": 0.5}}})
            check("AbsoluteMove zoom-only faults", False)
        except Fault:
            check("AbsoluteMove zoom-only faults", True)
        # SetPreset / RemovePreset
        tok = await ptz.SetPreset({"ProfileToken": "wide", "PresetName": "gate"})
        check("SetPreset returns token", bool(getattr(tok, "PresetToken", tok)), tok)
        pre = await ptz.GetPresets({"ProfileToken": "wide"})
        check("GetPresets lists home + gate", sorted(p.Name for p in pre) == ["gate", "home"], pre)
        await ptz.RemovePreset({"ProfileToken": "wide", "PresetToken": getattr(tok, "PresetToken", tok)})
        pre = await ptz.GetPresets({"ProfileToken": "wide"})
        check("RemovePreset", [p.Name for p in pre] == ["home"], pre)

        # auth: only GetSystemDateAndTime and GetCapabilities work authless
        r = await dev.authless_GetSystemDateAndTime()
        check("authless GetSystemDateAndTime works", r.UTCDateTime.Date.Year >= 2024, r)
        try:
            r = await dev.authless_GetCapabilities({"Category": "All"})
            check("authless GetCapabilities works", r.Media.XAddr.startswith("http://"), r)
        except Fault as e:
            check("authless GetCapabilities works", False, e)
        for label, coro in [
            ("GetProfiles", media.authless_GetProfiles()),
            ("GetStatus", ptz.authless_GetStatus({"ProfileToken": "wide"})),
            ("GetDeviceInformation", dev.authless_GetDeviceInformation()),
            ("GetServices", dev.authless_GetServices({"IncludeCapability": False})),
            ("RelativeMove", ptz.authless_RelativeMove({"ProfileToken": "wide", "Translation": {"PanTilt": {"x": 0, "y": 0}}})),
        ]:
            try:
                await coro
                check(f"authless {label} is refused", False)
            except Fault as e:
                check(f"authless {label} is refused (NotAuthorized)", "authentication required" in str(e), e)
        check("no PtzControl call leaked from refused authless RelativeMove", not any(
            c.startswith("relative_move") for c in srv.calls[-1:]), srv.calls[-3:])
    finally:
        await cam.close()


async def run_auth_failures(srv):
    # wrong password, PasswordText (tls_insecure: true)
    cam = camera(srv, pw="wrong", tls_insecure=True)
    try:
        await cam.update_xaddrs()
        check("wrong password: update_xaddrs faults", False)
    except Fault as e:
        check("wrong password: update_xaddrs faults with 'bad username or password'", "bad username or password" in str(e), e)
    finally:
        await cam.close()
    # Frigate would log and stop: _init_onvif returns False
    f = Frigate(camera(srv, pw="wrong"), "wide")
    try:
        ok = await f.init_onvif()
    except Fault:
        ok = False  # onvif.py:184-187 catches and returns False
    check("wrong password: _init_onvif aborts (returns False)", ok is False)
    await f.cam.close()

    # unknown user
    cam = camera(srv, user="nobody")
    try:
        await cam.update_xaddrs()
        check("unknown user faults", False)
    except Fault:
        check("unknown user faults", True)
    finally:
        await cam.close()

    # PasswordDigest (tls_insecure: false => encrypt=True, onvif.py:163)
    cam = camera(srv, tls_insecure=False)
    try:
        await cam.update_xaddrs()
        check("digest: update_xaddrs faults", False)
    except Fault as e:
        check("digest: Fault text tells user to set tls_insecure: true", "tls_insecure: true" in str(e), e)
    finally:
        await cam.close()

    # digest + adjust_time: authless time still works, then the digest Fault
    cam = camera(srv, tls_insecure=False, ignore_time_mismatch=True)
    try:
        await cam.update_xaddrs()
        check("digest + adjust_time: still faults on GetCapabilities", False)
    except Fault as e:
        check("digest + adjust_time: Fault names tls_insecure", "tls_insecure: true" in str(e), e)
    finally:
        await cam.close()


async def run_tracker_gate():
    srv = Server("--tracker", "wide")
    try:
        for profile, expect_ok in (("wide", True), ("tele", False)):
            f = Frigate(camera(srv), profile)
            try:
                assert await f.init_onvif()
                m = srv.mark()
                try:
                    await f.move_relative(0.1, 0.1)
                    ok = True
                except Fault:
                    ok = False
                f.c["active"] = False
                check(f"--tracker wide: RelativeMove on {profile} {'accepted' if expect_ok else 'refused'}", ok == expect_ok, srv.since(m))
                # UI jog is allowed from both profiles (plan §6)
                m = srv.mark()
                await f.move(0.5, 0)
                check(f"--tracker wide: ContinuousMove allowed on {profile}", srv.since(m)[0].startswith("continuous_move"))
                await f.stop()
            finally:
                await f.cam.close()
    finally:
        srv.close()


async def main():
    if not FAKE.exists():
        subprocess.check_call(["cargo", "build", "-p", "ptzd-onvif", "--example", "fake_server", "--offline"], cwd=WS)
    srv = Server()
    try:
        print(f"fake_server on {srv.host}:{srv.port}")
        for profile in ("wide", "tele"):
            await run_profile(srv, profile, False)
        await run_profile(srv, "wide", True)
        await run_misc(srv)
        await run_auth_failures(srv)
    finally:
        srv.close()
    await run_tracker_gate()
    bad = [n for n, ok in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(bad)}/{len(RESULTS)} checks passed")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))

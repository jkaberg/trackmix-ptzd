#!/usr/bin/env python
"""End-to-end checks of `ptzd run --mock` with the real onvif-zeep-async 4.0.4.

  tests/e2e.sh            (builds; VENV_PY = python with the library)

conformance.py asserts on the fake_server's CALL log, which ptzd does not print,
so only its call-log-free pieces (Frigate port, run_auth_failures) are reused
here. Everything else is re-checked against the real stack: Fake -> Motion ->
Guarded<KernelSim::realtime()> -> MockCamera.
"""
import asyncio
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "crates" / "onvif" / "tests"))
import conformance as C  # noqa: E402
from zeep.exceptions import Fault  # noqa: E402

PTZD = os.environ.get("PTZD", str(HERE.parent / "target" / "debug" / "ptzd"))
check = C.check
TMP = Path(tempfile.mkdtemp(prefix="ptzd-e2e-"))
DAEMONS = []


class StepTimeout(Exception):
    pass


def step(name, fn, *args, timeout=60):
    """Run one e2e step under its own wall-clock limit (SIGALRM, main thread). A hang
    fails with the step's name instead of blocking the run."""
    def on_alarm(*_):
        raise StepTimeout(name)
    old = signal.signal(signal.SIGALRM, on_alarm)
    signal.setitimer(signal.ITIMER_REAL, timeout)
    t0 = time.time()
    print(f"--- step {name} (limit {timeout}s)", flush=True)
    try:
        r = fn(*args)
        if asyncio.iscoroutine(r):
            r = asyncio.run(r)
        return r
    except StepTimeout:
        check(f"step {name} finished within {timeout}s", False)
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old)
        print(f"--- step {name} took {time.time() - t0:.1f}s", flush=True)


class Ext:  # what conformance.camera() needs
    def __init__(self, host, port):
        self.host, self.port = host, port


class Daemon:
    def __init__(self, name, conf):
        self.dir = TMP / name
        self.dir.mkdir(parents=True, exist_ok=True)
        self.state = self.dir / "state"
        self.ai_cfg = self.dir / "ai.cfg"
        self.conf = self.dir / "ptzd.conf"
        base = {
            "runtime_dir": str(self.dir / "run"),
            "state_dir": str(self.state),
            "ai_cfg": str(self.ai_cfg),
            "ptz_cfg": str(self.dir / "0_ptz.cfg"),
            "persist_debounce_secs": "0.2",
            "device_home_wait_secs": "0.5",
            "log_level": "debug",
        }
        base.update(conf)
        self.conf.write_text("".join(f"{k} = {v}\n" for k, v in base.items()))
        self.lines = []
        self.p = None
        DAEMONS.append(self)

    def start(self):
        self.t0 = time.time()
        self.lines.clear()
        self.p = subprocess.Popen([PTZD, "run", "--mock", "--config", str(self.conf)],
                                  stderr=subprocess.PIPE, text=True, bufsize=1)
        threading.Thread(target=lambda: [self.lines.append(l.rstrip()) for l in self.p.stderr], daemon=True).start()
        a = b = None
        for _ in range(200):
            time.sleep(0.05)
            a = self.find(r"onvif listening on (\S+)")
            b = self.find(r"mock reolink api on (\S+)")
            if a and b:
                break
        if not (a and b):
            raise RuntimeError("ptzd did not report its listen addresses:\n" + "\n".join(self.lines[-10:]))
        self.onvif = Ext(*a.rsplit(":", 1)[0:1], int(a.rsplit(":", 1)[1]))
        self.api = b
        return self

    def find(self, rx):
        for l in self.lines:
            m = re.search(rx, l)
            if m:
                return m.group(1)
        return None

    def ctl(self, *args):
        r = subprocess.run([PTZD, "ctl", "--config", str(self.conf), *args], capture_output=True, text=True, timeout=15)
        return r.returncode, r.stdout, r.stderr

    def status(self):
        rc, out, _ = self.ctl("status")
        return dict(l.split(" ", 1) if " " in l else (l, "") for l in out.splitlines()) if rc == 0 else {}

    def wait_ready(self, timeout=20):
        t0 = time.time()
        while time.time() - t0 < timeout:
            if self.status().get("ready") == "true":
                return time.time() - self.t0
            time.sleep(0.1)
        return None

    def kill(self):
        if self.p and self.p.poll() is None:
            self.p.kill()
            self.p.wait()

    def term(self):
        self.p.send_signal(signal.SIGTERM)
        try:
            return self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()
            return None

    # Reolink mock API (same public Login the daemon uses)
    def api_call(self, cmd, param, token=None):
        url = f"http://{self.api}/cgi-bin/api.cgi?cmd={cmd}" + (f"&token={token}" if token else "")
        body = json.dumps([{"cmd": cmd, "action": 0, "param": param}]).encode()
        with urllib.request.urlopen(urllib.request.Request(url, body), timeout=5) as r:
            return json.loads(r.read())[0]

    def smart_track(self):
        tok = self.api_call("Login", {"User": {"userName": "admin", "password": "test"}})["value"]["Token"]["name"]
        return self.api_call("GetAiCfg", {"channel": 0}, tok)["value"]["bSmartTrack"], tok


def fit(xs, ys):  # least squares y = a + b x (numpy lstsq equivalent)
    n = len(xs)
    sx, sy = sum(xs), sum(ys)
    sxx = sum(x * x for x in xs)
    sxy = sum(x * y for x, y in zip(xs, ys))
    b = (n * sxy - sx * sy) / (n * sxx - sx * sx)
    return (sy - b * sx) / n, b


async def frigate(d, profile="wide"):
    f = C.Frigate(C.camera(d.onvif), profile)
    ok = await f.init_onvif()
    return f, ok


async def idle(f, t=15.0):
    return await C.wait_idle(f, t)


async def calibration(d):
    """autotrack.py:431-600: 30 steps of RelativeMove(step, step, speed 1), timed until
    IDLE, GotoPreset(home) between, then lstsq on X=|pan|+|tilt| (:620-650)."""
    f, ok = await frigate(d)
    check("calib: init_onvif", ok)
    await f.move_to_preset("home")
    await f.camera_status()
    await idle(f)
    xs, ys = [], []
    n = 30
    for i in range(n):
        s = i / (n - 1)
        t0 = time.time()
        await f.move_relative(s, s, 1)
        f.motor_stopped = False  # Frigate clears motor_stopped after the move
        while not f.motor_stopped:
            await f.camera_status()
        ys.append(time.time() - t0)
        xs.append(abs(s) + abs(s))
        await f.move_to_preset("home")
        f.motor_stopped = False
        while not f.motor_stopped:
            await f.camera_status()
    a, b = fit(xs, ys)
    ok = 0.1 <= a <= 10.0 and 0 < b <= 2.0
    msg = f"calib: coefficients valid (0.1<=intercept<=10, 0<slope<=2): intercept={a:.3f}s slope={b:.3f}s/unit"
    check(msg, ok)
    print(f"CALIBRATION intercept={a:.4f}s slope={b:.4f}s/unit  first={ys[0]:.3f}s last={ys[-1]:.3f}s  y[0] (autotrack intercept)={ys[0]:.3f}s")
    await f.cam.close()
    return a, b


async def basic(d):
    f, ok = await frigate(d)
    check("init_onvif via ptzd (wide)", ok)
    c = f.c
    check("features pt, pt-r, pt-r-fov", c["features"] == ["pt", "pt-r", "pt-r-fov"], c["features"])
    check("presets include home", "home" in c["presets"], c["presets"])
    check("GetStatus IDLE once homed", await f.camera_status() == "IDLE")
    await f.move_relative(0.3, 0.2, 1)
    check("status MOVING right after RelativeMove", await f.camera_status() == "MOVING")
    check("returns to IDLE", await idle(f))
    rc, out, _ = d.ctl("preset", "set", "gate")
    check("ctl preset set gate", rc == 0 and "token" in out, out)
    pre = await c["ptz"].GetPresets({"ProfileToken": "wide"})
    check("GetPresets lists gate", "gate" in [p.Name for p in pre])
    rc, _, _ = d.ctl("preset", "rm", "gate")
    check("ctl preset rm gate", rc == 0)
    await f.move(0.5, 0)
    check("MOVING during ContinuousMove", await f.camera_status() == "MOVING")
    rc, _, _ = d.ctl("stop")
    check("ctl stop while moving", rc == 0)
    check("IDLE after ctl stop", await idle(f))
    await f.cam.close()
    await C.run_auth_failures(d.onvif)


def posfile(d):
    return json.loads((d.state / "pos").read_text())


async def limits_and_restart(d, limits):
    """Position is saved only at shutdown (W3), so each limit end is read from the
    pos file after a clean SIGTERM and the restart (marker + INIT_POS) is quick."""
    res = {}
    for name, pan in (("right", 1.0), ("left", -1.0)):
        f, _ = await frigate(d)
        ptz, tok = f.c["ptz"], f.c["move_request"].ProfileToken
        await ptz.AbsoluteMove({"ProfileToken": tok, "Position": {"PanTilt": {"x": pan, "y": 0.0}}})
        await f.camera_status()
        await idle(f)
        await f.cam.close()
        check(f"SIGTERM after {name} end exits 0", d.term() == 0)
        res[name] = posfile(d)["pan"]
        d.start()
        check(f"restart after {name} end ready", d.wait_ready() is not None)
    L, R = limits
    check(f"pan limits honoured: left end {res['left']} >= {L}, right end {res['right']} <= {R}",
          res["left"] >= L and res["right"] <= R and res["left"] < res["right"], res)
    f, _ = await frigate(d)
    ptz, tok = f.c["ptz"], f.c["move_request"].ProfileToken
    await ptz.AbsoluteMove({"ProfileToken": tok, "Position": {"PanTilt": {"x": 0.37, "y": -0.2}}})
    await f.camera_status()
    await idle(f)
    st = await ptz.GetStatus({"ProfileToken": tok})
    before = (st.Position.PanTilt.x, st.Position.PanTilt.y)
    await f.cam.close()
    time.sleep(0.5)
    rc = d.term()
    check("SIGTERM exits 0", rc == 0, rc)
    check("clean marker present after SIGTERM", (d.state / "clean").exists())
    check("ctl socket removed on exit", not (d.dir / "run" / "ctl.sock").exists())
    pos_saved = posfile(d)
    d.start()
    t = d.wait_ready()
    check(f"restart ready in {t and round(t, 2)}s (INIT_POS, no AUTOTEST seek ~4s+)", t is not None and t < 2.5, t)
    f, ok = await frigate(d)
    st = await f.c["ptz"].GetStatus({"ProfileToken": "wide"})
    after = (st.Position.PanTilt.x, st.Position.PanTilt.y)
    check(f"position restored without homing {before} -> {after}",
          abs(before[0] - after[0]) < 0.01 and abs(before[1] - after[1]) < 0.01, (before, after, pos_saved))
    check("presets survived restart (home)", "home" in f.c["presets"], f.c["presets"])
    await f.cam.close()


async def term_during_move(d):
    """SIGTERM mid-move: no clean marker (R4/W2), the next start re-homes (AUTOTEST)."""
    f, _ = await frigate(d)
    await f.move(0.5, 0)
    check("MOVING before SIGTERM", await f.camera_status() == "MOVING")
    await f.cam.close()
    check("SIGTERM during move exits 0", d.term() == 0)
    check("no clean marker after SIGTERM during a move", not (d.state / "clean").exists())
    d.start()
    t = d.wait_ready(30)
    check(f"next start re-homes (ready after {t and round(t, 1)}s, >= 3s means AUTOTEST seek)", t is not None and t >= 3.0, t)
    check("marker consumed by the boot", not (d.state / "clean").exists())


async def foreign_move(d):
    """The Reolink app / `device` jogs pan through the sim's DeviceHandle."""
    f, _ = await frigate(d)
    await idle(f)
    rc, out, err = d.ctl("sim-device-jog", "pan", "1700", "0")
    check("ctl sim-device-jog accepted", rc == 0, (out, err))
    await asyncio.sleep(0.8)  # worker idle_tick 0.5 s notices
    try:
        await f.move_relative(0.1, 0.0, 1)
        check("RelativeMove during a foreign jog Faults", False)
    except Fault as e:
        check("RelativeMove during a foreign jog Faults with the Reolink app text", "Reolink app" in str(e), e)
    rc, _, _ = d.ctl("stop")
    check("ctl stop during a foreign jog is accepted", rc == 0)
    # Our STOP must not cut the jog short: the daemon stays not-ready (foreign motion)
    # until the jog's own 350 steps at the slowest class (~4 s) are done.
    t0 = time.time()
    while time.time() - t0 < 15 and d.status().get("ready") != "true":
        await asyncio.sleep(0.2)
    dt = time.time() - t0
    check(f"foreign jog ran to its target, not stopped (not-ready lasted {dt:.1f}s more, >= 1.5s)", 1.5 <= dt < 15, dt)
    # review 2 W2: status and a new preset see the position the jog left, not the old one
    pan = float(d.status().get("pan", "nan"))
    check(f"status pan follows the foreign jog ({pan})", 0.2 < pan < 0.3, pan)
    rc, out, _ = d.ctl("preset", "set", "jogged")
    st = d.ctl("status")[1]
    row = next((l.split() for l in st.splitlines() if l.startswith("preset ") and l.split()[2:3] == ["jogged"]), None)
    check(f"ctl preset set stored the live position ({row})", rc == 0 and row is not None and 0.2 < float(row[3]) < 0.3, st)
    f.c["active"] = False
    try:
        await f.move_relative(0.1, 0.0, 1)
        check("RelativeMove accepted once the foreign jog is over", True)
    except Fault as e:
        check("RelativeMove accepted once the foreign jog is over", False, e)
    await f.cam.close()


async def two_clients(d):
    """Review 2 B1: Frigate with both lenses (plan §6): wide and tele init at the same time
    against one daemon, then keep polling both."""
    (fw, okw), (ft, okt) = await asyncio.gather(frigate(d, "wide"), frigate(d, "tele"))
    check("two clients: wide init_onvif", okw)
    check("two clients: tele init_onvif", okt)
    for _ in range(5):
        st = await asyncio.gather(fw.camera_status(), ft.camera_status())
        check("two clients: GetStatus on both", all(s in ("IDLE", "MOVING") for s in st), st)
    await asyncio.gather(fw.cam.close(), ft.cam.close())


def device_boot(d):
    """Review 2 B2, T14: `device` (sim) homes first; ptzd must only adopt: no INIT_POS or
    AUTOTEST of its own, and it logs the adoption."""
    check("device boot: ptzd adopted the calibrated position",
          any("already calibrated, adopting" in l for l in d.lines), d.lines[-5:])
    check("device boot: ptzd did not home or INIT_POS itself",
          not any("uncalibrated after the wait" in l or "homing here" in l for l in d.lines), d.lines[-5:])


def homing_failure_exits(d):
    """Review 2 W1: homing fails for good (head busy longer than home_idle_timeout): exit 1."""
    try:
        rc = d.p.wait(20)
    except subprocess.TimeoutExpired:
        rc = None
    check(f"homing failure exits non-zero for runit (rc={rc})", rc == 1, rc)


def runit_scripts():
    """finish skips the STOP on exit 0, STOPs and leaves a marker otherwise; run
    sleeps once after a failure."""
    rd = HERE.parent / "runit"
    stub = TMP / "stub.sh"
    calls = TMP / "stub.calls"
    stub.write_text(f'#!/bin/sh\necho "$@" >> {calls}\n')
    stub.chmod(0o755)
    mark = TMP / "failed"
    env = {**os.environ, "PTZD_BIN": str(stub), "PTZD_FAIL_MARK": str(mark), "PTZD_BACKOFF": "1"}

    def sh(script, *a):
        t0 = time.time()
        subprocess.run(["sh", str(rd / script), *a], env=env, check=True, timeout=20)
        return time.time() - t0

    sh("finish", "0")
    check("finish 0: no STOP, no marker", not calls.exists() and not mark.exists())
    sh("finish", "1")
    check("finish 1: STOP issued", calls.exists() and "stop" in calls.read_text())
    check("finish 1: failure marker left", mark.exists())
    calls.unlink()
    dt = sh("run")
    check(f"run after a failure sleeps first ({dt:.1f}s) then execs ptzd", dt >= 1.0 and "run" in calls.read_text())
    check("run consumed the marker", not mark.exists())
    dt = sh("run")
    check(f"run without a failure does not sleep ({dt:.1f}s)", dt < 0.8)


async def gate(d):
    """Self-mover gate. ai.cfg says smart_track on and the mock camera has it on."""
    time.sleep(8)  # homing done (sim AUTOTEST: ready after ~6.4 s in the D1 run)
    s = d.status()
    check("before any auth: moves gated, self_movers=smart_track (cfg file)",
          s.get("ready") == "false" and s.get("self_movers") == "smart_track", s)
    on, _ = d.smart_track()
    check("mock camera smart_track is ON before first auth", on == 1, on)
    f, ok = await frigate(d)  # Login -> sweep disables it
    check("first authed request ok", ok)
    on, tok = d.smart_track()
    check("auth sweep turned smart_track OFF on the camera", on == 0, on)
    check("ready after sweep (API snapshot overrides cfg file)", d.status().get("ready") == "true", d.status())
    # API snapshot older than reolink_stale_secs falls back to the (still 'on') file: Fault
    time.sleep(1.3)
    try:
        await f.move_relative(0.1, 0.1, 1)
        check("stale snapshot + cfg on: RelativeMove Faults", False)
    except Fault as e:
        check("stale snapshot + cfg on: RelativeMove Faults (self-movers active)", "self-movers active" in str(e), e)
        f.c["active"] = False
    # user turns it on in the app while the file is cleared; recheck must turn it off
    d.ai_cfg.write_text("<ai_cfg><smart_track>0</smart_track></ai_cfg>")
    d.api_call("SetAiCfg", {"bSmartTrack": 1, "channel": 0}, tok)
    t0 = time.time()
    while time.time() - t0 < 8 and d.smart_track()[0] != 0:
        time.sleep(0.25)
    check(f"recheck turned smart_track off again after {time.time() - t0:.1f}s", d.smart_track()[0] == 0)
    await asyncio.sleep(0.2)
    try:
        await f.move_relative(0.1, 0.1, 1)
        check("moves accepted after recheck", True)
    except Fault as e:
        check("moves accepted after recheck", False, e)
    await f.cam.close()


def main():
    rc = 1
    try:
        # D1: everything except the gate. Limits 800..1800 raw pan steps.
        lim = (800, 1800)
        # No sim `device` here: ptzd waits 0.5 s, then homes or INIT_POSes itself.
        d = Daemon("d1", {"mock_limit_left": lim[0], "mock_limit_right": lim[1], "reolink_recheck_secs": 600,
                          "mock_device_home_delay_secs": "off"}).start()
        print(f"ptzd onvif={d.onvif.host}:{d.onvif.port} reolink-mock={d.api}")
        step("d1 ready", lambda: print(f"ready after {d.wait_ready(30)}s"), timeout=40)
        step("basic", basic, d, timeout=60)
        step("calibration d1", calibration, d, timeout=60)
        step("limits_and_restart", limits_and_restart, d, lim, timeout=90)
        step("term_during_move", term_during_move, d, timeout=60)
        d.term()

        # D3: slower class (fast_speed 32 = class 4, div 2) for comparison
        # The sim `device` homes 0.2 s after start; the default 180 s wait ends early.
        s = Daemon("d3", {"fast_speed": 32, "device_home_wait_secs": "180"}).start()
        step("d3 ready", s.wait_ready, 30, timeout=40)
        step("device boot", device_boot, s, timeout=5)
        step("two clients", two_clients, s, timeout=40)
        step("calibration d3", calibration, s, timeout=60)
        s.term()

        # D4: foreign move
        fm = Daemon("d4", {"device_home_wait_secs": "180"}).start()
        step("d4 ready", fm.wait_ready, 30, timeout=40)
        step("foreign_move", foreign_move, fm, timeout=40)
        fm.term()

        # D2: gate. File says on; stale 1 s, recheck 4 s.
        g = Daemon("d2", {"device_home_wait_secs": "180", "mock_smart_track": "true", "reolink_recheck_secs": 4, "reolink_stale_secs": 1})
        g.ai_cfg.write_text("<ai_cfg><smart_track>1</smart_track></ai_cfg>")
        g.start()
        step("gate", gate, g, timeout=60)
        g.term()

        # review 2 W1: failing homing exits 1 (the sim device homes at once, 1 s idle limit)
        bad = Daemon("d5", {"mock_device_home_delay_secs": "0", "home_idle_timeout_secs": "1",
                            "device_home_wait_secs": "5"}).start()
        step("homing failure exits", homing_failure_exits, bad, timeout=30)
        step("runit scripts", runit_scripts, timeout=30)
        rc = 0
    except StepTimeout as e:
        print(f"FAIL step timed out: {e}")
    except Exception as e:  # noqa: BLE001
        print(f"FAIL {type(e).__name__}: {e}")
    finally:
        for dm in DAEMONS:
            dm.kill()
        shutil.rmtree(TMP, ignore_errors=True)
    bad = [n for n, ok in C.RESULTS if not ok]
    print(f"\n{len(C.RESULTS) - len(bad)}/{len(C.RESULTS)} checks passed")
    return 1 if bad or rc else 0


if __name__ == "__main__":
    sys.exit(main())

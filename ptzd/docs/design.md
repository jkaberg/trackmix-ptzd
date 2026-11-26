# ptzd: design and operation

`ptzd` is one static Rust binary that serves ONVIF PTZ to Frigate 0.18.3 and
drives the pan/tilt motors itself through `/dev/lens_zfi`. Code: `ptzd/`
(`ptzd/README.md`). Hardware facts with evidence: `research/ptzd-hardware.md` ("hw
doc" below).

Code comments cite sections as "plan §N"; numbers are stable.
**C** = confirmed from binaries or source, **U** = unproven, with the test (§7)
that settles it.

## 1. Status

- Installed over SSH as `/usr/sbin/ptzd` (sha256 `70cc4679...`), started at boot
  by the image's hook (`S99_zz_ptzd`, runsv). Verified:
  Frigate PTZ control, Reolink app pan limits and presets, survives a reboot, no
  API session leak over 3 restarts. Whether the 5-minute `starting` failsafe has
  ever fired is not known.
- Image 5435 carries `70cc4679...` and starts ptzd at boot unless
  `/mnt/para/ptzd/disabled` exists. Older images: 5433 carried `a3693b1f...` and
  started ptzd only with an `enabled` marker; 5434 was never released.
- On-camera tests: 11 of 16 settled, T7 partly, T8, T13, T15, T16 open. Results
  in §7.1. Built-in defaults (§10) match the measured values.
- Frigate autotracking works as expected (Frigate 0.18.3, pan/tilt only).
  Tracking covers the full range between the app pan limits without passing
  them and returns to the home preset after the track ends. Calibration
  (`calibrate_on_startup`) takes about 40 s; Frigate does not reset the option,
  so set it to false afterwards. A subject moving close to the camera stays
  slightly ahead of centre, a known latency limit of Frigate's ONVIF tracking.
  Not tested: centring a stationary subject.
- Not yet tested: ONVIF conformance after the logout fix (unit test and a live
  restart only).
- Five "please login first" API rejections (2026-11-25, 20:21 to 20:23) have not
  recurred since the 22:22 reboot.

No Reolink process is in the control path. `device` keeps running with motor
access (D1, §5), so ptzd is not the only writer.

## 2. Hardware in short

Details, citations and hazards H1 to H11: hw doc.

### 2.1 Platform

C: Novatek NA51055, Cortex-A9, kernel 4.19.91. Target
`armv7-unknown-linux-musleabihf`, fully static. NEON is U (T1) and not needed.

### 2.2 Pan/tilt: `/dev/lens_zfi`

- C: GPIO steppers, motor 3 = pan, motor 4 = tilt. Any other index can oops the
  kernel (H5). Open is non-exclusive and closing the fd does not stop motion.
- C: GET_INFO is pan `{3,2701,0,16}`, tilt `{4,1025,0,16}`.
- C: ioctls ptzd may use: GET_INFO, GET_STATE, GET_POS, SET_POS, STOP, INIT_POS,
  AUTOTEST, GET_CALIBRATION_STATE, SET_BACKLASH. Everything else is forbidden.
  Table: hw doc §2. The declared direction bits are unreliable.
- C: SET_POS is safe only when calibrated (H2) and idle (H3, review B1). It returns
  0 even when it rejects the move.
- C: a move that reaches 0 overtravels into the stop, 400 (pan) or 80 (tilt) steps
  (H1).
- C: speed and direction latch only at idle. Speed <= 15 is class 6, 16 to 31
  class 5, 32 to 47 class 4, >= 48 class 0. Dividers 8, 4, 2, 1 of a 1.4 ms tick;
  only class 0 ramps. Camera (T5): 87, 173, 345 and about 700
  steps per second, so one tick is about one step.
- C: STOP has no ramp. T10 settled on the camera (§7.1).
- C: AUTOTEST seeks the stop, then goes to pan 1353, tilt 100 at class 0. Done
  means GET_STATE == 0 (H7). Buffer `[motor,0,0,0,0]` (H6).
- C: backlash as `device` sets it: pan 0, tilt 16.
- Measured (T4): 8.2 pan (8.1 to 8.3) and 11.28 tilt steps per degree;
  the lens is level at raw tilt 28.

### 2.3 Zoom and focus

C: no zoom or focus motor is in use (hw doc §9). ptzd advertises neither.

## 3. What Frigate 0.18.3 needs

C, Frigate source at tag v0.18.3 and onvif-zeep-async 4.0.4 (pinned 4.0.*).

- FOV support is a substring match on `TranslationSpaceFov` (`onvif.py:335`).
- `MoveStatus` capability must be `"true"` (`onvif.py:975`).
- Pan/tilt values are `((c/size) - 0.5) * 2` (`autotrack.py:1197`): **1.0 is half
  the field of view**, positive tilt is up.
- Values are interpolated through the reported XRange/YRange
  (`onvif.py:644-657`), so ranges must be finite: -1..1.
- `calibrate_on_startup` fit bounds: 0.1 <= intercept <= 10 s and
  0 < slope <= 2 (`autotrack.py:608-648`).

| Op | service | Frigate caller | if it fails | ptzd |
|---|---|---|---|---|
| GetSystemDateAndTime | device | library, with `ignore_time_mismatch` | optional | system UTC, never authenticated |
| GetCapabilities | device | `update_xaddrs` (`client.py:486`) | **fatal** | Media and PTZ XAddrs; allowed without credentials |
| GetServices | device | newer library only | | same XAddrs |
| GetDeviceInformation | device | camera wizard | tolerated | static |
| GetProfiles | media | `_init_onvif` | **fatal** | `wide` and `tele`, each with encoder and PTZ config, no zoom spaces |
| GetProfile | media | none | | one of the above |
| GetVideoSources | media | `_init_onvif` | optional | one token per lens |
| GetStreamUri | media | camera wizard | tolerated | `wide_rtsp` / `tele_rtsp` (U, placeholders) |
| GetConfigurationOptions | ptz | `_init_onvif` | gates FOV | FOV and velocity spaces, -1..1 |
| GetServiceCapabilities | ptz | autotrack setup | gates autotracking | `MoveStatus="true"` |
| GetNodes, GetNode | ptz | wizard | tolerated | one node |
| GetStatus | ptz | init, then polled with no sleep (`onvif.py:999-1005`) | gates autotracking | MoveStatus (§4.3), `Position.PanTilt` in -1..1 |
| GetPresets, GotoPreset | ptz | `preset_<name>`, `return_preset` | empty list | own store (§4.3) |
| SetPreset, RemovePreset | ptz | never | | own store |
| ContinuousMove | ptz | UI (+-0.5, no Timeout) | UI breaks | towards the soft limit at `mid_speed`, with a dead-man |
| Stop | ptz | UI | logged | STOP ptzd's own motion only |
| RelativeMove (FOV) | ptz | autotracker, click-to-centre | logged | §4.2 |
| AbsoluteMove | ptz | zoom only | | pan/tilt works, zoom-only is a Fault |
| Imaging | imaging | focus | optional | not advertised |

Hard requirements (C):

- The device service must be at `/onvif/device_service`. The others are
  `/onvif/media_service` and `/onvif/ptz_service`.
- `GetStatus` must always carry `Position.PanTilt` (dereferenced at
  `onvif.py:384-385`).
- `Stop` arrives with `Zoom=true` (`onvif.py:550-557`); the zoom part is ignored.
- PTZConfiguration needs `DefaultContinuousPanTiltVelocitySpace` and
  `DefaultRelativePanTiltTranslationSpace` (`onvif.py:214-222`).
- Profiles are matched by token, then by name (`onvif.py:237-248`).
- Auth is WS-UsernameToken only. ptzd needs **PasswordText**, so
  `tls_insecure: true` (§9 Credentials).
- `return_preset` defaults to `"home"` (`config/camera/onvif.py:51`), preset names
  are lower-cased (`onvif.py:453-462`), and Frigate never calls SetPreset.
- Calibration moves use speed 1 (`autotrack.py:570`). At the slowest class a
  full wide move would take about 4.4 s and fail the slope bound; hence
  `fast_speed` 48 (U, T5).

Frigate config: `host` and `port: 8899`, the camera's own Reolink account,
`tls_insecure: true`, `profile: wide` or `tele`, `zooming: disabled`,
`return_preset` naming a ptzd preset, and autotracking on **one** camera only (§6).

## 4. Architecture

```
Frigate -- SOAP over HTTP :8899 --> ptzd
  onvif     HTTP/1.1, thread per connection; SOAP parse and replies
  reolink   localhost Reolink API: login, self-movers off, pan limits
  motion    the only owner of the fd: command queue, state machine, limits
  hw        /dev/lens_zfi ioctls (LensZfiFd), Guarded checks, KernelSim for tests
  core      shared types and traits
```

### 4.1 `hw`

- The only `unsafe` code. Motors are an enum, commands an allow-list (§2.2).
- `Guarded` sits between motion and the device. `set_pos` clamps to
  [min+margin, max-margin] (margin >= 1, default 20), then as its last two ioctls
  requires idle and calibrated, and sends only dir 0 or 1. INIT_POS requires
  idle and in range. A negative GET_STATE is an error, not "busy".
- U (T13): the window between the calibrated check and SET_POS when `device`
  starts an AUTOTEST exactly then.

### 4.2 RelativeMove (FOV) to steps

```
right = x * HFOV(profile) / 2            up = y * VFOV(profile) / 2
down  = (cur_tilt - tilt_level_step) / tilt_steps_per_deg   sign per tilt_reversed
pan_deg, tilt_deg = aim(right, up, down)
target = clamp(cur + round(deg * steps_per_deg))     sign per pan_reversed / tilt_reversed
```

`aim` turns the image point into a ray and returns the pan and tilt that centre it.
Looking down `d` degrees, a small sideways offset needs about `1/cos(d)` times the
pan; without this, moves at the home tilt stop about a third short. It uses the
tilt when the move is issued. Measured at tilt 546 (46 degrees
down): points 40 % off centre land within 0 to 6 % of the half-frame (wide), 0.1 %
for vertical-only moves. Tele, x only: 0 to 5 %.

Pan does not wrap. The clamp is the soft window (§4.3) intersected with the app's
pan limits (D4). Speed < 0.99 uses `mid_speed`, otherwise `fast_speed`.

### 4.3 Motion state machine

- **No live retarget** (review B1). A new request during ptzd's own move: STOP,
  wait for idle, then SET_POS. A request during foreign motion is refused.
- **Foreign motion is never stopped or interrupted** (W5): ptzd only STOPs axes it
  started.
- A move is also refused when: not homed or re-homing, failed, shutting down,
  self-movers still on or unknown (§5), or the allowed pan range is empty.
- MoveStatus is MOVING from acceptance for at least `dwell_secs` (0.15) and until
  idle plus `settle_secs` (0.15); also during re-homing and foreign motion.
- Retarget watchdog: GET_POS moving away from or past ptzd's target by more than 4
  steps means another writer changed the target: STOP, never re-issued. An axis
  that goes idle short of its target counts as interrupted and is not retried.
- Deadlines: ContinuousMove stops after `continuous_timeout_secs` (30). Any other
  move has distance / (class rate / `move_slack`) + `move_margin_secs`; on expiry
  STOP, report IDLE, and flag the axis as hung.
- Soft margin: `soft_margin` (20) steps at both ends. Safe on the camera (T3).
- Abrupt STOP may lose steps (T10), and there is no encoder. Re-home with
  `ptzd ctl rehome`, or daily with `rehome_hour_utc` (off by default). It returns
  to the position it had before.
- Files in `/mnt/para/ptzd/state/`: `presets.json` (raw steps; an unreadable file
  is renamed `presets.json.bad`), and `pos` plus `clean`, written **only** at a
  clean shutdown with both axes already idle.

### 4.4 Startup

1. Open the device; GET_INFO must match §2.2 exactly, otherwise exit (wrong model
   or firmware).
2. Read `pos` and `clean`, and delete `clean` at once: it is valid for one boot.
3. Wait up to `device_home_wait_secs` (180, U T14) for `device` to home the head.
   If it becomes calibrated, idle and unchanged for `adopt_stable_polls`, adopt its
   position. Busy for `home_idle_timeout_secs` is an error.
4. If nobody calibrated it: INIT_POS from a clean marker (verified by readback),
   else AUTOTEST pan then tilt, poll idle, require calibrated.
5. SET_BACKLASH. Create the `home` preset if missing; warn if it has less than half
   a wide FOV of headroom.
6. Serve. The Reolink sweep (§5) runs once someone has logged in.

A homing failure exits with code 1; runit waits 30 s and retries (review 2 W1).

Measured (T6): raw tilt + is down (`tilt_reversed`), pan + is right.

## 5. Living with `device`

`device` homes the motors at boot and moves them for app jogs, presets, guard
return, AI tracking and patrols (hw doc §6). The kernel does not arbitrate.
Per D1 ptzd coexists:

- At the first login and every 600 s ptzd reads the Reolink API and switches off
  what is on: auto-tracking, guard return, patrol. It writes only if needed and
  reads back.
- **Patterns** (`Tattern`) are not switched off, because the request body is
  unknown (U). They are read from `0_ptz.cfg`; if one is on, moves are refused until
  it is disabled in the app.
- Before any login, the state comes from `/mnt/para/0_ptz.cfg` and
  `/mnt/para/ai.cfg` (read-only). A missing file means nothing is configured, an
  unreadable one blocks moves. Once the API has been read it wins; after
  `reolink_stale_secs` (1800) without a read, the files count again.
- App jogs and presets stay possible. Most do STOP, wait idle, SET_POS;
  `MSG_PTZ_TOPOS` does not (T15), which is what the watchdog is for.
- ptzd uses INIT_POS and AUTOTEST only after waiting for `device`'s own homing
  (§4.4), because moving the driver's zero shifts all of `device`'s stored
  positions (hw doc §6).

Rejected alternative: hide `ptz_dev` by binding a modified dvr.xml. If revisited:
use a **non-existent** path without "tty" or "i2c" (an existing node would
receive SET_POS, "i2c" fakes success), expect the app's "set limit from current
position" to silently clear the limit, and run T7.

## 6. Two lenses, one head

- Frigate's wide and tele cameras both point at ptzd, with `profile: wide` and
  `profile: tele`. The profile in RelativeMove selects the FOV.
- Camera (T4): FOV 104 x 57.3 wide, 39.7 x 22.7 tele.
- U (T16): digital zoom on tele divides its FOV (review W3). Keep it at 1x while
  using tele PTZ.
- One head means one tracker: `tracker_profile` names the profile allowed to send
  RelativeMove; the other one gets a Fault. Default: both allowed. Stop,
  ContinuousMove and presets work from both.
- U (T2): both lenses move together, axes parallel.

## 7. On-camera tests

Results: §7.1. Run over SSH, slowest speed first, with `ptzd ctl stop` (or STOP on
both motors) ready.

- **Tool needed.** T3 to T6, T10 and T13 need raw ioctls. `ptzd ctl` has none,
  and `Guarded` would clamp T3's targets. Write a small armv7 binary on
  `ptzd_hw::LensZfiFd`. Reading `/proc/driver/gpio_motor` is safe for monitoring;
  never write it.
- Uncalibrated SET_POS is dangerous (H2): check GET_CALIBRATION_STATE first.
- T4's INIT_POS moves the driver's zero, so ptzd's and `device`'s positions are
  wrong afterwards: re-home with AUTOTEST on both motors.

| # | Test | Settles |
|---|---|---|
| T1 | `lsmod`, `dmesg \| grep -iE 'an41908\|lens\|motor'`, `ls /dev/lens_zfi /dev/nvt_isp`, `cat /proc/cpuinfo` | §2.3 no zoom motor; NEON |
| T2 | watch both streams while jogging | both lenses on the head; parallax |
| T3 | from tilt 200: SET_POS to 60/40/20/10 at speed 8, listen for a stall; same for pan from 300 | `soft_margin` |
| T4 | INIT_POS(3,0) at the stop, SET_POS 2700 at speed 8, mark the angle (or measure image shift per step); same for tilt 1024 | `pan_steps_per_deg`, `tilt_steps_per_deg`, FOV values; stop at the maximum side |
| T5 | time SET_POS from the soft margin to 2000 at speeds 8/24/40/64 (never from or to 0) | `step_rates`, `fast_speed`, `mid_speed`, MoveStatus timing |
| T6 | SET_POS tilt +100 raw; is the image going up or down? | `tilt_reversed`, `pan_reversed` |
| T7 | only if D1 is revisited: boot with a bind-mounted dvr.xml that hides `ptz_dev`; check streams, web UI, AI, watchdog uptime > 10 min | alternative in §5 |
| T8 | daemon restart while idle and while moving; power cut while moving | persistence and recovery path |
| T9 | from root on the camera, log in to the API over HTTP on 127.0.0.1 with the plain `Login {User:{userName,password}}` form; note the reply shape (`Token.name`, `leaseTime`) and any redirect. If HTTP is refused, try HTTPS 127.0.0.1:443 and FastCGI 127.0.0.1:9527 | `reolink_api`, API transport and login form. The web UI uses a digest handshake plus AES-wrapped bodies; if plain login is refused, ptzd needs that path |
| T10 | at each speed class: move 500 steps, STOP mid-move, return to a reference position, compare the image | step loss on abrupt STOP |
| T11 | `netstat -tlnp` before starting the daemon | port 8899 free; stock onvif and WS-Discovery ports |
| T12 | at 3 pan positions: `GetPtzCurPos` Ppos vs kernel GET_POS; which way does the image move | limit units (D4), `pan_left_is_low` |
| T13 | with the motor uncalibrated (fresh insmod) and idle: SET_POS with `dir=2` and a mid-range `pos`; check GET_STATE, GET_POS and `/proc/driver/gpio_motor` | is `dir=2` a true no-op uncalibrated (hw doc §3: helper returns -1, ioctl 0)? Until proven, `Guarded::set_pos` sends only dir 0/1 and re-checks calibrated right before SET_POS |
| T14 | cold boot with ptzd NOT started: log GET_STATE, GET_CALIBRATION_STATE and GET_POS of both motors every 100 ms from runsvdir start until 60 s after `device` idles; then boot with ptzd running and count AUTOTEST/INIT_POS runs (expect exactly one homing run per motor, by `device`) | `device_home_wait_secs`, `adopt_stable_polls`; ptzd must add no homing run and no position jump (review 2 B2) |
| T15 | `strace -f -e ioctl -p $(pidof device)` while: (a) calling an app preset during a ptzd move, (b) jogging in the app, (c) using every other app PTZ action | expect STOP x2, GET_STATE polls, then SET_POS x2 for preset and jog (hw doc §6). Find which action reaches `MSG_PTZ_TOPOS` (SET_POS with no STOP); that one relies on the watchdog |
| T16 | set a tele preset with digital zoom > 1x, call it, read the zoom back via the API | whether ptzd can read the live digital zoom to refuse or rescale tele moves (§6) |

T13 needs a driver that is still uncalibrated. T15 needs a static `strace`; the
image does not ship one.

### 7.1 Results (on the camera)

| # | Result |
|---|---|
| T1, T2, T9, T11, T14 | settled |
| T3 | `soft_margin` 20 is safe |
| T4 | tilt 11.28 steps/deg, pan about 8.2; `tilt_level_step` 28; FOV wide 104 x 57.3, tele 39.7 x 22.7 |
| T5 | 87, 173, 345, about 700 steps/s for classes 6, 5, 4, 0, about 3 % below the `step_rates` default |
| T6 | raw tilt + is down (`tilt_reversed`), raw pan + is right |
| T7 | partly: with `device` running, the app stores limits fine |
| T10 | settled |
| T12 | app limits are raw steps: `GetPtzCurPos` Ppos equals GET_POS. `pan_left_is_low` (LimitLeft 666 < LimitRight 1674, MOVE_LEFT lowers steps). Both limits hold exactly for moves from Frigate |
| T8, T13, T15, T16 | open |

Raw logs are not in this tree.

## 7a. Operation

Turn it on and off:

    touch /mnt/para/ptzd/disabled; sv down /mnt/tmp/sv/ptzd   # off
    rm /mnt/para/ptzd/disabled; reboot                        # on (default)

- `/etc/init.d/S99_zz_ptzd` (from `firmware/overlay/`) waits for `/mnt/para` and
  `/dev/lens_zfi`, then runs runsv on `/mnt/tmp/sv/ptzd`. If the camera died within
  5 minutes of the last start, `/mnt/para/ptzd/starting` is left behind; the
  next boot removes it and skips ptzd once, the boot after starts it again. A
  reboot loop caused by ptzd therefore alternates with a boot without it.
- Config: `/mnt/para/ptzd/ptzd.conf` if present, else `/etc/ptzd/ptzd.conf`.
- Control:
  `ptzd ctl [--config PATH] stop|status|rehome|preset set NAME|preset rm NAME`.
  `rehome` answering OK means queued. With no daemon running, `ctl stop` opens the
  device and STOPs both axes, whoever moves them.
- Shutdown: on SIGTERM/SIGINT ptzd stops its own motion and waits for idle. The
  clean marker is written only if both axes were already idle.
- A crash cannot stop the motors by itself (closing the fd is a no-op), so
  `ptzd/runit/finish` runs `ptzd ctl stop` after any non-zero exit and delays the
  restart by 30 s. After exit 0 it does nothing (review 2 R2).
- Logs: `/tmp/log/ptzd/current` (svlogd on tmpfs) and `/mnt/tmp/custom-ptzd.log`
  (boot hook). Nothing logs to flash.

## 8. Build

See `ptzd/README.md`. Plain HTTP, std threads, no async, no TLS. Release profile:
opt-level z, LTO, one codegen unit, panic abort, stripped; under 1.5 MB.

## 9. Decisions

- **D1: coexist with `device`** (§5). The app's limit setting needs `device` to
  read the live position. Residual risk: jogging from the app during tracking.
- **D2: no digital zoom**, no zoom or focus advertised.
- **D3: cover everything Frigate 0.18.3 uses** (§3), with the accounts already
  configured in the Reolink app.
- **D4: the app's pan limits are ptzd's outer limits on every move**, read from the
  API (`GetPtzTraceSection`), or from `0_ptz.cfg` before the first login. -1 means
  unset. Raw steps, unscaled (C, hw doc §7; T12 checks). There is no tilt limit in
  the app. With one side set, which step end it bounds depends on
  `pan_left_is_low` (U, T12). Limits in the
  wrong order are swapped with a warning; an empty range refuses moves.

**Credentials.** Frigate uses the camera's own Reolink account with
`tls_insecure: true`, which makes it send PasswordText (C, `onvif.py:163`,
`client.py:321`). ptzd accepts a request when a Reolink `Login` with those
credentials succeeds on localhost. Credentials and the one live session token
stay in RAM; nothing is stored. Failures are rate-limited per user and globally,
and cached logins survive a flood of bad ones. PasswordDigest gets a Fault that
names `tls_insecure: true`. The password crosses the LAN in clear, so this assumes
a trusted network. ptzd never reads the camera's account storage.

**Transport to the API.** Plain HTTP on `reolink_api` (127.0.0.1:80), with the plain
login form (T9).

## 10. Config keys

`key = value`; lines starting with `#` are comments. Unknown and duplicate keys are
errors. A missing file means all defaults (`ptzd/src/conf.rs` is authoritative).

| key | default | settled by |
|---|---|---|
| `log_level` | info | |
| `runtime_dir` | /tmp/ptzd (control socket, mode 0700) | |
| `state_dir` | /mnt/para/ptzd/state | |
| `lens_dev` | /dev/lens_zfi | |
| `soft_margin` | 20 | T3 |
| `reolink_api`, `reolink_channel` | 127.0.0.1:80, 0 | T9 |
| `ptz_cfg`, `ai_cfg` | /mnt/para/0_ptz.cfg, /mnt/para/ai.cfg | |
| `reolink_recheck_secs`, `reolink_stale_secs` | 600, 1800 | |
| `reolink_default_lease_secs`, `reolink_http_timeout_secs` | 3600, 5 | T9 |
| `auth_cache_ttl_secs` | 300 | |
| `auth_user_fail_limit`, `auth_global_fail_limit` | 3, 6 | |
| `auth_backoff_base_secs`, `auth_backoff_max_secs` | 30, 900 | |
| `onvif_bind`, `onvif_port` | 0.0.0.0, 8899 | T11 |
| `onvif_advertised_host` | from the connection | |
| `wide_rtsp`, `tele_rtsp` | placeholders | check on camera |
| `tracker_profile` | none (both may move) | |
| `onvif_max_connections`, `onvif_max_connections_per_ip` | 32, 16 | |
| `onvif_idle_timeout_secs`, `onvif_request_timeout_secs` | 60, 10 | |
| `wide_fov_h`, `wide_fov_v` | 104, 57.3 | T4 |
| `tele_fov_h`, `tele_fov_v` | 39.7, 22.7 | T4 |
| `pan_steps_per_deg`, `tilt_steps_per_deg` | 8.2, 11.28 | T4 |
| `tilt_level_step` | 28 | T4 |
| `pan_reversed`, `tilt_reversed` | false, true | T6 |
| `pan_left_is_low` | true | T12 |
| `fast_speed`, `mid_speed` | 48, 32 | T5 |
| `step_rates` | 89.3,178.6,357.1,714.3 (classes 6,5,4,0) | T5 |
| `rehome_hour_utc`, `rehome_idle_secs` | off, 600 | |
| `backlash_pan`, `backlash_tilt` | 0, 16 (cap 64) | |
| `continuous_timeout_secs`, `home_timeout_secs` | 30, 120 | |
| `home_idle_timeout_secs`, `device_home_wait_secs` | 120, 180 | T14 |
| `adopt_stable_polls`, `adopt_stable_interval_secs` | 5, 0.4 | T14 |
| `dwell_secs`, `settle_secs`, `poll_secs`, `idle_tick_secs` | 0.15, 0.15, 0.01, 0.5 | |
| `move_slack`, `move_margin_secs` | 3, 2 | T5 |
| `persist_debounce_secs` | accepted, ignored | |
| `mock_*` | only with `--mock` | |

## 11. Known limits

- Frigate autotracking cannot use zoom on the TrackMix: zoom moves only the tele
  lens, tracking runs on the fixed wide lens. Set `zooming: disabled`.
- At the pan limits the subject cannot be centred, by design.

- Reolink side, not ptzd: `SetEnc` on channel 1 (tele) rejects every GOP change
  (rspCode -4). HTTPS-FLV serves `channel0_main`, `channel0_sub`,
  `autotrack_main` and `autotrack_sub`; `channel1_*` and `ext` are not served.

- `device` can retarget a running ptzd move; ptzd notices after the fact and stops.
- After a crash the finish script STOPs both axes, including an app move in
  progress.
- On a wrong model or firmware the process exits 1 and is retried every 30 s.
- After AUTOTEST tilt is 100, which leaves less than half a wide VFOV below it
  (about 341 steps at the assumed scale, T4). Move the head somewhere better and
  run `ptzd ctl preset set home`.
- Passwords cross the LAN in clear. Anyone who can reach port 8899 can use up the
  login attempts.
- `/proc/driver/gpio_motor` is world-writable, so any local process can move the
  head without limits.

## 12. Finding ids cited in the code

Three reviews reuse the same ids. "plan review" or "review X" in a comment means the
first group, "review 2 X" the third; a bare id (R4, W5, M3) is the second group,
except bare `B1` (plan review) and `W1` in `ptzd/runit/run` (review 2).

| id | hazard | handling |
|---|---|---|
| review B1 | SET_POS during a move keeps the old direction and can run into the stop | SET_POS only at idle |
| review B2 | a zeroed AUTOTEST buffer homes nothing | `[motor,0,0,0,0]`, check rc, require calibrated |
| review W3 | tele digital zoom changes the FOV | keep 1x; T16 |
| review W4 | STOP has no ramp and may lose steps | moves end at target; T10; re-home |
| W1 | homing while `device` still moves | wait for idle, retry EBUSY |
| W2 | a clean marker reused after a later crash | deleted as soon as it is read |
| W3 | `pos` from an unclean stop | written only at clean shutdown |
| W4 | Frigate during a re-home | reports MOVING, returns to the old position |
| W5 | stopping or waiting on foreign motion | track owned axes only |
| R1 | calibrated lost between check and SET_POS | idle and calibrated are the last ioctls; T13 |
| R2 | INIT_POS ignored or clamped | readback |
| R3 | tests on an idealised driver | tests run on `Guarded<KernelSim>` |
| R4 | marker written after an abrupt STOP | none unless already idle |
| R5 | slow or many connections | absolute deadline, per-IP cap |
| R6 | login floods | atomic accounting, cached logins kept |
| R7 | one-sided pan limit | logged; T12 |
| R8 | a move that never ends | per-move deadline |
| R9 | leaked API sessions | unused tokens logged out; the held one on SIGTERM |
| M3 | unreadable presets overwritten | renamed `presets.json.bad` |
| M4 | no room around `home` for calibration | warning |
| M7 | unclear why moves are refused | log names the cfg key |
| M8 | hard-coded lease and timeout | config keys |
| review 2 B1 | Frigate with two lenses hit the per-IP cap | cap 16 |
| review 2 B2 | ptzd and `device` both homing at boot | wait for `device`, adopt |
| review 2 W1 | a daemon alive but unhomed | exit 1, runit retries |
| review 2 W2 | position stale after an app move | refreshed when idle and for `set_preset` |
| review 2 W3, R4, M4 | config values that break the maths or the poll loop | range checks |
| review 2 R1 | another writer retargets ptzd's move | watchdog STOP |
| review 2 R2 | finish script STOPs an app move after a clean stop | no STOP on exit 0 |
| review 2 R6 | queued move issued during shutdown | dropped |
| review 2 R7 | chmod on a directory ptzd did not create | refused |

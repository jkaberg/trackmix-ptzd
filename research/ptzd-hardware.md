# TrackMix pan/tilt hardware facts

What the stock firmware (5428) does with the motors, from static analysis.
Entries tagged Camera were measured on a camera.

- **C** = confirmed: the disassembly, string or source is cited.
- **U** = unproven: the test that settles it is named. T1 to T16 are in
  `ptzd/docs/design.md` §7.
- Addresses: `lens_zfi.ko` = `.text` offsets, `init` = `.init.text` offsets;
  `device` and `router` = virtual addresses (file offset + 0x10000);
  `cgiserver.cgi` = file offsets.
- `dev` = the driver's per-motor struct `g_motor_dev[m]` = bss+0x340+108*m.
  In the timer handler `r4` = dev+8, so `[r4,#X]` there is dev+X+8.
- `cfg` = the per-motor config in `.data` (Pan_motor_type7 +0xa2d4,
  Tilt_motor_type7 +0xa244).

To get the binaries: `firmware/tools/extract_pak.py firmware/stock/upgrade/*.paks <newdir>`.
Files are named by their path on the camera (`/mnt/app/device`,
`/mnt/app/lens_zfi.ko`, `/mnt/app/dvr.xml`, ...). The analysis used the 5429
rebuild, whose files equal stock 5428 except `start_app` and `version.json`
(`postmortem.md`).

## 1. Platform and modules

- C: Novatek NA51055, Cortex-A9 (device tree: `novatek,na51055`,
  `arm,cortex-a9`, `arm,pl310-cache`), kernel 4.19.91.
- U (T1): NEON. The build does not depend on it.
- C: `vg_boot.sh` is one line, `/sbin/insmod /mnt/app/lens_zfi.ko driver_type=7`.
  `dvr.xml` has `ptz_dev="/dev/lens_zfi"`, `support_ptz="3"`, `af_lens="-1"`,
  `support_digital_zoom="1"`, `reverse_pan="0"`, `reverse_tilt="1"`,
  `support_ptz_selftest="1"`, `default_ptz_speed="64"`.
- C: `driver_type` low byte 7 selects the **GPIO stepper** backend
  (`gpio_motor_*`, bit-banged from an hrtimer, handler 0x1a5c) for motor 3 =
  **pan** and motor 4 = **tilt** (init 0x38..0x94). High byte 0 means motors 1
  and 2 (LV8044 zoom/focus) are not registered (init 0xa0). GPIO pins: pan 39,
  37, 38, 36; tilt 32, 34, 33, 35 (cfg+4..+0x10).
- C: `driver_type == 7` exactly also sets `g_optocoupler_type = 1` and the timer
  interval to 1400 us (init 0x5c).
- C: insmod causes no motion. The selfcheck state is entered only if the
  `selfcheck` parameter has bit 0 set (0x239c), and `vg_boot.sh` does not pass it.
- C: after insmod a motor is enabled (dev+0x4 = 1), not calibrated
  (dev+0x5c = 0), cur = cfg+0x84 (pan 1353, tilt 100), move range =
  [cfg min, cfg max] (0x2350..0x2370).
- C: there is no UART MCU in the pan/tilt path. `device` picks the transport
  from the `ptz_dev` string (§6).

## 2. ioctl ABI of `/dev/lens_zfi`

C unless noted. Dispatcher 0x14..0x1284. Type must be `'L'` (0x4c), otherwise
-515 (userspace sees ENOTTY, 0x48). An unknown number, a copy fault or an
unregistered motor returns -14 (0x98). All fields are `u32`. The declared
direction and size bits are unreliable; use the sizes below.

| cmd | name | arg | behaviour |
|---|---|---|---|
| `0xc0104c01` | GET_INFO | `{motor,total,min,step}` 16 in/out | pan `{3,2701,0,16}`, tilt `{4,1025,0,16}` (0x29f0..0x2a10). total = cfg max + 1 |
| `0xc0044c02` | GET_STATE | `{motor}` 4 | the **return value** is 1 if state != 0, 0 if idle, negative on error (0x728, 0x2998). Read without the lock |
| `0xc0104c03` | GET_POS | 16 in/out | position in word 1, clamped to cfg [min,max], not to the move range (0x848, 0x25e4) |
| `0xc0104c04` | SET_POS | `{motor,pos,dir,speed}` 16 in | returns at once, and returns 0 even when it rejects the move (0x938, 0x273c, 0x27e8). Confirm with GET_STATE/GET_POS. See §3 |
| `0xc00c4c05` | MOVE_AUTO | `{motor,a,b}` 12 in | needs idle, else -16; b (speed) ignored, full speed, state 4 (0x2aa0). **Never send** |
| `0xc0044c06` | STOP | `{motor}` 4 | waits up to about 1 s for idle, then -16. States 5 and 6 are not interrupted (0x2b24..0x2b58). No deceleration (§3) |
| `0xc00c4c07` | GET_FOCUS_RANGE | 12 | loop bound comes from uninitialised stack. **Never send** |
| `0xc0104c08` | INIT_POS | `{motor,pos,_,_}` 16 in | no motion. Needs dev+0x4 enabled, else -14 (0x2be8); needs idle, else -16 (0x2bf8); sets calibrated = 1 and cur = target = pos clamped to the **move range** (0x2c0c) |
| `0xc0044c0a` | MOTOR_AUTOTEST | pass 20 zeroed bytes with word 0 = motor | needs enabled and idle. Clears calibrated and starts the homing seek (0x3190..0x31b4). See §3 and H6 |
| `0x40144c0e` | SET_ZF_ITEM | 20 in | stores a row of the unused zoom/focus table (0x1200) |
| `0x40084c10` | SET_BACKLASH | `{motor,val}` 8 in | writes cfg+0x74 (0x2c8c) |
| `0x40084c11` | GET_BACKLASH | 8 in/out | returns dev+0x30, the remaining countdown, not the configured value (0x2cf4) |
| `0x40084c12`, `13` | auto-calibration steps | 8 | stubs, return -1 |
| `0x40084c14` | CALIBRATION | `{motor,param}` 8 in | state 6 with no idle or enabled check; overrides a running move (0x2da0). **Never send** |
| `0x40084c15` | GET_CALIBRATION_STATE | `{motor,val}` 8 in/out | val = dev+0x5c (0x2e2c) |
| `0x40084c16` | moved steps | 8 in/out | val = dev+0x60 (0x2eb8). Name inferred |
| `0x40084c17` | TEST_MOTOR | 8 in | state 7 runs with no arrival or limit check until STOP (0x2f0c, 0x21f4). **Never send** |
| `0x400c4c18` | SET_MOTOR_MOVE_RANGE | `{motor,a,b}` 12 in | min = cfgmax\*a/D, max = cfgmax\*b/D, D = 90 for motor 4, else 360 (0x300c). min > max is not rejected. Calibrated SET_POS ignores the range (it clamps to cfg) |
| `0x80284c19`, `0x40144c1a` | opto get / calibrate | 40 / 20, word 0 must be 3 | internals not derived |
| `0x40044c1b` | SELFCHECK_IN_ORDER | 4 | returns 0 |
| `0x400c4c1c` | pin status | `{motor,bit,mask}` 12 in/out | reads GPIO levels (0x30b8). Name inferred |

- C: the motor index is **not bounds-checked**. 5 and 6 oops the kernel (H5).
- C: open is non-exclusive (0x1294), release is a no-op, so closing the fd does
  not stop motion. Locking is a per-motor spinlock (dev+8); there is no
  ioctl-wide lock.

## 3. Motion semantics

C unless noted.

- **States** (dev+0x1c): 0 idle, 1 stopping, 2 goto, 3 unused, 4 auto,
  5 selfcheck (homing), 6 calibration, 7 test (jump table 0x1b88).
- **Calibrated** (dev+0x5c): set by INIT_POS (0x2c0c) and at the end of the
  homing seek (0x200c); cleared by AUTOTEST (0x31a8) and at insmod.
- **Calibrated SET_POS**: pos is an absolute target clamped to cfg [min,max]
  (0x27f0..0x280c); the caller's `dir` is ignored. Internal direction =
  (pos <= cur), where 1 = decreasing.
- **Uncalibrated SET_POS** is dangerous (0x26ec, 0x2868..0x28d4): dir 0 sets
  cur = 0 and target = range max, dir 1 sets cur = range max and target = 0, so it
  drives the full span into a stop. `{pos<=0, dir=0}` drives **down**. This
  rewrite happens before the idle check, so it also corrupts a running move.
- Uncalibrated with any other dir: the helper returns -1 and the ioctl still
  returns 0 (0x27e8), but the internal direction code runs. U (T13): whether
  that is a true no-op.
- **Retargeting a live move is unsafe.** SET_POS stores the new target (0x280c)
  and exits if not idle (0x2818); speed and direction latch only at idle. If
  cur > target with direction up, it stops at once, short of the target
  (0x1d28). If cur < target with direction down, the direction is not checked
  (0x1d40) and it keeps stepping down through 0 with full overtravel.
- **Overtravel at the minimum**: a goto that reaches 0 keeps stepping until
  min-400 (pan) or min-80 (tilt), then sets cur = min (0x1a90, 0x1d40..0x1dac).
  An opto at the pan stop may cut it short (0x1b64). At the maximum it stops
  by count, exactly (0x1d94).
- **SET_POS to the current position** is not a special case in the driver: it
  still enters state 2 and stops at the next tick unless at the minimum
  (0x28e4, 0x1cf8). pos == cur == range max has a path that was not derived
  (0x28b8..0x28c8); the soft margin avoids it.
- **Speed classes** (0x2824..0x2844): <= 15 is class 6, 16 to 31 class 5, 32 to 47
  class 4, >= 48 class 0. They divide the tick by 8, 4, 2 and 1 (gonext
  0x1464..0x14f8). Class 0 ramps for the first 32 steps and near the target;
  the others do not.
- **Tick**: 1400 us (start_timer 0x2264), about 714/s. Camera (T5): 87, 173, 345
  and about 700 steps/s for classes 6, 5, 4 and 0, so one tick is about one step.
- **STOP** goes idle on the next tick with no ramp (0x1c78..0x1cb4). U (T10):
  how many steps an abrupt STOP loses per class.
- **AUTOTEST**: homing seek, then calibrated = 1, then a goto to cfg+0x84 (pan
  1353, tilt 100) at class 0 (0x1fe8..0x200c). `calibrated == 1` therefore does
  not mean the motion is done: wait for GET_STATE == 0. Tilt homing includes a
  full downward stall (0x1f80..0x1fc8).

## 4. Ranges and units

- C: pan 0..2700, tilt 0..1024 (cfg+0x7c), min 0 (cfg+0x80), step 16 (cfg+0x78).
- Camera (T4): about 8.2 pan and 11.28 tilt steps per degree. The pan figure was
  measured inside the app pan limits (666..1674).
- U: the full pan range in degrees. 2700 / 8.2 gives about 330, an extrapolation
  that assumes a constant ratio; the vendor lists 355. Test: remove the app
  limits, sweep 20 to 2680 and measure the angle.
- U (T3, T4): a mechanical stop at the maximum side. Only the minimum side is
  implied by the overtravel code.
- Camera (T6): raw tilt + is down, raw pan + is right. `device` remaps
  directions in userspace (`reverse_tilt="1"`); the kernel has no reverse option.
- Camera (T12): MOVE_LEFT lowers pan steps (`pan_left_is_low`).
- Camera (T2): settled.
- Camera (T4): field of view wide 104 x 57.3, tele 39.7 x 22.7 degrees.

## 5. `/proc/driver/gpio_motor`

- C: mode 0666 (0x37d0). Reading it shows position, target, state, direction
  and backlash: a safe read-only monitor.
- C: writing it bypasses every limit (letters a, b, c, m, n, p, r, s, t; jump
  table 0x327c). `m` permanently changes cfg max (0x3328), `t` is TEST_MOTOR, `r`
  ignores its arguments and forces 0..330 degrees (0x34d4), `n` only prints.
  **Never write it.** Any local process can.

## 6. `device` as the second writer

`device` is the only other program that drives the motors: no other binary in
`/mnt/app` issues `'L'` ioctls or names lens_zfi (C, byte scan of router,
cgiserver, recorder, netserver and onvif).

**Open path.** C: one caller (0x6091c), one attempt. 0x78588 picks the mode from
the path: "tty" is a serial MCU, "i2c" is unsupported but **returns success**,
anything else is `open(path, O_RDWR)` (0x785b8, 0x78744). On failure a sentinel
stays set (0x784f4, cleared only at 0x787bc), `device` logs
"serial_mcu_init Failed." and carries on, and every motor wrapper returns -1.
Exception: the state wrapper returns 0x101, "both busy" (0x79118). `support_ptz`
gates pan/tilt (2 or 3, 0x69b74) and zoom/focus (1 or 2, 0x69b58).

**Wrappers** (C):

| addr | what |
|---|---|
| 0x790fc | GET_STATE, returns pan \| tilt << 8 |
| 0x7925c | STOP, no wait |
| 0x793ac | jog |
| 0x797c0 / 0x7996c | SET_POS one axis / both axes (`{3,p,0,s}`, `{4,t,0,s}`) |
| 0x79b04 | GET_POS |
| 0x7a38c | AUTOTEST, one ioctl per motor |
| 0x7a458 | INIT_POS |
| 0x619dc | wait idle: GET_STATE every 10 ms, 1000 times |
| 0x61c44 | stop all, optionally wait idle |

The goto wrapper drops the mutex **before** its two SET_POS ioctls (0x799b0) and
ignores their result, so `device` does not serialise its own moves either.

**Boot** (C, state machine 0x68ed0): SET_BACKLASH pan 0, tilt 16 (0x68fd0);
SET_MOTOR_MOVE_RANGE pan (0, 360 - X), tilt untouched (0x69044); then INIT_POS
from the saved position if valid, else AUTOTEST (0x69254..0x69340); then wait
idle and a goto to the saved or default position. Which branch is taken depends
on 0x61780, 0xb930c and the selftest flag (0xb9d70), whose meaning is not fully
derived. U (T14): how long after boot this happens, and whether it moves again
after calibrating.

**Commands** (C, dispatcher 0x66e48):

- Preset call (0x666e8), jog (0x65f5c), guard return (0x669bc) and tracking park
  return (0x651c4) all do **STOP both, wait idle, then SET_POS**. If the wait
  fails there is no motion.
- Stop (0x61d00) sends STOP both and does not wait.
- Jog with hold targets the end of travel: max-1, or **0** for left and down,
  which ends in the overtravel stall.
- The exception is `MSG_PTZ_TOPOS` (0x1b6d, handler 0x63790, sent by netserver
  `nets_ptz_topos`): SET_POS at speed 64 with **no STOP and no GET_STATE**. U
  (T15): which app action reaches it.
- U (T15): that HTTP `PtzCtrl op:"ToPos"` is the preset call and not
  `MSG_PTZ_TOPOS`.
- U (T15): the check-then-act window between "wait idle" and the goto when
  tracking or cruise runs on another thread.

**Stored targets.** C: presets (128 records at obj+0x6aa0: pan, tilt, z, digital
zoom), guard (obj+0x69e8) and park (obj+0x6a7c) are raw driver steps taken
straight from GET_POS. Every motion path re-reads the driver, so a foreign
SET_POS does not desync `device`; a foreign INIT_POS or AUTOTEST that moves the
driver's zero silently shifts all of them. A preset call also restores digital
zoom (0x66938). U (T16): the units of z and d_z.

## 7. Reolink limits, self-movers and config files

- C: the app's pan limits are `GetPtzTraceSection`/`SetPtzTraceSection`
  `{PtzTraceSection:{channel,LimitLeft,LimitRight,ImgLeftName,ImgRightName}}`
  (cgiserver 0xdc1b8..0xdc1ec, web UI JS). -1 means unset. There is no tilt
  field in the API.
- C: the UI stores `GetPtzCurPos().Ppos`, which is the **raw step count** from
  GET_POS (device 0x6107c, 0x79b04, no arithmetic). Camera (T12): confirmed.
- C: if `device` cannot open `ptz_dev`, Ppos is -1 and "set from current
  position" silently clears the limit (the UI does not check the value).
- C: the limits are not passed to SET_MOTOR_MOVE_RANGE (its one caller is
  0x69054) and are compared only in 0x653f4, reached from message 0x1b68 with a
  16-byte payload (0x6855c), not from jog, preset or guard. U: that 0x1b68 is the
  AI-tracking move. Test: log 0x1b68 while tracking. Test for scope: set both
  limits, jog past the left one.
- C: settings that move the head by themselves, where they are switched off, and
  the key that persists them:

| feature | off | file and keys |
|---|---|---|
| auto-tracking | `SetAiCfg {channel, bSmartTrack:0}` | `ai.cfg`: `smart_track` |
| guard return | `SetPtzGuard` with `benable:0` | `0_ptz.cfg`: `grdenable` |
| patrol | `SetPtzPatrol` with `enable:0, running:0`; `PtzCtrl op:"StopPatrol"` | `0_ptz.cfg`: `cc%d_bval`, `cc%d_bstart` |
| pattern | `SetPtzTattern`, JSON body **U** (call `GetPtzTattern` and look) | `0_ptz.cfg`: `tc%d_bval`, `tc%d_bstart` |

  Router key pools: 0x5735c..0x57388 (ai.cfg), 0x56b88..0x56b9c (limits),
  0x61dc0..0x61de0 (ptz). cgiserver strings: 0xd4a02, 0xdf372, 0xdd2ac, 0xde335.
- U: `PtzCtrl op:"Auto"` persistence, and the meaning of `last_state` (start a
  patrol, reboot, `grep last_state /mnt/para/0_ptz.cfg`).
- C: `/mnt/para/0_ptz.cfg` (`p_limit_l`, `p_limit_r`) and `/mnt/para/ai.cfg` are
  loaded with TinyXML `LoadFile` and no decryption (router 0x5614c, 0x90440,
  0x906b0, 0x57128); AES is used only for config export (0x2e440).
- U: the exact XML layout (`head -c 400 /mnt/para/0_ptz.cfg`), and how far the
  file lags the live state (router caches and writes on Set). Read these files,
  never write them.

## 8. Web API as ptzd uses it

Only what the code relies on. All of it is U (T9) until a call succeeds on the
camera, except where marked C.

- `POST /cgi-bin/api.cgi?cmd=X[&token=T]`, body
  `[{"cmd":X,"action":0|1,"param":{...}}]`, over plain HTTP on localhost. A
  redirect to HTTPS is an error: ptzd has no TLS.
- `Login {"User":{"userName","password"}}` returns `Token.name` and
  `Token.leaseTime`. The web UI itself uses a digest handshake with AES-wrapped
  bodies; the plain form is accepted at 127.0.0.1:80 (T9).
- Token-expired codes -6, -27, -502, -21; login-refused codes -7, -105, -502;
  `auth_warning_info.unlock_time` on lockout.
- C (cgiserver strings, web UI JS): the command and field names in §7, and guard
  fields `cmdStr` (`setPos`, `toPos`), `benable`, `bexistPos`, `bSaveCurrentPos`,
  `timeout`, `imgName`. ptzd sends `cmdStr:"setPos"` with `benable:0`
  and `bSaveCurrentPos:0`. Measured: this changes one byte of
  `0_ptz.cfg` (the enable flag) and keeps the stored position; `toPos` answers 200
  and is ignored.
- Channel 0 is assumed to be the pan/tilt channel (`reolink_channel`).

## 9. Zoom and focus

- C: no zoom or focus motor is driven on this model. `start_app` loads
  `spi-na51055`, `nvt_af` and `nvt_mtr_an41908` only for `support_ptz` 1 or 2, and
  `device` gates its zoom/focus paths the same way (0x69b58). With 3, both are
  off. Confirm with T1 (`lsmod`).
- C: the tele "zoom" is a lens switch plus digital crop
  (`support_digital_zoom="1"`, `ptz_digital_zoom_control`). Digital zoom divides
  the tele field of view. U (T16): whether the live level can be read.
- The "2.7 to 13.5 mm, 1171 steps" numbers found in the AN41908 `.cfg` files
  belong to other products that share the firmware.
- If the modules were ever present, C: zoom/focus go through `/dev/nvt_isp`
  (requests `0xc00463nn` get, `0x400463nn` set, item 41 focus, 42 zoom, 20 bytes;
  the kernel ignores the size field and returns 0 unconditionally, 0xcf58), one
  job slot shared by both motors, and a command sent while busy is dropped
  silently (nvt_mtr_an41908 0x55b4).

## 10. Hardware hazards

| id | hazard (C) | rule |
|---|---|---|
| H1 | Moving to 0 grinds into the stop (400 pan / 80 tilt steps). Also from app hold-left/down | never target min; soft margin |
| H2 | Uncalibrated SET_POS fakes cur and drives the full span | never SET_POS before calibrated == 1 |
| H3 | Uncalibrated SET_POS during a move rewrites cur and target | same; check calibrated as the last step before SET_POS |
| H4 | `device` is a second writer; nothing arbitrates | check idle first, never touch a foreign move, watch for retargeting |
| H5 | Motor index 5 or 6 oopses the kernel | only 3 and 4, by type |
| H6 | AUTOTEST falls through into SET_ZF_ITEM (0x4b0): reads 20 bytes, and a fault returns -14 **after** the motor started. Word 0 is the motor: a zeroed buffer homes nothing | 20-byte buffer `[motor,0,0,0,0]` |
| H7 | calibrated == 1 comes before the final homing goto | done = GET_STATE == 0; negative is an error, not "busy" |
| H8 | TEST_MOTOR, CALIBRATION, MOVE_AUTO and proc `m`/`r`/`t` have no limits or change config for good | not in the allow-list; never write proc |
| H9 | STOP can fail with -16 after 1 s and does not stop homing | treat as "still moving" |
| H10 | Degrees are unproven | calibrate with T4 |
| H11 | Zoom/focus modules, if loaded, drop busy commands with rc 0 | ptzd does not touch them |

## 11. Claims that circulate and are wrong

- "Zoom/focus via LV8044 or AN41908": neither is active on this model (§1, §9).
- "A bad ioctl type returns -14": it is -515 (§2).
- "Equal position prints a message and does nothing": that string is in another
  function (§3).
- "`device` serialises its ioctls with a mutex": not the goto (§6).
- "Proc `r` and `n` take arguments": they do not (§5).
- "Frigate's 1.0 is the full field of view": it is half (`ptzd/docs/design.md` §3).

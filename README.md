# Custom firmware for the Reolink TrackMix PoE

Reolink's stock firmware 5428 plus root SSH ([dropbear](https://matt.ucc.asn.au/dropbear/dropbear.html))
and `ptzd`, an ONVIF PTZ daemon for [Frigate](https://frigate.video/) autotracking.
No stock file is modified; the only metadata change is the owner and mode of the
empty `/root` directory.

> [!NOTE]
> SSH and ptzd work on the camera, including Frigate autotracking (pan/tilt; zoom
> is not usable). ptzd starts at boot. Status and on-camera tests
> T1 to T16: [ptzd/docs/design.md](ptzd/docs/design.md) §1 and §7.

> [!WARNING]
> Only for board `IPC_529SD78MP` (TrackMix PoE). Prepare the SD card before
> flashing. If anything goes wrong, use the SD card recovery (Option 2).

## Installation

Reolink's firmware is not included. Download stock 5428 as described in
[firmware/stock/README.md](firmware/stock/README.md); the build checks its sha256.
Then build the images as root with `firmware/build.sh` (requirements in
[firmware/README.md](firmware/README.md)). Output in `firmware/build/image/`:

| File | What |
|---|---|
| `IPC_529SD78MP.5435_2604282150.pak` | Custom image |
| `IPC_529SD78MP.5431_2604282150.pak` | Rollback: stock 5428, only the rootfs version label raised |

### Option 1: Web UI (normal)

1. Copy `firmware/stock/upgrade/` to a FAT SD card as `/upgrade`.
2. In the web UI turn automatic updates off.
3. Open the firmware upgrade page, leave "reset configuration" unticked.
4. Upload the 5435 file. Keep power on until the camera is back.
5. The UI still reports 5428 because the app is unchanged. `cat /etc/custom_fw`
   over SSH shows the custom build.

### Option 2: SD card (recovery)

1. Put the SD card from Option 1 in the camera, power off.
2. Hold reset while powering on.
3. U-Boot writes stock 5428 from the card. **This erases the camera's settings.**
4. To go back to stock while the web UI still works, upload the 5431 rollback file
   instead.

### Is it safe to flash?

- The image replaces only the rootfs. Bootloader, kernel and Reolink's app are
  byte-identical to stock and never written; U-Boot checks every section's CRC
  before it erases anything.
- No stock file is changed; SSH and ptzd are added files started by two extra
  boot scripts. Both wait for the settings partition and never block boot.
- Flashing through the web UI and SD card recovery have both been done on this
  camera.
- If the camera dies within 5 minutes of starting ptzd, the next boot skips ptzd
  once, so a fault in ptzd cannot cause a reboot loop.

## Initial setup

Log in with `ssh root@CAMERA`, password `reolink`.

> [!IMPORTANT]
> The default password is public and identical on every camera. Change it first and
> keep the camera off the internet.

| Task | Command | Notes |
|---|---|---|
| Change password | `sshctl passwd` | 8 to 100 characters, stored as sha512 |
| Add a key | `cat key.pub \| ssh root@CAMERA 'cat >> /root/.ssh/authorized_keys'` | `ssh-copy-id` does not work |
| Keys only | `sshctl password-off` | refused until a key is stored |
| Passwords back on | `sshctl password-on` | |
| Turn ptzd off | `touch /mnt/para/ptzd/disabled; sv down /mnt/tmp/sv/ptzd` | stays off after reboot |
| Turn ptzd on again | `rm /mnt/para/ptzd/disabled; reboot` | |

> [!TIP]
> No answer on port 22 right after boot: wait a minute. SSH waits for the settings
> partition and, on the first start, for the kernel random generator.

> [!WARNING]
> Use `sshctl passwd`, not `passwd`. Busybox `passwd` stores a DES hash, which uses
> only the first 8 characters.

## After setup

Settings live in `/mnt/para/ssh` and survive reboots. A factory reset brings back
the default password and a new host key. Logs are in `/mnt/tmp/custom-ssh.log` and,
for ptzd, `/tmp/log/ptzd/current`. IPv4 only, no SFTP or forwarding: copy files with
`ssh root@CAMERA 'cat > /mnt/tmp/f' < f`.

> [!TIP]
> ptzd's ONVIF port 8899 is plain HTTP on every interface. Keep the camera on a
> trusted network.

## Frigate settings

These settings gave the best balance of tracking latency and load on Frigate 0.18.3
with an Intel Core i5-9500 and its UHD Graphics 630 iGPU: Quick Sync decoding and
an OpenVINO YOLOv9-s 320 detector on the iGPU. Other hardware may need other values.

```yaml
go2rtc:
  streams:
    trackmix:      # wide lens, HEVC 3840x2160, record only
      - rtsp://{FRIGATE_REOLINK_USER}:{FRIGATE_REOLINK_PASSWORD}@CAMERA:554/Preview_01_main
    trackmix_sub:  # wide lens, H.264 896x512 at 20 fps, detect
      - ffmpeg:https://CAMERA/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user={FRIGATE_REOLINK_USER}&password={FRIGATE_REOLINK_PASSWORD}

cameras:
  trackmix:
    ffmpeg:
      hwaccel_args: preset-intel-qsv-h264
      inputs:
        - path: rtsp://127.0.0.1:8554/trackmix
          input_args: preset-rtsp-restream
          roles: [record]
        - path: rtsp://127.0.0.1:8554/trackmix_sub
          # preset-rtsp-restream plus two decoder options; this -bsf:v
          # replaces the one from the hwaccel preset.
          input_args:
            - -user_agent
            - FFmpeg Frigate/0.18.3
            - -rtsp_transport
            - tcp
            - -timeout
            - "10000000"
            - -async_depth
            - "1"
            - -bsf:v
            - dump_extra,h264_metadata=level=20
          roles: [detect]
    detect:
      width: 896
      height: 512
      fps: 10
    onvif:
      host: CAMERA
      port: 8899
      user: '{FRIGATE_REOLINK_USER}'
      password: '{FRIGATE_REOLINK_PASSWORD}'
      tls_insecure: true   # makes Frigate send PasswordText, which ptzd needs
      profile: wide
      autotracking:
        enabled: true
        calibrate_on_startup: false
        zooming: disabled
        track: [person, car]
        return_preset: home  # a preset saved on the camera
        timeout: 10
```

On the camera, in `/mnt/para/ptzd/ptzd.conf`, then `sv restart /mnt/tmp/sv/ptzd`:

```
settle_secs = 1.3
```

| Setting | Why |
|---|---|
| Detect on the sub stream over HTTP-FLV | Frigate's recommended Reolink source; same latency as RTSP on this camera |
| `-async_depth 1`, `h264_metadata=level=20` | The sub stream does not signal its reorder depth, so Quick Sync assumes level 5.1 and holds about 0.9 s of frames. Level 2.0 limits that to one frame; the stream has no B-frames |
| `preset-intel-qsv-h264` | VAAPI fails on this stream (`Failed to sync surface`) |
| `fps: 10` | Frigate's frame-rate conversion holds frames. 10 fps delivers them about 0.3 s sooner than 5 fps, for about 10 points more iGPU load under motion |
| `settle_secs = 1.3` | ptzd reports the head as moving until Frigate's detect frames stop showing the move. Shorter, and Frigate reads moving frames as still ones and overshoots back and forth |
| `zooming: disabled` | Zoom moves only the tele lens, so the tracked image never changes size |

Calibrate once before raising `settle_secs`: start Frigate with
`calibrate_on_startup: true`, which writes `movement_weights` into the config, then
set it back to `false`. Calibration times moves by ptzd's moving state, which
`settle_secs` extends.

With these settings a move shows up in Frigate's detect frames about 0.6 to 0.7 s
after the command; about 0.35 s of that is the camera's own image pipeline. With a
different detect pipeline, `settle_secs` must still exceed the time from the motors
stopping to the last moving frame reaching Frigate.

## Documentation

- [firmware/README.md](firmware/README.md): build, verify, flash, recover
- [firmware/docs/ssh.md](firmware/docs/ssh.md): SSH requirements, state, troubleshooting
- [ptzd/README.md](ptzd/README.md) and [ptzd/docs/design.md](ptzd/docs/design.md): the daemon and its on-camera tests
- [research/](research/): firmware format and update paths, motor hardware, and a
  postmortem of a brick caused by an earlier image (fixed before 5433)

## License

MIT, see [LICENSE](LICENSE). Reolink's firmware, and so every built image, is
Reolink's copyright and not covered; do not redistribute built images. Third-party
licences: [firmware/third_party/alpine/README.md](firmware/third_party/alpine/README.md).

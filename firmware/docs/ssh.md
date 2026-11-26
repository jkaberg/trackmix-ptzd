# Root SSH on the TrackMix PoE: everything it took

**Status: works on the camera** since build 5433; later builds change only ptzd. `cat /etc/custom_fw` shows which
build runs.

Paths are relative to `firmware/`. **C** = confirmed from bytes, disassembly or a
VM test. **Camera** = seen on the real device. **U** = unproven, with the test that
settles it.

## Summary

Stock 5428 has no SSH server, and although `S25_Net` runs `telnetd`, busybox has
no such applet. Getting a shell took these steps:

1. build an image the web updater accepts **and actually writes** (§1)
2. change the read-only rootfs without losing anything stock relies on (§2)
3. run a prebuilt server on a system with a different libc (§3)
4. start it at the right point of the boot, without ever blocking it (§4)
5. make dropbear accept a password on this system (§5)
6. make dropbear accept keys on this system (§6)
7. keep per-camera state on the only writable storage (§7)

Failed attempts are listed in §10.

## 1. Getting the image onto the camera

- **Name.** A web upload must be called `IPC_529SD78MP.<version>.pak` and the
  version must differ from `5428_2604282150`. Only inequality is checked.
- **Section versions.** The updater flashes a section only if its version label
  differs from `/mnt/app/version.json`. A rootfs left at `14432` would be skipped,
  so the custom rootfs is labelled `914437` (5435; earlier builds used `914435` and `914436`).
- **Nothing else is raised.** Loader, fdt, uboot, kernel and app are never written
  and are byte-identical to stock.
- **CRCs.** `zlib.crc32(b, 0xffffffff) ^ 0xffffffff` per section, over the
  descriptor and over the file. No signature is checked in the web path (C).
- **Before erasing**, U-Boot loads every staged image to RAM and checks all CRCs.
  A damaged upload leaves the old firmware running (C, static).
- **Way back.** Stock 5428 carries rootfs `14432`, equal to `version.json`, so
  uploading stock does *not* remove a custom rootfs. The rollback image 5431 is
  stock with only the rootfs label raised to `914433`.

Container, flash map and updater details: `research/firmware-format.md`.

## 2. Changing rootfs without breaking the boot

rootfs is UBIFS inside one UBI volume, mounted read-only. The kernel command line
fixes `root=ubi0:rootfs rootfstype=ubifs init=/linuxrc`, so:

| Requirement | Why |
|---|---|
| UBIFS, LZO, 2 KiB pages, LEB 126976, max 239 LEBs | same superblock as stock; another filesystem cannot mount |
| Reuse stock's UBI layout volume and VID header | same volume name, id and reserved size |
| Start from Reolink's file, extract with `ubireader_extract_files -k` **as root** | without `-k` every file becomes 0644 and root-owned |
| `/bin/busybox` executable | `/linuxrc` and `/bin/sh` are links to it; otherwise the kernel panics |
| Every `/etc/init.d/S??*` executable | `rcS` silently skips the others |
| Keep uid 1013, 0777 modes and mtimes of stock files | the image differs from stock only by the listed changes |

`mkfs.ubifs -m 2048 -e 126976 -c 239 -x lzo` then `tools/pak.py` wraps it. Result:
197 of 239 blocks (stock 186). All 773 stock entries are unchanged except the
owner and mode of `/root` (§6); 25 entries are added. The full list is
`tools/layout.py`.

`tools/verify_image.py` compares every inode with stock and rejects anything not
on that list. `tools/test_tools.py` proves it rejects broken images.

## 3. Running dropbear

The camera has glibc 2.29 on 32-bit ARM (hard float). Dropbear is the prebuilt
Alpine edge `dropbear-2026.94`, which is musl and dynamic.

| File in the image | Purpose |
|---|---|
| `/lib/ld-musl-armhf.so.1` | musl loader. Its path is fixed in the binary and does not collide with glibc's `ld-linux-armhf.so.3` |
| `/etc/ld-musl-armhf.path` | contains `/usr/lib/custom-ssh`, so musl never searches the glibc libraries |
| `/usr/lib/custom-ssh/` | `dropbear`, `dropbearkey`, `libz`, `libutmps`, `libskarnet` |
| `/usr/sbin/dropbear`, `/usr/bin/dropbearkey` | wrappers |

**The wrappers are required.** `/etc/profile` sets
`LD_LIBRARY_PATH=/lib:/usr/local/lib:/usr/lib:/mnt/app`. musl honours it, loads
the glibc `libz` from there and crashes. The wrappers unset `LD_LIBRARY_PATH` and
`LD_PRELOAD` first.

Every package is pinned by sha256 and checked against Alpine's signature
(`tools/alpine.py`).

## 4. Boot order

    S00_PreReady  mounts /mnt/app, starts the watchdog monitor
    S07, S10, S15 drivers
    S25_Net       eth0 up, fallback 192.168.0.3
    S98_custom_ssh   <- custom, returns at once, works in the background
    S99_Sysctl    runs start_app: mounts /mnt/para, starts Reolink's programs
    S99_zz_ptzd      <- custom, ptzd

- It runs **before** `S99_Sysctl`, so SSH does not depend on `start_app`.
- `/mnt/para` is mounted inside `start_app`, *after* this script starts. The
  script waits up to 60 s for `/mnt/para ubifs rw` in `/proc/mounts`. It tests the
  mount, not the directory, because the directory exists on the read-only rootfs.
- Everything runs in a background subshell with stdin closed, so no failure stops
  `rcS`.

What `S98_custom_ssh` does, in order:

1. takes a lock directory, so a second start is a no-op
2. waits for `/mnt/para`
3. loads or creates `/mnt/para/ssh/host_ed25519`; a key that fails three reads is
   renamed `.bad`, not deleted
4. copies the host key to `/var/run/custom-ssh`, because dropbear re-reads it for
   every connection and a factory reset would otherwise cut SSH off
5. first boot only: installs the default password file (write, sync, rename, sync)
6. binds a copy of `/etc/passwd` (§5)
7. caps the log, which is on tmpfs, at 256 KiB
8. runs dropbear in a loop, restarting it 5 s after any exit

Dropbear flags:

| Flag | Meaning |
|---|---|
| `-F -E` | foreground, log to stderr. Without `-E` the log is empty: there is no syslog |
| `-j -k` | no local or remote port forwarding |
| `-p 0.0.0.0:22` | IPv4 only |
| `-r` | the camera's host key |
| `-P` | pid file, used by `sshctl` |
| `-s` | added when passwords are off, or when step 6 failed |

## 5. Password login

Password login needs three things.

- **`/etc/passwd` field.** Alpine's dropbear consults `/etc/shadow` only when
  root's field in `/etc/passwd` is `x`. Stock's is not. `/etc` is read-only and the
  stock file stays unchanged, so boot writes a copy with `x` to tmpfs and
  `mount --bind`s it over `/etc/passwd`. If that fails, dropbear runs with `-s`.
- **`/etc/shadow`.** Stock has none. The image links it to `/mnt/para/ssh/shadow`, so
  it is writable and per camera. Missing storage means a dangling link: every
  login is refused (C, VM).
- **A default.** `/etc/custom-ssh/shadow.default` (0600) holds the sha512 hash of
  `reolink`. It is copied only if no shadow file exists, so it never overwrites a
  changed password.

`sshctl passwd` writes a sha512 hash via `cryptpw`. Plain busybox `passwd` would
store DES, which uses only the first 8 characters.

## 6. Key login

Dropbear refuses `authorized_keys` if any directory from the file up to the home
directory is group- or world-writable or has another owner. Stock's `/root` is
0777 and owned by uid 1013. So:

- `/root` becomes 0700 root. **This is the only stock metadata change.**
- `/root/.ssh` is 0700 root, and `authorized_keys` in it links to
  `/mnt/para/ssh/authorized_keys`.
- **No key is in the image.** A baked key would be a back door into every camera.
  The verifier rejects images that contain one. The host key is made on the camera
  for the same reason.
- `ssh-copy-id` does not work: for dropbear servers it writes to
  `/etc/dropbear/authorized_keys`, which does not exist on the read-only rootfs
  (U: try it).

## 7. State on the camera

| Path | Content | Survives |
|---|---|---|
| `/mnt/para/ssh/host_ed25519` | host key | reboot, update |
| `/mnt/para/ssh/shadow` | password hash | reboot, update |
| `/mnt/para/ssh/authorized_keys` | your keys | reboot, update |
| `/mnt/para/ssh/no-password` | exists = passwords off | reboot, update |
| `/var/run/custom-ssh/` | host key copy, passwd copy | nothing (tmpfs) |
| `/mnt/tmp/custom-ssh.log` | log | nothing (tmpfs) |

Surviving an update is expected, because the updater does not write `para`; U until
one is done. Holding reset at power-on erases the first 3 MB of `para`. That brings
back the default password and a new host key. The app's router also reformats
`para` when `/mnt/para/version` is missing, so SSH may need one extra reboot after
a reset (U: reset once).

## 8. Using it

    ssh root@CAMERA                          # password: reolink
    sshctl passwd                            # do this first, 8 to 100 characters
    cat key.pub | ssh root@CAMERA 'cat >> /root/.ssh/authorized_keys'
    ssh root@CAMERA sshctl password-off      # keys only
    ssh root@CAMERA sshctl password-on

`password-off` is refused until a key line is stored. It does not check that the
key works, so log in with it once before closing the session.

No SFTP or scp: copy with `ssh root@CAMERA 'cat > /mnt/tmp/f' < f`. `/mnt/tmp` is
35 MB of RAM. The default password is public: keep the camera off the internet.

**First start.** Creating the host key waits for the kernel random generator.
Linux 4.19 has no timing fallback, and in an idle 4.19 VM it never became ready.
On the camera it becomes ready (Camera); the time is not measured.

## 9. Troubleshooting

| Symptom | Check |
|---|---|
| Connection refused right after boot | wait; `para` can take 60 s, the first start also needs randomness |
| Refused for good | rootfs may not be written. The UI always says 5428: check port 22 and `/etc/custom_fw` |
| `Permission denied`, default password | it was changed, passwords are off, or `/mnt/para` is not mounted |
| Key ignored | `/root` must be 0700 root, the key must be one line |
| Host key changed | `para` was reset |
| Log | `/mnt/tmp/custom-ssh.log` |

Recovery: `README.md`.

## 10. What failed on the way

| Attempt | Result | Lesson |
|---|---|---|
| Repack rootfs as SquashFS | cannot mount, bricked | keep UBIFS |
| Repack without `-k` | no exec bits, kernel panic (VM) | keep every mode and owner |
| `tcpsvd` root shell in `start_app` | no password, dies with the app | own script, before the app |
| Baked-in public key | back door if shared | per-camera keys |
| Plain password hash in `/etc/shadow` | rejected | passwd field must be `x` |
| dropbear without `-E` | empty log | no syslog here |
| dropbear started from a login shell | loaded glibc `libz`, crashed | wrappers |
| Host key served from flash | lost when `para` is wiped | tmpfs copy |
| Compiling dropbear | dropped | signed prebuilt package |
| Extra entropy daemon, sync loop, more commands | dropped | dropbear and ptzd only |

## 11. What is proven, and how

| Claim | Evidence |
|---|---|
| Web updater takes the image, U-Boot, kernel 4.19.91 and app coexist with it, `/mnt/para` mounts, RNG gets ready, login works | Camera |
| Only the listed changes exist | verifier, plus a separate from-scratch comparison |
| Default, wrong and empty password, password change, key login, off/on, simulated reboot, missing `para` | VM: 5433 on Linux 6.1 with simulated NAND and Reolink's init scripts |
| Time until SSH answers on a cold boot | U: time it |
| Real reboot: SSH and the default password still work | Camera |
| Real reboot keeps a changed password and key login | U: change, reboot, log in |
| Factory reset and later updates | U: do one |

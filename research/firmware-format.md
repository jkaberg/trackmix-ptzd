# TrackMix PoE firmware format, flash map and update paths

Board `IPC_529SD78MP`, stock `v3.0.0.5428_2604282150`
(sha256 `bda98ea8…7131`). **C** = confirmed from the bytes, disassembly or a test.
**U** = unproven, with the test that settles it. `U 0x…` is an address in stock
`/mnt/app/upgrade`, `B 0x…` a U-Boot file offset (base 0x0e000000).

## Container (C: rebuilding stock gives the same bytes)

| Offset | Field |
|---|---|
| 0x00 | `SKAP` |
| 0x04 | CRC of everything from 0x08 |
| 0x08 | total file size |
| 0x14 | `DIDB` |
| 0x18 | CRC of everything from 0x1c |
| 0x1c | board name |
| 0x3c | firmware version |
| 0x5c | file size minus 0x68 |
| 0x60 | section count (6) |
| 0x68 | first section |

Each section is an 88-byte `SGMI` header and then its data: +0x04 CRC of the
data, +0x08 name, +0x28 version, +0x48 size, +0x4c flash address, +0x50
partition size, +0x54 extension length (0). The order is loader, fdt, uboot,
kernel, rootfs, app.

CRC = `zlib.crc32(b, 0xffffffff) ^ 0xffffffff`. There is no signature or MD5 in
the web path (C, static). Secure-boot strings exist in U-Boot, but rootfs is not
enforced: the modified rootfs boots on the camera. Kernel enforcement is U and does
not matter, since the kernel is never changed.

## Flash map (128 MB SPI NAND, 128 KiB blocks, 2 KiB pages)

| mtd | Name | Address | Size |
|---|---|---|---|
| 0 | loader | 0x0 | 0x40000 |
| 1 | fdt | 0x40000 | 0x40000 |
| 2 | fdt.restore | 0x80000 | 0x40000 |
| 3 | uboot | 0xc0000 | 0x180000 |
| 4 | uenv | 0x240000 | 0x40000 |
| 5 | kernel | 0x280000 | 0x480000 |
| 6 | rootfs | 0x700000 | 0x2000000 |
| 7 | app | 0x2700000 | 0x1500000 |
| 8 | para (settings) | 0x3c00000 | 0x800000 |
| 9 | sp | 0x4400000 | 0x80000 |
| 10 | ext_para | 0x4480000 | 0x80000 |
| 11 | download (update staging) | 0x4500000 | 0x3b00000 |
| 12 | all | 0x0 | 0x8000000 |

rootfs and app are one-volume UBI images: erase counter 0, VID header at 2048,
data at 4096, LEB 126976, UBIFS with LZO. rootfs reserves 239 of 256 PEBs
(stock uses 186, custom 197), app 150 of 168.

## Boot sequence

1. U-Boot checks the SD card and the download partition, then boots the kernel
   with `root=ubi0:rootfs rootfstype=ubifs init=/linuxrc`.
2. `/linuxrc` → busybox init → `rcS` → every **executable** `/etc/init.d/S??*`.
3. `S00_PreReady` mounts app read-only and starts the watchdog monitor.
4. `S25_Net` sets the fallback address 192.168.0.3.
5. `S98_custom_ssh` (custom) starts SSH in the background.
6. `S99_Sysctl` → `start_app`: mounts `/mnt/para`, `vg_boot.sh` loads
   `lens_zfi.ko`, then the vendor daemons start.
7. `S99_zz_ptzd` (custom) starts ptzd in the background unless disabled.

Filesystem type, exec bits and that order are all load-bearing. rootfs must be
UBIFS, `/bin/busybox` must be 0755 or wider, and every `S??*` must be executable.

## Web updater (C, static trace)

- `.pak` names must start with `IPC_529SD78MP.` and the version after it must
  differ from `5428_2604282150` (U 0x46080, 0x464b8). Only inequality is
  checked, so lower versions pass.
- All sections are staged into the download partition. A section is flashed
  only if its version differs from `/mnt/app/version.json` (U 0x4b618). With
  only rootfs raised, **only rootfs is written**.
- The trigger header is written last (U 0x47d10). After the reboot U-Boot loads
  every image to RAM and checks all CRCs **before any erase** (B 0x20e6c), then
  erases, writes with read-back and bad-block skipping, and clears the trigger.
- Power loss during upload leaves the old firmware. Power loss during the flash
  step resumes on the next boot. A write that keeps failing loops forever.
- **Stock 5428 cannot undo a custom rootfs through the web UI**: its rootfs
  version 14432 equals version.json, so it is skipped. Use the rollback image.
- Custom labels are rootfs 914437 (custom 5435; earlier builds used 914435 and 914436) and 914433 (rollback). They differ from
  14432 and from each other, so either can follow the other or stock.

## SD card recovery in U-Boot (C, static; worked on this camera)

- FAT card with `/upgrade/descriptor` and the `.paks` it names. Hold reset while
  powering on (GPIO 53). `firmware/stock/upgrade/` is a complete set once downloaded (`firmware/stock/README.md`).
- Holding reset also erases the first 3 MB of `para`, so settings are lost.
- loader, fdt and uboot are skipped when identical. kernel, rootfs and app are
  always written, whatever their versions.

## Other facts

- `/` , `/etc` and `/root` are 0777 and owned by uid 1013. Dropbear checks every
  path component above `authorized_keys` up to the home directory, so key login
  needs a fixed `/root`. That is the one metadata change.
- dropbear (Alpine) reads `/etc/shadow` only if root's field in `/etc/passwd` is
  `x`. Stock differs, so boot binds a tmpfs copy over `/etc/passwd` (C: VM).
- The login profile puts `/lib` first in LD_LIBRARY_PATH, so musl binaries need
  wrappers that unset it. busybox has no `setsid`; `passwd` writes DES hashes.
- `S25_Net` calls `telnetd`, but no such applet exists in busybox (C: boot test).
- `/tmp` is a 5 MB tmpfs and `/mnt/tmp` a 35 MB tmpfs. Everything logged there
  needs a size cap.

# What broke the camera, and what changed because of it

Dates are 2026. The camera was recovered with stock 5428 on 25 November. The custom
image then gave SSH access.

## Verdict

The custom images were the problem, not the flashing hardware or the partition
layout. Partition map, bootloader, kernel and UBI geometry were identical to stock.
The filesystem contents differed.

## Failure 1: wrong filesystem type (flashed 23 November, about 10:20)

`trackmix_patched_v5429.paks` (sha256 `e5e2c224…`) had rootfs and app repacked as
SquashFS. The kernel command line says `rootfstype=ubifs` and `S00_PreReady`
mounts app as `ubifs`, so neither can mount. All modes were also 0644. Which
partitions the Linux updater wrote is U. If rootfs was skipped, Linux would boot with no
app and only the fallback address 192.168.0.3.

## Failure 2: no exec bits (built 23 November, 10:58)

`IPC_529SD78MP.5429_2604282150.pak` (sha256 `3476593d…`) was UBIFS, but every file
was 0644 and root-owned instead of 0777 and uid 1013. Cause: the repacker called
`ubireader_extract_files` **without `-k`**, which drops modes and owners.
`/bin/busybox` is then not executable.

C: reproduced in a VM. The image mounts, then
`can't execute '/linuxrc': Permission denied` and a kernel panic. Stock boots
through every init script in the same VM. No evidence shows this file reached flash.

## Failure 3: the recovery attempts could not work

- The SD card had no `/upgrade/descriptor`, so U-Boot ignored it.
- Reset must be held at power-on for that path.
- The files on the card were the broken images.
- The first web upload was refused because the name did not start with the board
  name.

## Failure 4: process

- **No comparison with stock.** A per-file mode check takes seconds and catches both
  images.
- **Whole filesystems were rebuilt** to change two files. Both bugs came from the
  rebuild.
- **No rollback image was prepared.** Stock cannot roll back a custom rootfs through
  the web UI.
- **`firmware/` was extracted from the broken image**, so later work inherited wrong
  modes.
- **An unauthenticated root shell** (`tcpsvd` on port 2323) was the only way in and
  depended on the app starting.
- **Notes were written as fact before verification.** Several contradicted the
  disassembly and were deleted.

## Corrected claims

| Claim | Reality |
|---|---|
| Section CRC unknown | Known, see `firmware-format.md`. It was valid in every image, so CRCs never protected anything |
| Reolink support sent a special build | It is the public 5428, byte for byte |
| The descriptor `model` must match | Only printed; `board` is compared with the image |
| ptzd listens on 127.0.0.1 by default | It binds 0.0.0.0; the log prints an internal address |

## What changed

1. `firmware/tools/build_image.py` starts from Reolink's file, refuses any other
   input, and keeps every mode, owner and mtime (`-k`, as root).
2. Only rootfs changes, and only by the list in `firmware/tools/layout.py`. loader,
   fdt, uboot, kernel and **app are byte-identical**. `start_app` is untouched.
3. `firmware/tools/verify_image.py` compares every entry with stock and rejects the
   rest. `firmware/tools/test_tools.py` proves it rejects both broken images and one
   mutation per check. It also parses the raw UBIFS (LPT, log, master, orphan area,
   every node CRC), because the first version trusted the index alone and a review
   built images that passed it and should not have.
4. The final image was booted in a VM on simulated NAND with the camera's partition
   map and Reolink's init scripts, with the broken image as the control.
5. A rollback image is built next to every custom image.
6. SSH starts before the app, so it survives an app failure. The image ships a public
   default password and no keys, and allows no login if its store is missing. The
   host key is served from tmpfs, so wiping `/mnt/para` does not cut off SSH.
7. An independent review found no blockers. Its findings are fixed and retested: the verifier gap above, host key read from
   flash on every connection, host key deleted after a transient read error, dropbear
   hash not enforced. A second review covered password login and found nothing that
   blocks boot. Build 5433 then removed haveged and the extra `sshctl` commands. It
   passed the verifier and the VM tests, was not reviewed again, and runs on the camera.

## Still unproven

| Item | Test |
|---|---|
| ptzd tests T8, T13, T15, T16 | `ptzd/docs/design.md` §1, §7.1 |
| Whether a web factory reset erases `/mnt/para` (SSH host key, ptzd state) | reset once, compare the host key |
| Whether a Reolink cloud update replaces the custom rootfs | keep auto-update off |
| The 5431 rollback image (never flashed) | the SD card recovery is the proven way back |

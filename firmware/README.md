# firmware

Builds the custom image: Reolink's stock 5428 plus dropbear SSH and ptzd. Paths in
this folder's docs are relative to `firmware/`.

| Path | Purpose |
|---|---|
| `stock/` | Stock 5428 from Reolink, downloaded by you ([stock/README.md](stock/README.md)). Build input and SD recovery set |
| `overlay/` | Boot hooks, wrappers and config added to rootfs |
| `third_party/alpine/` | Prebuilt dropbear and its libraries, signed by Alpine |
| `tools/` | Build, verify, test, extract (`layout.py` lists every change) |
| `build.sh` | Builds ptzd, then the image, then verifies and tests it |
| `docs/ssh.md` | SSH requirements, state, troubleshooting |
| `build/` | Output, not in git |

Format, flash map and update paths: `../research/firmware-format.md`.

## Build

    ./build.sh      # as root

First download the stock firmware into `stock/upgrade/` (see `stock/README.md`).

Needs mtd-utils, ubi_reader, python3 `lzallright`, openssl, cargo with the
`armv7-unknown-linux-musleabihf` target. Dropbear is prebuilt from Alpine, pinned
by hash and checked against Alpine's signature. The image contains no SSH key and
no host key. Output in `build/image/`:

| File | Contents |
|---|---|
| `IPC_529SD78MP.5435_2604282150.pak` | Custom image |
| `IPC_529SD78MP.5431_2604282150.pak` | **Rollback**: stock, only the rootfs version label raised |

The reference 5435 file has sha256
`7a093bade45d6f53565012f9b28dd4d9a09fb6df2d7e8bda35ab066da24a69c8`. A rebuild has
the same files but another hash, because `mkfs.ubifs` writes a random UUID and the
build's atime and ctime; `tools/verify_image.py` checks the content. The rollback
file is byte-reproducible (`190082a55d5e71b4b34ff87647028257a64b29eb7df4bde8cc232a6ee43e0ae9`).
`build.sh` stops if `build/image/` is not empty, so earlier images are never
overwritten.

Never flash a file that `tools/verify_image.py` has not passed. Any change needs a
new `BUILD` and `ROOTFS_VER` in `tools/layout.py` (and the names in `build.sh`),
or the camera skips it.

To look at the stock files (as root, keeps modes and owners):

    python3 tools/extract_pak.py stock/upgrade/*.paks build/stock-extracted

## Flash through the web UI

1. Have an SD card ready with `stock/upgrade/` copied to it as `/upgrade`.
2. Turn automatic updates off. Leave "reset configuration" unticked.
3. Upload the 5435 file. Keep power on until the camera is back.
4. The UI still reports 5428 because the app is unchanged. `cat /etc/custom_fw`
   over SSH shows the custom version.

## If something goes wrong

1. Web UI works: upload the 5431 rollback file.
2. No web UI: SD card, hold reset while powering on. This also erases settings.

Plain stock 5428 through the web UI does **not** remove the custom rootfs.

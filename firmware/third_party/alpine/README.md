# Vendored Alpine packages

Unmodified Alpine edge `main/armv7` packages, kept here because edge deletes old
versions. `firmware/tools/alpine.py` pins each by sha256, checks Alpine's signature
and unpacks only the files listed in `WANT`.

| Package | Licence | Upstream | Used for |
|---|---|---|---|
| dropbear 2026.94-r0 | MIT | https://matt.ucc.asn.au/dropbear/dropbear.html | `dropbear`, `dropbearkey` |
| musl 1.2.6-r6 | MIT | https://musl.libc.org/ | `/lib/ld-musl-armhf.so.1` |
| zlib 1.4-r0 | Zlib | https://zlib.net/ | `libz.so.1` |
| utmps-libs 0.1.3.4-r1 | ISC | https://skarnet.org/software/utmps/ | `libutmps.so.0.1` |
| skalibs-libs 2.16.0.0-r0 | ISC | https://skarnet.org/software/skalibs/ | `libskarnet.so.2.16` |

Licences are as declared by each package. `*.rsa.pub` is Alpine's signing key.

To update: download the new `.apk` files from
`https://dl-cdn.alpinelinux.org/alpine/edge/main/armv7/`, replace the old ones,
change names, hashes and library versions in `firmware/tools/alpine.py`, raise `BUILD` and
`ROOTFS_VER` in `firmware/tools/layout.py` (and the names in `firmware/build.sh`), run
`firmware/build.sh`.

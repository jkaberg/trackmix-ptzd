#!/usr/bin/env python3
"""Negative tests for verify_image.py. Run as root, needs the build outputs:

test_tools.py --stock S --ptzd P --image I [--bad IMG ...]
Each --bad image is a known-broken one that must be rejected.
"""
import argparse
import os
import pathlib
import struct
import subprocess
import sys
import tempfile

import extract_pak
import layout
import pak
import verify_image

ap = argparse.ArgumentParser()
for x in ("stock", "ptzd", "image"):
    ap.add_argument(f"--{x}", type=pathlib.Path, required=True)
ap.add_argument("--bad", type=pathlib.Path, nargs="*", default=[])
a = ap.parse_args()
STOCK, GOOD = a.stock.read_bytes(), a.image.read_bytes()
ADD = layout.added(a.ptzd)
fails = 0


def rejects(name, blob, add=ADD, why=None):
    global fails
    try:
        verify_image.verify(STOCK, blob, add)
        print("FAIL accepted:", name)
        fails += 1
    except Exception as e:
        ok = (why is None or why in str(e)) and isinstance(e, pak.Bad)
        fails += not ok
        print("ok  " if ok else "FAIL", name, "->", type(e).__name__, e)


def rebuilt(fn):
    p = pak.Pak(GOOD)
    fn(p)
    return p.build()


def flip(b, o):
    b = bytearray(b)
    b[o] ^= 1
    return bytes(b)


verify_image.verify(STOCK, GOOD, ADD)
print("ok   good image accepted")
pak.need(pak.Pak(STOCK).build() == STOCK, "container round trip")
for n in ("rootfs", "app"):
    u = pak.Ubi(pak.Pak(STOCK).sections[n].data)
    pak.need(u.repack(u.volume) == pak.Pak(STOCK).sections[n].data, "ubi round trip")
print("ok   stock round trips byte for byte")

for b in a.bad:
    rejects(f"known bad {b.name}", b.read_bytes())
    bad = pak.Pak(b.read_bytes()).sections["rootfs"].data
    rejects(f"rootfs payload of {b.name} under good headers", rebuilt(lambda p: p.replace("rootfs", bad)))
rejects("stock itself", STOCK)
rejects("truncated", GOOD[:-1])
rejects("trailing byte", GOOD + b"\0")
rejects("bit flip, stale CRC", flip(GOOD, len(GOOD) // 2), why="CRC")
for n in ("loader", "fdt", "uboot", "kernel", "app"):
    rejects(f"{n} changed, CRCs fixed", rebuilt(lambda p: p.replace(n, flip(p.sections[n].data, 9999))), why=n)
rejects("app version bumped", rebuilt(lambda p: p.sections["app"].set_ver("v3.0.0.5430_2604282150")), why="app")
rejects("rootfs version not bumped", rebuilt(lambda p: p.sections["rootfs"].set_ver("14432")), why="rootfs")


def addr(p):
    p.sections["rootfs"].hdr[0x4C] ^= 2


rejects("rootfs flash address changed", rebuilt(addr), why="rootfs")
rejects("board name changed", rebuilt(lambda p: p.head.__setitem__(0x1C, 0x58)), why="header")
rejects("fwver not bumped", rebuilt(lambda p: p.head.__setitem__(slice(0x3C, 0x5C), STOCK[0x3C:0x5C])), why="fwver")
for off, what in ((0x20, "EC"), (pak.VID_OFS + 8, "VID"), (pak.DATA_OFS + 3, "layout"), (2 * pak.PEB + pak.VID_OFS + 15, "VID"),
                  (2 * pak.PEB + pak.DATA_OFS + 40, "superblock")):
    rejects(f"UBI {what} byte flipped, container CRCs fixed",
            rebuilt(lambda p: p.replace("rootfs", flip(p.sections["rootfs"].data, off))))
rejects("missing PEB", rebuilt(lambda p: p.replace("rootfs", p.sections["rootfs"].data[:-pak.PEB])))
rejects("other ssh key", GOOD, {**ADD, "/root/.ssh/authorized_keys": (0o100600, b"ssh-ed25519 AAAA x\n")}, why="authorized_keys")

L = pak.LEB


def vol(fn):
    def go(p):
        u = pak.Ubi(p.sections["rootfs"].data)
        v = bytearray(u.volume)
        fn(v)
        p.replace("rootfs", u.repack(bytes(v)))
    return rebuilt(go)


def node_fix(v, at, off, fmt, val):
    n, = struct.unpack_from("<I", v, at + 16)
    struct.pack_into(fmt, v, at + off, val)
    struct.pack_into("<I", v, at + 4, pak.ubicrc(bytes(v[at + 8:at + n])))


def lpt_bogus(v):
    for k in (1, 2):
        node_fix(v, k * L, 24 + 16 + 32 + 48, "<I", 200)


def totals(v):
    for k in (1, 2):
        node_fix(v, k * L, 24 + 16 + 32 + 8, "<Q", 4096)


def second_master(v):
    for k in (1, 2):
        v[k * L + 2048:k * L + 2560] = v[k * L:k * L + 512]


def log_extra(v):
    v[4 * L:4 * L + 2048] = v[3 * L:3 * L + 2048]


def zero(a, n):
    return lambda v: v.__setitem__(slice(a, a + n), bytes(n))


for name, fn in (("LPT zeroed", zero(7 * L, 2048)), ("log start node destroyed", zero(3 * L, 4)),
                 ("bogus master lpt_lnum", lpt_bogus), ("wrong master totals", totals),
                 ("second master node", second_master), ("extra log LEB", log_extra),
                 ("orphan area not blank", zero(9 * L, 64))):
    rejects(name, vol(fn))
rejects("unpinned dropbear", GOOD, {**ADD, "/usr/lib/custom-ssh/dropbear": (0o100755, b"x")})
rejects("baked-in SSH key", GOOD, {**ADD, "/root/.ssh/authorized_keys": (0o100600, b"ssh-ed25519 AAAA x\n")})

# Filesystem mutations: rebuild the rootfs from the good tree with one change.
MUT = {
    "busybox 0644 (the real brick)": lambda r: os.chmod(r / "bin/busybox", 0o644),
    "stock file re-owned": lambda r: os.chown(r / "etc/inittab", 0, 0),
    "stock dir mode": lambda r: os.chmod(r / "etc", 0o755),
    "start_app edited": lambda r: open(r / "etc/init.d/start_app", "a").write("\n"),
    "same-size content edit": lambda r: (r / "etc/hostname").write_bytes((r / "etc/hostname").read_bytes()[::-1]),
    "symlink retargeted": lambda r: ((r / "linuxrc").unlink(), (r / "linuxrc").symlink_to("bin/bosybux"), os.lchown(r / "linuxrc", 1013, 1013)),
    "file removed": lambda r: (r / "etc/hosts").unlink(),
    "file added": lambda r: (r / "etc/extra").write_bytes(b""),
    "hook not executable": lambda r: os.chmod(r / "etc/init.d/S98_custom_ssh", 0o644),
    "authorized_keys baked in": lambda r: ((r / "root/.ssh/authorized_keys").unlink(), (r / "root/.ssh/authorized_keys").write_bytes(b"ssh-ed25519 AAAA x\n")),
    "shadow default readable": lambda r: os.chmod(r / "etc/custom-ssh/shadow.default", 0o644),
    "shadow retargeted": lambda r: ((r / "etc/shadow").unlink(), (r / "etc/shadow").symlink_to("/tmp/shadow")),
    "/root left 0777": lambda r: os.chmod(r / "root", 0o777),
    "dropbear swapped": lambda r: (r / "usr/lib/custom-ssh/dropbear").write_bytes(b"\x7fELF"),
    "mtime changed": lambda r: os.utime(r / "etc/passwd", (1, 1)),
}
with tempfile.TemporaryDirectory(dir=a.image.parent) as td:
    td = pathlib.Path(td)
    extract_pak.extract(GOOD, td / "x")
    for i, (name, fn) in enumerate(MUT.items()):
        r = td / f"m{i}"
        subprocess.run(["cp", "-a", td / "x" / "rootfs", r], check=True)
        fn(r)
        subprocess.run(["mkfs.ubifs", "-m", "2048", "-e", str(pak.LEB), "-c", "239", "-x", "lzo", "-r", r, "-o", td / "u"], check=True)
        u = pak.Ubi(pak.Pak(GOOD).sections["rootfs"].data).repack((td / "u").read_bytes())
        rejects(name, rebuilt(lambda p: p.replace("rootfs", u)))
        subprocess.run(["rm", "-rf", r], check=True)
    subprocess.run(["mkfs.ubifs", "-m", "2048", "-e", str(pak.LEB), "-c", "200", "-x", "lzo", "-r", td / "x/rootfs", "-o", td / "u"], check=True)
    u = pak.Ubi(pak.Pak(GOOD).sections["rootfs"].data).repack((td / "u").read_bytes())
    rejects("other UBIFS geometry", rebuilt(lambda p: p.replace("rootfs", u)), why="superblock")
print("FAILED" if fails else "ALL PASSED", fails)
sys.exit(bool(fails))

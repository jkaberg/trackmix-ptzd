#!/usr/bin/env python3
"""Refuse any image that is not stock 5428 plus exactly the changes in layout.py.

Reads only the finished file. Every inode in every volume is compared with
stock: type, mode, owner, link count, flags, mtime, symlink target, content.
Exit 0 means safe to consider flashing; anything else means do not flash.
"""
import argparse
import hashlib
import json
import pathlib
import struct
import sys

import alpine
import layout
import pak
from pak import need

FIXED_SB = [k for k in pak.SB_FIELDS if k not in ("leb_cnt", "uuid")]


def diff_tree(old, new, add=None, meta=None):
    add, meta = add or {}, meta or {}
    need(set(old) <= set(new), f"removed: {sorted(set(old) - set(new))[:5]}")
    extra = set(new) - set(old)
    need(extra == set(add), f"added paths differ: {sorted(extra ^ set(add))[:5]}")
    for path, o in old.items():
        n, want = new[path], dict(o)
        want.update(meta.get(path, {}))
        if path in meta:
            need(o["mode"] >> 12 == 4 and want["mode"] >> 12 == 4, f"{path}: not a directory")
        # new subdirectories raise the parent's link count
        want["nlink"] = o["nlink"] + sum(1 for a, (m, _) in add.items()
                                         if m >> 12 == 4 and (a.rsplit("/", 1)[0] or "/") == path)
        for k in want:
            need(n[k] == want[k], f"{path}: {k} changed" + ("" if k == "data" else f" {want[k]!r} -> {n[k]!r}"))
    for path, (mode, data) in add.items():
        n = new[path]
        need((n["mode"], n["uid"], n["gid"]) == (mode, 0, 0), f"{path}: added with {n['mode']:o} {n['uid']}:{n['gid']}")
        need(n["data"] == data, f"{path}: added content differs")


KEY_MARKS = (b"AAAAC3NzaC1", b"AAAAB3NzaC1", b"AAAAE2VjZHNh", b"PRIVATE KEY")


def check_elf(path, b, loader=False):
    need(b[:7] == b"\x7fELF\x01\x01\x01" and struct.unpack_from("<H", b, 18)[0] == 40, f"{path}: not 32-bit LE ARM")
    phoff, = struct.unpack_from("<I", b, 28)
    size, num = struct.unpack_from("<HH", b, 42)
    types = [struct.unpack_from("<I", b, phoff + i * size)[0] for i in range(num)]
    need(loader or 3 not in types, f"{path}: needs a dynamic loader")
    flags, = struct.unpack_from("<I", b, 36)
    need(flags >> 24 == 5 and not flags & 0x200, f"{path}: not EABI5 hard-float")


def verify(stock_blob, blob, add):
    need(hashlib.sha256(stock_blob).hexdigest() == pak.STOCK_SHA256, "stock input is not the known 5428")
    s, c = pak.Pak(stock_blob), pak.Pak(blob)
    s.check_crcs()
    c.check_crcs()
    need(c.fwver == layout.FWVER, f"fwver {c.fwver}")
    # header: only file/DIDB CRCs, sizes and fwver may differ
    mask = set(range(4, 12)) | set(range(0x18, 0x1C)) | set(range(0x3C, 0x60))
    need(all(s.head[i] == c.head[i] for i in range(pak.HDR) if i not in mask), "header fields changed")
    for n in pak.SECTIONS:
        a, b = s.sections[n], c.sections[n]
        if n == "rootfs":
            h = bytearray(a.hdr)
            struct.pack_into("<I", h, 4, b.crc)
            struct.pack_into("<I", h, 0x48, b.size)
            h[0x28:0x48] = layout.ROOTFS_VER.encode().ljust(32, b"\0")
            need(bytes(h) == bytes(b.hdr), "rootfs: section header")
        else:
            need(bytes(a.hdr) == bytes(b.hdr) and a.data == b.data, f"{n}: section changed")

    us, uc = (pak.Ubi(x.sections["rootfs"].data) for x in (s, c))
    need(uc.pebs[0] == us.pebs[0] and uc.pebs[1] == us.pebs[1], "UBI layout volume changed")
    need(uc.vid0 == us.vid0, "VID header changed")
    part = c.sections["rootfs"].part // pak.PEB
    need(uc.reserved + 4 <= part, "volume does not fit its partition")
    fs, fc = pak.Ubifs(us.volume), pak.Ubifs(uc.volume)
    for k in FIXED_SB:
        need(fs.sb[k] == fc.sb[k], f"superblock {k}: {fs.sb[k]} -> {fc.sb[k]}")
    fs.check_structure()  # the checker must agree with Reolink's own image first
    fc.check_structure()
    need(fc.sb["leb_cnt"] <= fc.sb["max_leb_cnt"], "leb_cnt over max")
    told, tnew = fs.tree(), fc.tree()
    diff_tree(told, tnew, add, layout.META_CHANGED)
    check_elf("/usr/sbin/ptzd", add["/usr/sbin/ptzd"][1])
    for path, b in alpine.files().items():
        need(add[path] == (0o100755, b), f"{path} is not the signed Alpine file")
        check_elf(path, b, loader=True)
    need(add["/root/.ssh/authorized_keys"] == (0o120777, layout.STORE + b"authorized_keys"), "authorized_keys must stay per camera")
    for path, (mode, data) in add.items():
        need(not data or mode >> 12 != 8 or not any(k in data for k in KEY_MARKS) or path in alpine.WANT, f"{path}: contains a key")
    h = lambda x: hashlib.sha256(x).hexdigest()
    return dict(sha256=h(blob), bytes=len(blob), stock_sha256=pak.STOCK_SHA256, fwver=c.fwver,
                rootfs_pebs=[len(us.pebs), len(uc.pebs)], stock_entries_verified=len(told),
                unchanged_sections=[n for n in pak.SECTIONS if n != "rootfs"],
                added={p: h(d) if d is not None else "dir" for p, (m, d) in sorted(add.items())},
                metadata_changed=sorted(layout.META_CHANGED))


def verify_rollback(stock_blob, blob):
    """Stock bytes with only the rootfs version label changed."""
    need(hashlib.sha256(stock_blob).hexdigest() == pak.STOCK_SHA256, "stock input is not the known 5428")
    p = pak.Pak(blob)
    p.check_crcs()
    need(p.sections["rootfs"].ver == layout.ROLLBACK_VER, "rollback rootfs version")
    p.sections["rootfs"].set_ver("14432")
    need(p.build() == stock_blob, "rollback differs from stock beyond the rootfs version")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for a in ("stock", "ptzd", "image"):
        ap.add_argument(f"--{a}", type=pathlib.Path, required=True)
    a = ap.parse_args()
    try:
        need(a.image.name == layout.FILENAME, f"file must be named {layout.FILENAME}")
        r = verify(a.stock.read_bytes(), a.image.read_bytes(), layout.added(a.ptzd))
    except (pak.Bad, OSError, ValueError, KeyError, IndexError, struct.error) as e:
        sys.exit(f"REJECTED: {type(e).__name__}: {e}")
    print(json.dumps(r, indent=1))
    print("PASS")


if __name__ == "__main__":
    main()

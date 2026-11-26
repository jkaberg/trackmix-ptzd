#!/usr/bin/env python3
"""Build the custom image from Reolink's stock image, then verify it.

Only the rootfs section changes. Run as root. The output embeds the SSH public
key and is refused unless verify_image.py passes on the finished file.
"""
import argparse
import json
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

import extract_pak
import layout
import pak
import verify_image

MTIME = 1777386000  # 2026-04-28, the stock build date


def put(root, path, mode, data):
    f = pathlib.Path(str(root) + path)
    pak.need(not f.is_symlink() and not f.exists(), f"{path} already in stock")
    if mode & 0o170000 == 0o040000:
        f.mkdir()
    elif mode & 0o170000 == 0o120000:
        f.symlink_to(data.decode())
    else:
        f.write_bytes(data)
    os.chown(f, 0, 0, follow_symlinks=False)
    if mode & 0o170000 != 0o120000:
        os.chmod(f, mode & 0o7777)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for a in ("stock", "ptzd", "outdir"):
        ap.add_argument(f"--{a}", type=pathlib.Path, required=True)
    a = ap.parse_args()
    if os.geteuid() != 0:
        sys.exit("run as root: mkfs.ubifs must see the stock owners")
    blob = a.stock.read_bytes()
    pak.need(hashlib.sha256(blob).hexdigest() == pak.STOCK_SHA256, "not the known stock 5428 image")
    add = layout.added(a.ptzd)
    a.outdir.mkdir(parents=True, exist_ok=True)
    out = a.outdir / layout.FILENAME
    if out.exists():
        sys.exit(f"{out} exists")

    with tempfile.TemporaryDirectory(dir=a.outdir) as td:
        td = pathlib.Path(td)
        p = extract_pak.extract(blob, td / "x")
        root = td / "x" / "rootfs"
        dirs = {os.path.dirname(x) or "/" for x in add}
        times = {d: os.stat(str(root) + d) for d in dirs if os.path.isdir(str(root) + d)}
        for path, (mode, data) in sorted(add.items()):
            put(root, path, mode, data)
        for path, m in layout.META_CHANGED.items():
            os.chown(str(root) + path, m["uid"], m["gid"])
            os.chmod(str(root) + path, m["mode"] & 0o7777)
        for path in add:
            os.utime(str(root) + path, (MTIME, MTIME), follow_symlinks=False)
        for d, st in times.items():
            os.utime(str(root) + d, ns=(st.st_atime_ns, st.st_mtime_ns))
        ubi = pak.Ubi(p.sections["rootfs"].data)
        img = td / "rootfs.ubifs"
        # Geometry equals the stock superblock; verify_image.py compares it.
        subprocess.run(["mkfs.ubifs", "-m", "2048", "-e", str(pak.LEB), "-c", str(ubi.reserved),
                        "-x", "lzo", "-r", root, "-o", img], check=True)
        p.replace("rootfs", ubi.repack(img.read_bytes()))
        p.sections["rootfs"].set_ver(layout.ROOTFS_VER)
        p.head[0x3C:0x5C] = layout.FWVER.encode().ljust(32, b"\0")
        result = p.build()
        tmp = td / layout.FILENAME
        tmp.write_bytes(result)
        report = verify_image.verify(blob, tmp.read_bytes(), add)
        tmp.rename(out)
    rb = pak.Pak(blob)
    rb.sections["rootfs"].set_ver(layout.ROLLBACK_VER)
    rbname = a.outdir / layout.ROLLBACK_NAME
    rbname.write_bytes(rb.build())
    verify_image.verify_rollback(blob, rbname.read_bytes())
    report["rollback"] = dict(file=rbname.name, sha256=hashlib.sha256(rbname.read_bytes()).hexdigest())
    (a.outdir / "manifest.json").write_text(json.dumps(dict(report, file=out.name), indent=1) + "\n")
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()

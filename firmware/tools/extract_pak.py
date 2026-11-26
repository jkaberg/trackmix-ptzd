#!/usr/bin/env python3
"""Extract a SKAP image: raw sections plus the rootfs and app trees.

Keeps modes, owners and mtimes (ubireader -k as root). Without -k every file
loses its exec bit; repacking such a tree is what bricked the camera.

Usage: extract_pak.py <image.pak> <new outdir>     (as root)
"""
import os
import pathlib
import subprocess
import sys
import tempfile

import pak


def extract(blob, out):
    if os.geteuid() != 0:
        raise SystemExit("run as root, or ownership is lost")
    p = pak.Pak(blob)
    p.check_crcs()
    out.mkdir(parents=True)
    (out / "sections").mkdir()
    for s in p.sections.values():
        (out / "sections" / s.name).write_bytes(s.data)
    for name in ("rootfs", "app"):
        with tempfile.TemporaryDirectory(dir=out) as td:
            subprocess.run(["ubireader_extract_files", "-k", "-o", td, out / "sections" / name],
                           check=True, stdout=subprocess.DEVNULL)
            (tree,) = pathlib.Path(td).glob("*/*")
            tree.rename(out / name)
        check(out / name, pak.read_tree(p.sections[name]))
    return p


def check(root, want):
    """The extracted tree must match the volume in every attribute we keep."""
    seen = {"/"}
    for d, dirs, files in os.walk(root):
        for n in dirs + files:
            seen.add("/" + os.path.relpath(os.path.join(d, n), root))
    pak.need(seen == set(want), "extracted path set differs")
    for p, e in want.items():
        f = str(root) + (p if p != "/" else "")
        st = os.lstat(f)
        pak.need((st.st_mode, st.st_uid, st.st_gid) == (e["mode"], e["uid"], e["gid"]), f"{p}: metadata")
        if e["mode"] & 0o170000 == 0o100000:
            pak.need(pathlib.Path(f).read_bytes() == e["data"], f"{p}: content")
        elif e["mode"] & 0o170000 == 0o120000:
            pak.need(os.readlink(f).encode() == e["data"], f"{p}: symlink")


if __name__ == "__main__":
    img, dst = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    if dst.exists():
        sys.exit(f"{dst} exists")
    extract(img.read_bytes(), dst)

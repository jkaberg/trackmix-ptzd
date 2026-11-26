"""The complete list of what the custom image changes. build_image.py applies
it, verify_image.py rejects anything else."""
import pathlib

import alpine

ROOT = pathlib.Path(__file__).resolve().parent.parent  # firmware/
OVL = ROOT / "overlay"
RUNIT = ROOT.parent / "ptzd" / "runit"
BUILD, SUFFIX = 5435, "_2604282150"
FWVER = f"v3.0.0.{BUILD}{SUFFIX}"
FILENAME = f"IPC_529SD78MP.{BUILD}{SUFFIX}.pak"
# Stock is 14432. The Linux updater compares with strcmp and skips equal ones, so
# use a number Reolink will not reach.
ROOTFS_VER = "914437"

# The web updater compares each section's version with /mnt/app/version.json
# (stock: 14432) and skips equal ones, so plain stock 5428 cannot undo a custom
# rootfs. This image is stock 5428 with only the rootfs version label raised.
ROLLBACK_VER = "914433"
ROLLBACK_NAME = f"IPC_529SD78MP.5431{SUFFIX}.pak"

# /root must be root-owned and not group/world writable or dropbear refuses
# its authorized_keys. This is the only metadata change to a stock path.
META_CHANGED = {"/root": dict(mode=0o040700, uid=0, gid=0, nlink=3)}


STORE = b"/mnt/para/ssh/"


def added(ptzd):
    """path -> (st_mode, content or symlink target)"""
    f, x, d = 0o100644, 0o100755, 0o040755
    rd = lambda p: p.read_bytes()
    return {
        "/root/.ssh": (0o040700, None),
        # Per camera, in settings storage. No key or host key ships in the image.
        "/root/.ssh/authorized_keys": (0o120777, STORE + b"authorized_keys"),
        "/etc/shadow": (0o120777, STORE + b"shadow"),
        "/etc/custom-ssh": (d, None),
        "/etc/custom-ssh/shadow.default": (0o100600, rd(OVL / "etc/custom-ssh/shadow.default")),
        "/etc/ld-musl-armhf.path": (f, rd(OVL / "etc/ld-musl-armhf.path")),
        alpine.LIB: (d, None),
        **{p: (x, b) for p, b in alpine.files().items()},
        "/usr/sbin/dropbear": (x, rd(OVL / "usr/sbin/dropbear")),
        "/usr/sbin/sshctl": (x, rd(OVL / "usr/sbin/sshctl")),
        "/usr/bin/dropbearkey": (x, rd(OVL / "usr/bin/dropbearkey")),
        "/usr/sbin/ptzd": (x, rd(ptzd)),
        "/etc/init.d/S98_custom_ssh": (x, rd(OVL / "etc/init.d/S98_custom_ssh")),
        "/etc/init.d/S99_zz_ptzd": (x, rd(OVL / "etc/init.d/S99_zz_ptzd")),
        "/etc/ptzd": (d, None),
        "/etc/ptzd/ptzd.conf": (f, rd(OVL / "etc/ptzd/ptzd.conf")),
        "/etc/ptzd/run": (x, rd(RUNIT / "run")),
        "/etc/ptzd/finish": (x, rd(RUNIT / "finish")),
        "/etc/ptzd/log.run": (x, rd(RUNIT / "log/run")),
        "/etc/custom_fw": (f, f"{FWVER} rootfs {ROOTFS_VER}, based on stock v3.0.0.5428{SUFFIX}\n".encode()),
    }

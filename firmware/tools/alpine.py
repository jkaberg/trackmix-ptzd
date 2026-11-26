"""Prebuilt dropbear and its libraries: Alpine edge armv7 packages, vendored in
third_party/alpine because edge deletes old versions. Nothing is compiled here.
Every package is checked by hash and by Alpine's signature before use."""
import hashlib
import io
import pathlib
import subprocess
import tarfile
import tempfile
import zlib

DIR = pathlib.Path(__file__).resolve().parent.parent / "third_party" / "alpine"
URL = "https://dl-cdn.alpinelinux.org/alpine/edge/main/armv7/"
# Same bytes on alpinelinux.org/keys, git.alpinelinux.org and the GitHub mirror.
KEY = ("alpine-devel@lists.alpinelinux.org-616adfeb.rsa.pub",
       "ebe717d228555aa58133c202314a451f81e71f174781fd7ff8d8970d6cfa60da")
PKGS = {
    "dropbear-2026.94-r0.apk": "d49836a38751bcfb6d2351a299e8cefacec6261c6e670a2ad6deb7243f1bc20d",
    "musl-1.2.6-r6.apk": "1cef933b1e5738fd615ffb00d8629fd4fa5902cf588743a52b320c728262351f",
    "skalibs-libs-2.16.0.0-r0.apk": "a652328b83e9f292653949c59f83c00a2b425c7b8acf5518505c792087fe0f5e",
    "utmps-libs-0.1.3.4-r1.apk": "fd14425b9cb2ba5fab91ba197546519423f57d009326f75f489722eef4b974db",
    "zlib-1.4-r0.apk": "d1b79a5717daa3c0bd406a7666aa75d0b16cb2c7c8557c77e6d885a356eb5ac8",
}
LIB = "/usr/lib/custom-ssh"
# image path -> (package, member)
WANT = {
    f"{LIB}/dropbear": ("dropbear-2026.94-r0.apk", "usr/sbin/dropbear"),
    f"{LIB}/dropbearkey": ("dropbear-2026.94-r0.apk", "usr/bin/dropbearkey"),
    f"{LIB}/libz.so.1": ("zlib-1.4-r0.apk", "usr/lib/libz.so.1.4"),
    f"{LIB}/libutmps.so.0.1": ("utmps-libs-0.1.3.4-r1.apk", "usr/lib/libutmps.so.0.1.3.4"),
    f"{LIB}/libskarnet.so.2.16": ("skalibs-libs-2.16.0.0-r0.apk", "usr/lib/libskarnet.so.2.16.0.0"),
    "/lib/ld-musl-armhf.so.1": ("musl-1.2.6-r6.apk", "lib/ld-musl-armhf.so.1"),
}


def _members(b):
    """An apk v2 file is three gzip streams: signature, control, data."""
    out = []
    while b:
        d = zlib.decompressobj(31)
        raw = d.decompress(b)
        out.append((b[:len(b) - len(d.unused_data)], raw))
        b = d.unused_data
    return out


def _tar(raw):
    return tarfile.open(fileobj=io.BytesIO(raw + bytes(1024)))


def package(name):
    """name -> data tar of a verified package"""
    b = (DIR / name).read_bytes()
    if hashlib.sha256(b).hexdigest() != PKGS[name]:
        raise SystemExit(f"{name}: hash mismatch")
    key = DIR / KEY[0]
    if hashlib.sha256(key.read_bytes()).hexdigest() != KEY[1]:
        raise SystemExit("Alpine key mismatch")
    sig, ctl, data = _members(b)
    t = _tar(sig[1])
    with tempfile.NamedTemporaryFile() as s, tempfile.NamedTemporaryFile() as d:
        s.write(t.extractfile(t.getmembers()[0]).read())
        s.flush()
        d.write(ctl[0])
        d.flush()
        subprocess.run(["openssl", "dgst", "-sha1", "-verify", key, "-signature", s.name, d.name],
                       check=True, capture_output=True)
    info = _tar(ctl[1]).extractfile(".PKGINFO").read().decode()
    want = next(x.split("= ")[1] for x in info.splitlines() if x.startswith("datahash"))
    if hashlib.sha256(data[0]).hexdigest() != want:
        raise SystemExit(f"{name}: data hash mismatch")
    return _tar(data[1])


def files():
    tars = {n: package(n) for n in PKGS}
    return {dst: tars[p].extractfile(m).read() for dst, (p, m) in WANT.items()}

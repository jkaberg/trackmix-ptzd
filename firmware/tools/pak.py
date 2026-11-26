"""Reolink SKAP/SGMI container, UBI and UBIFS readers shared by the tools here.

Layouts were read out of stock 5428 and its U-Boot; see docs/firmware-format.md.
Everything fails loudly: a parser that guesses is how the bad images got built.
"""
import stat
import struct
import zlib

import lzallright

_LZO = lzallright.LZOCompressor()  # one instance: each allocates a big work buffer
SECTIONS = ("loader", "fdt", "uboot", "kernel", "rootfs", "app")
STOCK_SHA256 = "bda98ea88766e8087d6e1f2454b6783a171305e0f7142e7df7d2f6aaee5b7131"
HDR, SGMI = 0x68, 88
PEB, VID_OFS, DATA_OFS = 0x20000, 2048, 4096
LEB = PEB - DATA_OFS


class Bad(Exception):
    pass


def need(cond, msg):
    if not cond:
        raise Bad(msg)


def crc(b):
    """Container CRC: zlib polynomial, init 0, no final xor."""
    return zlib.crc32(b, 0xFFFFFFFF) ^ 0xFFFFFFFF


def ubicrc(b):
    return zlib.crc32(b) ^ 0xFFFFFFFF


# --- container -------------------------------------------------------------
class Section:
    def __init__(self, hdr, data):
        self.hdr, self.data = bytearray(hdr), data
        self.name = bytes(hdr[8:40]).split(b"\0")[0].decode()
        self.crc, = struct.unpack_from("<I", hdr, 4)
        self.size, self.addr, self.part, self.ext = struct.unpack_from("<4I", hdr, 0x48)

    @property
    def ver(self):
        return bytes(self.hdr[0x28:0x48]).split(b"\0")[0].decode()

    def set_ver(self, v):
        v = v.encode()
        need(len(v) < 24, "section version too long")
        self.hdr[0x28:0x48] = v.ljust(32, b"\0")


class Pak:
    def __init__(self, blob):
        need(blob[:4] == b"SKAP" and blob[0x14:0x18] in (b"DIDB", b"BDID"), "not a SKAP/DIDB image")
        self.blob = blob
        self.head = bytearray(blob[:HDR])
        n, = struct.unpack_from("<I", blob, 0x60)
        need(n == len(SECTIONS), f"section count {n}")
        self.sections, o = {}, HDR
        for want in SECTIONS:
            h = blob[o:o + SGMI]
            need(h[:4] == b"SGMI" and len(h) == SGMI, f"no SGMI header at {o:#x}")
            size, = struct.unpack_from("<I", h, 0x48)
            s = Section(h, blob[o + SGMI:o + SGMI + size])
            need(s.name == want, f"section {s.name!r}, expected {want!r}")
            need(len(s.data) == size, f"{want}: truncated")
            need(s.ext == 0, f"{want}: extension data")
            self.sections[want] = s
            o += SGMI + size
        need(o == len(blob), f"{len(blob) - o} trailing bytes")

    @property
    def fwver(self):
        return bytes(self.head[0x3C:0x5C]).split(b"\0")[0].decode()

    def check_crcs(self):
        b = self.blob
        need(struct.unpack_from("<I", b, 4)[0] == crc(b[8:]), "file CRC")
        need(struct.unpack_from("<I", b, 8)[0] == len(b), "file size field")
        need(struct.unpack_from("<I", b, 0x18)[0] == crc(b[0x1C:]), "DIDB CRC")
        need(struct.unpack_from("<I", b, 0x5C)[0] == len(b) - HDR, "DIDB size field")
        for s in self.sections.values():
            need(s.crc == crc(s.data), f"{s.name}: section CRC")
            need(s.size <= s.part, f"{s.name}: larger than its partition")

    def replace(self, name, data):
        s = self.sections[name]
        s.data, s.size, s.crc = data, len(data), crc(data)
        struct.pack_into("<I", s.hdr, 4, s.crc)
        struct.pack_into("<I", s.hdr, 0x48, s.size)

    def build(self):
        body = b"".join(bytes(s.hdr) + s.data for s in self.sections.values())
        b = bytearray(self.head) + body
        struct.pack_into("<I", b, 0x5C, len(b) - HDR)
        struct.pack_into("<I", b, 0x18, crc(b[0x1C:]))
        struct.pack_into("<I", b, 8, len(b))
        struct.pack_into("<I", b, 4, crc(b[8:]))
        return bytes(b)


# --- UBI -------------------------------------------------------------------
class Ubi:
    """One-volume UBI image: two layout PEBs, then LEBs 0..n-1 in order."""

    def __init__(self, d):
        need(len(d) % PEB == 0 and len(d) >= 3 * PEB, "UBI size is not whole PEBs")
        self.pebs = [d[i:i + PEB] for i in range(0, len(d), PEB)]
        ec = self.pebs[0][:64]
        need(ec[:4] == b"UBI#" and struct.unpack(">I", ec[60:64])[0] == ubicrc(ec[:60]), "EC header")
        need(struct.unpack(">QII", ec[8:24]) == (0, VID_OFS, DATA_OFS), "EC header geometry")
        self.vid0 = None
        for i, p in enumerate(self.pebs):
            need(p[:DATA_OFS][:VID_OFS] == ec.ljust(VID_OFS, b"\xff") or p[:64] == ec, f"PEB {i}: EC header differs")
            v = p[VID_OFS:VID_OFS + 64]
            need(v[:4] == b"UBI!" and struct.unpack(">I", v[60:])[0] == ubicrc(v[:60]), f"PEB {i}: VID header")
            vol, lnum = struct.unpack(">II", v[8:16])
            if i < 2:
                need((vol, lnum) == (0x7FFFEFFF, i), f"PEB {i}: not layout volume")
            else:
                need((vol, lnum) == (0, i - 2), f"PEB {i}: vol {vol} leb {lnum}, expected 0/{i - 2}")
                if i == 2:
                    self.vid0 = v
                need(v[:12] + v[16:60] == self.vid0[:12] + self.vid0[16:60], f"PEB {i}: VID fields differ")
        need(self.pebs[0][DATA_OFS:] == self.pebs[1][DATA_OFS:], "layout copies differ")
        vt = self.pebs[0][DATA_OFS:]
        recs = [vt[i * 172:(i + 1) * 172] for i in range(LEB // 172)][:128]
        for i, r in enumerate(recs):
            need(struct.unpack(">I", r[168:])[0] == ubicrc(r[:168]), f"vtable record {i} CRC")
        used = [r for r in recs if struct.unpack(">I", r[:4])[0]]
        need(len(used) == 1, f"{len(used)} volumes")
        self.reserved, _, _, _, upd, nl = struct.unpack(">IIIBBH", used[0][:16])
        need(upd == 0, "volume update marker set")
        self.volname = used[0][16:16 + nl].decode()
        self.volume = b"".join(p[DATA_OFS:] for p in self.pebs[2:])
        need(len(self.pebs) - 2 <= self.reserved, "volume exceeds reserved PEBs")

    def repack(self, volume):
        need(len(volume) % LEB == 0, "UBIFS image is not whole LEBs")
        n = len(volume) // LEB
        need(n <= self.reserved, f"{n} LEBs, volume has {self.reserved}")
        out = list(self.pebs[:2])
        pre = bytearray(self.pebs[2][:DATA_OFS])
        for i in range(n):
            struct.pack_into(">I", pre, VID_OFS + 12, i)
            struct.pack_into(">I", pre, VID_OFS + 60, ubicrc(pre[VID_OFS:VID_OFS + 60]))
            out.append(bytes(pre) + volume[i * LEB:(i + 1) * LEB])
        return b"".join(out)


# --- UBIFS (read-only, via the on-flash index) -----------------------------
SB_FIELDS = ("pad key_hash key_fmt flags min_io leb_size leb_cnt max_leb_cnt max_bud_bytes log_lebs "
             "lpt_lebs orph_lebs jhead_cnt fanout lsave_cnt fmt_version default_compr pad1 rp_uid "
             "rp_gid rp_size time_gran uuid ro_compat").split()
SB_FMT = "<2sBBIIIIIQIIIIIIIH2sIIQI16sI"


def r5(name):
    a = 0
    for c in name:
        c -= 256 if c > 127 else 0
        a = (a + (c << 4) + (c >> 4)) * 11 & 0xFFFFFFFF
    a &= 0x1FFFFFFF
    return a + 3 if a <= 2 else a


def crc16(b):
    c = 0xFFFF
    for x in b:
        c ^= x
        for _ in range(8):
            c = c >> 1 ^ 0xA001 if c & 1 else c >> 1
    return c


MST_FIELDS = ("highest_inum cmt_no flags log_lnum root_lnum root_offs root_len gc_lnum ihead_lnum ihead_offs "
              "index_size total_free total_dirty total_used total_dead total_dark lpt_lnum lpt_offs nhead_lnum "
              "nhead_offs ltab_lnum ltab_offs lsave_lnum lsave_offs lscan_lnum empty_lebs idx_lebs leb_cnt").split()


class Ubifs:
    def __init__(self, vol):
        self.vol = vol
        t, b = self._node(0, 0)
        need(t == 6, "LEB 0 is not a superblock")
        self.sb = dict(zip(SB_FIELDS, struct.unpack_from(SB_FMT, b, 24)))
        need(self.sb["leb_size"] == LEB and self.sb["leb_cnt"] * LEB == len(vol), "superblock geometry")
        masters = [self._node(l, 0) for l in (1, 2)]
        need(all(t == 7 for t, _ in masters), "master nodes")
        need(masters[0][1][24:] == masters[1][1][24:], "master copies differ")
        self.ino, self.dent, self.data, self.seen = {}, {}, {}, set()
        self.last = (0, 0)
        self._walk(*struct.unpack_from("<II", masters[0][1], 48))

    def _node(self, l, o):
        p = l * LEB + o
        need(p + 24 <= len(self.vol), f"node {l}:{o} outside volume")
        magic, c, _, ln, t = struct.unpack_from("<IIQIB", self.vol, p)
        need(magic == 0x06101831, f"node magic at {l}:{o}")
        b = self.vol[p:p + ln]
        need(len(b) == ln and ubicrc(b[8:]) == c, f"node CRC at {l}:{o}")
        return t, b

    def _walk(self, l, o, level=None):
        need((l, o) not in self.seen, "index loop")
        self.seen.add((l, o))
        t, b = self._node(l, o)
        need(t == 9, "not an index node")
        cnt, lvl = struct.unpack_from("<HH", b, 24)
        need(level in (None, lvl) and cnt > 0, "index level")
        for i in range(cnt):
            bl, bo, bn, inum, k1 = struct.unpack_from("<IIIII", b, 28 + i * 20)
            need((inum, k1) >= self.last, "index keys out of order")
            if lvl:
                self._walk(bl, bo, lvl - 1)
            else:
                self.last = (inum, k1)
                need((bl, bo) not in self.seen, "leaf referenced twice")
                self.seen.add((bl, bo))
                t, nb = self._node(bl, bo)
                need(len(nb) == bn, "leaf length")
                self._leaf(inum, k1 >> 29, k1 & 0x1FFFFFFF, t, nb)

    def _leaf(self, inum, kt, low, t, b):
        need(struct.unpack_from("<II", b, 24) == (inum, kt << 29 | low), "leaf key mismatch")
        if t == 0:
            need(inum not in self.ino, f"duplicate inode {inum}")
            size, at, ct, mt = struct.unpack_from("<QQQQ", b, 48)
            nlink, uid, gid, mode, flags, dlen = struct.unpack_from("<6I", b, 92)
            xattr_cnt, = struct.unpack_from("<I", b, 116)
            need(xattr_cnt == 0, f"inode {inum} has xattrs")
            self.ino[inum] = dict(size=size, mtime=mt, nlink=nlink, uid=uid, gid=gid, mode=mode,
                                  flags=flags, inline=b[160:160 + dlen])
        elif t == 2:
            child, _, ty, nlen = struct.unpack_from("<QBBH", b, 40)
            if child:
                d = self.dent.setdefault(inum, {})
                name = b[56:56 + nlen].decode()
                need(low == r5(b[56:56 + nlen]), f"directory hash of {name}")
                need(name not in d, f"duplicate entry {name}")
                d[name] = child
        elif t == 1:
            sz, ct = struct.unpack_from("<IH", b, 40)
            blocks = self.data.setdefault(inum, {})
            need(low not in blocks, "duplicate data block")
            blocks[low] = (sz, ct, b[48:])
        else:
            raise Bad(f"unexpected leaf node type {t}")

    def content(self, inum):
        size, out = self.ino[inum]["size"], bytearray()
        blocks = self.data.get(inum, {})
        for i in range((size + 4095) // 4096):
            if i in blocks:
                sz, ct, raw = blocks[i]
                if ct == 0:
                    b = raw
                elif ct == 1:
                    b = bytes(_LZO.decompress(raw, sz))
                elif ct == 2:
                    b = zlib.decompress(raw, -15)
                else:
                    raise Bad(f"compression {ct}")
                need(len(b) == sz and sz <= 4096, f"inode {inum} block {i}: length")
                out += b.ljust(min(4096, size - i * 4096), b"\0")
            else:
                out += bytes(min(4096, size - i * 4096))
        need(not set(blocks) - set(range((size + 4095) // 4096)), f"inode {inum}: blocks past EOF")
        return bytes(out[:size])

    def tree(self):
        """path -> dict(mode, uid, gid, nlink, flags, mtime, data); data is file
        content, symlink target, device number or None."""
        res, used, todo = {}, set(), [(1, "")]
        while todo:
            i, path = todo.pop()
            x = self.ino[i]
            need(not (stat.S_ISDIR(x["mode"]) and i in used), f"directory loop at {path}")
            used.add(i)
            e = {k: x[k] for k in ("mode", "uid", "gid", "nlink", "flags", "mtime")}
            if stat.S_ISREG(x["mode"]):
                e["data"] = self.content(i)
            elif stat.S_ISDIR(x["mode"]):
                e["data"] = None
            else:
                e["data"] = x["inline"]
            res[path or "/"] = e
            for n, c in self.dent.get(i, {}).items():
                need(stat.S_ISDIR(x["mode"]), f"entries in non-directory {path}")
                todo.append((c, f"{path}/{n}"))
        need(used == set(self.ino), "orphan inodes")
        return res


    # --- everything a kernel mount trusts besides the index ----------------
    def _scan(self, l):
        """(offset, type, aligned end) of each node in LEB l; the rest is 0xff."""
        o, out = 0, []
        leb = self.vol[l * LEB:(l + 1) * LEB]
        while o < LEB and leb[o:].strip(b"\xff"):
            if leb[o] == 0xCE:  # padding bytes
                o += 1
                continue
            t, b = self._node(l, o)
            end = o + (len(b) + 7) // 8 * 8
            if t == 5:
                end = o + 28 + struct.unpack_from("<I", b, 24)[0]
            need(end <= LEB, f"LEB {l}: node past end")
            out.append((o, t, end))
            o = end
        return out

    def check_structure(self):
        """As mkfs writes it: clean master, empty journal, consistent LPT."""
        sb, n = self.sb, len(self.vol) // LEB
        blank = lambda l: not self.vol[l * LEB:(l + 1) * LEB].strip(b"\xff")
        real = lambda l: [x for x in self._scan(l) if x[1] != 5]
        # mkfs leaves a leftover inode node after the superblock, stock included;
        # the kernel reads only the node at offset 0.
        need([x[:2] for x in real(0)] in ([(0, 6)], [(0, 6), (4096, 0)]), "LEB 0 is not one superblock")
        for l in (1, 2):
            need([x[:2] for x in real(l)] == [(0, 7)], f"LEB {l} is not one master node")
        m = dict(zip(MST_FIELDS, struct.unpack_from("<QQIIIIIIIIQQQQQQIIIIIIIIIIII", self._node(1, 0)[1], 24)))
        lpt_first = 3 + sb["log_lebs"]
        orph = lpt_first + sb["lpt_lebs"]
        main = orph + sb["orph_lebs"]
        need(m["log_lnum"] == 3 and [x[:2] for x in real(3)] == [(0, 10)], "log is not one commit-start node")
        need(all(blank(l) for l in (*range(4, lpt_first), *range(orph, main))), "log or orphan area not empty")
        need(m["leb_cnt"] == n == sb["leb_cnt"] and m["highest_inum"] >= max(self.ino), "master counts")
        need(blank(m["gc_lnum"]) and main <= m["gc_lnum"] < n, "gc LEB")
        info, stray, tot = {}, [], dict(free=0, dirty=0, used=0, empty=0, idx=0, idxsz=0)
        for l in range(main, n):
            end = used = 0
            idx = False
            for o, t, e in self._scan(l):
                end = e
                if (l, o) in self.seen:
                    used += e - o
                    idx |= t == 9
                elif t != 5:
                    stray.append((t, struct.unpack_from("<I", self._node(l, o)[1], 24)[0]))
            end = -(-end // 2048) * 2048
            info[l] = (LEB - end, end - used, idx)
            tot["free"] += LEB - end
            tot["dirty"] += end - used
            tot["empty"] += end == 0
            tot["idx"] += idx
            tot["idxsz" if idx else "used"] += used
        # mkfs leaves exactly one unindexed node: a zero-numbered inode
        need(stray in ([], [(0, 0)]), f"nodes outside the index: {stray[:3]}")
        for a, b in (("free", "total_free"), ("dirty", "total_dirty"), ("used", "total_used"),
                     ("empty", "empty_lebs"), ("idx", "idx_lebs"), ("idxsz", "index_size")):
            need(tot[a] == m[b], f"master {b}: {m[b]}, scanned {tot[a]}")
        self._check_lpt(m, info, main, n, lpt_first)

    def _check_lpt(self, m, info, main, n, first):
        sb, big = self.sb, self.sb["max_leb_cnt"]
        spc, lnb, ofb, lsb = LEB.bit_length() - 3, sb["lpt_lebs"].bit_length(), (LEB - 1).bit_length(), LEB.bit_length()
        psz = (20 + (spc * 2 + 1) * 4 + 7) // 8
        nsz = (20 + (lnb + ofb) * 4 + 7) // 8
        tsz = (20 + sb["lpt_lebs"] * lsb * 2 + 7) // 8

        def read(l, o, size, kind):
            need(first <= l < first + sb["lpt_lebs"] and o + size <= LEB, "LPT address")
            b = self.vol[l * LEB + o:l * LEB + o + size]
            need(struct.unpack_from("<H", b)[0] == crc16(b[2:]), f"LPT CRC at {l}:{o}")
            v, p = int.from_bytes(b[2:], "little"), 0

            def get(k):
                nonlocal p
                r = v >> p & (1 << k) - 1
                p += k
                return r
            need(get(4) == kind, "LPT node type")
            return get

        levels, cover, checked = 1, 4, set()
        while cover < -(-(big - main) // 4):
            levels, cover = levels + 1, cover * 4
        todo = [(m["lpt_lnum"], m["lpt_offs"], levels, 0)]
        while todo:
            l, o, lev, num = todo.pop()
            if lev:
                g = read(l, o, nsz, 1)
                for i in range(4):
                    a, off = g(lnb) + first, g(ofb)
                    if a != first + sb["lpt_lebs"]:
                        todo.append((a, off, lev - 1, num * 4 + i))
                continue
            g = read(l, o, psz, 0)
            for i in range(4):
                got, leb = (g(spc) << 3, g(spc) << 3, bool(g(1))), main + num * 4 + i
                if leb < big:
                    need(got == info.get(leb, (LEB, 0, False)), f"LPT entry for LEB {leb}")
                    checked.add(leb)
        need(checked >= set(range(main, n)), "LPT does not cover every LEB")
        g = read(m["ltab_lnum"], m["ltab_offs"], tsz, 2)
        for _ in range(sb["lpt_lebs"]):
            need(g(lsb) <= LEB and g(lsb) <= LEB, "ltab")


def read_tree(section):
    return Ubifs(Ubi(section.data).volume).tree()

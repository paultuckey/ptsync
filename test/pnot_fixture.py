#!/usr/bin/env python3
"""Rewrite a QuickTime clip so it opens with a `pnot` preview atom.

Builds the layout 2000s Nikon compacts wrote — `pnot`, `PICT`, `mdat`, `moov`,
with no `ftyp` brand at all — which `file-format` 0.29 cannot sniff. See
`is_pnot_quicktime` in src/file_type.rs and the note in test/README.md.

Chunk offsets in `stco`/`co64` are absolute, so every entry is shifted by the
number of bytes inserted ahead of `mdat`; without that the file sniffs correctly
but no longer plays.

Usage: pnot_fixture.py <in.mov> <out.mov>
"""

import struct
import sys


def split_atoms(data):
    """Top-level atoms, in order, as (type, bytes)."""
    atoms, off = [], 0
    while off < len(data):
        size, kind = struct.unpack(">I4s", data[off : off + 8])
        if size < 8:
            raise ValueError(f"bad atom size {size} at {off}")
        atoms.append((kind, data[off : off + size]))
        off += size
    return atoms


def preview_atoms(width, height):
    """A `pnot` atom and the small version-2 `PICT` it points at."""
    pict = struct.pack(">HHHHH", 0, 0, 0, height, width)  # legacy size + bounds
    pict += b"\x00\x11\x02\xff"  # version 2 picture
    pict += b"\x0c\x00" + b"\xff\xfe\x00\x00"  # HeaderOp, -1 = standard
    pict += struct.pack(">HHHH", 0, 0, height, width) + b"\x00" * 8
    pict += b"\x00\x01" + struct.pack(">HHHHH", 10, 0, 0, height, width)  # Clip
    pict += b"\x00\xff"  # EndOfPicture
    if len(pict) % 2:
        pict += b"\x00"
    pict = struct.pack(">H", len(pict)) + pict[2:]
    pict = struct.pack(">I4s", len(pict) + 8, b"PICT") + pict
    # size, type, modification date, version, preview type, preview index
    pnot = struct.pack(">I4sIH4sH", 20, b"pnot", 0, 0, b"PICT", 1)
    return pnot, pict


def shift_chunk_offsets(buf, start, end, delta):
    """Add `delta` to every chunk offset in the container spanning start..end."""
    off = start
    while off + 8 <= end:
        size, kind = struct.unpack(">I4s", buf[off : off + 8])
        if size < 8:
            break
        if kind in (b"stco", b"co64"):
            count = struct.unpack(">I", buf[off + 12 : off + 16])[0]
            width = 4 if kind == b"stco" else 8
            fmt = ">I" if kind == b"stco" else ">Q"
            pos = off + 16
            for _ in range(count):
                value = struct.unpack(fmt, buf[pos : pos + width])[0]
                buf[pos : pos + width] = struct.pack(fmt, value + delta)
                pos += width
        elif kind in (b"moov", b"trak", b"mdia", b"minf", b"stbl"):
            shift_chunk_offsets(buf, off + 8, off + size, delta)
        off += size


def main(src, dst):
    data = open(src, "rb").read()
    atoms = dict(split_atoms(data))
    if b"mdat" not in atoms or b"moov" not in atoms:
        raise SystemExit(f"{src}: expected both mdat and moov")

    old_mdat_off = 0
    for kind, raw in split_atoms(data):
        if kind == b"mdat":
            break
        old_mdat_off += len(raw)

    pnot, pict = preview_atoms(160, 120)
    delta = len(pnot) + len(pict) - old_mdat_off
    moov = bytearray(atoms[b"moov"])
    shift_chunk_offsets(moov, 8, len(moov), delta)

    open(dst, "wb").write(pnot + pict + atoms[b"mdat"] + bytes(moov))
    print(f"{dst}: pnot/PICT/mdat/moov, chunk offsets shifted by {delta}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(sys.argv[1], sys.argv[2])

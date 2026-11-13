#!/usr/bin/env python3
"""Writes the generated test comics in testdata/: nested.cbz and webtoon.cbz.

Run it from the repository root after changing it, and commit the outputs.
The RAR and DOC fixtures are stored as files, because no free tool writes RAR.
"""

import pathlib
import struct
import zipfile
import zlib

OUT = pathlib.Path(__file__).resolve().parent


def png(width, height, pixel):
    """An 8-bit RGB PNG; pixel(x, y) returns the (r, g, b) of one pixel.
    Every row uses the Up filter, which keeps the gradients small."""
    raw = bytearray()
    above = bytes(3 * width)
    for y in range(height):
        row = bytes(c for x in range(width) for c in pixel(x, y))
        raw.append(2)
        raw.extend((c - a) & 0xFF for c, a in zip(row, above))
        above = row

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")


def write_zip(name, entries, compression):
    with zipfile.ZipFile(OUT / name, "w") as archive:
        for entry, data in entries:
            info = zipfile.ZipInfo(entry, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = compression
            archive.writestr(info, data)


def solid(index):
    """A flat 30 x 40 page whose color names its reading-order index."""
    color = (index * 40 % 256, (255 - index * 40) % 256, 128)
    return png(30, 40, lambda x, y: color)


# Five pages in two folders, in natural order: ch1/p1, ch1/p2, ch1/p10,
# ch2/p1, ch2/p2. The junk entries and the text file are not pages.
write_zip(
    "nested.cbz",
    [
        ("ComicInfo.xml", b'<?xml version="1.0"?>\n<ComicInfo><Pages>'
         b'<Page Image="0" Bookmark="Opening"/><Page Image="3" Bookmark="Second act"/>'
         b'</Pages></ComicInfo>\n'),
        ("ch1/p10.png", solid(2)),
        ("ch1/p2.png", solid(1)),
        ("ch1/p1.png", solid(0)),
        ("ch2/p2.png", solid(4)),
        ("ch2/p1.png", solid(3)),
        ("__MACOSX/ch1/._p1.png", b"\x00\x05\x16\x07resource fork"),
        ("ch1/.hidden.png", solid(9)),
        ("notes.txt", b"not a page\n"),
    ],
    zipfile.ZIP_DEFLATED,
)


def strip(page, width, height):
    """Red names the page; green counts rows and blue counts columns, so a
    test can check that the strip joins pages without gaps."""
    return png(width, height, lambda x, y: (page * 60, y % 256, x % 256))


# A webtoon: two 120-pixel pages, one 300 wide that shrinks to a narrow
# viewport and one 60 wide that keeps its size. Stored, as many are.
write_zip(
    "webtoon.cbz",
    [
        ("001.png", strip(1, 120, 500)),
        ("002.png", strip(2, 120, 700)),
        ("003.png", strip(3, 300, 400)),
        ("004.png", strip(4, 60, 300)),
    ],
    zipfile.ZIP_STORED,
)

"""Converts a libvirt screenshot (binary PPM or PNG) to PNG using only the standard library."""

import struct
import sys
import zlib

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def read_ppm(data: bytes) -> tuple[int, int, bytes]:
    if not data.startswith(b"P6"):
        raise SystemExit("unsupported screenshot format; expected binary PPM or PNG")

    fields: list[int] = []
    position = 2
    while len(fields) < 3:
        if data[position : position + 1].isspace():
            position += 1
        elif data[position : position + 1] == b"#":
            position = data.index(b"\n", position) + 1
        else:
            start = position
            while not data[position : position + 1].isspace():
                position += 1
            fields.append(int(data[start:position]))

    width, height, maximum = fields
    if maximum != 255:
        raise SystemExit(f"unsupported PPM depth {maximum}")

    start = position + 1
    return width, height, data[start : start + width * height * 3]


def chunk(kind: bytes, payload: bytes) -> bytes:
    body = kind + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))


def to_png(width: int, height: int, pixels: bytes) -> bytes:
    stride = width * 3
    scanlines = b"".join(b"\x00" + pixels[row * stride : (row + 1) * stride] for row in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return PNG_SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(scanlines, 6)) + chunk(b"IEND", b"")


def main(source: str, target: str) -> None:
    with open(source, "rb") as file:
        data = file.read()
    png = data if data.startswith(PNG_SIGNATURE) else to_png(*read_ppm(data))
    with open(target, "wb") as file:
        file.write(png)


if __name__ == "__main__":
    main(*sys.argv[1:3])

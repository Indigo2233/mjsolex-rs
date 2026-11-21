"""Generate a deterministic monochrome scan for Android smoke tests."""

import argparse
from array import array
from math import exp, sqrt
from pathlib import Path
from struct import pack_into
from sys import byteorder


def create_scan(output: Path) -> None:
    width, height, frames = 256, 64, 240
    header = bytearray(178)
    header[:14] = b"LUCAM-RECORDER"
    pack_into("<7I", header, 14, 0, 0, 1, width, height, 16, frames)
    columns = [(x - 127.5) / 104 for x in range(width)]
    spectrum = [
        [1 - 0.6 * exp(-0.5 * ((y - 27.4 - 3 * x - 5 * x * x) / 1.7) ** 2)
         - 0.16 * exp(-0.5 * ((y - 45 - 3 * x - 5 * x * x) / 1.2) ** 2)
         for x in columns]
        for y in range(height)
    ]
    with output.open("wb") as stream:
        stream.write(header)
        for frame in range(frames):
            scan_y = (frame - 119.5) / 104
            brightness = []
            for x in columns:
                radius_sq = x * x + scan_y * scan_y
                limb = 12500 + 23500 * sqrt(1 - radius_sq) if radius_sq < 1 else 1500
                spot = 1 - 0.49 * exp(-((x - 0.30) ** 2 + (scan_y + 0.32) ** 2) / 0.01)
                brightness.append(limb * spot)
            pixels = array("H", (round(base * band) for row in spectrum
                                 for base, band in zip(brightness, row)))
            if byteorder == "big":
                pixels.byteswap()
            pixels.tofile(stream)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    create_scan(parser.parse_args().output)

"""Pack the visible BASS2000 atlas bundled with Astro4j on a 0.01 Å grid."""

import argparse
from array import array
from itertools import islice
from pathlib import Path
import sys


def pack(source: Path, output: Path) -> None:
    start, stop = 3000, 10000
    values = array("H")
    next_wavelength = start
    with source.open("rb") as rows:
        # The first eight rows contain intensity calibration scans.
        for row in islice(rows, 8, None):
            fields = row.split()
            wavelength = int(fields[0])
            if start <= wavelength < stop:
                intensities = fields[-1]
                if wavelength != next_wavelength or len(intensities) != 2000:
                    raise ValueError(f"Unexpected atlas row at {wavelength} Å")
                values.extend(int(intensities[i : i + 4]) for i in range(0, 2000, 20))
                next_wavelength += 1
    if next_wavelength != stop:
        raise ValueError(f"Atlas is incomplete at {next_wavelength} Å")
    if sys.byteorder != "little":
        values.byteswap()
    output.write_bytes(values.tobytes())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="Astro4j jsolex-core/src/bass2000/atlasvi.dat")
    parser.add_argument("output", type=Path, help="Path for the packed atlas")
    args = parser.parse_args()
    pack(args.input, args.output)

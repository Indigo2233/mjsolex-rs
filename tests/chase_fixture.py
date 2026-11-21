"""Convert a CHASE/HIS level-1 H-alpha wavelength interval to monochrome SER."""

import argparse
from datetime import datetime
from pathlib import Path
from struct import pack_into

import numpy as np
from astropy.io import fits


def as_u16(samples):
    return (samples.astype("<i2").view("<u2") ^ np.uint16(0x8000)).astype("<u2", copy=False)


def convert(args):
    with args.fits.open("rb") as stream:
        header = fits.Header.fromfile(stream)
        data_offset = stream.tell()
    width, frames, planes = [header[f"NAXIS{i}"] for i in (1, 2, 3)]
    if (header["LVL_NUM"], header["BITPIX"], header["BSCALE"], header["BZERO"]) != (1, 16, 1, 32768):
        raise ValueError("Expected a level-1 CHASE unsigned 16-bit image cube")
    if not 0 <= args.first <= args.last < planes or args.step < 1:
        raise ValueError("Invalid wavelength plane range or spatial sample stride")

    height = args.last - args.first + 1
    plane_bytes = frames * width * 2
    start = data_offset + args.first * plane_bytes
    end = start + height * plane_bytes - 1
    source = args.band if args.band else args.fits
    if args.band and args.band.stat().st_size != height * plane_bytes:
        raise ValueError("The downloaded byte range has the wrong size")
    cube = np.memmap(source, dtype=">i2", mode="r", offset=0 if args.band else start,
                     shape=(height, frames, width))
    step = args.step
    sampled = cube[:, ::step, ::step]
    _, frame_count, slit_width = sampled.shape

    ser_header = bytearray(178)
    ser_header[:14] = b"LUCAM-RECORDER"
    pack_into("<7I", ser_header, 14, 0, 0, 1, slit_width, height, 16, frame_count)
    instrument = b"CHASE/HIS RSM L1 (derived)"
    ser_header[82:82 + len(instrument)] = instrument
    observed = datetime.fromisoformat(header["DATE_OBS"])
    ticks = (observed - datetime(1, 1, 1)).days * 864_000_000_000
    ticks += (observed.hour * 3600 + observed.minute * 60 + observed.second) * 10_000_000
    ticks += observed.microsecond * 10
    pack_into("<2Q", ser_header, 162, ticks, ticks)
    with args.output.open("wb") as stream:
        stream.write(ser_header)
        for start_row in range(0, frame_count, 32):
            slab = sampled[:, start_row:start_row + 32, :].transpose(1, 0, 2)
            as_u16(slab).tofile(stream)

    # The reference is the observed channel with the lowest mean inside the disk.
    stride = step * 8
    y, x = np.ogrid[0:frames:stride, 0:width:stride]
    disk = (x - header["CRPIX1"]) ** 2 + (y - header["CRPIX2"]) ** 2 < (0.8 * header["R_SUN"]) ** 2
    profile = cube[:, ::stride, ::stride][:, disk].mean(axis=1)
    if profile.argmin() in (0, height - 1):
        raise ValueError("The observed absorption minimum is outside the selected band")
    ref_row = int(profile.argmin())
    ref_plane = args.first + ref_row
    wavelength = header["CRVAL3"] + (ref_plane - header["CRPIX3"]) * header["CDELT3"]
    if args.reference:
        fits.PrimaryHDU(as_u16(sampled[ref_row])).writeto(args.reference, overwrite=True)
    print(f"{header['DATE_OBS']} UTC, FITS bytes {start}-{end}")
    print(f"SER {slit_width} x {height} x {frame_count}; reference row {ref_row}, "
          f"plane {ref_plane}, {wavelength:.4f} angstrom (archive wavelength convention)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fits", type=Path, help="Full FITS or its header range")
    parser.add_argument("output", type=Path, help="Derived SER")
    parser.add_argument("--band", type=Path, help="Raw HTTP byte range containing the selected planes")
    parser.add_argument("--first", type=int, required=True, help="First zero-based wavelength plane")
    parser.add_argument("--last", type=int, required=True, help="Last zero-based wavelength plane, inclusive")
    parser.add_argument("--step", type=int, default=2, help="Sample every Nth slit pixel and scan step")
    parser.add_argument("--reference", type=Path, help="Save the original line-core plane in FITS")
    convert(parser.parse_args())

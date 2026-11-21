use std::io::Cursor;

use mjsolex::{Error, SerReader, Spectrum, Trace, reconstruct};

fn ser_header(
    signature: &[u8; 14],
    width: u32,
    height: u32,
    depth: u32,
    count: u32,
    endian: u32,
) -> Vec<u8> {
    let mut bytes = vec![0; 178];
    bytes[..14].copy_from_slice(signature);
    for (offset, value) in [
        (22, endian),
        (26, width),
        (30, height),
        (34, depth),
        (38, count),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn mono(width: u32, height: u32, depth: u32, count: u32) -> Vec<u8> {
    ser_header(b"LUCAM-RECORDER", width, height, depth, count, 1)
}

#[test]
fn rejects_bad_headers_and_truncated_frames() {
    let mut truncated = mono(3, 2, 8, 2);
    truncated.extend([0; 11]);
    let mut color = mono(3, 2, 8, 1);
    color[18] = 8;
    assert!(matches!(
        SerReader::open(Cursor::new(color)),
        Err(Error::Color(8))
    ));

    for invalid in [
        vec![0; 177],
        vec![0; 178],
        mono(0, 1, 8, 1),
        mono(1, 1, 17, 1),
        mono(u32::MAX, u32::MAX, 16, 1),
        truncated,
    ] {
        assert!(matches!(
            SerReader::open(Cursor::new(invalid)),
            Err(Error::InvalidSer(_))
        ));
    }
}

#[test]
fn decodes_mono_depths_and_both_byte_orders() {
    for endian in [0, 1] {
        let mut bytes = ser_header(b"LUCAM-RECORDER", 2, 1, 12, 1, endian);
        for sample in [0x039a_u16, 0x0fff] {
            bytes.extend(if endian == 1 {
                sample.to_le_bytes()
            } else {
                sample.to_be_bytes()
            });
        }
        bytes.extend([0; 8]); // Optional timestamps follow frame data.
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        assert_eq!(
            Spectrum::average(&mut reader, |_| true).unwrap().pixels(),
            &[14752., 65520.]
        );
    }

    let mut bytes = mono(2, 1, 8, 1);
    bytes.extend([0x20, 0xff]);
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    assert_eq!(
        Spectrum::average(&mut reader, |_| true).unwrap().pixels(),
        &[8192., 65280.]
    );
}

fn curved_scan() -> Vec<u8> {
    let mut bytes = mono(64, 30, 16, 12);
    for frame in 0..12 {
        for y in 0..30 {
            for x in 0..64 {
                let row = curve(x);
                let line = (-((y as f64 - row) / 1.8).powi(2)).exp();
                let pixel = (31000. + 450. * f64::from(frame) - 18500. * line).round() as u16;
                bytes.extend(pixel.to_le_bytes());
            }
        }
    }
    bytes
}

fn curve(x: usize) -> f64 {
    let x = (x as f64 - 31.5) / 31.5;
    12.45 + 1.4 * x + 3.2 * x * x
}

#[test]
fn fits_curved_absorption_line_and_reconstructs_with_offset() {
    let mut reader = SerReader::open(Cursor::new(curved_scan())).unwrap();
    let average = Spectrum::average(&mut reader, |_| true).unwrap();
    for seed in [None, Some((11, 13))] {
        let trace = average.fit_line(seed).unwrap();
        for x in [0, 12, 28, 42, 63] {
            assert!(
                (trace.row(x) - curve(x)).abs() < 0.15,
                "column {x}: {}",
                trace.row(x)
            );
        }
        let center = reconstruct(&mut reader, trace, 0., |_| true).unwrap();
        let wing = reconstruct(&mut reader, trace, 3., |_| true).unwrap();
        assert_eq!((center.width(), center.height()), (64, 12));
        for (line, offset) in center.pixels().iter().zip(wing.pixels()) {
            assert!(*offset > *line + 7000);
        }
        assert!(center.pixels()[64 * 11] > center.pixels()[0] + 4800);
    }
}

#[test]
fn rejects_invalid_shift_and_cancels_processing() {
    let mut reader = SerReader::open(Cursor::new(curved_scan())).unwrap();
    assert!(matches!(
        Spectrum::average(&mut reader, |p| p < 0.4),
        Err(Error::Cancelled)
    ));
    let trace = Trace::constant(64, 12.);
    assert!(matches!(
        reconstruct(&mut reader, trace, 0., |p| p < 0.4),
        Err(Error::Cancelled)
    ));
    for invalid in [f64::NAN, f64::INFINITY, 90.] {
        assert!(matches!(
            reconstruct(&mut reader, trace, invalid, |_| true),
            Err(Error::InvalidTrace)
        ));
    }
}

#[test]
fn blank_spectrum_has_no_automatic_fit() {
    let mut bytes = mono(8, 8, 8, 1);
    bytes.extend([32; 64]);
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    let spectrum = Spectrum::average(&mut reader, |_| true).unwrap();
    assert!(matches!(spectrum.fit_line(None), Err(Error::NoLine)));
    assert_eq!(spectrum.fit_line(Some((2, 4))).unwrap().row(0), 4.);
}

#[test]
fn matches_jsolex_sampling_and_exports_lossless_png() {
    let cases = [
        (b"LUCAM-RECORDER", 1.75, 0.5, [2195, 14603, 22868, 8843]),
        (b"LUCAM-RECORDER", 0., -0.25, [96, 454, 3449, 21073]),
        (b"JSOLEX-TRIMMED", 2., 0.25, [2125, 23525, 15775, 875]),
    ];
    let rows = [
        [10, 0, 500, 12000],
        [200, 1000, 7000, 32000],
        [1500, 31000, 0, 500],
        [4000, 1100, 63100, 2000],
        [3000, 4000, 1, 65535],
        [59700, 9950, 65000, 3500],
    ];
    // Expected samples come from SolexVideoProcessor.processSingleFrame at 7ffdaf5c.
    for (signature, row, shift, expected) in cases {
        let mut bytes = ser_header(signature, 4, 6, 16, 1, 1);
        for sample in rows.into_iter().flatten() {
            bytes.extend(u16::to_le_bytes(sample));
        }
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        let reconstruction =
            reconstruct(&mut reader, Trace::constant(4, row), shift, |_| true).unwrap();
        assert_eq!(reconstruction.pixels(), &expected);

        let mut png = Vec::new();
        reconstruction.write_png(&mut png).unwrap();
        let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
        let mut data = [0; 8];
        let info = reader.next_frame(&mut data).unwrap();
        assert_eq!((info.width, info.height), (4, 1));
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(info.bit_depth, png::BitDepth::Sixteen);
        assert_eq!(data, expected.map(u16::to_be_bytes).concat().as_slice());
    }
}

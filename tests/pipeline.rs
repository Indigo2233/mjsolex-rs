use std::cell::Cell;
use std::io::{Cursor, Read, Seek, SeekFrom};

use mjsolex::{
    ByteOrder, ColorMode, DisplayImage, DisplayTone, Error, FitsMetadata, PhysicalFlat, Product,
    SerReader, Spectrum, Trace, reconstruct, reconstruct_offsets, reconstruct_with_stride,
};

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
    ser_header(b"LUCAM-RECORDER", width, height, depth, count, 0)
}

#[test]
fn exports_display_pixels_and_preserves_raw_16_bit_samples() {
    let mut bytes = mono(5, 1, 16, 1);
    let samples = [100_u16, 200, 300, 400, 500];
    bytes.extend(samples.into_iter().flat_map(u16::to_le_bytes));
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    let image = reconstruct(&mut reader, Trace::constant(5, 0.), 0., |_| true).unwrap();
    for product in [
        Product::Gray,
        Product::Hydrogen,
        Product::Calcium,
        Product::Helium,
    ] {
        let display = DisplayImage::new(&image, None, product).unwrap();
        let mut encoded = Vec::new();
        display.write_png(&mut encoded).unwrap();
        let mut png = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
        let mut pixels = vec![0; png.output_buffer_size().unwrap()];
        let info = png.next_frame(&mut pixels).unwrap();
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        let channels = if product == Product::Gray { 1 } else { 3 };
        for (i, pixel) in pixels.chunks_exact(channels).enumerate() {
            assert_eq!(pixel, &display.pixel(i)[..channels]);
        }
        assert_eq!(display.pixel(0), [0; 3]);
        assert_eq!(display.pixel(4).into_iter().max(), Some(255));
    }
    for (tone, expected_midpoint) in [(DisplayTone::Darker, 107), (DisplayTone::Brighter, 146)] {
        let display = DisplayImage::new(&image, None, Product::Gray)
            .unwrap()
            .with_tone(tone);
        let mut encoded = Vec::new();
        display.write_png(&mut encoded).unwrap();
        let mut png = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
        let mut pixels = vec![0; png.output_buffer_size().unwrap()];
        png.next_frame(&mut pixels).unwrap();
        assert_eq!(pixels[2], expected_midpoint);
        assert_eq!(pixels[2], display.pixel(2)[0]);
        assert_eq!([pixels[0], pixels[4]], [0, 255]);
    }
    let mut encoded = Vec::new();
    image.write_png(&mut encoded).unwrap();
    let mut png = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
    let mut pixels = vec![0; png.output_buffer_size().unwrap()];
    assert_eq!(
        png.next_frame(&mut pixels).unwrap().bit_depth,
        png::BitDepth::Sixteen
    );
    assert_eq!(
        pixels,
        samples
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>()
    );
}

#[test]
fn verifies_doppler_coverage_and_color_direction() {
    let mut bytes = ser_header(b"JSOLEX-TRIMMED", 4, 7, 16, 1, 0);
    for y in 0..7 {
        let samples = match y {
            2 => [200_u16, 450, 750, 600],
            4 => [200_u16, 750, 450, 600],
            _ => [600_u16; 4],
        };
        bytes.extend(samples.into_iter().flat_map(u16::to_le_bytes));
    }
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    let spectrum = Spectrum::average(&mut reader, |_| true).unwrap();
    let trace = Trace::constant(4, 3.);
    assert!(spectrum.supports_wings(trace, 0., 1.));
    assert!(!spectrum.supports_wings(trace, 2., 1.));
    assert!(!spectrum.supports_wings(trace, 0., 0.));
    let red = reconstruct(&mut reader, trace, 1., |_| true).unwrap();
    let blue = reconstruct(&mut reader, trace, -1., |_| true).unwrap();
    let doppler = DisplayImage::new(&red, Some(&blue), Product::Doppler).unwrap();
    let [r1, _, b1] = doppler.pixel(1);
    let [r2, _, b2] = doppler.pixel(2);
    assert!(r1 < b1 && b2 < r2);
    assert_eq!(doppler.pixel(3), [255; 3]);
    let reversed = DisplayImage::new(&blue, Some(&red), Product::Doppler).unwrap();
    assert_eq!(reversed.pixel(1), doppler.pixel(2));
}

#[test]
fn captured_flat_corrects_slit_response_and_rejects_invalid_inputs() {
    const WIDTH: u32 = 24;
    const HEIGHT: u32 = 5;
    let mut flat_ser = mono(WIDTH, HEIGHT, 16, 2);
    let mut scan_ser = mono(WIDTH, HEIGHT, 16, 1);
    for frame in 0..2 {
        for y in 0..HEIGHT as u16 {
            for x in 0..WIDTH {
                let gain = match x {
                    0 | 23 => 0,
                    x if x < WIDTH / 2 => 1,
                    _ => 2,
                };
                let flat_value = gain * (1000 + 700 * y + 23 * frame);
                flat_ser.extend(flat_value.to_le_bytes());
                if frame == 0 {
                    scan_ser.extend((gain * 3000_u16).to_le_bytes());
                }
            }
        }
    }
    let mut scan = SerReader::open(Cursor::new(scan_ser)).unwrap();
    let spectrum = Spectrum::average(&mut scan, |_| true).unwrap();
    let mut flat_reader = SerReader::open(Cursor::new(flat_ser)).unwrap();
    assert!(matches!(
        PhysicalFlat::from_ser(&mut flat_reader, &spectrum, |_| false),
        Err(Error::Cancelled)
    ));
    let mut incompatible = mono(WIDTH, HEIGHT, 8, 1);
    incompatible.resize(178 + (WIDTH * HEIGHT) as usize, 128);
    let mut incompatible = SerReader::open(Cursor::new(incompatible)).unwrap();
    assert!(matches!(
        PhysicalFlat::from_ser(
            &mut flat_reader,
            &Spectrum::average(&mut incompatible, |_| true).unwrap(),
            |_| true,
        ),
        Err(Error::FlatMismatch)
    ));
    let mut dark_ser = mono(WIDTH, HEIGHT, 16, 1);
    dark_ser.resize(178 + (WIDTH * HEIGHT * 2) as usize, 0);
    let mut dark = SerReader::open(Cursor::new(dark_ser)).unwrap();
    assert!(matches!(
        PhysicalFlat::from_ser(&mut dark, &spectrum, |_| true),
        Err(Error::InvalidPhysicalFlat)
    ));
    let flat = PhysicalFlat::from_ser(&mut flat_reader, &spectrum, |_| true).unwrap();
    let mut image =
        reconstruct(&mut scan, Trace::constant(WIDTH as usize, 2.), 0., |_| true).unwrap();
    flat.apply(&mut image).unwrap();
    let pixels = image.pixels();
    assert_eq!((pixels[0], pixels[WIDTH as usize - 1]), (0, 0));
    assert!(
        pixels[1..WIDTH as usize - 1]
            .iter()
            .all(|&sample| sample == 3000)
    );
    let metadata = FitsMetadata {
        flat_file: Some("slit-flat.ser".to_owned()),
        ..FitsMetadata::default()
    };
    let mut fits = Vec::new();
    image
        .write_fits_with_metadata(&mut fits, &metadata)
        .unwrap();
    let cards = str::from_utf8(&fits[..2880]).unwrap();
    assert!(cards.contains("FLATCORR= 'PHYSICAL'"));
    assert!(cards.contains("FLATFILE= 'slit-flat.ser'"));
}

#[test]
fn captured_flat_accepts_the_scan_byte_order_override() {
    const WIDTH: u32 = 16;
    const HEIGHT: u32 = 5;
    // Both files declare little-endian but have big-endian data. The flat
    // contains two broad levels, which are not enough to infer the order.
    let mut science = ser_header(b"LUCAM-RECORDER", WIDTH, HEIGHT, 16, 1, 0);
    let mut flat = science.clone();
    for _ in 0..HEIGHT {
        for x in 0..WIDTH {
            let gain = if x < WIDTH / 2 { 1 } else { 2 };
            science.extend((3000_u16 * gain).to_be_bytes());
            flat.extend((1000_u16 * gain).to_be_bytes());
        }
    }
    let order = Some(ByteOrder::Big);
    let mut scan = SerReader::open_with_byte_order(Cursor::new(science), order).unwrap();
    let spectrum = Spectrum::average(&mut scan, |_| true).unwrap();
    let trace = Trace::constant(WIDTH as usize, 2.);
    let mut raw = reconstruct(&mut scan, trace, 0., |_| true).unwrap();
    let mut wrong_order = SerReader::open(Cursor::new(&flat)).unwrap();
    PhysicalFlat::from_ser(&mut wrong_order, &spectrum, |_| true)
        .unwrap()
        .apply(&mut raw)
        .unwrap();
    assert_ne!(raw.pixels()[0], raw.pixels()[WIDTH as usize - 1]);

    let mut raw = reconstruct(&mut scan, trace, 0., |_| true).unwrap();
    let mut same_order = SerReader::open_with_byte_order(Cursor::new(&flat), order).unwrap();
    PhysicalFlat::from_ser(&mut same_order, &spectrum, |_| true)
        .unwrap()
        .apply(&mut raw)
        .unwrap();
    assert!(raw.pixels().iter().all(|&sample| sample == 3000));
}

#[test]
fn rejects_bad_headers_and_truncated_frames() {
    let mut truncated = mono(3, 2, 8, 2);
    truncated.extend([0; 11]);
    let mut color = mono(3, 2, 8, 1);
    color[18] = 16;
    assert!(matches!(
        SerReader::open(Cursor::new(color)),
        Err(Error::Color(16))
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
    for (flag, order) in [
        (0, ByteOrder::Little),
        (1, ByteOrder::Big),
        (0, ByteOrder::Big),
        (1, ByteOrder::Little),
    ] {
        let mut bytes = ser_header(b"LUCAM-RECORDER", 2, 1, 12, 1, flag);
        for sample in [0x039a_u16, 0x0fff] {
            bytes.extend(if order == ByteOrder::Little {
                sample.to_le_bytes()
            } else {
                sample.to_be_bytes()
            });
        }
        bytes.extend([0; 8]); // Optional timestamps follow frame data.
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.header().byte_order(), order);
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

#[test]
fn decodes_both_16_bit_flag_conventions_with_byte_crossings() {
    let samples: Vec<u16> = (0..256).map(|x| 200 + 9 * x).collect();
    let expected: Vec<_> = samples.iter().copied().map(f32::from).collect();
    for (order, encode) in [
        (ByteOrder::Little, u16::to_le_bytes as fn(u16) -> [u8; 2]),
        (ByteOrder::Big, u16::to_be_bytes),
    ] {
        for signature in [b"LUCAM-RECORDER", b"JSOLEX-TRIMMED"] {
            for flag in [0, 1] {
                let mut bytes = ser_header(signature, 256, 1, 16, 1, flag);
                bytes.extend(samples.iter().copied().flat_map(encode));
                let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
                assert_eq!(reader.header().byte_order(), order);
                assert_eq!(
                    Spectrum::average(&mut reader, |_| true).unwrap().pixels(),
                    expected
                );
            }
        }
    }
}

#[test]
fn follows_ser_player_flags_when_only_one_byte_varies() {
    let expected: Vec<_> = (0..256).map(|x| f32::from(x * 256_u16)).collect();
    for (flag, order) in [(0, ByteOrder::Little), (1, ByteOrder::Big)] {
        let mut bytes = ser_header(b"LUCAM-RECORDER", 256, 1, 16, 1, flag);
        for x in 0..=255 {
            bytes.extend(if order == ByteOrder::Little {
                [0, x]
            } else {
                [x, 0]
            });
        }
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.header().byte_order(), order);
        assert_eq!(
            Spectrum::average(&mut reader, |_| true).unwrap().pixels(),
            expected
        );
    }
}

#[test]
fn recognizes_indigo_byte_order_and_exports_timed_observation_metadata() {
    const UNIX: u64 = 621_355_968_000_000_000;
    let mut bytes = ser_header(b"LUCAM-RECORDER", 2, 1, 16, 3, 0);
    bytes[42..59].copy_from_slice(b"MobileObservatory");
    bytes[82..91].copy_from_slice(b"ASI 174MM");
    bytes[122..130].copy_from_slice(b"H-alpha ");
    bytes[170..178].copy_from_slice(&UNIX.to_le_bytes());
    for sample in [258u16, 770, 1285, 8, 905, 15] {
        bytes.extend(sample.to_le_bytes());
    }
    for seconds in [1, 2, 3] {
        bytes.extend((UNIX + seconds * 10_000_000).to_le_bytes());
    }
    let mut overridden =
        SerReader::open_with_byte_order(Cursor::new(&bytes), Some(ByteOrder::Big)).unwrap();
    let other = reconstruct(&mut overridden, Trace::constant(2, 0.), 0., |_| true).unwrap();
    assert_eq!(other.pixels()[0], 513);

    let mut reader = SerReader::open(Cursor::new(&bytes)).unwrap();
    let header = reader.header();
    let obs = reader.observation().clone();
    assert_eq!(header.byte_order(), ByteOrder::Little);
    assert_eq!(
        obs.start_utc.as_deref(),
        Some("1970-01-01T00:00:01.0000000")
    );
    assert_eq!(obs.end_utc.as_deref(), Some("1970-01-01T00:00:03.0000000"));
    assert_eq!(obs.observer, None);
    assert_eq!(obs.filter.as_deref(), Some("H-alpha"));
    let image =
        reconstruct_with_stride(&mut reader, Trace::constant(2, 0.), 0., 2, |_| true).unwrap();
    assert_eq!(image.pixels(), &[258, 770, 905, 15]);
    let metadata = FitsMetadata {
        observation: obs,
        source_header: Some(header),
        source_name: Some("Sun's recording.ser".to_owned()),
        frame_step: Some(2),
        offset_pixels: Some(0.25),
        line_center_angstrom: Some(6562.8),
        dispersion_angstrom: Some(-0.04),
        fwhm_pixels: Some(4.0),
        ..FitsMetadata::default()
    };
    let mut fits = Vec::new();
    image
        .write_fits_with_metadata(&mut fits, &metadata)
        .unwrap();
    let text = str::from_utf8(&fits[..2880]).unwrap();
    assert!(text.contains("DATE-OBS= '1970-01-01T00:00:01.0000000'"));
    assert!(text.contains("DATE-END= '1970-01-01T00:00:03.0000000'"));
    assert!(text.contains("TIMESYS = 'UTC'"));
    assert!(text.contains("FILTER  = 'H-alpha'"));
    assert!(text.contains("RECORDER= 'Indigo Observatory'"));
    assert!(text.contains("SERFILE = 'Sun''s recording.ser'"));
    assert!(text.contains("SCNSTEP =                    2"));
    assert!(text.contains("6.562790000000E3"));
    assert!(text.contains("1.600000000000E-1"));
}

#[test]
fn decodes_bayer_green_and_rgb_luminance() {
    for (identifier, color) in [(100, ColorMode::Rgb), (101, ColorMode::Bgr)] {
        let mut bytes = ser_header(b"LUCAM-RECORDER", 1, 1, 8, 1, 0);
        bytes[18] = identifier;
        bytes.extend(if color == ColorMode::Rgb {
            [50, 100, 25]
        } else {
            [25, 100, 50]
        });
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        let image = reconstruct(&mut reader, Trace::constant(1, 0.), 0., |_| true).unwrap();
        assert_eq!(image.pixels(), &[21492]);
    }

    // A narrow reconstruction starts at an odd source row. Verify that cropped
    // reads still use the original Bayer phase.
    for (identifier, even_row_green) in [(8, false), (9, true), (10, true), (11, false)] {
        let width = 64;
        let height = 128;
        let mut bytes = ser_header(b"JSOLEX-TRIMMED", width, height, 8, 1, 0);
        bytes[18] = identifier;
        for y in 0..height {
            for x in 0..width {
                let green = x % 2 == 0;
                bytes.push(if green == ((y % 2 == 0) == even_row_green) {
                    40 + (y % 50) as u8
                } else {
                    240
                });
            }
        }
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        let image = reconstruct(
            &mut reader,
            Trace::constant(width as usize, 63.),
            0.,
            |_| true,
        )
        .unwrap();
        assert!(image.pixels().iter().all(|&value| value == 53 * 256));
    }
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
fn detects_a_low_signal_absorption_line() {
    let mut bytes = mono(24, 19, 16, 2);
    for _ in 0..2 {
        for y in -9i32..=9 {
            for _ in 0..24 {
                let value = 220 + y.unsigned_abs().pow(2) as u16 / 5;
                bytes.extend(value.to_le_bytes());
            }
        }
    }
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    let spectrum = Spectrum::average(&mut reader, |_| true).unwrap();
    let trace = spectrum.fit_line(None).unwrap();
    assert!((trace.row(12) - 9.).abs() < 1.6);
}

#[test]
fn narrow_trace_reads_only_the_relevant_rows_and_preserves_endianness() {
    struct Counted<'a>(Cursor<Vec<u8>>, &'a Cell<usize>);
    impl Read for Counted<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buffer)?;
            self.1.set(self.1.get() + n);
            Ok(n)
        }
    }
    impl Seek for Counted<'_> {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(position)
        }
    }

    let mut bytes = ser_header(b"LUCAM-RECORDER", 32, 256, 12, 6, 0);
    for frame in 0..6 {
        for y in 0..256 {
            for x in 0..32 {
                let value = frame * 384 + x * 3 + y * 4;
                bytes.extend(u16::to_be_bytes(value));
            }
        }
    }

    let read = Cell::new(0);
    let mut reader = SerReader::open(Counted(Cursor::new(bytes), &read)).unwrap();
    // The order probe reads nine partial rows without touching the rest of the scan.
    assert_eq!(read.replace(0), 178 + 9 * 32 * 2);
    let image = reconstruct(&mut reader, Trace::constant(32, 120.25), 0., |_| true).unwrap();
    assert_eq!(read.get(), 6 * 4 * 32 * 2);
    for (index, &pixel) in image.pixels().iter().enumerate() {
        assert_eq!(
            usize::from(pixel),
            (index / 32 * 384 + index % 32 * 3 + 481) * 16
        );
    }
    let shifts = [0., 2., -2.];
    let trace = Trace::constant(32, 120.25);
    read.set(0);
    let images = reconstruct_offsets(&mut reader, trace, shifts, 2, |_| true).unwrap();
    // The three overlapping bands form eight rows, read once per sampled frame.
    assert_eq!(read.get(), 3 * 8 * 32 * 2);
    for (multi, shift) in images.iter().zip(shifts) {
        let single = reconstruct_with_stride(&mut reader, trace, shift, 2, |_| true).unwrap();
        assert_eq!(multi.pixels(), single.pixels());
    }
}

#[test]
fn reconstructs_one_spectral_row_without_interpolation() {
    for signature in [b"LUCAM-RECORDER", b"JSOLEX-TRIMMED"] {
        let mut bytes = ser_header(signature, 3, 1, 16, 2, 0);
        let pixels = [1_u16, 100, 65535, 90, 21, 0];
        for pixel in pixels {
            bytes.extend(pixel.to_le_bytes());
        }
        let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
        let spectrum = Spectrum::average(&mut reader, |_| true).unwrap();
        let trace = spectrum.fit_line(None).unwrap();
        let image = reconstruct(&mut reader, trace, 0., |_| true).unwrap();
        assert_eq!((image.width(), image.height()), (3, 2));
        assert_eq!(image.pixels(), &pixels);
        assert!(matches!(
            reconstruct(&mut reader, trace, 0.25, |_| true),
            Err(Error::InvalidTrace)
        ));
    }
}

#[test]
fn fits_preserves_unsigned_samples_and_pads_fits_blocks() {
    let mut bytes = ser_header(b"JSOLEX-TRIMMED", 3, 2, 16, 1, 0);
    for value in [0u16, 32768, 65535, 900, 900, 900] {
        bytes.extend(value.to_le_bytes());
    }
    let mut reader = SerReader::open(Cursor::new(bytes)).unwrap();
    let image = reconstruct(&mut reader, Trace::constant(3, 0.), 0., |_| true).unwrap();
    let mut fits = Vec::new();
    image.write_fits(&mut fits).unwrap();
    assert_eq!(fits.len(), 5760);
    let metadata = std::str::from_utf8(&fits[..2880]).unwrap();
    assert!(metadata.contains("NAXIS1  =                    3"));
    assert!(metadata.contains("BZERO   =                32768"));
    assert!(metadata.contains("ORIGIN  = 'MJSol''Ex'"));
    assert_eq!(&fits[2880..2886], &[0x80, 0, 0, 0, 0x7f, 0xff]);
    assert!(fits[2886..].iter().all(|byte| *byte == 0));
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
        let mut bytes = ser_header(signature, 4, 6, 16, 1, 0);
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

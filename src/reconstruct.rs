use std::io::{Read, Seek, Write};

use crate::{Error, SerReader, Trace};

pub(crate) const MAX_PIXELS: usize = 32 * 1024 * 1024;

pub struct Reconstruction {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u16>,
}

impl Reconstruction {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }

    pub fn write_png(&self, output: impl Write) -> Result<(), Error> {
        let mut encoder = png::Encoder::new(output, self.width as u32, self.height as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header()?;
        let mut stream = writer.stream_writer()?;
        let mut bytes = vec![0; self.width * 2];
        for row in self.pixels.chunks_exact(self.width) {
            for (sample, pair) in row.iter().zip(bytes.as_chunks_mut::<2>().0) {
                *pair = sample.to_be_bytes();
            }
            stream.write_all(&bytes)?;
        }
        stream.finish()?;
        writer.finish()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct Tap {
    index: usize,
    weight: f64,
}

struct Column {
    taps: [Tap; 4],
    count: usize,
}

// Adapted from the Gaussian subpixel sampling in Astro4j's
// SolexVideoProcessor.processSingleFrame (Apache-2.0).
pub fn reconstruct<R: Read + Seek>(
    reader: &mut SerReader<R>,
    trace: Trace,
    shift: f64,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    let header = reader.header();
    let width = header.width;
    let height = header.frames as usize;
    if width
        .checked_mul(height)
        .is_none_or(|size| size > MAX_PIXELS)
    {
        return Err(Error::OutputTooLarge);
    }
    if !shift.is_finite() || trace.width() != width {
        return Err(Error::InvalidTrace);
    }

    let offsets: &[f64] = if header.trimmed || header.height == 1 {
        &[0.]
    } else {
        &[-1., -0.5, 0., 0.5, 1.]
    };
    let weights: Vec<_> = offsets.iter().map(|dy| (-0.5 * dy * dy).exp()).collect();
    let mut columns = Vec::with_capacity(width);
    let mut first_row = header.height;
    let mut last_row = 0;
    for x in 0..width {
        let center = trace.row(x) + shift;
        let mut column = Column {
            taps: [Tap::default(); 4],
            count: 0,
        };
        let mut weight_sum = 0.;
        for (&offset, &weight) in offsets.iter().zip(&weights) {
            let y = center + offset;
            let low = y.floor();
            let high = if header.height == 1 && y == 0. {
                0
            } else if (0. ..(header.height - 1) as f64).contains(&low) {
                low as usize + 1
            } else {
                continue;
            };
            let fraction = y - low;
            first_row = first_row.min(low as usize);
            last_row = last_row.max(high);
            for (row, weight) in [
                (low as usize, (1. - fraction) * weight),
                (high, fraction * weight),
            ] {
                let index = row * width + x;
                if let Some(tap) = column.taps[..column.count]
                    .iter_mut()
                    .find(|tap| tap.index == index)
                {
                    tap.weight += weight;
                } else {
                    column.taps[column.count] = Tap { index, weight };
                    column.count += 1;
                }
            }
            weight_sum += weight;
        }
        for tap in &mut column.taps[..column.count] {
            tap.weight /= weight_sum;
        }
        columns.push(column);
    }
    if first_row == header.height {
        return Err(Error::InvalidTrace);
    }
    let band_height = last_row - first_row + 1;
    // Keep sequential reads when skipping a small number of bytes per frame.
    let use_band = band_height <= header.height / 2
        && (header.height - band_height) * header.row_bytes() >= 4096;
    let rows = if use_band {
        let start = first_row * width;
        for column in &mut columns {
            for tap in &mut column.taps[..column.count] {
                tap.index -= start;
            }
        }
        first_row..last_row + 1
    } else {
        0..header.height
    };

    let mut pixels = Vec::with_capacity(width * height);
    for i in 0..header.frames {
        if !progress(i as f32 / header.frames as f32) {
            return Err(Error::Cancelled);
        }
        let frame = reader.frame_rows(i, rows.clone())?;
        for column in &columns {
            let sample = column.taps[..column.count]
                .iter()
                .map(|tap| tap.weight * f64::from(header.sample(frame, tap.index)))
                .sum::<f64>();
            pixels.push(sample.round() as u16);
        }
    }
    if !progress(1.) {
        return Err(Error::Cancelled);
    }
    Ok(Reconstruction {
        width,
        height,
        pixels,
    })
}

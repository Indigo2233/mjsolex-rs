use std::io::{Read, Seek, Write};

use crate::{Error, Header, SerReader, Trace};

pub(crate) const MAX_PIXELS: usize = 32 * 1024 * 1024;
pub(crate) const MAX_DOPPLER_WING_PIXELS: usize = MAX_PIXELS / 4;

/// Number of images that participate in scan, disk, and framing transforms.
#[cfg(any(target_os = "android", test))]
pub(crate) fn transform_image_count(wings: bool) -> usize {
    1 + 2 * usize::from(wings)
}

#[cfg(test)]
mod transform_count_tests {
    use super::transform_image_count;

    #[test]
    fn geometry_transforms_center_and_both_wing_images() {
        assert_eq!(transform_image_count(false), 1);
        assert_eq!(transform_image_count(true), 3);
    }
}

impl Header {
    /// Smallest interval that keeps a reconstruction within the pixel budget.
    pub fn minimum_stride(self) -> u32 {
        self.stride_for_budget(MAX_PIXELS)
    }

    pub fn minimum_doppler_stride(self) -> u32 {
        self.stride_for_budget(MAX_DOPPLER_WING_PIXELS)
    }

    fn stride_for_budget(self, budget: usize) -> u32 {
        let rows = (budget / self.width.max(1)).max(1) as u32;
        self.frames.div_ceil(rows).max(1)
    }
}

pub struct Reconstruction {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u16>,
    pub(crate) synthetic_flat: bool,
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

    pub fn has_synthetic_flat(&self) -> bool {
        self.synthetic_flat
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
    progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    reconstruct_with_stride(reader, trace, shift, 1, progress)
}

pub fn reconstruct_with_stride<R: Read + Seek>(
    reader: &mut SerReader<R>,
    trace: Trace,
    shift: f64,
    stride: u32,
    progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    let [image] = reconstruct_offsets(reader, trace, [shift], stride, progress)?;
    Ok(image)
}

/// Extract multiple spectral offsets from each sampled frame before advancing
/// the reader. Results share the scan sampling and follow the offset order.
pub fn reconstruct_offsets<R: Read + Seek, const N: usize>(
    reader: &mut SerReader<R>,
    trace: Trace,
    shifts: [f64; N],
    stride: u32,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<[Reconstruction; N], Error> {
    let header = reader.header();
    let width = header.width;
    if stride == 0 {
        return Err(Error::InvalidStride);
    }
    let frames = header.frames.div_ceil(stride);
    let height = frames as usize;
    let size = width.checked_mul(height).ok_or(Error::OutputTooLarge)?;
    if size.checked_mul(N).is_none_or(|count| count > MAX_PIXELS) {
        return Err(Error::OutputTooLarge);
    }
    if N == 0 || shifts.iter().any(|shift| !shift.is_finite()) || trace.width() != width {
        return Err(Error::InvalidTrace);
    }

    let offsets: &[f64] = if header.trimmed || header.height == 1 {
        &[0.]
    } else {
        &[-1., -0.5, 0., 0.5, 1.]
    };
    let weights: Vec<_> = offsets.iter().map(|dy| (-0.5 * dy * dy).exp()).collect();
    let mut columns: [Vec<Column>; N] = std::array::from_fn(|_| Vec::new());
    let mut first_row = header.height;
    let mut last_row = 0;
    for (columns, shift) in columns.iter_mut().zip(shifts) {
        let (first, last) = plan_columns(header, trace, shift, offsets, &weights, columns)?;
        first_row = first_row.min(first);
        last_row = last_row.max(last);
    }
    let band_height = last_row - first_row + 1;
    // Keep sequential reads when skipping a small number of bytes per frame.
    let use_band = band_height <= header.height / 2
        && (header.height - band_height) * header.row_bytes() >= 4096;
    let rows = if use_band {
        let start = first_row * width;
        for column in columns.iter_mut().flatten() {
            for tap in &mut column.taps[..column.count] {
                tap.index -= start;
            }
        }
        first_row..last_row + 1
    } else {
        0..header.height
    };

    let mut images: [Reconstruction; N] = std::array::from_fn(|_| Reconstruction {
        width,
        height,
        pixels: Vec::new(),
        synthetic_flat: false,
    });
    for image in &mut images {
        image.pixels.try_reserve_exact(size)?;
    }
    for i in 0..frames {
        if !progress(i as f32 / frames as f32) {
            return Err(Error::Cancelled);
        }
        let frame = reader.frame_rows(i * stride, rows.clone())?;
        for (image, columns) in images.iter_mut().zip(&columns) {
            for column in columns {
                let sample = column.taps[..column.count]
                    .iter()
                    .map(|tap| tap.weight * f64::from(header.sample(frame, tap.index, rows.start)))
                    .sum::<f64>();
                image.pixels.push(sample.round() as u16);
            }
        }
    }
    if !progress(1.) {
        return Err(Error::Cancelled);
    }
    Ok(images)
}

fn plan_columns(
    header: Header,
    trace: Trace,
    shift: f64,
    offsets: &[f64],
    weights: &[f64],
    columns: &mut Vec<Column>,
) -> Result<(usize, usize), Error> {
    let width = header.width;
    columns.try_reserve_exact(width)?;
    let mut first_row = header.height;
    let mut last_row = 0;
    for x in 0..width {
        let center = trace.row(x) + shift;
        let mut column = Column {
            taps: [Tap::default(); 4],
            count: 0,
        };
        let mut weight_sum = 0.;
        for (&offset, &weight) in offsets.iter().zip(weights) {
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
    Ok((first_row, last_row))
}

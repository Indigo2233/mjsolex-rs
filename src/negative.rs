use crate::geometry::FittedDisk;
use crate::reconstruct::MAX_PIXELS;
use crate::{Error, Reconstruction};

mod sky;

const MAX_PIXEL_VALUE: f64 = 65_535.;
const DISK_BOOST: f32 = 1.2;
const CONTRAST_EXPONENT: i32 = 3;
const PROMINENCE_STRETCH: f32 = 20.;
const ECLIPSE_STRETCH: f32 = 50.;
const AUTO_STRETCH_SHADOWS_CLIP: f64 = -2.8;
const AUTO_STRETCH_TARGET_BG: f64 = 0.25;
const MAD_TO_SIGMA: f64 = 1.4826;
const FALLBACK_TILE_SIZE: usize = 128;
const FALLBACK_BINS: usize = 512;
const FALLBACK_CLIP: f64 = 0.8;
const SHARPEN_KERNEL: usize = 7;
const PROGRESS_CHUNK_PIXELS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegativeOptions {
    /// Show prominences as dark features against a white sky.
    pub invert_prominences: bool,
    /// Darkening of the prominence signal, from 0 to 100 percent in inverted mode.
    pub prominence_strength: u8,
}

impl Default for NegativeOptions {
    fn default() -> Self {
        Self {
            invert_prominences: false,
            prominence_strength: 100,
        }
    }
}

/// Build a JSol'Ex-style negative. A fitted disk is inverted
/// independently from prominences. Images without a reliable limb use
/// JSol'Ex's CLAHE fallback.
pub fn reconstruct_negative(
    image: &Reconstruction,
    progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    reconstruct_negative_with_options(image, NegativeOptions::default(), progress)
}

pub fn reconstruct_negative_with_options(
    image: &Reconstruction,
    options: NegativeOptions,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    checked_pixel_count(image)?;
    if !progress(0.) {
        return Err(Error::Cancelled);
    }
    let mut data = to_f32(image, &mut progress)?;
    match crate::geometry::DiskGeometry::fit_limb(image) {
        Ok(disk) => {
            data = reconstruct_disk_negative(image, data, disk, options, &mut progress)?;
        }
        Err(Error::NoSolarLimb) => {
            if options.invert_prominences {
                return Err(Error::NoSolarLimb);
            }
            reconstruct_fallback_negative(&mut data, image.width, image.height, &mut progress)?
        }
        Err(error) => return Err(error),
    }
    to_u16(image, data, &mut progress)
}

/// Mask a fitted solar disk and show off-limb prominences in high contrast.
pub fn reconstruct_eclipse(
    image: &Reconstruction,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    let count = checked_pixel_count(image)?;
    if !progress(0.) {
        return Err(Error::Cancelled);
    }
    let disk = crate::geometry::DiskGeometry::fit_limb(image)?;
    let sky_level = disk.sky;
    let mask = DiskMask::new(image, disk, &mut progress)?;
    let background =
        sky::SkyProfile::estimate(image, &mask, sky_level, |p| progress(0.03 + 0.82 * p))?;
    let mut data = try_vec(count)?;
    data.resize(count, 0.);
    composite_prominences(
        image,
        &mask,
        &background,
        ProminenceStyle::Eclipse,
        &mut data,
        &mut progress,
    )?;
    drop(mask);
    to_u16(image, data, &mut progress)
}

fn checked_pixel_count(image: &Reconstruction) -> Result<usize, Error> {
    let count = image
        .width
        .checked_mul(image.height)
        .ok_or(Error::OutputTooLarge)?;
    if count > MAX_PIXELS {
        return Err(Error::OutputTooLarge);
    }
    if count == 0 || image.pixels.len() != count {
        return Err(Error::InvalidTrace);
    }
    Ok(count)
}

fn reconstruct_disk_negative(
    source: &Reconstruction,
    mut data: Vec<f32>,
    disk: FittedDisk,
    options: NegativeOptions,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<Vec<f32>, Error> {
    let sky_level = disk.sky;
    let mask = DiskMask::new(source, disk, progress)?;
    let boost = |value: f32| (value * DISK_BOOST).min(MAX_PIXEL_VALUE as f32);
    let peak = disk_bounds(&data, &mask, progress, 0.03, 0.07)?.1;
    let maximum = boost(peak).max(1.);
    for_each_chunk(&mut data, progress, 0.07, 0.23, |value| {
        let inverse = (maximum - boost(*value)).max(0.) / maximum;
        *value = inverse.powi(CONTRAST_EXPONENT) * MAX_PIXEL_VALUE as f32;
    })?;

    sharpen_7x7(&mut data, source.width, &mask, progress)?;

    let (minimum, maximum) = disk_bounds(&data, &mask, progress, 0.55, 0.62)?;
    if maximum > minimum {
        let scale = MAX_PIXEL_VALUE as f32 / (maximum - minimum);
        for_each_chunk(&mut data, progress, 0.62, 0.68, |value| {
            *value = ((*value - minimum) * scale).clamp(0., MAX_PIXEL_VALUE as f32);
        })?;
    }
    auto_stretch(&mut data, &mask, progress)?;
    let background =
        sky::SkyProfile::estimate(source, &mask, sky_level, |p| progress(0.8 + 0.05 * p))?;
    composite_prominences(
        source,
        &mask,
        &background,
        ProminenceStyle::Natural(options),
        &mut data,
        progress,
    )?;
    Ok(data)
}

fn reconstruct_fallback_negative(
    data: &mut [f32],
    width: usize,
    height: usize,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let maximum = maximum_chunked(data, progress, 0.02, 0.03)?;
    if maximum > 0. {
        let scale = MAX_PIXEL_VALUE / f64::from(maximum);
        for_each_chunk(data, progress, 0.03, 0.05, |value| {
            *value = (f64::from(*value) * scale) as f32;
        })?;
    }
    clahe(
        data,
        width,
        height,
        FALLBACK_TILE_SIZE,
        FALLBACK_BINS,
        FALLBACK_CLIP,
        &mut |p| progress(0.05 + 0.75 * p),
    )?;
    let maximum = maximum_chunked(data, progress, 0.8, 0.85)?;
    for_each_chunk(data, progress, 0.85, 0.9, |value| {
        *value = maximum - *value;
    })?;
    if !progress(0.9) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn clahe(
    data: &mut [f32],
    width: usize,
    height: usize,
    tile_size: usize,
    bins: usize,
    clip_ratio: f64,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    if data.is_empty() || width == 0 || height == 0 || tile_size == 0 || bins == 0 {
        return Ok(());
    }
    let columns = width.div_ceil(tile_size);
    let rows = height.div_ceil(tile_size);
    let mut maps = Vec::new();
    maps.try_reserve_exact(columns * rows)?;
    for tile_y in 0..rows {
        for tile_x in 0..columns {
            let mut histogram = try_vec(bins)?;
            histogram.resize(bins, 0_u32);
            let x0 = tile_x * tile_size;
            let x1 = (x0 + tile_size).min(width);
            let y0 = tile_y * tile_size;
            let y1 = (y0 + tile_size).min(height);
            for y in y0..y1 {
                for x in x0..x1 {
                    histogram[histogram_bin(data[y * width + x], bins)] += 1;
                }
            }
            let count = (x1 - x0) * (y1 - y0);
            let clip_limit = (clip_ratio * count as f64 / bins as f64) as u32;
            let mut excess = 0_u32;
            for value in &mut histogram {
                excess += value.saturating_sub(clip_limit);
                *value = (*value).min(clip_limit);
            }
            let increment = excess / bins as u32;
            let remainder = excess % bins as u32;
            for (index, value) in histogram.iter_mut().enumerate() {
                *value += increment + u32::from((index as u32) < remainder);
            }
            let mut cumulative = try_vec(bins)?;
            cumulative.resize(bins, 0_f32);
            let mut running = 0_u32;
            for (map, &count) in cumulative.iter_mut().zip(&histogram) {
                running += count;
                *map = running as f32;
            }
            let maximum = *cumulative.last().unwrap_or(&0_f32);
            if maximum > 0. {
                for value in &mut cumulative {
                    *value /= maximum;
                }
            }
            maps.push(cumulative);
            if !progress((tile_y * columns + tile_x) as f32 / (columns * rows) as f32 * 0.7) {
                return Err(Error::Cancelled);
            }
        }
    }
    for y in 0..height {
        for x in 0..width {
            let bin = histogram_bin(data[y * width + x], bins);
            let (left, right, dx) = tile_weights(x, tile_size, columns);
            let (top, bottom, dy) = tile_weights(y, tile_size, rows);
            let at = |tx: usize, ty: usize| maps[ty * columns + tx][bin];
            let upper = lerp(at(left, top), at(right, top), dx);
            let lower = lerp(at(left, bottom), at(right, bottom), dx);
            data[y * width + x] = lerp(upper, lower, dy) * MAX_PIXEL_VALUE as f32;
        }
        if y % 64 == 0 && !progress(0.7 + 0.3 * y as f32 / height as f32) {
            return Err(Error::Cancelled);
        }
    }
    Ok(())
}

fn histogram_bin(value: f32, bins: usize) -> usize {
    ((f64::from(value) * (bins - 1) as f64 / MAX_PIXEL_VALUE).round() as usize).min(bins - 1)
}

fn tile_weights(point: usize, tile_size: usize, limit: usize) -> (usize, usize, f32) {
    let coordinate = ((point as f32 + 0.5) / tile_size as f32 - 0.5).clamp(0., (limit - 1) as f32);
    let lower = coordinate as usize;
    (lower, (lower + 1).min(limit - 1), coordinate.fract())
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn advance_progress(
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
    processed: usize,
    total: usize,
) -> bool {
    progress(start + (end - start) * processed as f32 / total.max(1) as f32)
}

fn for_each_chunk(
    data: &mut [f32],
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
    mut apply: impl FnMut(&mut f32),
) -> Result<(), Error> {
    let total = data.len();
    for (index, chunk) in data.chunks_mut(PROGRESS_CHUNK_PIXELS).enumerate() {
        for value in chunk {
            apply(value);
        }
        let processed = ((index + 1) * PROGRESS_CHUNK_PIXELS).min(total);
        if !advance_progress(progress, start, end, processed, total) {
            return Err(Error::Cancelled);
        }
    }
    Ok(())
}

fn maximum_chunked(
    data: &[f32],
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
) -> Result<f32, Error> {
    let mut maximum = 0_f32;
    for (index, chunk) in data.chunks(PROGRESS_CHUNK_PIXELS).enumerate() {
        for &value in chunk {
            maximum = maximum.max(value);
        }
        let processed = ((index + 1) * PROGRESS_CHUNK_PIXELS).min(data.len());
        if !advance_progress(progress, start, end, processed, data.len()) {
            return Err(Error::Cancelled);
        }
    }
    Ok(maximum)
}

fn disk_bounds(
    data: &[f32],
    mask: &DiskMask,
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
) -> Result<(f32, f32), Error> {
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for (index, (chunk, coverages)) in data
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
        .enumerate()
    {
        for (&value, &coverage) in chunk.iter().zip(coverages) {
            if coverage == 16 {
                minimum = minimum.min(value);
                maximum = maximum.max(value);
            }
        }
        let processed = ((index + 1) * PROGRESS_CHUNK_PIXELS).min(data.len());
        if !advance_progress(progress, start, end, processed, data.len()) {
            return Err(Error::Cancelled);
        }
    }
    Ok((minimum, maximum))
}

fn apply_transfer_chunked(
    data: &mut [f32],
    shadows: f64,
    midtones: f64,
    highlights: f64,
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
) -> Result<(), Error> {
    for_each_chunk(data, progress, start, end, |value| {
        let clamped = f64::from(*value).clamp(shadows, highlights);
        let position = (clamped - shadows) / (highlights - shadows);
        *value = transfer(position, midtones).unwrap_or(0.) as f32 * MAX_PIXEL_VALUE as f32;
    })
}

fn sharpen_7x7(
    data: &mut [f32],
    width: usize,
    mask: &DiskMask,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let height = data.len() / width;
    let radius = SHARPEN_KERNEL / 2;
    let mut rows = try_vec(SHARPEN_KERNEL * width)?;
    rows.resize(SHARPEN_KERNEL * width, 0.);
    let mut vertical = try_vec(width)?;
    vertical.resize(width, 0.);
    for slot in 0..SHARPEN_KERNEL {
        let source_y = slot.saturating_sub(radius).min(height - 1);
        let values = &mut rows[slot * width..(slot + 1) * width];
        horizontal_row(data, source_y, width, radius, values);
        for (sum, value) in vertical.iter_mut().zip(values.iter()) {
            *sum += value;
        }
    }

    let neighbor_weight = 1. / (SHARPEN_KERNEL * SHARPEN_KERNEL - 1) as f32;
    // Fade sharpening before the kernel begins to include sky pixels.
    let limb_margin =
        radius as f64 * ((1. + mask.shear.abs()) / mask.radius_x).hypot(1. / mask.radius_y);
    let limit = (1. - limb_margin).max(0.).powi(2);
    let full = (1. - 2. * limb_margin).max(0.).powi(2);
    let transition = (limit - full).max(f64::EPSILON);
    let mut first = 0;
    for y in 0..height {
        for (x, &column_sum) in vertical.iter().enumerate() {
            let i = y * width + x;
            if mask.coverage[i] == 16 {
                let center = data[i];
                let radius = mask.radius_squared(x as f64, y as f64);
                let strength = ((limit - radius) / transition).clamp(0., 1.) as f32;
                let neighbor_average = (column_sum - center) * neighbor_weight;
                data[i] += strength * (center - neighbor_average);
            }
        }
        if y % 16 == 0 && !progress(0.23 + 0.32 * y as f32 / height as f32) {
            return Err(Error::Cancelled);
        }
        if y + 1 < height {
            let values = &mut rows[first * width..(first + 1) * width];
            for (sum, value) in vertical.iter_mut().zip(values.iter()) {
                *sum -= value;
            }
            // The rolling buffer keeps the original sums for the rows already sharpened.
            // The next input row is always below the row just written, even at the border.
            let incoming = (y + radius + 1).min(height - 1);
            horizontal_row(data, incoming, width, radius, values);
            for (sum, value) in vertical.iter_mut().zip(values.iter()) {
                *sum += value;
            }
            first = (first + 1) % SHARPEN_KERNEL;
        }
    }
    Ok(())
}

fn horizontal_row(source: &[f32], y: usize, width: usize, radius: usize, output: &mut [f32]) {
    let row = &source[y * width..(y + 1) * width];
    for (x, output) in output.iter_mut().enumerate().take(width) {
        let mut sum = 0.;
        for offset in 0..SHARPEN_KERNEL {
            let source_x = (x + offset).saturating_sub(radius).min(width - 1);
            sum += row[source_x];
        }
        *output = sum;
    }
}

fn auto_stretch(
    data: &mut [f32],
    mask: &DiskMask,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let (median, mad) = median_and_mad(data, mask, progress)?;
    let normalized_median = f64::from(median) / MAX_PIXEL_VALUE;
    let normalized_mad = (f64::from(mad) / MAX_PIXEL_VALUE * MAD_TO_SIGMA).max(0.00001);
    let shadows = (normalized_median + AUTO_STRETCH_SHADOWS_CLIP * normalized_mad).max(0.);
    let midtone_position = if shadows < 1. {
        (normalized_median - shadows) / (1. - shadows)
    } else {
        0.
    };
    let midtones = transfer(midtone_position, AUTO_STRETCH_TARGET_BG)
        .filter(|value| value.is_finite() && *value > 0.)
        .unwrap_or(0.5);
    apply_transfer_chunked(
        data,
        shadows * MAX_PIXEL_VALUE,
        midtones,
        MAX_PIXEL_VALUE,
        progress,
        0.72,
        0.8,
    )?;
    Ok(())
}

fn median_and_mad(
    data: &[f32],
    mask: &DiskMask,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(u16, u16), Error> {
    let mut histogram = try_vec(65_536)?;
    histogram.resize(65_536, 0_u32);
    for (index, (chunk, coverage)) in data
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
        .enumerate()
    {
        for (&value, &coverage) in chunk.iter().zip(coverage) {
            if coverage == 16 {
                histogram[pixel_level(value)] += 1;
            }
        }
        let processed = index * PROGRESS_CHUNK_PIXELS + chunk.len();
        if !advance_progress(progress, 0.68, 0.7, processed, data.len()) {
            return Err(Error::Cancelled);
        }
    }
    let median = histogram_median(&histogram).unwrap_or(0);
    let mut deviations = try_vec(65_536)?;
    deviations.resize(65_536, 0_u32);
    for (index, (chunk, coverage)) in data
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
        .enumerate()
    {
        for (&value, &coverage) in chunk.iter().zip(coverage) {
            if coverage == 16 {
                deviations[pixel_level((f64::from(value) - f64::from(median)).abs() as f32)] += 1;
            }
        }
        let processed = index * PROGRESS_CHUNK_PIXELS + chunk.len();
        if !advance_progress(progress, 0.7, 0.72, processed, data.len()) {
            return Err(Error::Cancelled);
        }
    }
    Ok((median, histogram_median(&deviations).unwrap_or(0)))
}

fn histogram_median(histogram: &[u32]) -> Option<u16> {
    let total: u64 = histogram.iter().map(|&value| u64::from(value)).sum();
    if total == 0 {
        return None;
    }
    let first_rank = (total - 1) / 2;
    let second_rank = total / 2;
    let mut running = 0_u64;
    let mut first = None;
    for (level, &count) in histogram.iter().enumerate() {
        running += u64::from(count);
        if first.is_none() && running > first_rank {
            first = Some(level as u32);
        }
        if running > second_rank {
            let second = level as u32;
            return Some(((first? + second) / 2) as u16);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum ProminenceStyle {
    Natural(NegativeOptions),
    Eclipse,
}

fn composite_prominences(
    source: &Reconstruction,
    mask: &DiskMask,
    background: &sky::SkyProfile,
    style: ProminenceStyle,
    data: &mut [f32],
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let max = MAX_PIXEL_VALUE as f32;
    let eclipse = matches!(style, ProminenceStyle::Eclipse);
    let stretch = if eclipse {
        ECLIPSE_STRETCH
    } else {
        PROMINENCE_STRETCH
    };
    let dn_scale = stretch / max;
    let stretch_scale = 1. / (background.white * dn_scale).asinh();
    let (sky, contrast) = match style {
        ProminenceStyle::Natural(options) if options.invert_prominences => {
            (1., -f32::from(options.prominence_strength.min(100)) / 100.)
        }
        _ => (0., 1.),
    };
    let radius = mask.radius_x.min(mask.radius_y);
    let feather_width = (0.006 * radius).clamp(2., 8.) / radius;
    let inner_radius_squared = (1. - feather_width).powi(2);
    let sample_radius = 1. + 1. / radius;
    let sample_radius_squared = sample_radius.powi(2);
    let total = data.len();
    for (y, ((pixels, coverages), values)) in source
        .pixels
        .chunks_exact(source.width)
        .zip(mask.coverage.chunks_exact(source.width))
        .zip(data.chunks_exact_mut(source.width))
        .enumerate()
    {
        for (x, ((&pixel, &coverage), value)) in
            pixels.iter().zip(coverages).zip(values).enumerate()
        {
            if eclipse && coverage == 16 {
                continue;
            }
            let x = x as f64;
            let radius_squared = mask.radius_squared(x, y as f64);
            if !eclipse && radius_squared <= inner_radius_squared {
                continue;
            }
            let disk_weight = if eclipse {
                f32::from(coverage) / 16.
            } else if radius_squared < 1. {
                let t = ((1. - radius_squared.sqrt()) / feather_width) as f32;
                t * t * (3. - 2. * t)
            } else {
                0.
            };
            // Use a sample from just outside the limb on both sides of the
            // boundary. Otherwise disk light would create a false bright ring.
            let (sample, sky_radius_squared) = if radius_squared < sample_radius_squared {
                let scale = sample_radius / radius_squared.sqrt();
                let sample_x = mask.center_x + (x - mask.center_x) * scale;
                let sample_y = mask.center_y + (y as f64 - mask.center_y) * scale;
                (
                    crate::framing::sample_bilinear(source, sample_x, sample_y),
                    sample_radius_squared,
                )
            } else {
                (pixel, radius_squared)
            };
            let residual = if sample > 0 {
                background.residual(sample, sky_radius_squared)
            } else {
                0.
            };
            let response = if residual > 0. {
                (residual * dn_scale).asinh() * stretch_scale
            } else {
                0.
            };
            let signal = if eclipse {
                response.min(1.)
            } else {
                prominence_shoulder(response)
            };
            let sky_pixel = (sky + contrast * signal) * max;
            *value = lerp(sky_pixel, *value, disk_weight);
        }
        if !advance_progress(progress, 0.85, 0.9, (y + 1) * source.width, total) {
            return Err(Error::Cancelled);
        }
    }
    Ok(())
}

fn prominence_shoulder(signal: f32) -> f32 {
    const KNEE: f32 = 0.65;
    const HEADROOM: f32 = 0.29;
    if signal <= KNEE {
        return signal;
    }
    // Match the linear slope at the knee and approach 94% without clipping.
    let excess = signal - KNEE;
    KNEE + HEADROOM * excess / (HEADROOM + excess)
}

fn transfer(value: f64, midtones: f64) -> Option<f64> {
    if value <= 0. {
        return Some(0.);
    }
    if value >= 1. {
        return Some(1.);
    }
    let denominator = (2. * midtones - 1.) * value - midtones;
    (denominator != 0.).then(|| (midtones - 1.) * value / denominator)
}

fn pixel_level(value: f32) -> usize {
    value.round().clamp(0., MAX_PIXEL_VALUE as f32) as usize
}

fn to_f32(
    image: &Reconstruction,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<Vec<f32>, Error> {
    let mut data = try_vec(image.pixels.len())?;
    for (index, values) in image.pixels.chunks(PROGRESS_CHUNK_PIXELS).enumerate() {
        data.extend(values.iter().map(|&value| f32::from(value)));
        if !advance_progress(
            progress,
            0.,
            0.02,
            ((index + 1) * PROGRESS_CHUNK_PIXELS).min(image.pixels.len()),
            image.pixels.len(),
        ) {
            return Err(Error::Cancelled);
        }
    }
    Ok(data)
}

fn to_u16(
    image: &Reconstruction,
    data: Vec<f32>,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    let mut pixels = try_vec(data.len())?;
    for (index, value) in data.into_iter().enumerate() {
        if index % 65_536 == 0
            && !progress(0.9 + 0.1 * index as f32 / image.pixels.len().max(1) as f32)
        {
            return Err(Error::Cancelled);
        }
        pixels.push(value.round().clamp(0., MAX_PIXEL_VALUE as f32) as u16);
    }
    if !progress(1.) {
        return Err(Error::Cancelled);
    }
    Ok(Reconstruction {
        width: image.width,
        height: image.height,
        pixels,
        synthetic_flat: image.synthetic_flat,
    })
}

fn try_vec<T>(capacity: usize) -> Result<Vec<T>, Error> {
    let mut values = Vec::new();
    values.try_reserve_exact(capacity)?;
    Ok(values)
}

struct DiskMask {
    center_x: f64,
    center_y: f64,
    radius_x: f64,
    radius_y: f64,
    shear: f64,
    coverage: Vec<u8>,
}

impl DiskMask {
    fn new(
        image: &Reconstruction,
        disk: FittedDisk,
        progress: &mut impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        let mut mask = Self {
            center_x: disk.center_x,
            center_y: disk.center_y,
            radius_x: disk.geometry.slit_radius,
            radius_y: disk.geometry.scan_radius,
            shear: disk.geometry.shear,
            coverage: try_vec(image.pixels.len())?,
        };
        mask.coverage.resize(image.pixels.len(), 0);
        // Only pixels whose subpixel samples may cross the limb need supersampling.
        let margin = 0.375 * ((1. + mask.shear.abs()) / mask.radius_x).hypot(1. / mask.radius_y);
        let full_coverage = (1. - margin).max(0.).powi(2);
        let no_coverage = (1. + margin).powi(2);
        for y in 0..image.height {
            if !advance_progress(progress, 0.02, 0.03, y, image.height) {
                return Err(Error::Cancelled);
            }
            for x in 0..image.width {
                let radius = mask.radius_squared(x as f64, y as f64);
                mask.coverage[y * image.width + x] = if radius < full_coverage {
                    16
                } else if radius > no_coverage {
                    0
                } else {
                    mask.coverage_at(x as f64, y as f64)
                };
            }
        }
        Ok(mask)
    }

    fn coverage_at(&self, x: f64, y: f64) -> u8 {
        let mut inside = 0;
        for sample_y in 0..4 {
            for sample_x in 0..4 {
                let px = x - 0.5 + (sample_x as f64 + 0.5) / 4.;
                let py = y - 0.5 + (sample_y as f64 + 0.5) / 4.;
                inside += usize::from(self.radius_squared(px, py) <= 1.);
            }
        }
        inside as u8
    }

    fn radius_squared(&self, x: f64, y: f64) -> f64 {
        let y = y - self.center_y;
        let dy = y / self.radius_y;
        let dx = (x - self.center_x - self.shear * y) / self.radius_x;
        dx * dx + dy * dy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reconstruction(width: usize, height: usize, pixels: Vec<u16>) -> Reconstruction {
        Reconstruction {
            width,
            height,
            pixels,
            synthetic_flat: false,
        }
    }

    fn disk(width: usize, height: usize, sky: u16, center: (f64, f64), radius: f64) -> Vec<u16> {
        (0..width * height)
            .map(|index| {
                let x = (index % width) as f64 - center.0;
                let y = (index / width) as f64 - center.1;
                if x.hypot(y) <= radius {
                    (12_000. + 20_000. * (1. - x.hypot(y) / radius)).round() as u16
                } else {
                    sky
                }
            })
            .collect()
    }

    #[test]
    fn disk_negative_keeps_prominences_visible_and_preserves_input() {
        let width = 128;
        let height = 96;
        let mut pixels = disk(width, height, 300, (64., 48.), 34.);
        for y in 40..56 {
            for x in 97..109 {
                pixels[y * width + x] = 2_400;
            }
        }
        let source = reconstruction(width, height, pixels);
        let original = source.pixels().to_vec();
        let negative = reconstruct_negative(&source, |_| true).unwrap();
        assert_eq!(source.pixels(), original);
        assert_eq!((negative.width(), negative.height()), (width, height));
        let disk_level = negative.pixels()[48 * width + 64];
        let sky_level = negative.pixels()[2 * width + 2];
        let prominence_level = negative.pixels()[48 * width + 103];
        assert_ne!(disk_level, original[48 * width + 64]);
        assert_eq!(sky_level, 0);
        assert!(prominence_level > sky_level);
    }

    #[test]
    fn inverted_prominences_make_the_limb_feature_darker() {
        let width = 128;
        let height = 96;
        let mut pixels = disk(width, height, 0, (64., 48.), 34.);
        for y in 40..56 {
            for x in 97..109 {
                pixels[y * width + x] = 2_400;
            }
        }
        let source = reconstruction(width, height, pixels);
        let normal = reconstruct_negative(&source, |_| true).unwrap();
        let inverted = reconstruct_negative_with_options(
            &source,
            NegativeOptions {
                invert_prominences: true,
                prominence_strength: 100,
            },
            |_| true,
        )
        .unwrap();
        let disk_core = 48 * width + 64;
        let prominence = 48 * width + 103;
        assert!(inverted.pixels()[disk_core] > 0);
        assert!(inverted.pixels()[prominence] < normal.pixels()[prominence]);
    }

    #[test]
    fn prominence_inversion_strength_preserves_polarity() {
        let width = 128;
        let height = 96;
        let mut pixels = disk(width, height, 0, (64., 48.), 34.);
        for y in 40..56 {
            for x in 97..109 {
                pixels[y * width + x] = if x < 104 { 2_400 } else { 1_200 };
            }
        }
        let source = reconstruction(width, height, pixels);
        let full = reconstruct_negative_with_options(
            &source,
            NegativeOptions {
                invert_prominences: true,
                prominence_strength: 100,
            },
            |_| true,
        )
        .unwrap();
        let half = reconstruct_negative_with_options(
            &source,
            NegativeOptions {
                invert_prominences: true,
                prominence_strength: 50,
            },
            |_| true,
        )
        .unwrap();
        let index = 48 * width + 103;
        let fainter_prominence = 48 * width + 107;
        assert!(half.pixels()[index] > full.pixels()[index]);
        assert!(half.pixels()[index] < half.pixels()[fainter_prominence]);
        assert!(half.pixels()[fainter_prominence] < u16::MAX);
        assert_eq!(half.pixels()[0], u16::MAX);
    }

    #[test]
    fn low_dn_negative_separates_scattered_light_from_prominences() {
        let (width, height) = (220, 220);
        let pixels = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                let radius = (x as f64 - 110.).hypot(y as f64 - 110.);
                if radius <= 74. {
                    (120. + 130. * (1. - radius / 74.)).round() as u16
                } else if !(68..=365).contains(&(x + y)) {
                    0
                } else {
                    let sky = (24. - (radius - 74.) / 2.5).max(2.) as u16;
                    let prominence = (187..=191).contains(&x) && (107..=113).contains(&y);
                    sky + ((x + y) % 4) as u16 + if prominence { 14 } else { 0 }
                }
            })
            .collect();
        let source = reconstruction(width, height, pixels);
        let normal = reconstruct_negative(&source, |_| true).unwrap();
        let inverted = reconstruct_negative_with_options(
            &source,
            NegativeOptions {
                invert_prominences: true,
                prominence_strength: 100,
            },
            |_| true,
        )
        .unwrap();
        let at = |x, y| y * width + x;
        let disk = at(70, 130);
        let limb = at(178, 110);
        let sky = at(31, 110);
        let prominence = at(189, 110);
        assert_eq!(normal.pixels()[disk], inverted.pixels()[disk]);
        assert!(normal.pixels()[limb] < 60_000);
        assert_eq!(normal.pixels()[sky], 0);
        assert_eq!(inverted.pixels()[sky], u16::MAX);
        assert!(normal.pixels()[prominence] > 10_000);
        assert!(inverted.pixels()[prominence] < 55_535);
    }

    #[test]
    fn natural_negatives_preserve_distinct_tones_in_strong_prominences() {
        let (width, height) = (128, 96);
        let mut pixels = disk(width, height, 200, (64., 48.), 34.);
        let fainter = 48 * width + 106;
        let brighter = 48 * width + 110;
        pixels[fainter] = 6_000;
        pixels[brighter] = 14_000;
        let source = reconstruction(width, height, pixels);
        let normal = reconstruct_negative(&source, |_| true).unwrap();
        let inverted = reconstruct_negative_with_options(
            &source,
            NegativeOptions {
                invert_prominences: true,
                prominence_strength: 100,
            },
            |_| true,
        )
        .unwrap();
        assert!(0 < normal.pixels()[fainter]);
        assert!(normal.pixels()[fainter] < normal.pixels()[brighter]);
        assert!(normal.pixels()[brighter] < u16::MAX);
        assert!(0 < inverted.pixels()[brighter]);
        assert!(inverted.pixels()[brighter] < inverted.pixels()[fainter]);
        assert!(inverted.pixels()[fainter] < u16::MAX);
        let eclipse = reconstruct_eclipse(&source, |_| true).unwrap();
        assert!(normal.pixels()[48 * width + 84] > 0);
        assert_eq!(eclipse.pixels()[48 * width + 84], 0);
        assert_eq!(eclipse.pixels()[fainter], u16::MAX);
        assert_eq!(eclipse.pixels()[brighter], u16::MAX);
    }

    #[test]
    fn negative_smooths_the_transition_to_quiet_sky_at_the_limb() {
        let (width, height) = (128, 96);
        let source = reconstruction(width, height, disk(width, height, 200, (64., 48.), 34.));
        let negative = reconstruct_negative(&source, |_| true).unwrap();
        let pixels = &negative.pixels()[48 * width..49 * width];
        let limb = crate::DiskGeometry::fit_limb(&source).unwrap();
        let dy = 48. - limb.center_y;
        let right_edge = limb.center_x
            + limb.geometry.shear * dy
            + limb.geometry.slit_radius * (1. - (dy / limb.geometry.scan_radius).powi(2)).sqrt();
        let edge = right_edge.floor() as usize;
        let transition = &pixels[edge - 1..=edge + 1];
        assert_eq!(transition[2], 0);
        assert!(
            0 < transition[1] && transition[1] < transition[0],
            "{transition:?}"
        );
        assert!(transition[0] < u16::MAX);
    }

    #[test]
    fn fallback_negative_uses_current_maximum_and_handles_black_input() {
        let source = reconstruction(8, 4, (0..32).map(|i| (i * 499) as u16).collect());
        let negative = reconstruct_negative(&source, |_| true).unwrap();
        assert_eq!(negative.pixels().len(), source.pixels().len());
        let minimum = negative.pixels().iter().copied().min();
        let maximum = negative.pixels().iter().copied().max();
        assert!(minimum.is_some_and(|value| value < 1_024));
        assert!(maximum.is_some_and(|value| value > 60_000));

        let black = reconstruction(5, 1, vec![0; 5]);
        let negative = reconstruct_negative(&black, |_| true).unwrap();
        assert_eq!(negative.pixels(), &[0; 5]);
    }

    #[test]
    fn negative_cancellation_is_reported_and_respects_the_budget() {
        let source = reconstruction(128, 96, disk(128, 96, 300, (64., 48.), 34.));
        assert!(matches!(
            reconstruct_negative(&source, |progress| progress < 0.2),
            Err(Error::Cancelled)
        ));
        let oversized = Reconstruction {
            width: MAX_PIXELS + 1,
            height: 1,
            pixels: Vec::new(),
            synthetic_flat: false,
        };
        assert!(matches!(
            reconstruct_negative(&oversized, |_| {
                panic!("budget must be checked before progress")
            }),
            Err(Error::OutputTooLarge)
        ));
    }

    #[test]
    fn postprocessing_accepts_images_above_eight_mipixels() {
        let (width, height) = (4096, 2049);
        let source = reconstruction(width, height, vec![0; width * height]);
        assert!(matches!(
            reconstruct_negative(&source, |_| false),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            reconstruct_eclipse(&source, |_| false),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn disk_mask_cancels_during_a_full_budget_build() {
        let width = 4096;
        let height = MAX_PIXELS / width;
        assert_eq!(width * height, MAX_PIXELS);
        let source = reconstruction(width, height, vec![0; width * height]);
        let disk = FittedDisk {
            geometry: crate::DiskGeometry {
                slit_radius: 1024.,
                scan_radius: 1024.,
                shear: 0.,
            },
            center_x: 2048.,
            center_y: 1024.,
            sky: 0,
        };
        let mut callbacks = 0;
        assert!(matches!(
            DiskMask::new(&source, disk, &mut |_| {
                callbacks += 1;
                callbacks == 1
            }),
            Err(Error::Cancelled)
        ));
        assert_eq!(callbacks, 2);
    }

    #[test]
    fn disk_negative_polls_cancellation_after_mask_construction() {
        let width = 512;
        let height = 512;
        let source = reconstruction(width, height, disk(width, height, 300, (256., 256.), 180.));
        let mut preprocessing_callbacks = 0;
        assert!(matches!(
            reconstruct_negative(&source, |progress| {
                if progress >= 0.03 {
                    preprocessing_callbacks += 1;
                    preprocessing_callbacks == 1
                } else {
                    true
                }
            }),
            Err(Error::Cancelled)
        ));
        assert_eq!(preprocessing_callbacks, 2);
    }

    #[test]
    fn transfer_maps_black_midtone_and_white_to_expected_levels() {
        let mut values = [0., MAX_PIXEL_VALUE as f32 / 2., MAX_PIXEL_VALUE as f32];
        apply_transfer_chunked(
            &mut values,
            0.,
            0.25,
            MAX_PIXEL_VALUE,
            &mut |_| true,
            0.,
            1.,
        )
        .unwrap();
        assert_eq!(values[0], 0.);
        assert!((f64::from(values[1]) - MAX_PIXEL_VALUE * 0.75).abs() < 0.01);
        assert_eq!(values[2], MAX_PIXEL_VALUE as f32);
    }
}

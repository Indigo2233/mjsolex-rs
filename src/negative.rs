use crate::geometry::FittedDisk;
use crate::{Error, Reconstruction};

const MAX_PIXEL_VALUE: f64 = 65_535.;
const DISK_BOOST: f64 = 1.2;
const CONTRAST_EXPONENT: f64 = 3.;
const PROMINENCE_STRETCH: f64 = 20.;
const AUTO_STRETCH_SHADOWS_CLIP: f64 = -2.8;
const AUTO_STRETCH_TARGET_BG: f64 = 0.25;
const MAD_TO_SIGMA: f64 = 1.4826;
const FALLBACK_TILE_SIZE: usize = 128;
const FALLBACK_BINS: usize = 512;
const FALLBACK_CLIP: f64 = 0.8;
const SHARPEN_KERNEL: usize = 7;
pub(crate) const NEGATIVE_PIXEL_BUDGET: usize = 8 * 1024 * 1024;
const PROGRESS_CHUNK_PIXELS: usize = 65_536;

/// Build the negative product used by JSol'Ex. A fitted disk is inverted
/// independently from prominences. Images without a reliable limb use
/// JSol'Ex's CLAHE fallback.
pub fn reconstruct_negative(
    image: &Reconstruction,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<Reconstruction, Error> {
    let count = image
        .width
        .checked_mul(image.height)
        .ok_or(Error::OutputTooLarge)?;
    if count > NEGATIVE_PIXEL_BUDGET {
        return Err(Error::OutputTooLarge);
    }
    if image.pixels.len() != count {
        return Err(Error::InvalidTrace);
    }
    if !progress(0.) {
        return Err(Error::Cancelled);
    }
    let mut data = to_f32(image, &mut progress)?;
    match crate::geometry::DiskGeometry::fit_limb(image) {
        Ok(disk) => reconstruct_disk_negative(image, &mut data, disk, &mut progress)?,
        Err(Error::NoSolarLimb) => {
            reconstruct_fallback_negative(&mut data, image.width, image.height, &mut progress)?
        }
        Err(error) => return Err(error),
    }
    to_u16(image, data, &mut progress)
}

fn reconstruct_disk_negative(
    source: &Reconstruction,
    data: &mut [f32],
    disk: FittedDisk,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let width = source.width;
    let mask = DiskMask::new(source, disk, progress)?;
    if !progress(0.03) {
        return Err(Error::Cancelled);
    }
    for_each_chunk(data, progress, 0.03, 0.075, |value| {
        *value = (f64::from(*value) * DISK_BOOST).min(MAX_PIXEL_VALUE) as f32;
    })?;
    let maximum = maximum_chunked(data, progress, 0.075, 0.09)?;
    for_each_chunk(data, progress, 0.09, 0.12, |value| {
        *value = maximum - *value;
    })?;
    for_each_chunk(data, progress, 0.12, 0.14, |value| {
        *value = f64::from(*value).powf(CONTRAST_EXPONENT) as f32;
    })?;
    normalize_to_full_range_chunked(data, progress, 0.14, 0.17)?;
    if !progress(0.17) {
        return Err(Error::Cancelled);
    }

    let mut sharpened = try_vec(data.len())?;
    sharpen_7x7(data, width, &mut sharpened, progress)?;
    let sharpened_len = sharpened.len();
    for (index, (chunk, coverage)) in sharpened
        .chunks_mut(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
        .enumerate()
    {
        for (value, &coverage) in chunk.iter_mut().zip(coverage) {
            *value *= f32::from(coverage) / 16.;
        }
        let processed = (index + 1) * PROGRESS_CHUNK_PIXELS;
        if !advance_progress(
            progress,
            0.62,
            0.65,
            processed.min(sharpened_len),
            sharpened_len,
        ) {
            return Err(Error::Cancelled);
        }
    }
    normalize_to_full_range_chunked(&mut sharpened, progress, 0.65, 0.68)?;

    auto_stretch(&mut sharpened, &mask, progress)?;
    let black_point =
        estimate_black_point(source, &mask, &mut |p| progress(0.8 + 0.04 * p))? * DISK_BOOST;
    add_prominences(source, &mask, black_point, &mut sharpened, progress)?;
    for (target, source) in data
        .chunks_mut(PROGRESS_CHUNK_PIXELS)
        .zip(sharpened.chunks(PROGRESS_CHUNK_PIXELS))
    {
        target.copy_from_slice(source);
    }
    if !progress(0.9) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn reconstruct_fallback_negative(
    data: &mut [f32],
    width: usize,
    height: usize,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let maximum = maximum_chunked(data, progress, 0., 0.01)?;
    if maximum > 0. {
        let scale = MAX_PIXEL_VALUE / f64::from(maximum);
        for_each_chunk(data, progress, 0.01, 0.05, |value| {
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

fn bounds_chunked(
    data: &[f32],
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
) -> Result<Option<(f32, f32)>, Error> {
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for (index, chunk) in data.chunks(PROGRESS_CHUNK_PIXELS).enumerate() {
        for &value in chunk {
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
        let processed = ((index + 1) * PROGRESS_CHUNK_PIXELS).min(data.len());
        if !advance_progress(progress, start, end, processed, data.len()) {
            return Err(Error::Cancelled);
        }
    }
    Ok(
        (minimum.is_finite() && maximum.is_finite() && maximum > minimum)
            .then_some((minimum, maximum)),
    )
}

fn normalize_to_full_range_chunked(
    data: &mut [f32],
    progress: &mut impl FnMut(f32) -> bool,
    start: f32,
    end: f32,
) -> Result<(), Error> {
    let midpoint = start + (end - start) / 2.;
    let Some((minimum, maximum)) = bounds_chunked(data, progress, start, midpoint)? else {
        return Ok(());
    };
    let scale = MAX_PIXEL_VALUE as f32 / (maximum - minimum);
    for_each_chunk(data, progress, midpoint, end, |value| {
        *value = (*value - minimum) * scale;
    })
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
    source: &[f32],
    width: usize,
    output: &mut Vec<f32>,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    if width == 0 || !source.len().is_multiple_of(width) {
        return Err(Error::InvalidTrace);
    }
    let height = source.len() / width;
    let radius = SHARPEN_KERNEL / 2;
    let mut rows = try_vec(SHARPEN_KERNEL * width)?;
    rows.resize(SHARPEN_KERNEL * width, 0.);
    let mut vertical = try_vec(width)?;
    vertical.resize(width, 0.);
    for slot in 0..SHARPEN_KERNEL {
        let source_y = slot.saturating_sub(radius).min(height - 1);
        let values = &mut rows[slot * width..(slot + 1) * width];
        horizontal_row(source, source_y, width, radius, values);
        for (sum, value) in vertical.iter_mut().zip(values.iter()) {
            *sum += value;
        }
    }

    output.try_reserve_exact(source.len())?;
    output.resize(source.len(), 0.);
    let neighbor_weight = 1. / (SHARPEN_KERNEL * SHARPEN_KERNEL - 1) as f32;
    let mut first = 0;
    for y in 0..height {
        for x in 0..width {
            let center = source[y * width + x];
            output[y * width + x] = 2. * center - (vertical[x] - center) * neighbor_weight;
        }
        if y % 16 == 0 && !progress(0.37 + 0.18 * y as f32 / height as f32) {
            return Err(Error::Cancelled);
        }
        if y + 1 < height {
            let values = &mut rows[first * width..(first + 1) * width];
            for (sum, value) in vertical.iter_mut().zip(values.iter()) {
                *sum -= value;
            }
            let incoming = (y + radius + 1).min(height - 1);
            horizontal_row(source, incoming, width, radius, values);
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
    for (chunk, coverage) in data
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
    {
        for (&value, &coverage) in chunk.iter().zip(coverage) {
            if coverage == 16 {
                histogram[pixel_level(value)] += 1;
            }
        }
        if !advance_progress(progress, 0.68, 0.7, chunk.len(), data.len()) {
            return Err(Error::Cancelled);
        }
    }
    let median = histogram_median(&histogram).unwrap_or(0);
    let mut deviations = try_vec(65_536)?;
    deviations.resize(65_536, 0_u32);
    for (chunk, coverage) in data
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
    {
        for (&value, &coverage) in chunk.iter().zip(coverage) {
            if coverage == 16 {
                deviations[pixel_level((f64::from(value) - f64::from(median)).abs() as f32)] += 1;
            }
        }
        if !advance_progress(progress, 0.7, 0.72, chunk.len(), data.len()) {
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

fn add_prominences(
    source: &Reconstruction,
    mask: &DiskMask,
    black_point: f64,
    data: &mut [f32],
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for (index, pixels) in source.pixels.chunks(PROGRESS_CHUNK_PIXELS).enumerate() {
        let start = index * PROGRESS_CHUNK_PIXELS;
        for (offset, &pixel) in pixels.iter().enumerate() {
            let value = prominence_stretch(
                pixel,
                f32::from(mask.coverage[start + offset]) / 16.,
                black_point,
            );
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
        if !advance_progress(
            progress,
            0.8,
            0.84,
            start + pixels.len(),
            source.pixels.len(),
        ) {
            return Err(Error::Cancelled);
        }
    }
    if !progress(0.84) {
        return Err(Error::Cancelled);
    }
    if !minimum.is_finite() || !maximum.is_finite() || maximum <= minimum {
        return Ok(());
    }
    let scale = MAX_PIXEL_VALUE / (maximum - minimum);
    let total = data.len();
    for (index, ((pixels, coverage), values)) in source
        .pixels
        .chunks(PROGRESS_CHUNK_PIXELS)
        .zip(mask.coverage.chunks(PROGRESS_CHUNK_PIXELS))
        .zip(data.chunks_mut(PROGRESS_CHUNK_PIXELS))
        .enumerate()
    {
        for ((&pixel, &coverage), value) in pixels.iter().zip(coverage).zip(values.iter_mut()) {
            let prominence = (prominence_stretch(pixel, f32::from(coverage) / 16., black_point)
                - minimum)
                * scale;
            *value = (f64::from(*value) + prominence).clamp(0., MAX_PIXEL_VALUE) as f32;
        }
        if !advance_progress(
            progress,
            0.85,
            0.9,
            ((index + 1) * PROGRESS_CHUNK_PIXELS).min(total),
            total,
        ) {
            return Err(Error::Cancelled);
        }
    }
    if !progress(0.88) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn prominence_stretch(pixel: u16, coverage: f32, black_point: f64) -> f64 {
    let source = f64::from(pixel);
    let original = match coverage {
        coverage if coverage > 0.999 => black_point,
        coverage if coverage > 0.001 => {
            black_point * f64::from(coverage) + source * (1. - f64::from(coverage))
        }
        _ => source,
    };
    if original <= 0. {
        return 0.;
    }
    (original * PROMINENCE_STRETCH).asinh() / PROMINENCE_STRETCH.asinh() * MAX_PIXEL_VALUE
}

fn estimate_black_point(
    image: &Reconstruction,
    mask: &DiskMask,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<f64, Error> {
    let mut estimate = 0.;
    let mut count = 0_u64;
    for (row, pixels) in image.pixels.chunks(image.width).enumerate() {
        for (x, &pixel) in pixels.iter().enumerate() {
            if mask.contains(x, row) || pixel == 0 {
                continue;
            }
            let off_center = 2. * (x as f64 - mask.center_x).hypot(row as f64 - mask.center_y)
                / (image.width + image.height) as f64;
            count += 1;
            estimate += (off_center * f64::from(pixel) - estimate) / count as f64;
        }
        if !advance_progress(
            progress,
            0.,
            1.,
            (row + 1) * image.width,
            image.pixels.len(),
        ) {
            return Err(Error::Cancelled);
        }
    }
    Ok(estimate)
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
        for y in 0..image.height {
            if !progress(0.025 * (y + 1) as f32 / image.height.max(1) as f32) {
                return Err(Error::Cancelled);
            }
            for x in 0..image.width {
                mask.coverage[y * image.width + x] = mask.coverage_at(x as f64, y as f64);
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
                let dy = (py - self.center_y) / self.radius_y;
                let dx = (px - self.center_x - self.shear * (py - self.center_y)) / self.radius_x;
                inside += usize::from(dx * dx + dy * dy <= 1.);
            }
        }
        inside as u8
    }

    fn contains(&self, x: usize, y: usize) -> bool {
        let dy = (y as f64 - self.center_y) / self.radius_y;
        let dx =
            (x as f64 - self.center_x - self.shear * (y as f64 - self.center_y)) / self.radius_x;
        dx * dx + dy * dy <= 1.
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
        assert!(sky_level > 0);
        assert!(prominence_level > sky_level);
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
            width: NEGATIVE_PIXEL_BUDGET + 1,
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
    fn disk_mask_cancels_during_a_full_budget_build() {
        let width = 4096;
        let height = NEGATIVE_PIXEL_BUDGET / width;
        assert_eq!(width * height, NEGATIVE_PIXEL_BUDGET);
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

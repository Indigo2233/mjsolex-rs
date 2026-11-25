use super::{DiskMask, advance_progress, lerp, try_vec};
use crate::{Error, Reconstruction};

const SAMPLE_BUDGET: usize = 150_000;
const RINGS: usize = 32;
const FIRST_RADIUS_SQUARED: f64 = 1.012 * 1.012;
const RING_STEP_SQUARED: f64 = 0.04;
const PROMINENCE_RADIUS_SQUARED: f64 = 1.15 * 1.15;
const SKY_IQR_MULTIPLIER: f32 = 2.25;
const MIN_BAND_SAMPLES: usize = 12;

pub(super) struct SkyProfile {
    cutoff: [f32; RINGS],
    pub(super) white: f32,
}

impl SkyProfile {
    pub(super) fn estimate(
        source: &Reconstruction,
        mask: &DiskMask,
        fallback_sky: u16,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        // An odd stride also samples alternating columns on even-width images.
        let stride = source.pixels.len().div_ceil(SAMPLE_BUDGET) | 1;
        let count = source.pixels.len().div_ceil(stride);
        let mut sky_samples = try_vec(count)?;
        let mut disk_samples = try_vec(count)?;
        let mut total_per_ring = [0_usize; RINGS];
        let samples = || source.pixels.iter().copied().enumerate().step_by(stride);
        let radius_at =
            |i| mask.radius_squared((i % source.width) as f64, (i / source.width) as f64);

        for (sample, (i, pixel)) in samples().enumerate() {
            if sample % 4_096 == 0 && !advance_progress(&mut progress, 0., 0.5, sample, count) {
                return Err(Error::Cancelled);
            }
            if mask.coverage[i] == 16 {
                if pixel > 0 {
                    disk_samples.push(pixel);
                }
                continue;
            }
            let radius = radius_at(i);
            if radius >= FIRST_RADIUS_SQUARED {
                let ring = (ring_position(radius) as usize).min(RINGS - 1);
                total_per_ring[ring] += 1;
                if pixel > 0 {
                    sky_samples.push((ring, pixel));
                }
            }
        }

        let cutoff = ring_cutoffs(&mut sky_samples, &total_per_ring, fallback_sky);
        let mut profile = Self { cutoff, white: 1. };
        // Reference the prominence stretch to real near-limb signal and use the
        // disk level to keep a nearly empty sky from stretching isolated noise.
        let disk_level = f32::from(percentile(&mut disk_samples, 50));
        let mut residuals = disk_samples;
        residuals.clear();
        for (sample, (i, pixel)) in samples().enumerate() {
            if sample % 4_096 == 0 && !advance_progress(&mut progress, 0.5, 1., sample, count) {
                return Err(Error::Cancelled);
            }
            if pixel > 0 && mask.coverage[i] == 0 {
                let radius = radius_at(i);
                if (FIRST_RADIUS_SQUARED..=PROMINENCE_RADIUS_SQUARED).contains(&radius) {
                    let residual = profile.residual(pixel, radius).round() as u16;
                    if residual > 0 {
                        residuals.push(residual);
                    }
                }
            }
        }
        profile.white = f32::from(percentile(&mut residuals, 98))
            .max(disk_level * 0.1)
            .max(1.);
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        Ok(profile)
    }

    pub(super) fn residual(&self, pixel: u16, radius_squared: f64) -> f32 {
        // Estimate each cutoff at the center of its annulus and interpolate at the pixel.
        let position = (ring_position(radius_squared) - 0.5).clamp(0., (RINGS - 1) as f64) as f32;
        let lower = position as usize;
        let upper = (lower + 1).min(RINGS - 1);
        let cutoff = lerp(self.cutoff[lower], self.cutoff[upper], position.fract());
        (f32::from(pixel) - cutoff).max(0.)
    }
}

fn ring_position(radius_squared: f64) -> f64 {
    (radius_squared - FIRST_RADIUS_SQUARED) / RING_STEP_SQUARED
}

fn ring_cutoffs(
    samples: &mut [(usize, u16)],
    totals: &[usize; RINGS],
    fallback: u16,
) -> [f32; RINGS] {
    let mut cutoffs = [f32::from(fallback); RINGS];
    samples.sort_unstable();
    let mut previous = None;
    for ring in samples.chunk_by(|a, b| a.0 == b.0) {
        let index = ring[0].0;
        // Padding contributes no DN. Also avoid fitting isolated bright features
        // as the background of an otherwise black annulus.
        if ring.len() < MIN_BAND_SAMPLES || ring.len() * 4 < totals[index] {
            continue;
        }
        let level = |p: usize| f32::from(ring[(ring.len() - 1) * p / 100].1);
        let cutoff = level(50) + SKY_IQR_MULTIPLIER * (level(75) - level(25));
        if let Some((last_index, last_value)) = previous {
            let span = (index - last_index) as f32;
            for (offset, value) in cutoffs[last_index + 1..index].iter_mut().enumerate() {
                *value = lerp(last_value, cutoff, (offset + 1) as f32 / span);
            }
        } else {
            cutoffs[..index].fill(cutoff);
        }
        cutoffs[index] = cutoff;
        previous = Some((index, cutoff));
    }
    if let Some((last_index, last_value)) = previous {
        cutoffs[last_index + 1..].fill(last_value);
    }
    cutoffs
}

fn percentile(samples: &mut [u16], p: usize) -> u16 {
    if samples.is_empty() {
        return 0;
    }
    let index = (samples.len() - 1) * p / 100;
    *samples.select_nth_unstable(index).1
}

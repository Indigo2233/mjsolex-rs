#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BandingAxis {
    #[default]
    Off,
    /// Normalize brightness between positions along the slit.
    Slit,
    /// Normalize brightness between successive scan rows.
    Scan,
    /// Test both directions and apply each supported correction.
    Both,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanSettings {
    pub banding: BandingAxis,
    /// Width of the reference neighborhood in pixels. None selects it automatically.
    pub banding_width: Option<u8>,
    pub oscillation: bool,
    pub jagged: bool,
}

impl ScanSettings {
    pub fn enabled(self) -> bool {
        self.banding != BandingAxis::Off || self.oscillation || self.jagged
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanReport {
    pub banding: BandingAxis,
    /// Global attenuation of the signal above the sky pedestal during banding correction.
    pub banding_scale: Option<f64>,
    pub banding_width: Option<u8>,
    pub jagged: bool,
    /// Fitted period in reconstructed scan rows and amplitude in slit pixels.
    pub oscillation: Option<(f64, f64)>,
}

#[cfg(any(target_os = "android", test))]
mod model {
    use super::*;
    use crate::geometry::{DiskGeometry, FittedDisk, chord_edges, limb_levels};
    use crate::regression::weighted_fit;
    use crate::{Error, Reconstruction};

    const MAX_BAND_HALF_WINDOW: usize = 64;
    const MANUAL_MAX_HALF_WINDOW: usize = 25;

    pub(crate) struct ScanPlan {
        size: [usize; 2],
        sky: u16,
        banding: [Option<BandCorrection>; 2],
        motion: Option<Motion>,
        pub(crate) report: ScanReport,
    }

    struct BandCorrection {
        start: usize,
        gains: Vec<f64>,
        scale: f64,
    }

    impl BandCorrection {
        fn gain(&self, index: usize) -> f64 {
            self.gains
                .get(index.wrapping_sub(self.start))
                .copied()
                .unwrap_or(self.scale)
        }
    }

    struct Motion {
        wave: Option<Wave>,
        start: usize,
        rows: Vec<[f64; 2]>,
        disk: FittedDisk,
    }

    #[derive(Clone, Copy)]
    struct Wave {
        radians_per_row: f64,
        coefficient: [f64; 2],
    }

    impl Wave {
        fn shift(self, row: usize) -> f64 {
            let (s, c) = (row as f64 * self.radians_per_row).sin_cos();
            self.coefficient[0] * s + self.coefficient[1] * c
        }
    }

    impl ScanPlan {
        pub(crate) fn prepare(
            image: &Reconstruction,
            settings: ScanSettings,
            mut progress: impl FnMut(f32) -> bool,
        ) -> Result<Self, Error> {
            checkpoint(progress(0.))?;
            let disk = DiskGeometry::fit_limb(image)?;
            let sky = disk.sky;
            let half_window = settings.banding_width.map_or_else(
                || {
                    ((disk.geometry.slit_radius.min(disk.geometry.scan_radius) / 7.).round()
                        as usize)
                        .clamp(3, MAX_BAND_HALF_WINDOW)
                },
                |width| (usize::from(width) / 2).clamp(3, MANUAL_MAX_HALF_WINDOW),
            );
            let mut banding = [None, None];
            for (i, axis) in [BandingAxis::Slit, BandingAxis::Scan]
                .into_iter()
                .enumerate()
            {
                if settings.banding == axis || settings.banding == BandingAxis::Both {
                    banding[i] = banding_model(
                        image,
                        &disk,
                        axis,
                        half_window,
                        settings.banding == BandingAxis::Both,
                        |p| progress((i as f32 + p) * 0.125),
                    )?;
                }
            }
            let banding_axis = match (banding[0].is_some(), banding[1].is_some()) {
                (true, true) => BandingAxis::Both,
                (true, false) => BandingAxis::Slit,
                (false, true) => BandingAxis::Scan,
                (false, false) => BandingAxis::Off,
            };
            let motion = if settings.oscillation || settings.jagged {
                motion_model(image, disk, settings, |p| progress(0.25 + 0.75 * p))?
            } else {
                None
            };
            checkpoint(progress(1.))?;
            let report = ScanReport {
                banding: banding_axis,
                banding_scale: (banding_axis != BandingAxis::Off)
                    .then(|| banding.iter().flatten().map(|model| model.scale).product()),
                banding_width: (banding_axis != BandingAxis::Off)
                    .then_some((2 * half_window + 1) as u8),
                jagged: motion.as_ref().is_some_and(|m| !m.rows.is_empty()),
                oscillation: motion.as_ref().and_then(|m| m.wave).map(|wave| {
                    (
                        std::f64::consts::TAU / wave.radians_per_row,
                        wave.coefficient[0].hypot(wave.coefficient[1]),
                    )
                }),
            };
            Ok(Self {
                size: [image.width, image.height],
                sky,
                banding,
                motion,
                report,
            })
        }

        pub(crate) fn apply(
            &self,
            image: &mut Reconstruction,
            mut progress: impl FnMut(f32) -> bool,
        ) -> Result<(), Error> {
            if self.size != [image.width, image.height] {
                return Err(Error::InvalidFraming);
            }
            let mut source = if self.motion.is_some() {
                vec![0; image.width]
            } else {
                Vec::new()
            };
            for (y, row) in image.pixels.chunks_exact_mut(image.width).enumerate() {
                checkpoint(progress(y as f32 / image.height as f32))?;
                let row_gain = self.banding[1].as_ref().map_or(1., |model| model.gain(y));
                if let Some(model) = &self.banding[0] {
                    for (x, sample) in row.iter_mut().enumerate() {
                        apply_gain(sample, self.sky, row_gain * model.gain(x));
                    }
                } else if row_gain != 1. {
                    for sample in &mut *row {
                        apply_gain(sample, self.sky, row_gain);
                    }
                }
                if let Some(motion) = &self.motion {
                    let periodic = motion.wave.map_or(0., |wave| wave.shift(y));
                    let [left, right] = motion
                        .rows
                        .get(y.wrapping_sub(motion.start))
                        .copied()
                        .unwrap_or([0.; 2]);
                    let shift = periodic + (left + right) / 2.;
                    let half_width_change = (right - left) / 2.;
                    if shift.abs() + half_width_change.abs() < 0.01 {
                        continue;
                    }
                    source.copy_from_slice(row);
                    let (center, half) = motion.disk.chord(y as f64);
                    let slope = half_width_change / half.max(1.);
                    for (x, target) in row.iter_mut().enumerate() {
                        // Outside the measured chord, carry the nearer limb's displacement.
                        let x = x as f64;
                        let stretch = slope * (x - center).clamp(-half, half);
                        *target = sample_line(&source, x + shift + stretch);
                    }
                }
            }
            checkpoint(progress(1.))
        }
    }

    fn checkpoint(active: bool) -> Result<(), Error> {
        if active {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    impl FittedDisk {
        fn chord(&self, row: f64) -> (f64, f64) {
            let dy = row - self.center_y;
            let radius = self.geometry.scan_radius;
            (
                self.center_x + self.geometry.shear * dy,
                self.geometry.slit_radius * (1. - (dy / radius).powi(2)).max(0.).sqrt(),
            )
        }
    }

    fn apply_gain(value: &mut u16, sky: u16, gain: f64) {
        if *value > sky && gain != 1. {
            *value = (f64::from(sky) + f64::from(*value - sky) * gain).round() as u16;
        }
    }

    fn sample_line(source: &[u16], x: f64) -> u16 {
        if !(0. ..=(source.len() - 1) as f64).contains(&x) {
            return 0;
        }
        let left = x as usize;
        let a = f64::from(source[left]);
        let b = f64::from(source[(left + 1).min(source.len() - 1)]);
        (a + x.fract() * (b - a)).round() as u16
    }

    fn disk_range(center: f64, radius: f64, limit: usize) -> std::ops::Range<usize> {
        let start = (center - radius).ceil().max(0.) as usize;
        let end = ((center + radius).floor() as usize + 1).min(limit);
        start.min(end)..end
    }

    // A band level is measured in three sectors of each line. Requiring a
    // coherent variation rejects local active regions and clipped limb samples.
    fn banding_model(
        image: &Reconstruction,
        disk: &FittedDisk,
        axis: BandingAxis,
        half_window: usize,
        automatic: bool,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Option<BandCorrection>, Error> {
        let vertical = axis == BandingAxis::Slit;
        let g = disk.geometry;
        let tilted = g.shear * g.scan_radius;
        let projected_x = g.slit_radius.hypot(tilted);
        let (center, radius, cross_center, cross_radius, shear, size, cross_size) = if vertical {
            (
                disk.center_x,
                projected_x,
                disk.center_y,
                g.scan_radius * g.slit_radius / projected_x,
                tilted * g.scan_radius / projected_x,
                image.width,
                image.height,
            )
        } else {
            (
                disk.center_y,
                g.scan_radius,
                disk.center_x,
                g.slit_radius,
                tilted,
                image.height,
                image.width,
            )
        };
        let range = disk_range(center, 0.91 * radius, size);
        let edge_window = half_window.min(MANUAL_MAX_HALF_WINDOW);
        let signal_at = |index: usize, across: usize| {
            let pixel = if vertical {
                image.pixels[across * image.width + index]
            } else {
                image.pixels[index * image.width + across]
            };
            let signal = pixel.saturating_sub(disk.sky);
            (signal > 4 && pixel < 65_280).then_some(u64::from(signal))
        };
        let mut measurements = vec![None; range.len()];
        for (i, measurement) in measurements
            .iter_mut()
            .enumerate()
            .take(range.len().saturating_sub(edge_window))
            .skip(edge_window)
        {
            checkpoint(progress(0.85 * i as f32 / range.len() as f32))?;
            let index = range.start + i;
            let available = i.min(range.len() - 1 - i);
            // Broader references reach outside a wide band on a large disk.
            // Narrow the window near the perimeter to keep sampling there.
            let reference_window = half_window.min((available / 2).max(edge_window));
            let t = (index as f64 - center) / radius;
            let mid = cross_center + shear * t;
            let half = cross_radius * (0.96_f64.powi(2) - t * t).sqrt();
            if half < 18. {
                continue;
            }
            let mut residual = [None; 3];
            for (sector, delta) in residual.iter_mut().enumerate() {
                let left = mid + half * (-0.75 + 0.5 * sector as f64);
                let right = left + 0.5 * half;
                let bounds = |value: f64| (value.round().max(0.) as usize).min(cross_size);
                let span = bounds(left)..bounds(right);
                let stride = span.len().div_ceil(96).max(1);
                let mut pairs = [[0_u64; 3]; MAX_BAND_HALF_WINDOW];
                // Match the sample positions across lines so changing the sample grid
                // cannot turn solar texture into a band correction.
                for across in span.step_by(stride) {
                    let Some(current) = signal_at(index, across) else {
                        continue;
                    };
                    for (step, pair) in pairs[..reference_window].iter_mut().enumerate() {
                        let distance = step + 1;
                        if let (Some(a), Some(b)) = (
                            signal_at(index - distance, across),
                            signal_at(index + distance, across),
                        ) {
                            pair[0] += 2 * current;
                            pair[1] += a + b;
                            pair[2] += 1;
                        }
                    }
                }
                // Use the median of symmetric reference pairs. A neighboring
                // stripe then has limited influence on the gain of a clean line.
                let mut ratios = [0.; MAX_BAND_HALF_WINDOW];
                let mut count = 0;
                for &[current, reference, probes] in &pairs[..reference_window] {
                    if probes >= 8 {
                        ratios[count] = reference as f64 / current as f64;
                        count += 1;
                    }
                }
                if count >= reference_window.div_ceil(2) {
                    let median = *ratios[..count]
                        .select_nth_unstable_by(count / 2, f64::total_cmp)
                        .1;
                    *delta = Some(median.ln());
                }
            }
            if let [Some(a), Some(b), Some(c)] = residual {
                *measurement = Some([a, b, c]);
            }
        }

        let mut gains = vec![1.; range.len()];
        let mut applied = false;
        let mut detected = false;
        let mut energy = [0_f64; 3];
        let mut covariance = [0_f64; 3];
        for (i, &measurement) in measurements.iter().enumerate() {
            checkpoint(progress(0.85 + 0.15 * i as f32 / range.len() as f32))?;
            let Some(measured) = measurement else {
                continue;
            };
            let at_edge = i.min(range.len() - 1 - i).saturating_sub(edge_window);
            if automatic && at_edge > 0 {
                let detrend = (2 * half_window).min(at_edge);
                let baseline = &measurements[i - detrend..=i + detrend];
                // Remove smooth curvature in the evidence used to select an axis.
                let residual = std::array::from_fn(|sector| {
                    measured[sector] - sector_baseline(baseline, sector)
                });
                detected |= shared_band(residual).is_some();
                for (sector, &value) in residual.iter().enumerate() {
                    let current = value.clamp(-0.14, 0.14);
                    let next = residual[(sector + 1) % 3].clamp(-0.14, 0.14);
                    energy[sector] += current * current;
                    covariance[sector] += current * next;
                }
            }
            if let Some(median) = shared_band(measured) {
                let taper = (at_edge as f64 / edge_window as f64).min(1.);
                gains[i] = (taper * median.clamp(-0.14, 0.14)).exp();
                applied |= gains[i] != 1.;
            }
        }
        // Automatic mode requires shared signal across all sectors. This avoids
        // selecting an axis for isolated coincident changes in solar detail.
        let coherent = covariance
            .iter()
            .enumerate()
            .all(|(i, &sum)| sum >= 0.28 * (energy[i] * energy[(i + 1) % 3]).sqrt());
        if !applied || (automatic && !(detected && coherent)) {
            return Ok(None);
        }
        // Keep the correction proportional above the sky level everywhere,
        // including the unmeasured rows or columns and all spectral offsets.
        let maximum = gains.iter().copied().fold(1., f64::max);
        for gain in &mut gains {
            *gain /= maximum;
        }
        Ok(Some(BandCorrection {
            start: range.start,
            gains,
            scale: maximum.recip(),
        }))
    }

    fn sector_baseline(neighbors: &[Option<[f64; 3]>], sector: usize) -> f64 {
        let mut nearby = [0.; 4 * MAX_BAND_HALF_WINDOW + 1];
        let mut count = 0;
        for other in neighbors.iter().flatten() {
            nearby[count] = other[sector];
            count += 1;
        }
        // The center is present. Both quartiles preserve alternating bands and
        // limit the influence of isolated stripes on the baseline.
        let quartile = (count - 1) / 4;
        let upper = count - 1 - quartile;
        let (_, lower, above) = nearby[..count].select_nth_unstable_by(quartile, f64::total_cmp);
        let higher = upper.checked_sub(quartile + 1).map_or(*lower, |position| {
            *above.select_nth_unstable_by(position, f64::total_cmp).1
        });
        (*lower + higher) / 2.
    }

    fn shared_band(mut sectors: [f64; 3]) -> Option<f64> {
        sectors.sort_by(f64::total_cmp);
        let [low, median, high] = sectors;
        (median.abs() >= 0.002 && low * high > 0. && high - low <= median.abs() * 1.3 + 0.002)
            .then_some(median)
    }

    fn motion_model(
        image: &Reconstruction,
        disk: FittedDisk,
        settings: ScanSettings,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Option<Motion>, Error> {
        let (sky, threshold) = limb_levels(&image.pixels).ok_or(Error::NoSolarLimb)?;
        let scan_extent = disk.geometry.scan_radius * if settings.jagged { 0.985 } else { 0.90 };
        let range = disk_range(disk.center_y, scan_extent, image.height);
        let limit =
            (0.10 * disk.geometry.slit_radius).clamp(3., if settings.jagged { 128. } else { 32. });
        let sky = f64::from(sky);
        let contrast = (threshold - sky) / 0.20;
        let thresholds = [0.16, 0.23, 0.30, 0.37, 0.44, 0.50].map(|level| sky + level * contrast);
        let mut edges = Vec::with_capacity(range.len());
        for y in range.clone() {
            checkpoint(progress(
                0.45 * (y - range.start) as f32 / range.len() as f32,
            ))?;
            let row = &image.pixels[y * image.width..(y + 1) * image.width];
            let (mid, half) = disk.chord(y as f64);
            let measured = if settings.jagged && half >= 12. {
                // A range of brightness levels follows the limb transition and
                // reduces shifts caused by a single noisy intensity crossing.
                thresholds
                    .iter()
                    .try_fold((0., 0.), |(left, right), &level| {
                        let (a, b) = chord_edges(row, level)?;
                        Some((
                            left + a / thresholds.len() as f64,
                            right + b / thresholds.len() as f64,
                        ))
                    })
            } else {
                chord_edges(row, threshold)
            };
            let offsets = measured.and_then(|(left, right)| {
                let offsets = [left - (mid - half), right - (mid + half)];
                (half >= 12. && offsets.iter().all(|v| v.abs() <= limit)).then_some(offsets)
            });
            edges.push(offsets);
        }
        let wave = if settings.oscillation {
            fit_wave(&edges, range.start, |p| progress(0.45 + 0.4 * p))?
        } else {
            None
        };
        if let Some(wave) = wave {
            for (i, pair) in edges.iter_mut().enumerate() {
                if let Some(pair) = pair {
                    let shift = wave.shift(range.start + i);
                    *pair = pair.map(|value| value - shift);
                }
            }
        }
        let mut rows = Vec::new();
        if settings.jagged {
            const MIN_SHIFT: f64 = 0.45;
            let neighbor_radius =
                (disk.geometry.scan_radius / 24.).round().clamp(24., 72.) as usize;
            let neighbor_scale = (neighbor_radius + 1) as f64;
            let max_shift = (0.008 * disk.geometry.slit_radius).clamp(2., 12.);
            rows.resize(edges.len(), [0.; 2]);
            let mut candidates = 0;
            let mut coherent = 0;
            for (i, &edge) in edges.iter().enumerate() {
                checkpoint(progress(0.85 + 0.15 * i as f32 / edges.len() as f32))?;
                let Some(edge) = edge else { continue };
                let start = i.saturating_sub(neighbor_radius);
                let neighbors = &edges[start..(i + neighbor_radius + 1).min(edges.len())];
                if neighbors.iter().flatten().count() < 7 {
                    continue;
                }
                // Smooth deviations from the ellipse so its sharp curvature near
                // the poles does not get mistaken for scan motion.
                let offsets = std::array::from_fn(|side| {
                    let samples = neighbors.iter().enumerate().filter_map(|(j, pair)| {
                        let x = (start as f64 + j as f64 - i as f64) / neighbor_scale;
                        Some(([1., x], (*pair)?[side], (1. - x * x).powi(2)))
                    });
                    weighted_fit(samples).map_or(0., |[baseline, _]| edge[side] - baseline)
                });
                let [left, right] = offsets.map(f64::abs);
                let peak = left.max(right);
                let shared = left.min(right);
                if peak >= MIN_SHIFT {
                    candidates += 1;
                }
                if shared >= MIN_SHIFT && peak <= 0.65 + 1.8 * shared {
                    coherent += 1;
                }
                // Unequal limb shifts can change the chord width. Suppress large
                // changes that have little support on the opposite limb.
                let (_, half) = disk.chord((range.start + i) as f64);
                let supported = (1.5 + 2. * shared).min(max_shift).min(0.25 * half);
                let strength = (supported / peak.max(supported)).powi(2);
                let distance = (range.start as f64 + i as f64 - disk.center_y).abs();
                let taper = ((scan_extent - distance) / 16.).clamp(0., 1.);
                rows[i] = offsets.map(|offset| offset * strength * taper);
            }
            // Require repeatable variation on both limbs before changing the image.
            if coherent < 4 || 8 * coherent < candidates {
                rows.clear();
            }
        }
        Ok((wave.is_some() || !rows.is_empty()).then_some(Motion {
            wave,
            start: range.start,
            rows,
            disk,
        }))
    }

    fn fit_wave(
        edges: &[Option<[f64; 2]>],
        start: usize,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Option<Wave>, Error> {
        const MAX_SAMPLES: usize = 1800;
        let step = edges.len().div_ceil(MAX_SAMPLES).max(1);
        let mut samples: Vec<_> = edges
            .iter()
            .enumerate()
            .step_by(step)
            .filter_map(|(i, pair)| {
                let [left, right] = (*pair)?;
                ((left - right).abs() <= 4.).then_some([(start + i) as f64, (left + right) / 2.])
            })
            .collect();
        if samples.len() < 64 {
            return Ok(None);
        }
        let span = samples[samples.len() - 1][0] - samples[0][0];
        let Some([intercept, slope]) =
            weighted_fit(samples.iter().map(|&[y, v]| ([1., y / span], v, 1.)))
        else {
            return Ok(None);
        };
        let mut baseline = 0.;
        for [y, value] in &mut samples {
            *value -= intercept + slope * *y / span;
            baseline += *value * *value;
        }
        if baseline < 0.03 * samples.len() as f64 {
            return Ok(None);
        }
        let mut best = None;
        let mut best_rss = baseline;
        let bins = (span / (4. * step as f64)) as usize;
        // Two samples per frequency bin and at least three observed cycles.
        for bin in 6..=bins {
            checkpoint(progress(bin as f32 / bins as f32))?;
            let frequency = std::f64::consts::PI * bin as f64 / span;
            let basis = |row: f64| {
                let (s, c) = (row * frequency).sin_cos();
                [s, c]
            };
            let Some(coefficient) =
                weighted_fit(samples.iter().map(|&[row, value]| (basis(row), value, 1.)))
            else {
                continue;
            };
            let rss = samples
                .iter()
                .map(|&[row, value]| {
                    let [s, c] = basis(row);
                    (value - s * coefficient[0] - c * coefficient[1]).powi(2)
                })
                .sum::<f64>();
            if rss < best_rss {
                best_rss = rss;
                best = Some(Wave {
                    radians_per_row: frequency,
                    coefficient,
                });
            }
        }
        Ok(best.filter(|wave| {
            best_rss <= 0.64 * baseline && wave.coefficient[0].hypot(wave.coefficient[1]) >= 0.25
        }))
    }
}

#[cfg(any(target_os = "android", test))]
pub(crate) use model::ScanPlan;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Reconstruction;
    use crate::geometry::{chord_edges, limb_levels};

    fn disk(
        shift: impl Fn(usize) -> f64,
        response: impl Fn(usize, usize) -> f64,
    ) -> Reconstruction {
        distorted_disk(|y| [shift(y), 0.], response)
    }

    fn distorted_disk(
        distortion: impl Fn(usize) -> [f64; 2],
        response: impl Fn(usize, usize) -> f64,
    ) -> Reconstruction {
        let (width, height) = (256, 240);
        let pixels = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                let dy = (y as f64 - 119.5) / 86.;
                let [shift, expansion] = distortion(y);
                let half = 88. * (1. - dy * dy).max(0.).sqrt() + expansion;
                let lx = x as f64 - 127.5 - shift;
                let coverage = if dy.abs() < 1. {
                    (half - lx.abs() + 0.5).clamp(0., 1.)
                } else {
                    0.
                };
                let mu = (1. - (lx / 88.).powi(2) - dy.powi(2)).max(0.).sqrt();
                (300. + coverage * (24_000. + 7_000. * mu) * response(x, y)).round() as u16
            })
            .collect();
        Reconstruction {
            width,
            height,
            pixels,
            synthetic_flat: false,
        }
    }

    fn edge_error(image: &Reconstruction) -> f64 {
        let (_, threshold) = limb_levels(&image.pixels).unwrap();
        let mean_square = image
            .pixels
            .chunks_exact(image.width)
            .skip(57)
            .take(126)
            .map(|row| {
                let (left, right) = chord_edges(row, threshold).unwrap();
                ((left + right) / 2. - 127.5).powi(2)
            })
            .sum::<f64>()
            / 126.;
        mean_square.sqrt()
    }

    #[test]
    fn corrects_coherent_periodic_and_irregular_scan_displacements() {
        let mut image = disk(
            |y| {
                2.5 * (std::f64::consts::TAU * y as f64 / 21.).sin()
                    + [0., 1.35, -0.42, -1.0, 0.57][(y * (y + 1) / 2) % 5]
            },
            |_, _| 1.,
        );
        let before = edge_error(&image);
        let settings = ScanSettings {
            oscillation: true,
            jagged: true,
            ..ScanSettings::default()
        };
        let plan = ScanPlan::prepare(&image, settings, |_| true).unwrap();
        let (period, amplitude) = plan.report.oscillation.unwrap();
        assert!((period - 21.).abs() < 0.8);
        assert!((amplitude - 2.5).abs() < 0.6);
        assert!(plan.report.jagged);
        plan.apply(&mut image, |_| true).unwrap();
        let after = edge_error(&image);
        assert!(after < 0.45 * before, "edge RMS {before} -> {after}");
    }

    #[test]
    fn reduces_longer_nonrepeating_limb_waves() {
        let mut image = disk(
            |y| {
                let x = y as f64 - 120.;
                3.5 * (x * (0.129 + 0.00036 * x)).sin()
            },
            |_, _| 1.,
        );
        let before = edge_error(&image);
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                jagged: true,
                ..Default::default()
            },
            |_| true,
        )
        .unwrap();
        plan.apply(&mut image, |_| true).unwrap();
        let after = edge_error(&image);
        assert!(after < 0.65 * before, "edge RMS {before} -> {after}");
    }

    #[test]
    fn one_sided_prominence_does_not_move_scan_rows() {
        let mut image = disk(|_| 0., |_, _| 1.);
        for y in 113..126 {
            let end = 224 + (y % 3) * 3;
            let row = &mut image.pixels[y * image.width..(y + 1) * image.width];
            for value in &mut row[212..end] {
                *value = (*value).max(10_000);
            }
        }
        let original = image.pixels.clone();
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                jagged: true,
                ..ScanSettings::default()
            },
            |_| true,
        )
        .unwrap();
        assert!(!plan.report.jagged);
        plan.apply(&mut image, |_| true).unwrap();
        assert_eq!(image.pixels, original);
    }

    #[test]
    fn corrects_chord_width_changes_without_shifting_the_scan_center() {
        let clean = disk(|_| 0., |_, _| 1.);
        let mut image = distorted_disk(
            |y| [0., [0., 2.1, -1.8, 0., 1.3, -2.5, 0.][y % 7]],
            |_, _| 1.,
        );
        let width_error = |image: &Reconstruction| {
            let (_, threshold) = limb_levels(&image.pixels).unwrap();
            image
                .pixels
                .chunks_exact(image.width)
                .zip(clean.pixels.chunks_exact(clean.width))
                .skip(57)
                .take(126)
                .map(|(row, reference)| {
                    let (left, right) = chord_edges(row, threshold).unwrap();
                    let (ref_left, ref_right) = chord_edges(reference, threshold).unwrap();
                    ((right - left) - (ref_right - ref_left)).abs()
                })
                .sum::<f64>()
                / 126.
        };
        let before = width_error(&image);
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                jagged: true,
                ..Default::default()
            },
            |_| true,
        )
        .unwrap();
        assert!(plan.report.jagged);
        plan.apply(&mut image, |_| true).unwrap();
        let after = width_error(&image);
        assert!(
            after < 0.55 * before,
            "mean chord width error {before} -> {after}"
        );
        assert!(edge_error(&image) < 0.1);
    }

    fn banding_error(clean: &Reconstruction, image: &Reconstruction) -> f64 {
        let ratios: Vec<_> = (80..160)
            .flat_map(|y| (88..168).map(move |x| y * image.width + x))
            .map(|i| f64::from(image.pixels[i] - 300) / f64::from(clean.pixels[i] - 300))
            .collect();
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        ratios
            .into_iter()
            .map(|ratio| (ratio / mean - 1.).powi(2))
            .sum::<f64>()
            .sqrt()
    }

    #[test]
    fn removes_weak_bands_consistent_across_the_disk() {
        for axis in [BandingAxis::Slit, BandingAxis::Scan] {
            let clean = disk(|_| 0., |_, _| 1.);
            let mut striped = disk(
                |_| 0.,
                |x, y| {
                    let index = if axis == BandingAxis::Slit { x } else { y };
                    [0.994, 0.994, 1.006, 1.006][index % 4]
                },
            );
            let before = banding_error(&clean, &striped);
            let plan = ScanPlan::prepare(
                &striped,
                ScanSettings {
                    banding: axis,
                    banding_width: Some(23),
                    ..ScanSettings::default()
                },
                |_| true,
            )
            .unwrap();
            plan.apply(&mut striped, |_| true).unwrap();
            assert!(
                banding_error(&clean, &striped) < 0.4 * before,
                "{axis:?}: {before} -> {}",
                banding_error(&clean, &striped)
            );
        }
    }

    #[test]
    fn avoids_dark_and_bright_halos_next_to_slit_bands() {
        let clean = disk(|_| 0., |_, _| 1.);
        let mut image = disk(
            |_| 0.,
            |x, _| match x {
                104..=110 => 0.91,
                140..=146 => 1.08,
                _ => 1.,
            },
        );
        let before = banding_error(&clean, &image);
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                banding: BandingAxis::Slit,
                banding_width: Some(49),
                ..Default::default()
            },
            |_| true,
        )
        .unwrap();
        plan.apply(&mut image, |_| true).unwrap();
        let after = banding_error(&clean, &image);
        assert!(after < 0.3 * before, "banding error {before} -> {after}");
    }

    #[test]
    fn auto_corrects_crossed_bands_and_reports_the_supported_directions() {
        let clean = disk(|_| 0., |_, _| 1.);
        let slit = |x| [0.92, 1.06, 1.08, 0.98][x % 4];
        let scan = |y| [1.07, 0.95, 0.94, 1.04][y % 4];
        for (expected, mut image) in [
            (BandingAxis::Slit, disk(|_| 0., |x, _| slit(x))),
            (BandingAxis::Both, disk(|_| 0., |x, y| slit(x) * scan(y))),
        ] {
            let before = banding_error(&clean, &image);
            let plan = ScanPlan::prepare(
                &image,
                ScanSettings {
                    banding: BandingAxis::Both,
                    ..Default::default()
                },
                |_| true,
            )
            .unwrap();
            assert_eq!(plan.report.banding, expected);
            plan.apply(&mut image, |_| true).unwrap();
            let after = banding_error(&clean, &image);
            assert!(
                after < 0.35 * before,
                "{expected:?} banding error {before} -> {after}"
            );
            assert_eq!(image.pixels[0], 300);
        }
    }

    #[test]
    fn auto_reference_reaches_past_a_broad_slit_band() {
        let size = 1024;
        let sky = 300.;
        let center = (size - 1) as f64 / 2.;
        let pixels: Vec<_> = (0..size * size)
            .map(|i| {
                let x = (i % size) as f64 - center;
                let y = (i / size) as f64 - center;
                let inside = 1. - (x * x + y * y) / 448_f64.powi(2);
                (sky + if inside > 0. {
                    20_000. + 4000. * inside.sqrt()
                } else {
                    0.
                })
                .round() as u16
            })
            .collect();
        let mut image = Reconstruction {
            width: size,
            height: size,
            pixels: pixels.clone(),
            synthetic_flat: false,
        };
        for row in image.pixels.chunks_exact_mut(size) {
            for (start, strength) in [(305, 1.075), (695, 0.92)] {
                for value in &mut row[start..start + 39] {
                    *value = (sky + (f64::from(*value) - sky) * strength).round() as u16;
                }
            }
        }
        let contrast = |image: &Reconstruction, x| {
            let base_ratio = |x| {
                let i = 511 * size + x;
                (f64::from(image.pixels[i]) - sky) / (f64::from(pixels[i]) - sky)
            };
            (base_ratio(x) / base_ratio(size / 2) - 1.).abs()
        };
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                banding: BandingAxis::Both,
                ..Default::default()
            },
            |_| true,
        )
        .unwrap();
        assert_eq!(plan.report.banding, BandingAxis::Slit);
        let before = [contrast(&image, 324), contrast(&image, 714)];
        plan.apply(&mut image, |_| true).unwrap();
        let after = [contrast(&image, 324), contrast(&image, 714)];
        assert!(
            after[0] < before[0] * 0.4 && after[1] < before[1] * 0.4,
            "broad stripe contrast {before:?} -> {after:?}"
        );
    }

    #[test]
    fn reduces_banding_along_either_axis_and_keeps_the_sky_level() {
        let clean = disk(|_| 0., |_, _| 1.);
        for axis in [BandingAxis::Scan, BandingAxis::Slit] {
            let mut image = disk(
                |_| 0.,
                |x, y| {
                    let q = if axis == BandingAxis::Slit { x } else { y };
                    [0.91, 1.04, 1.09, 0.96][q % 4]
                },
            );
            let before = banding_error(&clean, &image);
            let plan = ScanPlan::prepare(
                &image,
                ScanSettings {
                    banding: axis,
                    banding_width: Some(17),
                    ..ScanSettings::default()
                },
                |_| true,
            )
            .unwrap();
            assert_eq!(plan.report.banding, axis);
            assert_eq!(plan.report.banding_width, Some(17));
            assert!(plan.report.banding_scale.unwrap() < 1.);
            plan.apply(&mut image, |_| true).unwrap();
            assert!(banding_error(&clean, &image) < before * 0.24);
            assert_eq!(image.pixels[0], 300);
        }
        let plan = ScanPlan::prepare(
            &clean,
            ScanSettings {
                oscillation: true,
                ..ScanSettings::default()
            },
            |_| true,
        )
        .unwrap();
        assert!(plan.report.oscillation.is_none());
    }

    #[test]
    fn banding_reserves_headroom_for_bright_features() {
        let mut image = disk(|_| 0., |_, y| [0.91, 1.04, 1.09, 0.96][y % 4]);
        let plan = ScanPlan::prepare(
            &image,
            ScanSettings {
                banding: BandingAxis::Scan,
                ..ScanSettings::default()
            },
            |_| true,
        )
        .unwrap();
        let pixel = 120 * image.width + 120;
        image.pixels[pixel..pixel + 2].copy_from_slice(&[65_000, 65_400]);
        plan.apply(&mut image, |_| true).unwrap();
        assert!(image.pixels[pixel] < image.pixels[pixel + 1]);
        assert!(image.pixels[pixel + 1] <= 65_400);
    }
}

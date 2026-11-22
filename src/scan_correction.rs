#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BandingAxis {
    #[default]
    Off,
    /// Normalize brightness between positions along the slit.
    Slit,
    /// Normalize brightness between successive scan rows.
    Scan,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanSettings {
    pub banding: BandingAxis,
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

    pub(crate) struct ScanPlan {
        size: [usize; 2],
        sky: u16,
        banding: Option<BandCorrection>,
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
            let banding = if settings.banding != BandingAxis::Off {
                banding_model(image, &disk, settings.banding, |p| progress(p * 0.25))?
            } else {
                None
            };
            let motion = if settings.oscillation || settings.jagged {
                motion_model(image, disk, settings, |p| progress(0.25 + 0.75 * p))?
            } else {
                None
            };
            checkpoint(progress(1.))?;
            let report = ScanReport {
                banding: if banding.is_some() {
                    settings.banding
                } else {
                    BandingAxis::Off
                },
                banding_scale: banding.as_ref().map(|model| model.scale),
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
                if let Some(model) = &self.banding {
                    if self.report.banding == BandingAxis::Slit {
                        for (x, sample) in row.iter_mut().enumerate() {
                            apply_gain(sample, self.sky, model.gain(x));
                        }
                    } else {
                        let gain = model.gain(y);
                        for sample in &mut *row {
                            apply_gain(sample, self.sky, gain);
                        }
                    }
                }
                if let Some(motion) = &self.motion {
                    let periodic = motion.wave.map_or(0., |wave| wave.shift(y));
                    let [left, right] = motion
                        .rows
                        .get(y.wrapping_sub(motion.start))
                        .copied()
                        .unwrap_or([0., 0.]);
                    if periodic.abs().max(left.abs()).max(right.abs()) < 0.01 {
                        continue;
                    }
                    let (mid, half) = motion.disk.chord(y as f64);
                    source.copy_from_slice(row);
                    for (x, target) in row.iter_mut().enumerate() {
                        let t = ((x as f64 - (mid - half)) / (2. * half).max(1.)).clamp(0., 1.);
                        *target =
                            sample_line(&source, x as f64 + periodic + left + t * (right - left));
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
        let range = disk_range(center, 0.85 * radius, size);
        let mut levels = Vec::with_capacity(range.len());
        for index in range.clone() {
            checkpoint(progress(
                (index - range.start) as f32 / range.len() as f32 * 0.6,
            ))?;
            let t = (index as f64 - center) / radius;
            let mid = cross_center + shear * t;
            let half = cross_radius * (0.92_f64.powi(2) - t * t).sqrt();
            let mut sectors = [f64::NAN; 3];
            if half >= 18. {
                for (sector, level) in sectors.iter_mut().enumerate() {
                    let mut sum = 0.;
                    let mut count = 0.;
                    for probe in 0..12 {
                        let v = -0.75 + (sector * 12 + probe) as f64 / 36. * 1.5;
                        let p = (mid + half * v).round();
                        if !(0. ..cross_size as f64).contains(&p) {
                            continue;
                        }
                        let p = p as usize;
                        let sample = if vertical {
                            image.pixels[p * image.width + index]
                        } else {
                            image.pixels[index * image.width + p]
                        };
                        if sample.saturating_sub(disk.sky) > 4 && sample < 65_280 {
                            sum += f64::from(sample - disk.sky);
                            count += 1.;
                        }
                    }
                    if count >= 8. {
                        *level = (sum / count).ln();
                    }
                }
            }
            levels.push(sectors);
        }
        let half_window = (radius / 12.).round().clamp(3., 12.) as usize;
        let mut gains = vec![1.; levels.len()];
        let mut applied = false;
        for (i, gain) in gains
            .iter_mut()
            .enumerate()
            .take(levels.len().saturating_sub(half_window))
            .skip(half_window)
        {
            checkpoint(progress(0.6 + 0.4 * i as f32 / levels.len() as f32))?;
            let mut residual = [0.; 3];
            let mut valid = true;
            for (sector, delta) in residual.iter_mut().enumerate() {
                let local = |j: usize| levels[j][sector];
                let mut sum = 0.;
                let mut count = 0.;
                for step in 1..=half_window {
                    let a = local(i - step);
                    let b = local(i + step);
                    if a.is_finite() && b.is_finite() {
                        sum += a + b;
                        count += 2.;
                    }
                }
                if !local(i).is_finite() || count < half_window as f64 {
                    valid = false;
                    break;
                }
                *delta = sum / count - local(i);
            }
            if !valid {
                continue;
            }
            residual.sort_by(f64::total_cmp);
            let median = residual[1];
            if median.abs() >= 0.008 && residual[2] - residual[0] <= median.abs().max(0.025) {
                let taper = (i.min(levels.len() - 1 - i).saturating_sub(half_window) as f64
                    / half_window as f64)
                    .min(1.);
                *gain = (taper * median.clamp(-0.14, 0.14)).exp();
                applied |= *gain != 1.;
            }
        }
        if !applied {
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

    fn motion_model(
        image: &Reconstruction,
        disk: FittedDisk,
        settings: ScanSettings,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Option<Motion>, Error> {
        let (_, threshold) = limb_levels(&image.pixels).ok_or(Error::NoSolarLimb)?;
        let range = disk_range(
            disk.center_y,
            disk.geometry.scan_radius * 0.90,
            image.height,
        );
        let limit = (0.10 * disk.geometry.slit_radius).clamp(3., 32.);
        let mut edges = Vec::with_capacity(range.len());
        for y in range.clone() {
            checkpoint(progress(
                0.45 * (y - range.start) as f32 / range.len() as f32,
            ))?;
            let row = &image.pixels[y * image.width..(y + 1) * image.width];
            let (mid, half) = disk.chord(y as f64);
            let offsets = chord_edges(row, threshold).and_then(|(left, right)| {
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
            rows.resize(edges.len(), [0., 0.]);
            let mut applied = false;
            for (i, &edge) in edges.iter().enumerate() {
                checkpoint(progress(0.85 + 0.15 * i as f32 / edges.len() as f32))?;
                let Some(edge) = edge else { continue };
                let smooth = &edges[i.saturating_sub(8)..(i + 9).min(edges.len())];
                let (sum, count) = smooth
                    .iter()
                    .flatten()
                    .fold(([0.; 2], 0), |(sum, count), pair| {
                        ([sum[0] + pair[0], sum[1] + pair[1]], count + 1)
                    });
                if count < 7 {
                    continue;
                }
                let jitter = std::array::from_fn(|side| edge[side] - sum[side] / f64::from(count));
                if jitter[0].abs().max(jitter[1].abs()) >= 0.45 {
                    rows[i] = jitter.map(|v| v.clamp(-limit / 2., limit / 2.));
                    applied = true;
                }
            }
            if !applied {
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
        let (width, height) = (256, 240);
        let pixels = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                let dy = (y as f64 - 119.5) / 86.;
                let half = 88. * (1. - dy * dy).max(0.).sqrt();
                let lx = x as f64 - 127.5 - shift(y);
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
            let error = |image: &Reconstruction| {
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
            };
            let before = error(&image);
            let plan = ScanPlan::prepare(
                &image,
                ScanSettings {
                    banding: axis,
                    ..ScanSettings::default()
                },
                |_| true,
            )
            .unwrap();
            assert_eq!(plan.report.banding, axis);
            assert!(plan.report.banding_scale.unwrap() < 1.);
            plan.apply(&mut image, |_| true).unwrap();
            assert!(error(&image) < before * 0.24);
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

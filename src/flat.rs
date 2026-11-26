use crate::geometry::FittedDisk;
use crate::regression::weighted_fit;
use crate::{ByteOrder, DiskGeometry, Error, Observation, Reconstruction, SerReader, Spectrum};
use std::io::{Read, Seek};

const FIT_RADIUS: f64 = 0.85;
const GRID_SIDE: usize = 160;
const MIN_GAIN: f64 = 0.8;
const MIN_RESPONSE_CHANGE: f64 = 0.04;
// Exclude near-white samples, including an 8-bit SER expanded into 16 bits.
const SATURATION_CUTOFF: u16 = 65_280;

/// Per-column slit throughput correction estimated from a captured flat SER.
pub struct PhysicalFlat {
    gains: Vec<f32>,
}

impl PhysicalFlat {
    pub fn from_recording<R: Read + Seek>(
        input: R,
        scan: &Spectrum,
        observation: &Observation,
        order: Option<ByteOrder>,
        progress: impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        let mut reader =
            SerReader::open_with_byte_order_hint(input, order, Some((scan.slit().0, observation)))?;
        Self::from_ser(&mut reader, scan, progress)
    }

    pub fn from_ser<R: Read + Seek>(
        reader: &mut SerReader<R>,
        scan: &Spectrum,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        let (scan, (left, right)) = scan.slit();
        let flat = reader.header();
        if (scan.width, scan.height, scan.depth, scan.color)
            != (flat.width, flat.height, flat.depth, flat.color)
        {
            return Err(Error::FlatMismatch);
        }
        let spectrum = Spectrum::average(reader, |p| progress(p * 0.9))?;
        let slit_width = right - left + 1;
        let usable = |value: f32| (4. ..f32::from(SATURATION_CUTOFF)).contains(&value);
        let mut row_samples = Vec::with_capacity(slit_width);
        let mut rows = Vec::new();
        for row in spectrum
            .pixels()
            .chunks_exact(flat.width)
            .step_by(flat.height.div_ceil(64))
        {
            let row = &row[left..=right];
            row_samples.clear();
            row_samples.extend(row.iter().copied().filter(|&v| usable(v)));
            if row_samples.len() < slit_width * 3 / 4 || row_samples.is_empty() {
                continue;
            }
            rows.push((row, median(&mut row_samples)));
        }
        if rows.is_empty() {
            return Err(Error::InvalidPhysicalFlat);
        }
        // Normalize each wavelength independently. The median across wavelengths
        // rejects isolated defective samples while retaining narrow slit shadows.
        let mut gains = vec![1.; flat.width];
        let slit_gains = &mut gains[left..=right];
        let mut column_samples = Vec::with_capacity(rows.len());
        for (x, gain) in slit_gains.iter_mut().enumerate() {
            if x % 256 == 0 && !progress(0.9 + 0.1 * x as f32 / slit_width as f32) {
                return Err(Error::Cancelled);
            }
            column_samples.clear();
            for (row, level) in &rows {
                let value = row[x];
                if usable(value) && value >= level / 8. {
                    column_samples.push(value / level);
                }
            }
            if column_samples.len() < rows.len().div_ceil(2) {
                return Err(Error::InvalidPhysicalFlat);
            }
            *gain = median(&mut column_samples);
        }
        let (low, high) = slit_gains
            .iter()
            .fold((f32::INFINITY, 0_f32), |(low, high), &v| {
                (low.min(v), high.max(v))
            });
        if low < high * 0.25 {
            return Err(Error::InvalidPhysicalFlat);
        }
        row_samples.clear();
        row_samples.extend_from_slice(slit_gains);
        let reference = median(&mut row_samples);
        for gain in slit_gains {
            *gain = reference / *gain;
        }
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        Ok(Self { gains })
    }

    pub fn apply(&self, image: &mut Reconstruction) -> Result<(), Error> {
        self.apply_to(&mut [image]).map(|_| ())
    }

    /// Correct all products with one headroom scale so that wing ratios remain comparable.
    pub(crate) fn apply_to(&self, images: &mut [&mut Reconstruction]) -> Result<f64, Error> {
        if images.iter().any(|image| image.width != self.gains.len()) {
            return Err(Error::FlatMismatch);
        }
        let limit = f32::from(u16::MAX);
        let mut peak = limit;
        for image in images.iter() {
            for row in image.pixels.chunks_exact(image.width) {
                for (&pixel, &gain) in row.iter().zip(&self.gains) {
                    peak = peak.max(f32::from(pixel) * gain);
                }
            }
        }
        let scale = limit / peak;
        for image in images {
            for row in image.pixels.chunks_exact_mut(image.width) {
                for (pixel, &gain) in row.iter_mut().zip(&self.gains) {
                    *pixel = (f32::from(*pixel) * gain * scale).round() as u16;
                }
            }
        }
        Ok(f64::from(scale))
    }
}

fn median(samples: &mut [f32]) -> f32 {
    let even = samples.len().is_multiple_of(2);
    let (left, middle, _) = samples.select_nth_unstable_by(samples.len() / 2, f32::total_cmp);
    if even {
        middle.midpoint(left.iter().copied().fold(f32::NEG_INFINITY, f32::max))
    } else {
        *middle
    }
}

#[derive(Clone, Copy)]
struct Sample {
    mu: f64,
    slit: f64,
    log_signal: f64,
}

impl Sample {
    fn basis(self) -> [f64; 6] {
        let mu_squared = self.mu * self.mu;
        [
            1.,
            self.mu,
            mu_squared,
            mu_squared * self.mu,
            self.slit,
            self.slit * self.slit,
        ]
    }
}

impl Reconstruction {
    /// Estimate and remove smooth throughput variation along the slit of a full-disk scan.
    /// Returns whether the image was changed. Errors and cancellation leave it untouched.
    /// Cancellation is checked throughout model estimation and before applying the gains.
    pub fn correct_synthetic_flat(
        &mut self,
        progress: impl FnMut(f32) -> bool,
    ) -> Result<bool, Error> {
        self.correct_synthetic_flat_at_strength(100, progress)
    }

    pub(crate) fn correct_synthetic_flat_at_strength(
        &mut self,
        percent: u8,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<bool, Error> {
        if self.synthetic_flat || percent == 0 {
            return Ok(false);
        }
        if !progress(0.) {
            return Err(Error::Cancelled);
        }
        let disk = DiskGeometry::fit_limb(self)?;
        let samples = collect_samples(self, &disk, &mut progress)?;
        let mut clipped = Vec::new();
        clipped.try_reserve_exact(samples.len())?;
        let estimate = |fit: Option<([f64; 6], f64)>| {
            weighted_fit(samples.iter().map(|&sample| {
                let basis = sample.basis();
                let weight = fit.map_or(1., |(coefficients, cutoff)| {
                    let error = (predicted(basis, coefficients) - sample.log_signal).abs();
                    cutoff / error.max(cutoff)
                });
                (basis, sample.log_signal, weight)
            }))
            .ok_or(Error::NoFlatSamples)
        };
        let mut coefficients = estimate(None)?;
        for iteration in 0..3 {
            if !progress(0.4 + 0.13 * iteration as f32) {
                return Err(Error::Cancelled);
            }
            clipped.clear();
            clipped.extend(
                samples.iter().map(|sample| {
                    (predicted(sample.basis(), coefficients) - sample.log_signal).abs()
                }),
            );
            let mid = clipped.len() / 2;
            let residual = *clipped.select_nth_unstable_by(mid, f64::total_cmp).1;
            coefficients = estimate(Some((coefficients, (3.7 * residual).max(0.012))))?;
        }
        let (left, right) = samples
            .iter()
            .map(|sample| sample.slit)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), x| {
                (low.min(x), high.max(x))
            });
        if left > -0.4 || right < 0.4 {
            return Err(Error::NoFlatSamples);
        }
        let mut gains = Vec::new();
        gains.try_reserve_exact(self.width)?;
        for x in 0..self.width {
            let slit = ((x as f64 - disk.center_x) / disk.geometry.slit_radius).clamp(left, right);
            gains.push(slit * (coefficients[4] + slit * coefficients[5]));
        }
        let (low, high) = gains
            .iter()
            .copied()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), value| {
                (low.min(value), high.max(value))
            });
        let spread = high - low;
        if !spread.is_finite() {
            return Err(Error::NoFlatSamples);
        }
        if spread.exp_m1() < MIN_RESPONSE_CHANGE {
            return Ok(false);
        }
        if !progress(0.85) {
            return Err(Error::Cancelled);
        }
        // Normalize to the faintest fitted column. Attenuation preserves headroom
        // and the sky pedestal. Limit the whole correction without flattening it.
        let strength = (-MIN_GAIN.ln() / spread).min(1.) * f64::from(percent.min(100)) / 100.;
        for gain in &mut gains {
            *gain = ((low - *gain) * strength).exp();
        }
        for row in self.pixels.chunks_exact_mut(self.width) {
            for (pixel, &gain) in row.iter_mut().zip(&gains) {
                let signal = pixel.saturating_sub(disk.sky);
                let reduction = f64::from(signal) * (1. - gain);
                *pixel -= reduction.round() as u16;
            }
        }
        self.synthetic_flat = true;
        Ok(true)
    }
}

fn collect_samples(
    image: &Reconstruction,
    disk: &FittedDisk,
    progress: &mut impl FnMut(f32) -> bool,
) -> Result<Vec<Sample>, Error> {
    let geometry = disk.geometry;
    let horizontal_radius = geometry
        .slit_radius
        .hypot(geometry.shear * geometry.scan_radius);
    let bounds = |center: f64, radius: f64, size: usize| {
        let a = (center - FIT_RADIUS * radius).max(0.).floor() as usize;
        let b = ((center + FIT_RADIUS * radius).ceil() as usize + 1).min(size);
        a..b
    };
    let rows = bounds(disk.center_y, geometry.scan_radius, image.height);
    let columns = bounds(disk.center_x, horizontal_radius, image.width);
    let dy = rows.len().div_ceil(GRID_SIDE).max(1);
    let dx = columns.len().div_ceil(GRID_SIDE).max(1);
    let mut samples = Vec::new();
    samples.try_reserve_exact(rows.len().div_ceil(dy) * columns.len().div_ceil(dx))?;
    for (index, y) in rows.clone().step_by(dy).enumerate() {
        if !progress(0.4 * index as f32 / rows.len().div_ceil(dy) as f32) {
            return Err(Error::Cancelled);
        }
        let y_offset = y as f64 - disk.center_y;
        let radial_y = y_offset / geometry.scan_radius;
        for x in columns.clone().step_by(dx) {
            let slit = (x as f64 - disk.center_x) / geometry.slit_radius;
            let radial_x = slit - geometry.shear * y_offset / geometry.slit_radius;
            let radius_squared = radial_x * radial_x + radial_y * radial_y;
            let pixel = image.pixels[y * image.width + x];
            if radius_squared <= FIT_RADIUS * FIT_RADIUS
                && pixel > disk.sky
                && pixel < SATURATION_CUTOFF
            {
                samples.push(Sample {
                    mu: ((1. - radius_squared).sqrt() - 0.75) * 4.,
                    slit,
                    log_signal: f64::from(pixel - disk.sky).ln(),
                });
            }
        }
    }
    if samples.len() < 128 {
        return Err(Error::NoFlatSamples);
    }
    Ok(samples)
}

fn predicted(basis: [f64; 6], coefficients: [f64; 6]) -> f64 {
    basis.iter().zip(coefficients).map(|(x, a)| x * a).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_flat_preserves_wing_ratios_with_shared_headroom() {
        let flat = PhysicalFlat {
            gains: vec![1., 2.],
        };
        let image = |signal| Reconstruction {
            width: 2,
            height: 1,
            pixels: vec![12_000, signal],
            synthetic_flat: false,
        };
        let mut center = image(20_000);
        let mut red = image(40_000);
        let mut blue = image(10_000);
        let scale = flat
            .apply_to(&mut [&mut center, &mut red, &mut blue])
            .unwrap();
        assert!((scale - 65_535. / 80_000.).abs() < 1e-7);
        assert_eq!(red.pixels[1], 65_535);
        assert!(center.pixels[1].abs_diff(32_768) <= 1);
        assert!(blue.pixels[1].abs_diff(16_384) <= 1);
        assert_eq!(center.pixels[0], red.pixels[0]);
        assert_eq!(red.pixels[0], blue.pixels[0]);

        let metadata = crate::FitsMetadata {
            flat_file: Some("test-flat.ser".to_owned()),
            physical_flat_scale: Some(scale),
            ..crate::FitsMetadata::default()
        };
        let mut fits = Vec::new();
        center
            .write_fits_with_metadata(&mut fits, &metadata)
            .unwrap();
        assert!(
            std::str::from_utf8(&fits[..2880])
                .unwrap()
                .contains("FLATSCL =")
        );
    }

    fn disk(cx: f64, vignette: bool) -> Reconstruction {
        let (width, height) = (256, 232);
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            let row = y as f64 - 115.;
            for x in 0..width {
                let slit = (x as f64 - cx) / 97.;
                let disk_x = slit - 0.16 * row / 97.;
                let disk_y = row / 91.;
                let r2 = disk_x * disk_x + disk_y * disk_y;
                let mut signal = if r2 < 1. {
                    14_000. + 26_000. * (1. - r2).sqrt()
                } else {
                    0.
                };
                if (disk_x - 0.36).powi(2) + (disk_y + 0.17).powi(2) < 0.12_f64.powi(2) {
                    signal *= 0.55;
                }
                if (disk_x + 0.28).powi(2) + (disk_y - 0.25).powi(2) < 0.1_f64.powi(2) {
                    signal *= 1.35;
                }
                if vignette {
                    signal *= (0.10 * slit - 0.11 * slit * slit).exp();
                }
                pixels.push((800. + signal).round() as u16);
            }
        }
        Reconstruction {
            width,
            height,
            pixels,
            synthetic_flat: false,
        }
    }

    fn response_error(reference: &Reconstruction, candidate: &Reconstruction) -> f64 {
        let ratios: Vec<_> = (65..166)
            .flat_map(|y| (70..181).map(move |x| y * reference.width + x))
            .map(|i| f64::from(candidate.pixels[i] - 800) / f64::from(reference.pixels[i] - 800))
            .collect();
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        (ratios
            .iter()
            .map(|value| (value / mean - 1.).powi(2))
            .sum::<f64>()
            / ratios.len() as f64)
            .sqrt()
    }

    #[test]
    fn restores_injected_slit_response_and_preserves_limb_darkening() {
        let reference = disk(125., false);
        let mut image = disk(125., true);
        let before = response_error(&reference, &image);
        let original = image.pixels.clone();
        assert!(image.correct_synthetic_flat(|_| true).unwrap());
        let after = response_error(&reference, &image);
        assert!(after < before / 4., "{before:.4} -> {after:.4}");
        assert!(
            image
                .pixels
                .iter()
                .zip(original)
                .all(|(&a, b)| a <= b && a >= 800)
        );
        assert!(image.has_synthetic_flat());
        let geometry = DiskGeometry::fit(&image).unwrap();
        let output = geometry.correct(&image, |_| true).unwrap();
        let mut fits = Vec::new();
        output.write_fits(&mut fits).unwrap();
        assert!(
            str::from_utf8(&fits[..2880])
                .unwrap()
                .contains("FLATCORR= 'SYNTHETIC'")
        );
    }

    #[test]
    fn synthetic_flat_strength_controls_attenuation() {
        let reference = disk(125., false);
        let mut image = disk(125., true);
        let original = image.pixels.clone();
        let before = response_error(&reference, &image);
        assert!(
            !image
                .correct_synthetic_flat_at_strength(0, |_| true)
                .unwrap()
        );
        assert_eq!(image.pixels, original);
        assert!(
            image
                .correct_synthetic_flat_at_strength(50, |_| true)
                .unwrap()
        );
        let partial = response_error(&reference, &image);

        let mut full = disk(125., true);
        assert!(full.correct_synthetic_flat(|_| true).unwrap());
        assert!(response_error(&reference, &full) < partial && partial < before);
        assert!(
            image
                .pixels
                .iter()
                .zip(&full.pixels)
                .zip(original)
                .all(|((&partial, &full), original)| full <= partial && partial <= original)
        );
    }

    #[test]
    fn leaves_clean_cropped_and_cancelled_scans_untouched() {
        let mut clean = disk(125., false);
        let pixels = clean.pixels.clone();
        assert!(!clean.correct_synthetic_flat(|_| true).unwrap());
        assert_eq!(clean.pixels, pixels);

        let mut cropped = disk(60., true);
        let pixels = cropped.pixels.clone();
        assert!(matches!(
            cropped.correct_synthetic_flat(|_| true),
            Err(Error::NoSolarLimb)
        ));
        assert_eq!(cropped.pixels, pixels);

        let mut cancelled = disk(125., true);
        let pixels = cancelled.pixels.clone();
        assert!(matches!(
            cancelled.correct_synthetic_flat(|p| p < 0.55),
            Err(Error::Cancelled)
        ));
        assert_eq!(cancelled.pixels, pixels);
    }

    #[test]
    fn saturated_disk_cannot_supply_a_flat_model() {
        let mut image = disk(125., false);
        for pixel in &mut image.pixels {
            if *pixel > 800 {
                *pixel = SATURATION_CUTOFF;
            }
        }
        let pixels = image.pixels.clone();
        assert!(matches!(
            image.correct_synthetic_flat(|_| true),
            Err(Error::NoFlatSamples)
        ));
        assert_eq!(image.pixels, pixels);
    }
}

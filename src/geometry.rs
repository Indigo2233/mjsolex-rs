use crate::reconstruct::MAX_PIXELS;
use crate::regression::weighted_fit;
use crate::{Error, Reconstruction};

#[derive(Clone, Copy, Debug)]
pub struct DiskGeometry {
    /// Horizontal half-chord at the fitted scan center, in source pixels.
    pub slit_radius: f64,
    /// Distance from the fitted scan center to the upper or lower limb.
    pub scan_radius: f64,
    /// Change in the horizontal chord center per scan row.
    pub shear: f64,
}

pub(crate) struct FittedDisk {
    pub(crate) geometry: DiskGeometry,
    pub(crate) center_x: f64,
    pub(crate) center_y: f64,
    pub(crate) sky: u16,
}

struct Chord {
    row: f64,
    center: f64,
    squared_radius: f64,
}

impl DiskGeometry {
    /// Fit the centers and squared half lengths of horizontal chords through the limb.
    pub fn fit(image: &Reconstruction) -> Result<Self, Error> {
        Self::fit_limb(image).map(|disk| disk.geometry)
    }

    pub(crate) fn fit_limb(image: &Reconstruction) -> Result<FittedDisk, Error> {
        let (width, height) = (image.width, image.height);
        let (sky, threshold) = limb_levels(&image.pixels).ok_or(Error::NoSolarLimb)?;
        let x_scale = (width - 1) as f64 / 2.;
        let y_scale = (height - 1) as f64 / 2.;
        let mut chords = Vec::new();
        for (y, row) in image
            .pixels
            .chunks_exact(width)
            .enumerate()
            .step_by(height.div_ceil(256))
        {
            let Some((left, right)) = chord_edges(row, threshold) else {
                continue;
            };
            chords.push(Chord {
                row: y as f64 / y_scale - 1.,
                center: (left + right) / (2. * x_scale) - 1.,
                squared_radius: ((right - left) / (2. * x_scale)).powi(2),
            });
        }
        let total = chords.len();
        if total < 16 {
            return Err(Error::NoSolarLimb);
        }
        let (line, ellipse) = fit_chords(&chords).ok_or(Error::NoSolarLimb)?;
        let center_error = |chord: &Chord| (line[0] + line[1] * chord.row - chord.center).abs();
        let radius_error = |chord: &Chord| {
            let predicted = (ellipse[2] * chord.row + ellipse[1]) * chord.row + ellipse[0];
            (predicted - chord.squared_radius).abs()
        };
        let center_limit = robust_limit(&chords, center_error, 2. / x_scale).min(0.06);
        let radius_limit = robust_limit(&chords, radius_error, 4. / x_scale).min(0.08);
        chords.retain(|p| center_error(p) <= center_limit && radius_error(p) <= radius_limit);
        if chords.len() < 16 || chords.len() * 3 < total * 2 {
            return Err(Error::NoSolarLimb);
        }

        let ([intercept, slope], [constant, linear, quadratic]) =
            fit_chords(&chords).ok_or(Error::NoSolarLimb)?;
        if quadratic >= 0. {
            return Err(Error::NoSolarLimb);
        }
        let y = -linear / (2. * quadratic);
        let peak = (quadratic * y + linear) * y + constant;
        if peak <= 0. {
            return Err(Error::NoSolarLimb);
        }
        let scan_radius = (-peak / quadratic).sqrt();
        let slit_radius = peak.sqrt();
        let x = intercept + slope * y;
        let horizontal_radius = slit_radius.hypot(slope * scan_radius);
        // Reject partial disks and fits based only on a small part of the visible limb.
        if y.abs() + scan_radius > 1. + 2. / y_scale
            || x.abs() + horizontal_radius > 1. + 2. / x_scale
            || !chords.iter().any(|p| p.row <= y - 0.65 * scan_radius)
            || !chords.iter().any(|p| p.row >= y + 0.65 * scan_radius)
        {
            return Err(Error::NoSolarLimb);
        }
        let geometry = Self {
            slit_radius: slit_radius * x_scale,
            scan_radius: scan_radius * y_scale,
            shear: slope * x_scale / y_scale,
        };
        if !(0.25..=4.).contains(&(geometry.slit_radius / geometry.scan_radius))
            || geometry.shear.abs() > 2.
        {
            return Err(Error::NoSolarLimb);
        }
        Ok(FittedDisk {
            geometry,
            center_x: (x + 1.) * x_scale,
            center_y: (y + 1.) * y_scale,
            sky,
        })
    }

    /// Remove scan shear and scale the scan axis to make the fitted limb circular.
    pub fn correct(
        self,
        image: &Reconstruction,
        progress: impl FnMut(f32) -> bool,
    ) -> Result<Reconstruction, Error> {
        self.correct_with_budget(image, MAX_PIXELS, progress)
    }

    pub(crate) fn correct_with_budget(
        self,
        image: &Reconstruction,
        max_pixels: usize,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Reconstruction, Error> {
        let scale = self.slit_radius / self.scan_radius;
        if !(0.25..=4.).contains(&scale) || !self.shear.is_finite() || self.shear.abs() > 2. {
            return Err(Error::NoSolarLimb);
        }
        let x_end = -self.shear * (image.height - 1) as f64;
        let min_x = x_end.min(0.).floor();
        let width =
            ((image.width - 1) as f64 + x_end.max(0.)).ceil() as usize + (-min_x) as usize + 1;
        let height = (((image.height - 1) as f64) * scale).ceil() as usize + 1;
        if width
            .checked_mul(height)
            .is_none_or(|count| count > max_pixels)
        {
            return Err(Error::OutputTooLarge);
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(width * height)?;
        for row in 0..height {
            if !progress(row as f32 / height as f32) {
                return Err(Error::Cancelled);
            }
            let y = row as f64 / scale;
            if y > (image.height - 1) as f64 {
                pixels.resize(pixels.len() + width, 0);
                continue;
            }
            let y0 = y as usize;
            let y1 = (y0 + 1).min(image.height - 1);
            let top = &image.pixels[y0 * image.width..(y0 + 1) * image.width];
            let bottom = &image.pixels[y1 * image.width..(y1 + 1) * image.width];
            let shift = min_x + self.shear * y;
            let fraction_y = y - y0 as f64;
            for col in 0..width {
                let x = col as f64 + shift;
                let pixel = if (0. ..=(image.width - 1) as f64).contains(&x) {
                    let x0 = x as usize;
                    let x1 = (x0 + 1).min(image.width - 1);
                    let fraction_x = x - x0 as f64;
                    let row_value = |row: &[u16]| {
                        f64::from(row[x0]) * (1. - fraction_x) + f64::from(row[x1]) * fraction_x
                    };
                    (row_value(top) * (1. - fraction_y) + row_value(bottom) * fraction_y).round()
                        as u16
                } else {
                    0
                };
                pixels.push(pixel);
            }
        }
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        Ok(Reconstruction {
            width,
            height,
            pixels,
            synthetic_flat: image.synthetic_flat,
        })
    }
}

fn limb_levels(pixels: &[u16]) -> Option<(u16, f64)> {
    let stride = pixels.len().div_ceil(100_000);
    let mut bins = vec![0u32; 65536];
    let count = pixels.len().div_ceil(stride);
    for &pixel in pixels.iter().step_by(stride) {
        bins[usize::from(pixel)] += 1;
    }
    let percentile = |target| {
        let mut seen = 0;
        bins.iter().position(|&count| {
            seen += count as usize;
            seen > target
        })
    };
    let sky = percentile(count / 20)?;
    let disk = percentile(count * 19 / 20)?;
    (disk > sky + 6).then(|| (sky as u16, sky as f64 + (disk - sky) as f64 * 0.20))
}

fn chord_edges(row: &[u16], threshold: f64) -> Option<(f64, f64)> {
    let inside = |samples: &[u16]| samples.iter().all(|&value| f64::from(value) >= threshold);
    let left = row.windows(3).position(inside)?;
    let right = row.windows(3).rposition(inside)? + 2;
    if left == 0 || right == row.len() - 1 || right - left < 6 {
        return None;
    }
    let crossing = |inside, outside| {
        let a = f64::from(row[inside]);
        let b = f64::from(row[outside]);
        if b >= threshold {
            0.
        } else {
            (a - threshold) / (a - b)
        }
    };
    Some((
        left as f64 - crossing(left, left - 1),
        right as f64 + crossing(right, right + 1),
    ))
}

fn fit_chords(chords: &[Chord]) -> Option<([f64; 2], [f64; 3])> {
    let line = weighted_fit(chords.iter().map(|s| ([1., s.row], s.center, 1.)))?;
    let ellipse = weighted_fit(
        chords
            .iter()
            .map(|s| ([1., s.row, s.row * s.row], s.squared_radius, 1.)),
    )?;
    Some((line, ellipse))
}

fn robust_limit(chords: &[Chord], residual: impl Fn(&Chord) -> f64, floor: f64) -> f64 {
    let mut errors: Vec<_> = chords.iter().map(residual).collect();
    let mid = errors.len() / 2;
    (3. * *errors.select_nth_unstable_by(mid, f64::total_cmp).1).max(floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(cx: f64, cy: f64) -> Reconstruction {
        let (width, height) = (164, 120);
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            let row = y as f64 - cy;
            for x in 0..width {
                let col = x as f64 - cx - 0.31 * row;
                let r2 = (row / 35.).powi(2) + (col / 55.).powi(2);
                pixels.push(if r2 <= 1. {
                    4000 + (2000. * (1. - r2).sqrt()) as u16
                } else {
                    20
                });
            }
        }
        Reconstruction {
            width,
            height,
            pixels,
            synthetic_flat: false,
        }
    }

    #[test]
    fn removes_disk_shear_and_scan_axis_scale() {
        let raw = disk(80., 60.);
        let geometry = DiskGeometry::fit(&raw).unwrap();
        assert!((geometry.slit_radius - 55.).abs() < 1.);
        assert!((geometry.scan_radius - 35.).abs() < 1.);
        assert!((geometry.shear - 0.31).abs() < 0.02);
        assert!(matches!(
            geometry.correct(&raw, |p| p < 0.4),
            Err(Error::Cancelled)
        ));
        let corrected = geometry.correct(&raw, |_| true).unwrap();
        let fitted = DiskGeometry::fit(&corrected).unwrap();
        assert!((fitted.scan_radius / fitted.slit_radius - 1.).abs() < 0.03);
        assert!(fitted.shear.abs() < 0.02);
    }

    #[test]
    fn budgets_geometry_output_after_scaling() {
        let raw = disk(80., 60.);
        let geometry = DiskGeometry {
            slit_radius: 70.,
            scan_radius: 35.,
            shear: 0.,
        };
        let budget = raw.width() * (2 * raw.height() - 1);
        assert!(matches!(
            geometry.correct_with_budget(&raw, budget - 1, |_| {
                panic!("an oversized result must be rejected before resampling");
            }),
            Err(Error::OutputTooLarge)
        ));
        let corrected = geometry
            .correct_with_budget(&raw, budget, |_| true)
            .unwrap();
        assert_eq!(corrected.pixels().len(), budget);
    }

    #[test]
    fn rejects_partial_disks_and_uniform_scans() {
        assert!(matches!(
            DiskGeometry::fit(&disk(35., 60.)),
            Err(Error::NoSolarLimb)
        ));
        assert!(matches!(
            DiskGeometry::fit(&disk(80., 20.)),
            Err(Error::NoSolarLimb)
        ));
        let mut blank = disk(80., 60.);
        blank.pixels.fill(12000);
        assert!(matches!(DiskGeometry::fit(&blank), Err(Error::NoSolarLimb)));
    }
}

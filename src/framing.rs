#[cfg(any(target_os = "android", test))]
use crate::{DiskGeometry, Error, Reconstruction};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Framing {
    pub flip_x: bool,
    pub flip_y: bool,
    /// Clockwise rotation in the reconstructed image, after the flips.
    pub rotation_deg: f64,
    /// Size of the square frame, as a factor of the projected disk diameter.
    pub crop_factor: Option<f64>,
}

#[cfg(any(target_os = "android", test))]
pub(crate) struct FrameTransform {
    width: usize,
    height: usize,
    source_size: [usize; 2],
    source_center: [f64; 2],
    output_center: [f64; 2],
    basis: [[f64; 2]; 2],
}

#[cfg(any(target_os = "android", test))]
impl Framing {
    pub(crate) fn prepare(
        self,
        image: &Reconstruction,
        max_pixels: usize,
    ) -> Result<Option<FrameTransform>, Error> {
        if !self.rotation_deg.is_finite()
            || self.crop_factor.is_some_and(|f| !(1. ..=2.).contains(&f))
        {
            return Err(Error::InvalidFraming);
        }
        let rotation = self.rotation_deg.rem_euclid(360.);
        if !self.flip_x && !self.flip_y && rotation == 0. && self.crop_factor.is_none() {
            return Ok(None);
        }
        let (sin, cos) = match rotation {
            0. => (0., 1.),
            90. => (1., 0.),
            180. => (0., -1.),
            270. => (-1., 0.),
            _ => rotation.to_radians().sin_cos(),
        };
        let flip_x = if self.flip_x { -1. } else { 1. };
        let flip_y = if self.flip_y { -1. } else { 1. };
        let basis = [[cos * flip_x, -sin * flip_y], [sin * flip_x, cos * flip_y]];
        let source_center = [
            (image.width - 1) as f64 / 2.,
            (image.height - 1) as f64 / 2.,
        ];
        let apply = |x, y| basis.map(|[a, b]| a * x + b * y);
        let (width, height, center) = if let Some(factor) = self.crop_factor {
            let disk = DiskGeometry::fit_limb(image)?;
            let center = apply(
                disk.center_x - source_center[0],
                disk.center_y - source_center[1],
            );
            let geometry = disk.geometry;
            let radius = basis
                .map(|[a, b]| {
                    (a * geometry.slit_radius)
                        .hypot((a * geometry.shear + b) * geometry.scan_radius)
                })
                .into_iter()
                .fold(0., f64::max);
            let side = (2. * radius * factor).ceil() as usize + 1;
            (side, side, center)
        } else {
            let dimensions = basis.map(|[a, b]| {
                (2. * (a.abs() * source_center[0] + b.abs() * source_center[1])).ceil() as usize + 1
            });
            (dimensions[0], dimensions[1], [0., 0.])
        };
        if width.checked_mul(height).is_none_or(|n| n > max_pixels) {
            return Err(Error::OutputTooLarge);
        }
        Ok(Some(FrameTransform {
            width,
            height,
            source_size: [image.width, image.height],
            source_center,
            output_center: [
                (width - 1) as f64 / 2. - center[0],
                (height - 1) as f64 / 2. - center[1],
            ],
            basis,
        }))
    }
}

#[cfg(any(target_os = "android", test))]
impl FrameTransform {
    pub(crate) fn apply(
        &self,
        image: &Reconstruction,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Reconstruction, Error> {
        if self.source_size != [image.width, image.height] {
            return Err(Error::InvalidFraming);
        }
        let [[a, b], [c, d]] = self.basis;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(self.width * self.height)?;
        for y in 0..self.height {
            if !progress(y as f32 / self.height as f32) {
                return Err(Error::Cancelled);
            }
            let v = y as f64 - self.output_center[1];
            let base_x = self.source_center[0] + c * v;
            let base_y = self.source_center[1] + d * v;
            for x in 0..self.width {
                let u = x as f64 - self.output_center[0];
                pixels.push(sample(image, base_x + a * u, base_y + b * u));
            }
        }
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        Ok(Reconstruction {
            width: self.width,
            height: self.height,
            pixels,
            synthetic_flat: image.synthetic_flat,
        })
    }
}

#[cfg(any(target_os = "android", test))]
fn sample(image: &Reconstruction, x: f64, y: f64) -> u16 {
    let max_x = (image.width - 1) as f64;
    let max_y = (image.height - 1) as f64;
    if !(-0.5..=max_x + 0.5).contains(&x) || !(-0.5..=max_y + 0.5).contains(&y) {
        return 0;
    }
    let (x, y) = (x.clamp(0., max_x), y.clamp(0., max_y));
    let (x0, y0) = (x as usize, y as usize);
    let x1 = (x0 + 1).min(image.width - 1);
    let y1 = (y0 + 1).min(image.height - 1);
    let at = |row| {
        let left = f64::from(image.pixels[row * image.width + x0]);
        let right = f64::from(image.pixels[row * image.width + x1]);
        left + (x - x0 as f64) * (right - left)
    };
    let top = at(y0);
    (top + (y - y0 as f64) * (at(y1) - top)).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_flips_without_changing_pixel_values() {
        let source = Reconstruction {
            width: 3,
            height: 2,
            pixels: vec![1, 2, 3, 4, 5, 6],
            synthetic_flat: true,
        };
        let frame = Framing {
            flip_x: true,
            rotation_deg: 90.,
            ..Framing::default()
        };
        assert!(matches!(
            frame.prepare(&source, 5),
            Err(Error::OutputTooLarge)
        ));
        let transform = frame.prepare(&source, 6).unwrap().unwrap();
        assert!(matches!(
            transform.apply(&source, |p| p < 0.5),
            Err(Error::Cancelled)
        ));
        let result = transform.apply(&source, |_| true).unwrap();
        assert_eq!((result.width(), result.height()), (2, 3));
        assert_eq!(result.pixels(), &[6, 3, 5, 2, 4, 1]);
        assert!(result.has_synthetic_flat());
    }

    #[test]
    fn crop_contains_an_offset_rotated_disk() {
        let (width, height) = (192, 170);
        let source = Reconstruction {
            width,
            height,
            pixels: (0..width * height)
                .map(|i| {
                    let x = (i % width) as f64 - 78.;
                    let y = (i / width) as f64 - 102.;
                    if (x / 43.).powi(2) + (y / 38.).powi(2) <= 1. {
                        7000
                    } else {
                        0
                    }
                })
                .collect(),
            synthetic_flat: false,
        };
        let frame = Framing {
            rotation_deg: 37.,
            crop_factor: Some(1.2),
            ..Framing::default()
        };
        let transform = frame.prepare(&source, 100_000).unwrap().unwrap();
        let result = transform.apply(&source, |_| true).unwrap();
        assert_eq!(result.width(), result.height());
        assert!((95..110).contains(&result.width()));
        let disk = DiskGeometry::fit_limb(&result).unwrap();
        let center = (result.width() - 1) as f64 / 2.;
        assert!((disk.center_x - center).abs() < 1.);
        assert!((disk.center_y - center).abs() < 1.);
        let metadata = crate::FitsMetadata {
            framing: frame,
            scan_corrections: crate::ScanReport {
                banding: crate::BandingAxis::Slit,
                banding_scale: Some(0.91),
                banding_width: Some(17),
                jagged: true,
                oscillation: Some((21., 2.5)),
            },
            ..crate::FitsMetadata::default()
        };
        let mut fits = Vec::new();
        result
            .write_fits_with_metadata(&mut fits, &metadata)
            .unwrap();
        let header = std::str::from_utf8(&fits[..2880]).unwrap();
        for key in [
            "ROTANGLE=",
            "CROPFACT=",
            "BANDCORR= 'SLIT'",
            "BANDSCL",
            "BANDWID",
            "OSCPER",
            "OSCAMP",
            "JAGGCOR",
        ] {
            assert!(header.contains(key), "{key}");
        }
    }

    #[test]
    fn a_rotated_single_row_retains_its_ends() {
        let source = Reconstruction {
            width: 3,
            height: 1,
            pixels: vec![500, 1000, 1500],
            synthetic_flat: false,
        };
        let frame = Framing {
            rotation_deg: 45.,
            ..Framing::default()
        };
        let result = frame
            .prepare(&source, 9)
            .unwrap()
            .unwrap()
            .apply(&source, |_| true)
            .unwrap();
        assert_eq!(result.pixels(), &[500, 0, 0, 0, 1000, 0, 0, 0, 1500]);
    }
}

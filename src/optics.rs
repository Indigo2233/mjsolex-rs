/// Reflective grating spectroheliograph parameters in the plane of dispersion.
#[derive(Clone, Copy, Debug)]
pub struct OpticalSetup {
    pub lines_per_mm: f64,
    pub order: u32,
    pub included_angle_deg: f64,
    pub camera_focal_length_mm: f64,
    pub pixel_size_um: f64,
    pub spectral_binning: u32,
}

impl Default for OpticalSetup {
    fn default() -> Self {
        Self {
            lines_per_mm: 0.,
            order: 1,
            included_angle_deg: 0.,
            camera_focal_length_mm: 0.,
            pixel_size_um: 0.,
            spectral_binning: 1,
        }
    }
}

impl OpticalSetup {
    /// Local wavelength step at the reference line, in Å per recorded pixel.
    /// The vertical orientation of the recording determines its sign separately.
    pub fn dispersion_angstrom(self, wavelength: f64) -> Option<f64> {
        if [
            wavelength,
            self.lines_per_mm,
            self.camera_focal_length_mm,
            self.pixel_size_um,
        ]
        .into_iter()
        .any(|value| !value.is_finite() || value <= 0.)
            || self.order == 0
            || self.spectral_binning == 0
            || !(0. ..180.).contains(&self.included_angle_deg)
        {
            return None;
        }

        let half_angle = self.included_angle_deg.to_radians() / 2.;
        let order_density = f64::from(self.order) * self.lines_per_mm;
        // α - β is the included angle. Solve mλ = d(sin α + sin β).
        let mean_angle = (wavelength * 1e-7 * order_density / (2. * half_angle.cos())).asin();
        let incidence = mean_angle + half_angle;
        if !(0. ..std::f64::consts::FRAC_PI_2).contains(&incidence) {
            return None;
        }
        let diffraction = mean_angle - half_angle;
        let pixel = self.pixel_size_um * f64::from(self.spectral_binning);
        let dispersion =
            10_000. * pixel * diffraction.cos() / (order_density * self.camera_focal_length_mm);
        (dispersion.is_finite() && dispersion > 0.).then_some(dispersion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_grating_equation_and_scales_with_binning() {
        let wavelength = 6562.81;
        let mut setup = OpticalSetup {
            lines_per_mm: 2400.,
            order: 1,
            included_angle_deg: 34.,
            camera_focal_length_mm: 125.,
            pixel_size_um: 2.4,
            spectral_binning: 1,
        };
        let first = setup.dispersion_angstrom(wavelength).unwrap();
        assert!((0.06..0.07).contains(&first));
        setup.spectral_binning = 2;
        assert_eq!(setup.dispersion_angstrom(wavelength), Some(2. * first));

        // In Littrow geometry, hold the incidence angle fixed and step the
        // camera by a fraction of a pixel in either direction.
        setup.included_angle_deg = 0.;
        setup.lines_per_mm = 1200.;
        setup.order = 2;
        let density = 2400.;
        let angle = (wavelength * 1e-7_f64 * density / 2.).asin();
        let step = (setup.pixel_size_um * 2. / 1000. / setup.camera_focal_length_mm).atan();
        let expected = 1e7 * ((angle + step / 2.).sin() - (angle - step / 2.).sin()) / density;
        assert!((setup.dispersion_angstrom(wavelength).unwrap() - expected).abs() < 1e-8);
    }

    #[test]
    fn rejects_missing_and_impossible_optics() {
        let mut setup = OpticalSetup {
            lines_per_mm: 2400.,
            order: 1,
            included_angle_deg: 34.,
            camera_focal_length_mm: 125.,
            pixel_size_um: 0.,
            spectral_binning: 1,
        };
        assert_eq!(setup.dispersion_angstrom(6562.81), None);
        setup.pixel_size_um = 2.4;
        assert_eq!(setup.dispersion_angstrom(f64::NAN), None);
        assert_eq!(setup.dispersion_angstrom(0.), None);
        setup.included_angle_deg = 60.;
        assert_eq!(setup.dispersion_angstrom(7900.), None);
        setup.included_angle_deg = 34.;
        setup.order = 2;
        assert_eq!(setup.dispersion_angstrom(6562.81), None);
    }
}

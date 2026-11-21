// BASS2000 data and attribution are documented in assets/bass2000-NOTICE.txt.
const MIN_ANGSTROM: f64 = 3000.;
const MAX_ANGSTROM: f64 = 9999.99;
const ATLAS: &[u8; 1_400_000] = include_bytes!("../assets/bass2000.bin");

fn intensity_at(wavelength: f64) -> Option<f64> {
    if !(MIN_ANGSTROM..=MAX_ANGSTROM).contains(&wavelength) {
        return None;
    }
    let samples = ATLAS.as_chunks::<2>().0;
    let index = ((wavelength - MIN_ANGSTROM) * 100.).min((samples.len() - 1) as f64);
    let lower = index as usize;
    let upper = (lower + 1).min(samples.len() - 1);
    let a = f64::from(u16::from_le_bytes(samples[lower]));
    let b = f64::from(u16::from_le_bytes(samples[upper]));
    Some(a + index.fract() * (b - a))
}

pub(crate) struct ReferenceProfile {
    pub(crate) samples: Vec<[f64; 2]>,
    pub(crate) observed_peak: f64,
}

/// Resample the atlas at the observed offsets. Match the two series' peaks as
/// in JSol'Ex so both can be displayed on the same relative-intensity scale.
pub(crate) fn reference_profile(
    observed: &[[f64; 2]],
    zero_angstrom: f64,
    angstrom_per_pixel: f64,
) -> Option<ReferenceProfile> {
    intensity_at(zero_angstrom)?;
    if !angstrom_per_pixel.is_finite() || angstrom_per_pixel == 0. {
        return None;
    }
    let observed_peak = observed.iter().map(|p| p[1]).fold(0., f64::max);
    let mut reference_peak = 0_f64;
    let mut samples = Vec::with_capacity(observed.len());
    for &[offset, _] in observed {
        if let Some(reference) = intensity_at(zero_angstrom + offset * angstrom_per_pixel) {
            reference_peak = reference_peak.max(reference);
            samples.push([offset, reference]);
        }
    }
    if samples.len() < 2
        || !observed_peak.is_finite()
        || observed_peak <= 0.
        || reference_peak <= 0.
    {
        return None;
    }
    for sample in &mut samples {
        sample[1] = (sample[1] / reference_peak) * observed_peak;
    }
    Some(ReferenceProfile {
        samples,
        observed_peak,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_h_alpha_atlas_samples_and_interpolates() {
        let expected = [(6562.78, 1610.), (6562.8, 1606.), (6562.82, 1604.)];
        for (wavelength, value) in expected {
            assert!((intensity_at(wavelength).unwrap() - value).abs() < 1e-8);
        }
        let middle = intensity_at(6562.805).unwrap();
        let endpoints = (intensity_at(6562.8).unwrap() + intensity_at(6562.81).unwrap()) / 2.;
        assert!((middle - endpoints).abs() < 1e-8);
        assert!(intensity_at(MIN_ANGSTROM).is_some());
        assert!(intensity_at(MAX_ANGSTROM).is_some());
        assert!(intensity_at(10000.).is_none());
    }

    #[test]
    fn aligns_both_dispersion_directions_and_scales_to_the_observed_peak() {
        let observed = [[-1., 290.], [0., 240.], [1., 280.]];
        let positive = reference_profile(&observed, 6562.8, 0.02).unwrap();
        let negative = reference_profile(&observed, 6562.8, -0.02).unwrap();
        assert_eq!(positive.observed_peak, 290.);
        assert_eq!(positive.samples[0], [-1., 290.]);
        for (a, b) in positive.samples.iter().zip(negative.samples.iter().rev()) {
            assert_eq!(a[1], b[1]);
        }
        assert!(reference_profile(&observed, 6562.8, 0.).is_none());
        assert!(reference_profile(&observed, 0., 0.02).is_none());
        assert!(reference_profile(&observed, 6562.8, f64::NAN).is_none());
    }
}

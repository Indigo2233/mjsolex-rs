// BASS2000 data and attribution are documented in assets/bass2000-NOTICE.txt.
use crate::LineProfile;

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
    pub(crate) line_depth: Option<LineDepthComparison>,
}

pub(crate) struct LineDepthComparison {
    pub(crate) observed: f64,
    pub(crate) reference: f64,
}

impl LineDepthComparison {
    pub(crate) fn ratio(&self) -> f64 {
        self.observed / self.reference
    }
}

/// Resample the atlas at the observed offsets. Match the two series' peaks as
/// in JSol'Ex so both can be displayed on the same relative-intensity scale.
pub(crate) fn reference_profile(
    profile: &LineProfile,
    zero_angstrom: f64,
    angstrom_per_pixel: f64,
) -> Option<ReferenceProfile> {
    intensity_at(zero_angstrom)?;
    if !angstrom_per_pixel.is_finite() || angstrom_per_pixel == 0. {
        return None;
    }
    let observed = &profile.samples;
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
    let line_depth = compare_line_depth(profile, &samples);
    for sample in &mut samples {
        sample[1] = (sample[1] / reference_peak) * observed_peak;
    }
    Some(ReferenceProfile {
        samples,
        observed_peak,
        line_depth,
    })
}

fn compare_line_depth(
    profile: &LineProfile,
    reference: &[[f64; 2]],
) -> Option<LineDepthComparison> {
    let fit = profile.fit?;
    let radius = [
        -profile.samples.first()?[0],
        profile.samples.last()?[0],
        -reference.first()?[0],
        reference.last()?[0],
    ]
    .into_iter()
    .reduce(f64::min)?;
    // Keep both shoulder bands outside the observed half-depth crossings.
    if radius < (fit.center.abs() + fit.fwhm).max(4.) {
        return None;
    }
    Some(LineDepthComparison {
        observed: local_line_depth(&profile.samples, radius)?,
        reference: local_line_depth(reference, radius)?,
    })
}

/// Measure the depression relative to a baseline through the two shoulders of
/// the same symmetric window. Retain the sampled core depth of each profile.
fn local_line_depth(samples: &[[f64; 2]], radius: f64) -> Option<f64> {
    let start = samples.partition_point(|p| p[0] < -radius);
    let end = samples.partition_point(|p| p[0] <= radius);
    let samples = &samples[start..end];
    if samples.len() < 9 {
        return None;
    }
    let band = (samples.len() / 8).clamp(2, 4);
    let mean = |samples: &[[f64; 2]]| {
        let sum = samples
            .iter()
            .fold([0.; 2], |sum, p| [sum[0] + p[0], sum[1] + p[1]]);
        sum.map(|v| v / samples.len() as f64)
    };
    let [left_x, left] = mean(&samples[..band]);
    let [right_x, right] = mean(&samples[samples.len() - band..]);
    let baseline = |x: f64| left + (right - left) * (x - left_x) / (right_x - left_x);
    let (index, &[x, core]) = samples
        .iter()
        .enumerate()
        .filter(|(_, p)| p[0].abs() <= 2.)
        .min_by(|(_, a), (_, b)| (a[1] / baseline(a[0])).total_cmp(&(b[1] / baseline(b[0]))))?;
    let normalized = core / baseline(x);
    let brighter = |p: &[f64; 2]| p[1] / baseline(p[0]) >= normalized;
    if core >= left.min(right)
        || !samples.get(index.checked_sub(1)?).is_some_and(brighter)
        || !samples.get(index + 1).is_some_and(brighter)
    {
        return None;
    }
    let depth = 1. - normalized;
    (0.005..=1.).contains(&depth).then_some(depth)
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
        let observed = LineProfile::new(vec![[-1., 290.], [0., 240.], [1., 280.]]);
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

    #[test]
    fn compares_absorption_depth_in_the_same_window_and_both_directions() {
        let wavelength = 6562.82;
        let profile = |dispersion: f64, bounds: std::ops::RangeInclusive<i32>| {
            LineProfile::new(
                bounds
                    .map(|x| {
                        let x = f64::from(x);
                        let atlas = intensity_at(wavelength + x * dispersion).unwrap();
                        [x, 2. * atlas + 16_000.]
                    })
                    .collect(),
            )
        };
        for dispersion in [0.08, -0.08] {
            let full = profile(dispersion, -32..=32);
            let atlas = reference_profile(&full, wavelength, dispersion).unwrap();
            let depth = atlas.line_depth.unwrap();
            assert!((0.7..0.85).contains(&depth.reference));
            assert!((0.47..0.52).contains(&depth.ratio()));
            let clipped = profile(dispersion, -14..=4);
            assert!(
                reference_profile(&clipped, wavelength, dispersion)
                    .unwrap()
                    .line_depth
                    .is_none()
            );
        }
    }

    #[test]
    fn rejects_a_monotonic_reference_even_with_a_valid_observed_line() {
        let profile = LineProfile::new(
            (-15..=15)
                .map(|x| {
                    let x = f64::from(x);
                    [x, 1000. - 500. * (-(x / 3.).powi(2)).exp()]
                })
                .collect(),
        );
        assert!(profile.fit.is_some());
        let reference: Vec<_> = profile
            .samples
            .iter()
            .map(|&[x, _]| [x, 100. + x])
            .collect();
        assert!(compare_line_depth(&profile, &reference).is_none());
    }
}

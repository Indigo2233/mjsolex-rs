use crate::regression::weighted_fit;

#[derive(Clone, Debug)]
pub struct LineProfile {
    /// Mean intensity at each pixel offset relative to the selected trace.
    pub samples: Vec<[f64; 2]>,
    pub fit: Option<LineFit>,
}

#[derive(Clone, Copy, Debug)]
pub struct LineFit {
    pub center: f64,
    pub fwhm: f64,
    pub rmse: f64,
    continuum: f64,
    slope: f64,
    depth: f64,
    mix: f64,
}

impl LineFit {
    pub fn intensity_at(self, offset: f64) -> f64 {
        self.continuum_at(offset) - self.depth * shape(offset, self.center, self.fwhm, self.mix)
    }

    pub fn continuum_at(self, offset: f64) -> f64 {
        self.continuum + self.slope * offset
    }

    pub fn half_depth_at(self, offset: f64) -> f64 {
        self.continuum_at(offset) - self.depth / 2.
    }
}

impl LineProfile {
    pub(crate) fn new(samples: Vec<[f64; 2]>) -> Self {
        let fit = fit(&samples);
        Self { samples, fit }
    }
}

fn shape(offset: f64, center: f64, width: f64, mix: f64) -> f64 {
    let r2 = ((offset - center) / width).powi(2);
    mix / (1. + 4. * r2) + (1. - mix) * (-4. * std::f64::consts::LN_2 * r2).exp()
}

// A pseudo-Voigt with a common FWHM for both components and a linear continuum.
fn fit(samples: &[[f64; 2]]) -> Option<LineFit> {
    if samples.len() < 9 {
        return None;
    }
    let edge = (samples.len() / 6).clamp(1, 4);
    let mean = |samples: &[[f64; 2]]| {
        let sum = samples
            .iter()
            .fold([0.; 2], |sum, p| [sum[0] + p[0], sum[1] + p[1]]);
        sum.map(|n| n / samples.len() as f64)
    };
    let [x0, y0] = mean(&samples[..edge]);
    let [x1, y1] = mean(&samples[samples.len() - edge..]);
    let continuum = |x: f64| y0 + (y1 - y0) * (x - x0) / (x1 - x0);
    let depth_at = |p: [f64; 2]| continuum(p[0]) - p[1];
    let (middle, deepest) = samples
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, p)| p[0].abs() <= 2.)
        .max_by(|(_, a), (_, b)| depth_at(*a).total_cmp(&depth_at(*b)))?;
    let depth = depth_at(deepest);
    if depth < (0.005 * continuum(deepest[0])).max(1.) {
        return None;
    }
    let half = depth / 2.;
    let crossing = |pair: &[[f64; 2]]| {
        let [a, b] = [pair[0], pair[1]];
        let (da, db) = (depth_at(a), depth_at(b));
        (da != db && (da - half) * (db - half) <= 0.)
            .then(|| a[0] + (b[0] - a[0]) * (half - da) / (db - da))
    };
    let left = samples[..=middle].windows(2).rev().find_map(crossing)?;
    let right = samples[middle..].windows(2).find_map(crossing)?;
    let measured = right - left;
    let minimum = samples.first()?[0];
    let maximum = samples.last()?[0];
    if measured < 1. || left - minimum < 1. || maximum - right < 1. {
        return None;
    }
    let radius = (2. * measured).max(4.);
    let model: Vec<_> = samples
        .iter()
        .copied()
        .filter(|p| (p[0] - deepest[0]).abs() <= radius)
        .collect();
    let scale = radius.max(1.);
    let candidate = |center: f64, width: f64, mix: f64| {
        if (center - deepest[0]).abs() > 2.
            || !(1. ..=1.8 * measured).contains(&width)
            || center - width / 2. <= minimum
            || center + width / 2. >= maximum
            || !(0. ..=1.).contains(&mix)
        {
            return None;
        }
        let [continuum, slope, depth] = weighted_fit(
            model
                .iter()
                .map(|&[x, y]| ([1., x / scale, -shape(x, center, width, mix)], y, 1.)),
        )?;
        if depth <= 0. || continuum <= 0. {
            return None;
        }
        let mut fit = LineFit {
            center,
            fwhm: width,
            rmse: 0.,
            continuum,
            slope: slope / scale,
            depth,
            mix,
        };
        fit.rmse = (model
            .iter()
            .map(|&[x, y]| (fit.intensity_at(x) - y).powi(2))
            .sum::<f64>()
            / model.len() as f64)
            .sqrt();
        Some(fit)
    };

    let mut best = candidate(deepest[0], measured, 0.5)?;
    let mut steps = [0.5, measured * 0.25, 0.5];
    for _ in 0..10 {
        let previous = best.rmse;
        for (axis, step) in steps.into_iter().enumerate() {
            let center = [best.center, best.fwhm, best.mix];
            for direction in [-1., 1.] {
                let mut next = center;
                next[axis] += step * direction;
                if let Some(result) = candidate(next[0], next[1], next[2])
                    && result.rmse < best.rmse
                {
                    best = result;
                }
            }
        }
        if best.rmse >= previous {
            steps.iter_mut().for_each(|step| *step *= 0.5);
        }
    }
    (best.rmse <= 0.18 * best.depth).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_observed_profile_without_claiming_a_width_for_clipped_wings() {
        for mix in [0., 0.35, 1.] {
            let data: Vec<_> = (-20..=20)
                .map(|x| {
                    let x = f64::from(x);
                    [x, 1000. + 1.5 * x - 430. * shape(x, 0.6, 4.7, mix)]
                })
                .collect();
            let fit = fit(&data).unwrap();
            assert!((fit.center - 0.6).abs() < 0.13, "{fit:?}");
            assert!((fit.fwhm - 4.7).abs() < 0.23, "{fit:?}");
            assert!(
                (fit.intensity_at(fit.center + fit.fwhm / 2.)
                    - fit.half_depth_at(fit.center + fit.fwhm / 2.))
                .abs()
                    < 1e-9
            );
        }
        let clipped: Vec<_> = (-10..=0)
            .map(|x| {
                [
                    f64::from(x),
                    1000. - 500. * shape(f64::from(x), 0., 4., 0.4),
                ]
            })
            .collect();
        assert!(fit(&clipped).is_none());
    }
}

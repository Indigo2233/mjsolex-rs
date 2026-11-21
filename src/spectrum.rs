use std::io::{Read, Seek};

use crate::regression::weighted_fit;
use crate::{Error, Header, SerReader};

const AVERAGE_FRAMES: u32 = 64;
const TRACK_RADIUS: usize = 4;
const CONTINUUM_RADIUS: usize = 8;

pub struct Spectrum {
    header: Header,
    pixels: Vec<f32>,
    illuminated: (usize, usize),
}

#[derive(Clone, Copy, Debug)]
pub struct Trace {
    coefficients: [f64; 3],
    width: usize,
}

impl Trace {
    pub fn row(self, column: usize) -> f64 {
        let x = normalized(column, self.width);
        let [constant, slope, curvature] = self.coefficients;
        (curvature * x + slope) * x + constant
    }

    pub fn constant(width: usize, row: f64) -> Self {
        Self {
            coefficients: [row, 0., 0.],
            width,
        }
    }

    pub(crate) fn width(self) -> usize {
        self.width
    }
}

fn normalized(column: usize, width: usize) -> f64 {
    2. * column as f64 / width.saturating_sub(1).max(1) as f64 - 1.
}

struct Sample {
    x: usize,
    y: f64,
    depth: f64,
}

impl Spectrum {
    pub fn average<R: Read + Seek>(
        reader: &mut SerReader<R>,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        let header = reader.header();
        let mut pixels = vec![0.; header.pixel_count()];
        let count = header.frames.min(AVERAGE_FRAMES);
        for i in 0..count {
            if !progress(i as f32 / count as f32) {
                return Err(Error::Cancelled);
            }
            let index = (u64::from(i) * u64::from(header.frames - 1)
                / u64::from((count - 1).max(1))) as u32;
            let frame = reader.frame(index)?;
            header.accumulate(frame, &mut pixels);
        }
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        pixels.iter_mut().for_each(|pixel| *pixel /= count as f32);
        let mut columns = vec![0.; header.width];
        for row in pixels.chunks_exact(header.width) {
            for (sum, value) in columns.iter_mut().zip(row) {
                *sum += value;
            }
        }
        let peak = columns.iter().copied().fold(0., f32::max);
        let threshold = peak * 0.15;
        let left = columns.iter().position(|&x| x >= threshold).unwrap_or(0);
        let right = columns
            .iter()
            .rposition(|&x| x >= threshold)
            .unwrap_or(header.width - 1);
        Ok(Self {
            header,
            pixels,
            illuminated: (left, right),
        })
    }

    pub fn pixels(&self) -> &[f32] {
        &self.pixels
    }

    pub fn fit_line(&self, seed: Option<(usize, usize)>) -> Result<Trace, Error> {
        let (left, right) = self.illuminated;
        let (width, height) = (self.header.width, self.header.height);
        if let Some((x, y)) = seed
            && (x >= width || y >= height)
        {
            return Err(Error::InvalidTrace);
        }
        if height == 1 {
            return Ok(Trace::constant(width, 0.));
        }
        if height < 3 {
            return seed
                .map(|(_, y)| Trace::constant(width, y as f64))
                .ok_or(Error::NoLine);
        }

        let center = (left + right) / 2;
        let initial = if let Some((x, y)) = seed {
            self.nearby_minimum(x.clamp(left, right), y as f64)
        } else {
            (1..height - 1)
                .filter_map(|y| self.local_minimum(center, y))
                .max_by(|a, b| a.depth.total_cmp(&b.depth))
        };
        let Some(initial) = initial else {
            return seed
                .map(|(_, y)| Trace::constant(width, y as f64))
                .ok_or(Error::NoLine);
        };
        if self.header.trimmed {
            return Ok(Trace::constant(width, initial.y.round()));
        }
        let (anchor, y) = (initial.x, initial.y);
        let mut samples = vec![initial];
        let mut track = |columns: &mut dyn Iterator<Item = usize>| {
            let mut last = y;
            for x in columns {
                if let Some(sample) = self.nearby_minimum(x, last) {
                    last = sample.y;
                    samples.push(sample);
                }
            }
        };
        track(&mut (left..anchor).rev());
        track(&mut (anchor + 1..=right));
        let fallback = || seed.map(|_| Trace::constant(width, y)).ok_or(Error::NoLine);
        if samples.len() < 6 || samples.len() * 4 < right - left + 1 {
            return fallback();
        }
        let Some(first) = fit(&samples, width) else {
            return fallback();
        };
        let mut errors: Vec<_> = samples
            .iter()
            .map(|p| (p.y - first.row(p.x)).abs())
            .collect();
        let mid = errors.len() / 2;
        let threshold = (*errors.select_nth_unstable_by(mid, f64::total_cmp).1 * 3.).max(0.65);
        samples.retain(|p| (p.y - first.row(p.x)).abs() <= threshold);
        Ok(fit(&samples, width).unwrap_or(first))
    }

    fn nearby_minimum(&self, x: usize, row: f64) -> Option<Sample> {
        let center = row.round() as usize;
        let lo = center.saturating_sub(TRACK_RADIUS).max(1);
        let hi = (center + TRACK_RADIUS).min(self.header.height - 2);
        let score = |p: &Sample| p.depth / (1. + (row - p.y).abs());
        (lo..=hi)
            .filter_map(|y| self.local_minimum(x, y))
            .max_by(|a, b| score(a).total_cmp(&score(b)))
    }

    fn local_minimum(&self, x: usize, y: usize) -> Option<Sample> {
        let value = |row| f64::from(self.pixels[row * self.header.width + x]);
        let before = value(y - 1);
        let center = value(y);
        let after = value(y + 1);
        if center > before || center > after || (center == before && center == after) {
            return None;
        }
        let lower = (y.saturating_sub(CONTINUUM_RADIUS)..y)
            .map(value)
            .fold(0., f64::max);
        let upper = (y + 1..=(y + CONTINUUM_RADIUS).min(self.header.height - 1))
            .map(value)
            .fold(0., f64::max);
        let continuum = lower.min(upper);
        let depth = continuum - center;
        if depth < (continuum * 0.015).max(1.) {
            return None;
        }
        let delta = (before - after) / (2. * (before + after - 2. * center));
        Some(Sample {
            x,
            y: y as f64 + delta,
            depth,
        })
    }
}

fn fit(samples: &[Sample], width: usize) -> Option<Trace> {
    let coefficients = weighted_fit(samples.iter().map(|sample| {
        let x = normalized(sample.x, width);
        ([1., x, x * x], sample.y, sample.depth)
    }))?;
    Some(Trace {
        coefficients,
        width,
    })
}

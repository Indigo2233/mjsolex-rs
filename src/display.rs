use std::io::Write;

use crate::{Error, LocalContrast, Reconstruction};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Product {
    #[default]
    Gray,
    Hydrogen,
    Calcium,
    Helium,
    Doppler,
}

impl Product {
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Gray => "",
            Self::Hydrogen => "-halpha-color",
            Self::Calcium => "-calcium-color",
            Self::Helium => "-helium-color",
            Self::Doppler => "-doppler",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Stretch {
    low: u32,
    high: u32,
    peak: u32,
}

impl Stretch {
    /// Estimate the display range on a bounded grid. Use the same range for preview and export.
    pub fn estimate(width: usize, height: usize, value: impl Fn(usize) -> u16) -> Self {
        let dx = width.div_ceil(1200).max(1);
        let dy = height.div_ceil(1200).max(1);
        let samples = || {
            (0..height)
                .step_by(dy)
                .flat_map(move |y| (0..width).step_by(dx).map(move |x| y * width + x))
                .map(&value)
        };
        let (min, max) = samples().fold((u16::MAX, 0), |(lo, hi), v| (lo.min(v), hi.max(v)));
        let span = u32::from(max.saturating_sub(min).max(1));
        let mut histogram = [0; 256];
        for v in samples() {
            histogram[(u32::from(v - min) * 255 / span) as usize] += 1;
        }
        let size = width.div_ceil(dx) * height.div_ceil(dy);
        let percentile = |rank| {
            let mut count = 0;
            let bin = histogram.iter().position(|n| {
                count += n;
                count > rank
            });
            bin.unwrap_or(255) as u32 * span / 255 + u32::from(min)
        };
        let low = percentile(size / 100);
        let high = percentile(size * 99 / 100).max(low + (span / 128).max(1));
        Self {
            low,
            high,
            peak: high,
        }
    }

    pub(crate) fn estimate_with_highlights(
        width: usize,
        height: usize,
        value: impl Fn(usize) -> u16,
    ) -> Self {
        let mut stretch = Self::estimate(width, height, &value);
        if let Some(peak) = (0..width * height).map(value).max() {
            stretch.peak = stretch.high.max(u32::from(peak));
        }
        stretch
    }

    pub fn map(self, sample: u16) -> u8 {
        let sample = u32::from(sample);
        let delta = sample.saturating_sub(self.low);
        if self.peak == self.high {
            return (255 * delta / (self.high - self.low)).min(255) as u8;
        }
        // Set aside the top display levels for the tail above the 99th percentile.
        const KNEE: u32 = 224;
        if sample <= self.high {
            (KNEE * delta / (self.high - self.low)) as u8
        } else {
            (KNEE + (255 - KNEE) * (sample.min(self.peak) - self.high) / (self.peak - self.high))
                as u8
        }
    }
}

pub struct DisplayImage<'a> {
    red: &'a Reconstruction,
    blue: Option<&'a Reconstruction>,
    product: Product,
    stretch: Stretch,
    local: Option<&'a LocalContrast>,
    palette: [[u8; 3]; 256],
    settings: DisplaySettings,
    levels: [u8; 256],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayTone {
    Darker,
    #[default]
    Standard,
    Brighter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayInversion {
    #[default]
    Off,
    Linear,
    Curve,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplaySettings {
    pub tone: DisplayTone,
    pub inversion: DisplayInversion,
}

impl<'a> DisplayImage<'a> {
    pub fn new(
        image: &'a Reconstruction,
        blue: Option<&'a Reconstruction>,
        product: Product,
    ) -> Result<Self, Error> {
        Self::with_map(image, blue, product, None)
    }

    pub fn enhanced(
        image: &'a Reconstruction,
        product: Product,
        local: &'a LocalContrast,
    ) -> Result<Self, Error> {
        Self::with_map(image, None, product, Some(local))
    }

    fn with_map(
        image: &'a Reconstruction,
        blue: Option<&'a Reconstruction>,
        product: Product,
        local: Option<&'a LocalContrast>,
    ) -> Result<Self, Error> {
        if (product == Product::Doppler) != blue.is_some()
            || blue.is_some_and(|other| {
                (other.width(), other.height()) != (image.width(), image.height())
            })
            || local.is_some_and(|map| map.size != [image.width(), image.height()])
        {
            return Err(Error::InvalidTrace);
        }
        let stretch = local.map_or_else(
            || {
                Stretch::estimate_with_highlights(image.width(), image.height(), |i| {
                    let red = image.pixels()[i];
                    blue.map_or(red, |wing| red.midpoint(wing.pixels()[i]))
                })
            },
            |map| map.stretch,
        );
        let palette = std::array::from_fn(|gray| {
            let level = gray as f32 / 255.;
            let gray = gray as u8;
            let channel = |exponent, scale| (255. * level.powf(exponent) * scale) as u8;
            match product {
                Product::Gray | Product::Doppler => [gray; 3],
                Product::Hydrogen => [gray, channel(1.5, 0.83), channel(2.7, 0.54)],
                Product::Calcium => [channel(2.1, 0.84), channel(1.65, 0.77), gray],
                Product::Helium => [gray, channel(1.25, 0.96), channel(2.1, 0.62)],
            }
        });
        Ok(Self {
            red: image,
            blue,
            product,
            stretch,
            local,
            palette,
            settings: DisplaySettings::default(),
            levels: std::array::from_fn(|value| value as u8),
        })
    }

    pub fn with_tone(self, tone: DisplayTone) -> Self {
        let settings = DisplaySettings {
            tone,
            ..self.settings
        };
        self.with_settings(settings)
    }

    pub fn with_settings(mut self, settings: DisplaySettings) -> Self {
        self.settings = settings;
        let exponent = match settings.tone {
            DisplayTone::Darker => 1.25,
            DisplayTone::Standard => 1.,
            DisplayTone::Brighter => 0.8,
        };
        let inversion = if self.product == Product::Doppler {
            DisplayInversion::Off
        } else {
            settings.inversion
        };
        self.levels = std::array::from_fn(|value| {
            let x = value as f32 / 255.;
            let level = match inversion {
                DisplayInversion::Off => x,
                DisplayInversion::Linear => 1. - x,
                // The complement of the Beta(2, 2) CDF is monotone, with its
                // steepest slope at the midtone and zero slope at both ends.
                DisplayInversion::Curve => 1. - x * x * (3. - 2. * x),
            };
            (255. * level.powf(exponent)).round() as u8
        });
        self
    }

    pub fn pixel(&self, index: usize) -> [u8; 3] {
        let red = self.red.pixels()[index];
        if let Some(wing) = self.blue {
            let blue = wing.pixels()[index];
            if red == 0 || blue == 0 {
                return [0; 3];
            }
            // A redshift deepens the red-wing absorption, giving I_blue > I_red.
            let sum = f32::from(red) + f32::from(blue);
            let signal = (4. * (f32::from(blue) - f32::from(red)) / sum).clamp(-1., 1.);
            let level = f32::from(self.levels[self.stretch.map(red.midpoint(blue)) as usize]);
            return [
                (level * (1. + signal).clamp(0.2, 1.5)).min(255.) as u8,
                (level * (1. - 0.55 * signal.abs())) as u8,
                (level * (1. - signal).clamp(0.2, 1.5)).min(255.) as u8,
            ];
        }
        let gray = self
            .local
            .map_or_else(|| self.stretch.map(red), |map| map.map(index, red));
        self.palette[self.levels[gray as usize] as usize]
    }

    pub fn write_png(&self, output: impl Write) -> Result<(), Error> {
        let (width, height) = (self.red.width(), self.red.height());
        let mut encoder = png::Encoder::new(output, width as u32, height as u32);
        let gray = self.product == Product::Gray;
        encoder.set_color(if gray {
            png::ColorType::Grayscale
        } else {
            png::ColorType::Rgb
        });
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut stream = writer.stream_writer()?;
        let mut row = Vec::with_capacity(width * 3);
        for y in 0..height {
            row.clear();
            for x in 0..width {
                let color = self.pixel(y * width + x);
                if gray {
                    row.push(color[0]);
                } else {
                    row.extend(color);
                }
            }
            stream.write_all(&row)?;
        }
        stream.finish()?;
        writer.finish()?;
        Ok(())
    }
}

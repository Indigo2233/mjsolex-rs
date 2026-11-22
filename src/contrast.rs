use crate::{Error, Reconstruction, Stretch};

/// A bounded CLAHE lookup table prepared from the reconstructed strength image.
/// The map is shared by all enhanced products and leaves the linear data intact.
pub struct LocalContrast {
    pub(crate) stretch: Stretch,
    pub(crate) size: [usize; 2],
    tile: usize,
    tiles: [usize; 2],
    maps: Vec<[u8; 256]>,
}

impl LocalContrast {
    pub fn prepare(
        image: &Reconstruction,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<Self, Error> {
        if !progress(0.) {
            return Err(Error::Cancelled);
        }
        let (width, height) = (image.width(), image.height());
        let tile = width.max(height).div_ceil(24).max(64);
        let [cols, rows] = [width.div_ceil(tile), height.div_ceil(tile)];
        let stretch = Stretch::estimate(width, height, |i| image.pixels()[i]);
        let mut maps = Vec::new();
        maps.try_reserve_exact(cols * rows)?;
        for ty in 0..rows {
            for tx in 0..cols {
                let mut histogram = [0; 256];
                let x = tx * tile..((tx + 1) * tile).min(width);
                let y = ty * tile..((ty + 1) * tile).min(height);
                let area = x.len() * y.len();
                for iy in y {
                    if !progress((ty * cols + tx) as f32 / (cols * rows) as f32) {
                        return Err(Error::Cancelled);
                    }
                    let start = iy * width;
                    for &pixel in &image.pixels()[start + x.start..start + x.end] {
                        let level = stretch.map(pixel);
                        if level > 3 {
                            histogram[level as usize] += 1;
                        }
                    }
                }
                maps.push(clipped_map(histogram, area));
            }
        }
        if !progress(1.) {
            return Err(Error::Cancelled);
        }
        Ok(Self {
            stretch,
            size: [width, height],
            tile,
            tiles: [cols, rows],
            maps,
        })
    }

    pub(crate) fn map(&self, index: usize, value: u16) -> u8 {
        let gray = self.stretch.map(value);
        if gray <= 3 {
            return gray;
        }
        let x = index % self.size[0];
        let y = index / self.size[0];
        let axis = |point: usize, limit: usize| {
            let coordinate =
                ((point as f32 + 0.5) / self.tile as f32 - 0.5).clamp(0., (limit - 1) as f32);
            let lower = coordinate as usize;
            (lower, (lower + 1).min(limit - 1), coordinate.fract())
        };
        let [cols, rows] = self.tiles;
        let (left, right, dx) = axis(x, cols);
        let (top, bottom, dy) = axis(y, rows);
        let lerp = |a: f32, b: f32, t| a + (b - a) * t;
        let lookup = |tx: usize, ty: usize| f32::from(self.maps[ty * cols + tx][gray as usize]);
        let local = lerp(
            lerp(lookup(left, top), lookup(right, top), dx),
            lerp(lookup(left, bottom), lookup(right, bottom), dx),
            dy,
        );
        // A gradual transition leaves the sky black and reduces halos at the limb.
        let strength = 0.58 * (f32::from(gray - 3) / 32.).min(1.);
        lerp(f32::from(gray), local, strength).round() as u8
    }
}

fn clipped_map(mut histogram: [usize; 256], area: usize) -> [u8; 256] {
    let count = histogram.iter().sum::<usize>();
    if count < 32.max(area / 4) || histogram.iter().filter(|&&n| n != 0).count() < 3 {
        return std::array::from_fn(|level| level as u8);
    }
    let limit = (3 * count / 256).max(1);
    let mut excess = 0;
    for value in &mut histogram {
        excess += value.saturating_sub(limit);
        *value = (*value).min(limit);
    }
    let mut running = 0;
    std::array::from_fn(|level| {
        running += histogram[level];
        let cdf = running + (level + 1) * excess / 256;
        (255 * cdf / count) as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayImage, Product};

    #[test]
    fn enhances_local_contrast_without_mutating_strength_or_black_sky() {
        let (width, height) = (192, 128);
        let image = Reconstruction {
            width,
            height,
            pixels: (0..width * height)
                .map(|i| match i % width {
                    0..=23 => 0,
                    24..=79 => 50_000,
                    x => 24_000 + ((x % 16) * 180) as u16,
                })
                .collect(),
            synthetic_flat: false,
        };
        let unchanged = image.pixels.clone();
        let base = DisplayImage::new(&image, None, Product::Gray).unwrap();
        let contrast = LocalContrast::prepare(&image, |_| true).unwrap();
        let enhanced = DisplayImage::enhanced(&image, Product::Gray, &contrast).unwrap();
        let color = DisplayImage::enhanced(&image, Product::Hydrogen, &contrast).unwrap();
        let row = 75 * width;
        let spread = |view: &DisplayImage<'_>| view.pixel(row + 143)[0] - view.pixel(row + 128)[0];
        assert!(spread(&enhanced) > spread(&base));
        assert_eq!(enhanced.pixel(row), [0; 3]);
        assert_eq!(enhanced.pixel(row + 143)[0], color.pixel(row + 143)[0]);
        assert_eq!(image.pixels(), unchanged);
        assert!(matches!(
            LocalContrast::prepare(&image, |p| p < 0.25),
            Err(Error::Cancelled)
        ));
    }
}

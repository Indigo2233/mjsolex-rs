use std::io::Write;
use std::path::Path;

use crate::{
    DisplayImage, DisplaySettings, Error, FitsMetadata, LocalContrast, Product, Reconstruction,
};

pub const PALETTES: [Product; 3] = [Product::Hydrogen, Product::Calcium, Product::Helium];

pub struct Images {
    pub(crate) center: Reconstruction,
    pub(crate) negative: Option<Reconstruction>,
    pub(crate) eclipse: Option<Reconstruction>,
    pub(crate) wings: Option<[Reconstruction; 2]>,
    pub(crate) palettes: [bool; 3],
    pub(crate) wing_offset: f64,
    pub(crate) metadata: FitsMetadata,
    pub(crate) enhancement: Option<LocalContrast>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct View {
    pub product: Product,
    pub enhanced: bool,
}

impl View {
    pub const GRAY: Self = Self {
        product: Product::Gray,
        enhanced: false,
    };
}

#[derive(Clone, Copy)]
pub enum Export {
    Display(View, DisplaySettings),
    RawPng,
    Fits,
}

impl Images {
    pub fn center(&self) -> &Reconstruction {
        &self.center
    }

    pub fn metadata(&self) -> &FitsMetadata {
        &self.metadata
    }

    pub fn views(&self) -> impl Iterator<Item = View> + '_ {
        let intensity = std::iter::once(Product::Gray).chain(
            PALETTES
                .into_iter()
                .zip(self.palettes)
                .filter_map(|(product, selected)| selected.then_some(product)),
        );
        [
            (Product::Negative, &self.negative),
            (Product::Eclipse, &self.eclipse),
        ]
        .into_iter()
        .filter_map(|(product, image)| {
            image.as_ref().map(|_| View {
                product,
                enhanced: false,
            })
        })
        .chain(intensity.clone().map(|product| View {
            product,
            enhanced: false,
        }))
        .chain(
            intensity
                .filter(|_| self.enhancement.is_some())
                .map(|product| View {
                    product,
                    enhanced: true,
                }),
        )
        .chain(self.wings.as_ref().map(|_| View {
            product: Product::Doppler,
            enhanced: false,
        }))
    }

    pub fn exports(&self, settings: DisplaySettings) -> impl Iterator<Item = Export> + '_ {
        self.views()
            .map(move |view| Export::Display(view, settings))
            .chain([Export::RawPng, Export::Fits])
    }

    pub(crate) fn display(
        &self,
        view: View,
        settings: DisplaySettings,
    ) -> Result<DisplayImage<'_>, Error> {
        let image = if view.enhanced {
            let enhancement = self.enhancement.as_ref().ok_or(Error::InvalidTrace)?;
            DisplayImage::enhanced(&self.center, view.product, enhancement)
        } else if view.product == Product::Negative {
            let negative = self.negative.as_ref().ok_or(Error::InvalidTrace)?;
            DisplayImage::new(negative, None, Product::Negative)
        } else if view.product == Product::Eclipse {
            let eclipse = self.eclipse.as_ref().ok_or(Error::InvalidTrace)?;
            DisplayImage::new(eclipse, None, Product::Eclipse)
        } else if view.product == Product::Doppler {
            let [red, blue] = self.wings.as_ref().ok_or(Error::DopplerCoverage)?;
            DisplayImage::new(red, Some(blue), view.product)
        } else {
            DisplayImage::new(&self.center, None, view.product)
        }?;
        Ok(image.with_settings(settings))
    }

    pub fn write(&self, export: Export, output: impl Write) -> Result<(), Error> {
        match export {
            Export::Display(view, settings) => self.display(view, settings)?.write_png(output),
            Export::RawPng => self.center.write_png(output),
            Export::Fits => self
                .center
                .write_fits_with_metadata(output, &self.metadata)
                .map_err(Into::into),
        }
    }

    pub fn file_name(&self, export: Export) -> String {
        let stem = self
            .metadata
            .source_name
            .as_deref()
            .and_then(|name| Path::new(name).file_stem())
            .and_then(|name| name.to_str())
            .unwrap_or("scan");
        let synthetic = !matches!(
            export,
            Export::Display(
                View {
                    product: Product::Doppler,
                    ..
                },
                _
            )
        ) && self.center.has_synthetic_flat();
        let flat = if synthetic || self.metadata.flat_file.is_some() {
            "-flat"
        } else {
            ""
        };
        let disk = if self.metadata.geometry.is_some() {
            "-disk"
        } else {
            ""
        };
        let shift = self
            .metadata
            .offset_pixels
            .filter(|shift| *shift != 0.)
            .map(|shift| format!("-{shift:+.2}px"))
            .unwrap_or_default();
        let suffix = match export {
            Export::Display(
                View {
                    product: Product::Doppler,
                    ..
                },
                _,
            ) => format!("-doppler-w{:.2}px.png", self.wing_offset),
            Export::Display(
                View {
                    product: product @ (Product::Negative | Product::Eclipse),
                    enhanced: false,
                },
                _,
            ) => format!("{}.png", product.suffix()),
            Export::Display(view, _) => {
                let kind = if view.enhanced { "enhanced" } else { "display" };
                format!("{}-{kind}.png", view.product.suffix())
            }
            Export::RawPng => "-raw.png".into(),
            Export::Fits => ".fits".into(),
        };
        format!("{stem}-reconstruction{shift}{flat}{disk}{suffix}")
    }

    pub(super) fn count(&self) -> usize {
        crate::reconstruct::transform_image_count(self.wings.is_some())
    }

    /// Commit matching geometry to the center and both wings only after all
    /// resampling succeeds.
    pub(super) fn transform(
        &mut self,
        mut transform: impl FnMut(&Reconstruction, usize) -> Result<Reconstruction, Error>,
    ) -> Result<(), Error> {
        let center = transform(&self.center, 0)?;
        let wings = self
            .wings
            .as_ref()
            .map(|[red, blue]| Ok::<_, Error>([transform(red, 1)?, transform(blue, 2)?]))
            .transpose()?;
        self.center = center;
        self.wings = wings;
        Ok(())
    }
}

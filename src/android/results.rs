use super::*;

pub(super) const PALETTES: [Product; 3] = [Product::Hydrogen, Product::Calcium, Product::Helium];

pub(super) struct Images {
    pub(super) center: Reconstruction,
    pub(super) wings: Option<[Reconstruction; 2]>,
    pub(super) palettes: [bool; 3],
    pub(super) wing_offset: f64,
    pub(super) metadata: FitsMetadata,
}

#[derive(Clone, Copy)]
pub(super) enum Export {
    Display(Product),
    RawPng,
    Fits,
}

impl Export {
    pub(super) fn mime(self) -> &'static str {
        match self {
            Self::Fits => "application/fits",
            _ => "image/png",
        }
    }
}

impl Images {
    pub(super) fn products(&self) -> impl Iterator<Item = Product> + '_ {
        std::iter::once(Product::Gray)
            .chain(
                PALETTES
                    .into_iter()
                    .zip(self.palettes)
                    .filter_map(|(product, selected)| selected.then_some(product)),
            )
            .chain(self.wings.as_ref().map(|_| Product::Doppler))
    }

    pub(super) fn exports(&self) -> impl Iterator<Item = Export> + '_ {
        self.products()
            .map(Export::Display)
            .chain([Export::RawPng, Export::Fits])
    }

    fn display(&self, product: Product) -> Result<DisplayImage<'_>, Error> {
        if product == Product::Doppler {
            let [red, blue] = self.wings.as_ref().ok_or(Error::DopplerCoverage)?;
            DisplayImage::new(red, Some(blue), product)
        } else {
            DisplayImage::new(&self.center, None, product)
        }
    }

    pub(super) fn preview(&self, product: Product) -> Result<ColorImage, Error> {
        let display = self.display(product)?;
        Ok(preview_rgb(
            self.center.width(),
            self.center.height(),
            |i| display.pixel(i),
        ))
    }

    pub(super) fn write(&self, export: Export, output: impl Write) -> Result<(), Error> {
        match export {
            Export::Display(product) => self.display(product)?.write_png(output),
            Export::RawPng => self.center.write_png(output),
            Export::Fits => self
                .center
                .write_fits_with_metadata(output, &self.metadata)
                .map_err(Into::into),
        }
    }

    pub(super) fn file_name(&self, export: Export) -> String {
        let stem = self
            .metadata
            .source_name
            .as_deref()
            .and_then(|name| Path::new(name).file_stem())
            .and_then(|name| name.to_str())
            .unwrap_or("scan");
        let synthetic = !matches!(export, Export::Display(Product::Doppler))
            && self.center.has_synthetic_flat();
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
            Export::Display(Product::Doppler) => format!("-doppler-w{:.2}px.png", self.wing_offset),
            Export::Display(product) => format!("{}-display.png", product.suffix()),
            Export::RawPng => "-raw.png".into(),
            Export::Fits => ".fits".into(),
        };
        format!("{stem}-reconstruction{shift}{flat}{disk}{suffix}")
    }
}

pub(super) fn write_destination(
    mut file: DocumentFile,
    images: &Images,
    export: Export,
) -> Result<(), Error> {
    let result = {
        let mut writer = BufWriter::new(&mut file.file);
        images
            .write(export, &mut writer)
            .and_then(|()| writer.flush().map_err(Into::into))
    };
    let closed = file.finish_export(result.is_err());
    result?;
    closed?;
    Ok(())
}

pub(super) fn write_in_directory(
    env: &mut JNIEnv<'_>,
    activity: &JObject<'_>,
    directory: &JObject<'_>,
    images: &Images,
    export: Export,
) -> Result<(), Error> {
    let result = env.with_local_frame(10, |env| {
        let name = env.new_string(images.file_name(export))?;
        let mime = env.new_string(export.mime())?;
        let uri = env
            .call_method(
                activity,
                "createExportFile",
                "(Landroid/net/Uri;Ljava/lang/String;Ljava/lang/String;)Landroid/net/Uri;",
                &[
                    JValue::Object(directory),
                    JValue::Object(&mime),
                    JValue::Object(&name),
                ],
            )?
            .l()?;
        if uri.is_null() {
            return Ok(Err(io::Error::other(
                "The selected folder cannot create a file",
            )
            .into()));
        }
        let written = (|| -> Result<(), Error> {
            let descriptor = env
                .call_method(
                    activity,
                    "openExportFile",
                    "(Landroid/net/Uri;)Landroid/os/ParcelFileDescriptor;",
                    &[JValue::Object(&uri)],
                )
                .and_then(|value| value.l())
                .map_err(io::Error::other)?;
            if descriptor.is_null() {
                return Err(io::Error::other("The provider could not open the export file").into());
            }
            let file = match DocumentFile::from_android(env, &descriptor, true) {
                Ok(file) => file,
                Err(error) => {
                    clear_exception(env);
                    let _ = env.call_method(&descriptor, "close", "()V", &[]);
                    return Err(error.into());
                }
            };
            write_destination(file, images, export)
        })();
        if written.is_err() {
            clear_exception(env);
            let _ = env.call_method(
                activity,
                "deleteExportFile",
                "(Landroid/net/Uri;)V",
                &[JValue::Object(&uri)],
            );
            clear_exception(env);
        }
        Ok::<_, jni::errors::Error>(written)
    });
    clear_exception(env);
    result.map_err(io::Error::other)?
}

fn clear_exception(env: &JNIEnv<'_>) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

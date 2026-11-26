use super::*;

impl Export {
    pub(super) fn mime(self) -> &'static str {
        match self {
            Self::Fits => "application/fits",
            _ => "image/png",
        }
    }
}

impl Images {
    pub(super) fn preview(
        &self,
        view: View,
        settings: DisplaySettings,
    ) -> Result<ColorImage, Error> {
        let display = self.display(view, settings)?;
        Ok(preview_rgb(
            self.center.width(),
            self.center.height(),
            |i| display.pixel(i),
        ))
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
    name: &str,
) -> Result<String, Error> {
    let result = env.with_local_frame(10, |env| {
        let requested = env.new_string(name)?;
        let mime = env.new_string(export.mime())?;
        let uri = env
            .call_method(
                activity,
                "createExportFile",
                "(Landroid/net/Uri;Ljava/lang/String;Ljava/lang/String;)Landroid/net/Uri;",
                &[
                    JValue::Object(directory),
                    JValue::Object(&mime),
                    JValue::Object(&requested),
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
        if let Err(error) = written {
            clear_exception(env);
            let _ = env.call_method(
                activity,
                "deleteExportFile",
                "(Landroid/net/Uri;)V",
                &[JValue::Object(&uri)],
            );
            clear_exception(env);
            return Ok(Err(error));
        }
        let actual = (|| {
            let actual = env
                .call_method(
                    activity,
                    "exportFileName",
                    "(Landroid/net/Uri;Ljava/lang/String;)Ljava/lang/String;",
                    &[JValue::Object(&uri), JValue::Object(&requested)],
                )?
                .l()?;
            Ok::<String, jni::errors::Error>(env.get_string(&JString::from(actual))?.into())
        })();
        Ok::<_, jni::errors::Error>(Ok(actual.unwrap_or_else(|_| name.to_owned())))
    });
    clear_exception(env);
    result.map_err(io::Error::other)?
}

pub(super) fn clear_exception(env: &JNIEnv<'_>) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

use std::os::unix::fs::MetadataExt;

use super::*;

pub(super) struct Selection {
    pub(super) files: GlobalRef,
    pub(super) count: i32,
    pub(super) format: Format,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Format {
    #[default]
    All,
    Display,
    Raw,
    Fits,
}

impl Format {
    fn includes(self, export: Export) -> bool {
        matches!(
            (self, export),
            (Self::All, _)
                | (Self::Display, Export::Display(..))
                | (Self::Raw, Export::RawPng)
                | (Self::Fits, Export::Fits)
        )
    }
}

pub(super) enum Issue {
    Processing(Error),
    Export(String, Error),
    Corrections(Box<CorrectionWarnings>),
}

#[derive(Default)]
pub(super) struct Summary {
    pub(super) total: usize,
    pub(super) completed: usize,
    pub(super) failed: usize,
    pub(super) saved: usize,
    pub(super) cancelled: bool,
    pub(super) issues: Vec<(String, Issue)>,
}

struct Reference {
    header: Header,
    trace: Trace,
    slit: (usize, usize),
    byte_order: Option<ReferenceByteOrder>,
}

struct ReferenceByteOrder {
    value: ByteOrder,
    source: Option<String>,
    file_id: Option<(u64, u64)>,
}

fn file_id(file: &File) -> Option<(u64, u64)> {
    let metadata = file.metadata().ok()?;
    metadata.is_file().then(|| (metadata.dev(), metadata.ino()))
}

impl Mobile {
    pub(super) fn reconstruct_sequence(&mut self, selection: Selection, directory: GlobalRef) {
        let Some(options) = self.reconstruction_options() else {
            return;
        };
        let (Some(scan), Some(trace)) = (&self.scan, self.trace) else {
            return;
        };
        let reference = Reference {
            header: scan.header,
            trace,
            slit: scan.spectrum.slit().1,
            byte_order: scan.byte_order.map(|value| ReferenceByteOrder {
                value,
                source: scan.source.clone(),
                file_id: file_id(&scan.file.file),
            }),
        };
        let vm = self.vm.clone();
        let activity = self.activity.clone();
        let cache = self.cache.clone();
        self.start_job(
            ("Reconstructing SER sequence", "正在重建 SER 序列"),
            true,
            move |events, cancel| {
                let mut env = vm.attach_current_thread().map_err(io::Error::other)?;
                let mut progress = report(events, cancel);
                let files: &JObjectArray<'_> = selection.files.as_obj().into();
                let total = selection.count as usize;
                let mut summary = Summary {
                    total,
                    ..Summary::default()
                };
                let mut names = Vec::new();
                for index in 0..selection.count {
                    if !progress(0.) {
                        summary.cancelled = true;
                        return Ok(Work::SequenceFinished(summary));
                    }
                    let name = env.with_local_frame(4, |env| {
                        let uri = env.get_object_array_element(files, index)?;
                        let name = env
                            .call_method(
                                activity.as_obj(),
                                "sequenceFileName",
                                "(Landroid/net/Uri;)Ljava/lang/String;",
                                &[JValue::Object(&uri)],
                            )?
                            .l()?;
                        let name = env.get_string(&JString::from(name))?.into();
                        Ok::<String, jni::errors::Error>(name)
                    });
                    results::clear_exception(&env);
                    names.push((index, name.unwrap_or_else(|_| "scan.ser".into())));
                }
                names.sort_by(|a, b| a.1.cmp(&b.1));
                let digits = total.to_string().len().max(3);
                for (position, (index, name)) in names.into_iter().enumerate() {
                    let mut scan_progress = |p| progress((position as f32 + p) / total as f32);
                    if !scan_progress(0.) {
                        summary.cancelled = true;
                        break;
                    }
                    let number = position + 1;
                    let label = format!("{number:0digits$} · {name}");
                    events.post(Event::SequenceFile(number, total, name.clone()));
                    let images = open_scan(
                        &mut env,
                        activity.as_obj(),
                        files,
                        index,
                        reference.byte_order.as_ref(),
                    )
                    .and_then(|(file, order)| {
                        let file = seekable_document(file, &cache, cancel)?;
                        reconstruct_scan(&file, name, order, &reference, &options, |p| {
                            scan_progress(p * 0.85)
                        })
                    });
                    let (images, warnings) = match images {
                        Ok(images) => images,
                        Err(Error::Cancelled) => {
                            summary.cancelled = true;
                            break;
                        }
                        Err(error) => {
                            summary.failed += 1;
                            summary.issues.push((label, Issue::Processing(error)));
                            continue;
                        }
                    };
                    if warnings.has_errors() {
                        summary
                            .issues
                            .push((label.clone(), Issue::Corrections(Box::new(warnings))));
                    }
                    let exports = || {
                        images
                            .exports(DisplayTone::Standard)
                            .filter(|&export| selection.format.includes(export))
                    };
                    let count = exports().count();
                    let mut failed = false;
                    for (written, export) in exports().enumerate() {
                        if !scan_progress(0.85 + 0.15 * written as f32 / count as f32) {
                            summary.cancelled = true;
                            break;
                        }
                        let name = format!("{number:0digits$}-{}", images.file_name(export));
                        match results::write_in_directory(
                            &mut env,
                            activity.as_obj(),
                            directory.as_obj(),
                            &images,
                            export,
                            &name,
                        ) {
                            Ok(()) => summary.saved += 1,
                            Err(error) => {
                                failed = true;
                                summary
                                    .issues
                                    .push((label.clone(), Issue::Export(name, error)));
                            }
                        }
                    }
                    if summary.cancelled {
                        break;
                    }
                    if failed {
                        summary.failed += 1;
                    } else {
                        summary.completed += 1;
                    }
                    scan_progress(1.);
                }
                Ok(Work::SequenceFinished(summary))
            },
        );
    }
}

fn open_scan(
    env: &mut JNIEnv<'_>,
    activity: &JObject<'_>,
    files: &JObjectArray<'_>,
    index: i32,
    reference: Option<&ReferenceByteOrder>,
) -> Result<(DocumentFile, Option<ByteOrder>), Error> {
    let result = env.with_local_frame(6, |env| {
        let uri = env.get_object_array_element(files, index)?;
        let same_source = if let Some(source) = reference.and_then(|r| r.source.as_ref()) {
            let uri_text = env
                .call_method(&uri, "toString", "()Ljava/lang/String;", &[])?
                .l()?;
            let actual: String = env.get_string(&JString::from(uri_text))?.into();
            actual == *source
        } else {
            false
        };
        let descriptor = env
            .call_method(
                activity,
                "openSequenceFile",
                "(Landroid/net/Uri;)Landroid/os/ParcelFileDescriptor;",
                &[JValue::Object(&uri)],
            )?
            .l()?;
        if descriptor.is_null() {
            return Ok(Err(io::Error::other("The provider could not open the SER")));
        }
        let file = DocumentFile::from_android(env, &descriptor, false).map(|file| {
            let order = reference
                .filter(|r| {
                    same_source || r.file_id.is_some_and(|id| Some(id) == file_id(&file.file))
                })
                .map(|r| r.value);
            (file, order)
        });
        if file.is_err() {
            results::clear_exception(env);
            let _ = env.call_method(&descriptor, "close", "()V", &[]);
        }
        Ok::<_, jni::errors::Error>(file)
    });
    results::clear_exception(env);
    Ok(result.map_err(io::Error::other)??)
}

fn reconstruct_scan(
    file: &DocumentFile,
    name: String,
    order: Option<ByteOrder>,
    reference: &Reference,
    options: &ReconstructionOptions,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<(Images, CorrectionWarnings), Error> {
    let mut reader = SerReader::open_with_byte_order(&file.file, order)?;
    let layout = |h: Header| (h.width, h.height, h.color, h.depth, h.trimmed);
    if layout(reader.header()) != layout(reference.header) {
        return Err(Error::SequenceMismatch);
    }
    let spectrum = Spectrum::average(&mut reader, |p| progress(0.15 * p))?;
    let (_, (left, right)) = spectrum.slit();
    if options.physical_flat.is_some() && (left < reference.slit.0 || right > reference.slit.1) {
        return Err(Error::FlatCoverage);
    }
    let trace = spectrum.track_line(reference.trace)?;
    let wings = options.wing_shift(&spectrum, trace);
    let fwhm = spectrum.line_profile(trace).fit.map(|fit| fit.fwhm);
    drop(spectrum);
    let result = options.build(reader, name, trace, fwhm, wings, |p| {
        progress(0.15 + 0.85 * p)
    })?;
    file.check_error()?;
    Ok(result)
}

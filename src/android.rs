use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use eframe::egui::{
    self, Align, Color32, ColorImage, Context, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean};
use jni::{JNIEnv, JavaVM};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use winit::platform::android::activity::AndroidApp;

use crate::android_document::DocumentFile;
use crate::ser::cache_recording;
use crate::{
    APP_NAME, ByteOrder, DiskGeometry, DisplayImage, Error, FitsMetadata, Header, LineProfile,
    Observation, PhysicalFlat, Product, Reconstruction, SerReader, Spectrum, Stretch, Trace,
    reconstruct_with_stride,
};

mod language;
mod ui;

use language::{Language, Notice};

const OPEN_SER: i32 = 10;
const SAVE_PNG: i32 = 11;
const SAVE_FITS: i32 = 12;
const OPEN_SHARED: i32 = 13;
const OPEN_FLAT: i32 = 14;
const SAVE_RAW_PNG: i32 = 15;
const BACKGROUND: Color32 = Color32::from_rgb(17, 21, 27);
const CARD: Color32 = Color32::from_rgb(28, 35, 44);
const TEXT: Color32 = Color32::from_rgb(240, 243, 247);
const MUTED: Color32 = Color32::from_rgb(171, 185, 199);
const AMBER: Color32 = Color32::from_rgb(255, 190, 99);

type Document = Option<(DocumentFile, String)>;

static PICKER: Mutex<Option<(GlobalRef, Events)>> = Mutex::new(None);

#[derive(Clone)]
struct Events {
    sender: mpsc::Sender<Event>,
    context: Context,
}

impl Events {
    fn post(&self, event: Event) {
        if self.sender.send(event).is_ok() {
            self.context.request_repaint();
        }
    }
}

enum Event {
    Document(i32, Result<Document, String>),
    SharedAvailable,
    Progress(f32),
    Finished(Result<Work, Error>),
}

enum Work {
    Imported(Box<Imported>),
    FlatImported(PhysicalFlat, String),
    Reconstructed {
        image: Arc<Reconstruction>,
        blue: Option<Arc<Reconstruction>>,
        product: Product,
        wing_offset: f64,
        preview: ColorImage,
        metadata: Box<FitsMetadata>,
        warning: Option<Notice>,
    },
    Exported(String),
}

struct Imported {
    file: DocumentFile,
    name: String,
    header: Header,
    observation: Observation,
    byte_order: Option<ByteOrder>,
    spectrum: Spectrum,
    preview: ColorImage,
    trace: Result<Trace, Error>,
}

struct Scan {
    file: DocumentFile,
    name: String,
    header: Header,
    observation: Observation,
    byte_order: Option<ByteOrder>,
    spectrum: Arc<Spectrum>,
    texture: TextureHandle,
}

struct Output {
    image: Arc<Reconstruction>,
    blue: Option<Arc<Reconstruction>>,
    product: Product,
    wing_offset: f64,
    texture: TextureHandle,
    metadata: FitsMetadata,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FlatMode {
    #[default]
    Off,
    Synthetic,
    Physical,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Scan,
    Spectrum,
    Result,
}

struct Job {
    label: (&'static str, &'static str),
    cancel: Arc<AtomicBool>,
    progress: Option<f32>,
    cancellable: bool,
}

struct Mobile {
    vm: JavaVM,
    activity: GlobalRef,
    cache: PathBuf,
    events: Events,
    receiver: mpsc::Receiver<Event>,
    picker: Option<i32>,
    shared_available: bool,
    job: Option<Job>,
    scan: Option<Scan>,
    trace: Option<Trace>,
    profile: Option<LineProfile>,
    shift: f64,
    wavelength: f64,
    dispersion: f64,
    stride: u32,
    correct_disk: bool,
    flat_mode: FlatMode,
    physical_flat: Option<(Arc<PhysicalFlat>, String)>,
    product: Product,
    wing_offset: f64,
    output: Option<Output>,
    page: Page,
    language: Language,
    note: Option<Notice>,
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    // SAFETY: android-activity exposes this process's live JavaVM pointer.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.and_then(|vm| {
        let mut env = vm.attach_current_thread()?;
        let result = env.with_local_frame(8, |env| {
            let activity = own_activity(env, &app)?;
            let cache = env
                .call_method(activity.as_obj(), "getCacheDir", "()Ljava/io/File;", &[])?
                .l()?;
            let cache = env
                .call_method(cache, "getAbsolutePath", "()Ljava/lang/String;", &[])?
                .l()?;
            let cache: String = env.get_string(&JString::from(cache))?.into();
            let language = env
                .call_method(
                    activity.as_obj(),
                    "getUiLanguage",
                    "()Ljava/lang/String;",
                    &[],
                )?
                .l()?;
            let language: String = env.get_string(&JString::from(language))?.into();
            Ok::<_, jni::errors::Error>((
                activity,
                PathBuf::from(cache),
                Language::from_code(&language),
            ))
        });
        if matches!(&result, Err(jni::errors::Error::JavaException)) {
            let _ = env.exception_clear();
        }
        drop(env);
        result.map(|(activity, cache, language)| (vm, activity, cache, language))
    });
    let (vm, activity, cache, language) = match result {
        Ok(initialized) => initialized,
        Err(error) => {
            eprintln!("{APP_NAME} could not initialize the Android activity: {error}");
            return;
        }
    };
    let options = eframe::NativeOptions {
        android_app: Some(app),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            Ok(Box::new(Mobile::new(
                vm,
                activity,
                cache,
                language,
                &cc.egui_ctx,
            )))
        }),
    ) {
        eprintln!("{APP_NAME} failed to start: {error}");
    }
}

fn own_activity(env: &JNIEnv<'_>, app: &AndroidApp) -> jni::errors::Result<GlobalRef> {
    let raw = env.get_raw();
    // SAFETY: env is valid on this thread. android-activity retains this unowned
    // global reference while app is alive. NewLocalRef returns a distinct local
    // reference in our JNI frame, which JObject::from_raw can wrap.
    let local = unsafe {
        let new_local_ref = (**raw)
            .NewLocalRef
            .ok_or(jni::errors::Error::JNIEnvMethodNotFound("NewLocalRef"))?;
        JObject::from_raw(new_local_ref(raw, app.activity_as_ptr().cast()))
    };
    if local.is_null() {
        if env.exception_check()? {
            return Err(jni::errors::Error::JavaException);
        }
        return Err(jni::errors::Error::NullPtr("activity local reference"));
    }
    env.new_global_ref(local)
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_mjsolex_mobile_MainActivity_onDocument(
    mut env: JNIEnv<'_>,
    activity: JObject<'_>,
    request: i32,
    descriptor: JObject<'_>,
    name: JString<'_>,
    error: JString<'_>,
) -> jboolean {
    let Some(events) = activity_events(&env, &activity) else {
        return JNI_FALSE;
    };
    let document: jni::errors::Result<Result<Document, String>> = if !error.is_null() {
        env.get_string(&error).map(|message| Err(message.into()))
    } else if descriptor.is_null() {
        Ok(Ok(None))
    } else {
        let name = if name.is_null() {
            Ok("scan".into())
        } else {
            env.get_string(&name).map(Into::into)
        };
        name.map(|name| {
            DocumentFile::from_android(
                &mut env,
                descriptor,
                !matches!(request, OPEN_SER | OPEN_SHARED | OPEN_FLAT),
            )
            .map(|file| Some((file, name)))
            .map_err(|error| format!("access:{error}"))
        })
    };
    let document = document.unwrap_or_else(|error| Err(format!("access:{error}")));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    let adopted = matches!(document, Ok(Some(_)));
    events.post(Event::Document(request, document));
    if adopted { JNI_TRUE } else { JNI_FALSE }
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_mjsolex_mobile_MainActivity_onShareAvailable(
    env: JNIEnv<'_>,
    activity: JObject<'_>,
) {
    if let Some(events) = activity_events(&env, &activity) {
        events.post(Event::SharedAvailable);
    }
}

fn activity_events(env: &JNIEnv<'_>, activity: &JObject<'_>) -> Option<Events> {
    let (owner, events) = PICKER.lock().ok().and_then(|route| route.clone())?;
    matches!(env.is_same_object(activity, owner.as_obj()), Ok(true)).then_some(events)
}

impl Mobile {
    fn new(
        vm: JavaVM,
        activity: GlobalRef,
        cache: PathBuf,
        language: Language,
        context: &Context,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let events = Events {
            sender,
            context: context.clone(),
        };
        if let Ok(mut route) = PICKER.lock() {
            *route = Some((activity.clone(), events.clone()));
        }
        context.style_mut_of(egui::Theme::Dark, |style| {
            style.visuals.panel_fill = BACKGROUND;
            style.visuals.override_text_color = Some(TEXT);
            style.visuals.selection.bg_fill = Color32::from_rgb(65, 77, 91);
            style.spacing.item_spacing = vec2(8., 8.);
            style.spacing.interact_size.y = 42.;
        });
        context.set_theme(egui::Theme::Dark);
        language::install_fonts(context);
        Self {
            vm,
            activity,
            cache,
            events,
            receiver,
            picker: None,
            shared_available: true,
            job: None,
            scan: None,
            trace: None,
            profile: None,
            shift: 0.,
            wavelength: 0.,
            dispersion: 0.,
            stride: 1,
            correct_disk: false,
            flat_mode: FlatMode::default(),
            physical_flat: None,
            product: Product::default(),
            wing_offset: 2.,
            output: None,
            page: Page::Scan,
            language,
            note: None,
        }
    }

    fn switch_language(&mut self) {
        let language = self.language.other();
        let result = self.vm.attach_current_thread().and_then(|mut env| {
            let result = env.with_local_frame(4, |env| {
                let code = env.new_string(language.code())?;
                env.call_method(
                    self.activity.as_obj(),
                    "setUiLanguage",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&code)],
                )?;
                Ok::<_, jni::errors::Error>(())
            });
            if matches!(&result, Err(jni::errors::Error::JavaException)) {
                let _ = env.exception_clear();
            }
            result
        });
        match result {
            Ok(()) => self.language = language,
            Err(error) => self.note = Some(Notice::SaveLanguage(error.to_string())),
        }
    }

    fn pick(&mut self, request: i32) {
        let output = self.output.as_ref();
        if !matches!(request, OPEN_SER | OPEN_FLAT) && output.is_none() {
            return;
        }
        let result = self.vm.attach_current_thread().and_then(|mut env| {
            let result = env.with_local_frame(8, |env| {
                let activity = self.activity.as_obj();
                if matches!(request, OPEN_SER | OPEN_FLAT) {
                    env.call_method(activity, "pickSer", "(I)V", &[JValue::Int(request)])?;
                } else {
                    let name = self
                        .scan
                        .as_ref()
                        .map(|scan| scan.name.as_str())
                        .unwrap_or("scan");
                    let stem = Path::new(name)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("scan");
                    let ext = if request == SAVE_FITS { "fits" } else { "png" };
                    let Some(output) = &self.output else {
                        return Ok(());
                    };
                    let details = &output.metadata;
                    let flat = if output.image.has_synthetic_flat() || details.flat_file.is_some() {
                        "-flat"
                    } else {
                        ""
                    };
                    let geometry = if details.geometry.is_some() {
                        "-disk"
                    } else {
                        ""
                    };
                    let shift = details
                        .offset_pixels
                        .filter(|shift| *shift != 0.)
                        .map(|shift| format!("-{shift:+.2}px"))
                        .unwrap_or_default();
                    let product = match request {
                        SAVE_PNG if output.product == Product::Doppler => {
                            format!("-doppler-w{:.2}px", output.wing_offset)
                        }
                        SAVE_PNG => format!("{}-display", output.product.suffix()),
                        SAVE_RAW_PNG => "-raw".into(),
                        _ => String::new(),
                    };
                    let name = env.new_string(format!(
                        "{stem}-reconstruction{shift}{flat}{geometry}{product}.{ext}"
                    ))?;
                    env.call_method(
                        activity,
                        "createImage",
                        "(ILjava/lang/String;)V",
                        &[JValue::Int(request), JValue::Object(&name)],
                    )?;
                }
                Ok::<_, jni::errors::Error>(())
            });
            if matches!(&result, Err(jni::errors::Error::JavaException)) {
                let _ = env.exception_clear();
            }
            result
        });
        match result {
            Ok(()) => {
                self.picker = Some(request);
                self.note = None;
            }
            Err(err) => {
                self.note = Some(Notice::FilePicker(err.to_string()));
            }
        }
    }

    fn activity_flag(&self, method: &str) -> jni::errors::Result<bool> {
        let mut env = self.vm.attach_current_thread()?;
        let result: jni::errors::Result<bool> = env.with_local_frame(4, |env| {
            env.call_method(self.activity.as_obj(), method, "()Z", &[])?
                .z()
        });
        if matches!(&result, Err(jni::errors::Error::JavaException)) {
            let _ = env.exception_clear();
        }
        result
    }

    fn busy(&self) -> bool {
        self.picker.is_some() || self.job.is_some()
    }

    fn read_shared(&mut self) {
        if self.busy() || !std::mem::take(&mut self.shared_available) {
            return;
        }
        match self.activity_flag("openSharedSer") {
            Ok(true) => {
                self.picker = Some(OPEN_SHARED);
                self.page = Page::Scan;
                self.note = None;
            }
            Ok(false) => {}
            Err(error) => self.note = Some(Notice::SharedRecording(error.to_string())),
        }
    }

    fn start_job(
        &mut self,
        label: (&'static str, &'static str),
        cancellable: bool,
        run: impl FnOnce(&Events, &AtomicBool) -> Result<Work, Error> + Send + 'static,
    ) {
        let events = self.events.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let result = std::thread::Builder::new()
            .name("solex-worker".into())
            .spawn(move || {
                let result = run(&events, &flag);
                events.post(Event::Finished(result));
            });
        match result {
            Ok(_) => {
                self.job = Some(Job {
                    label,
                    cancel,
                    progress: None,
                    cancellable,
                });
                self.note = None;
            }
            Err(error) => self.note = Some(Notice::Processing(error.into())),
        }
    }

    fn import(&mut self, file: DocumentFile, name: String, byte_order: Option<ByteOrder>) {
        let cache = self.cache.clone();
        self.start_job(
            ("Reading spectrum", "正在读取光谱"),
            true,
            move |events, cancel| {
                let file = seekable_document(file, &cache, cancel)?;
                let mut reader = SerReader::open_with_byte_order(&file.file, byte_order)?;
                let header = reader.header();
                let observation = reader.observation().clone();
                let spectrum = Spectrum::average(&mut reader, report(events, cancel))?;
                let trace = spectrum.fit_line(None);
                let preview = preview(header.width, header.height, |i| spectrum.pixels()[i] as u16);
                drop(reader);
                Ok(Work::Imported(Box::new(Imported {
                    file,
                    name,
                    header,
                    observation,
                    byte_order,
                    spectrum,
                    preview,
                    trace,
                })))
            },
        );
    }

    fn import_flat(&mut self, file: DocumentFile, name: String) {
        let Some(scan) = &self.scan else { return };
        let cache = self.cache.clone();
        let spectrum = scan.spectrum.clone();
        self.start_job(
            ("Reading flat SER", "正在读取平场 SER"),
            true,
            move |events, cancel| {
                let file = seekable_document(file, &cache, cancel)?;
                let mut reader = SerReader::open(&file.file)?;
                let flat = PhysicalFlat::from_ser(&mut reader, &spectrum, report(events, cancel))?;
                file.check_error()?;
                Ok(Work::FlatImported(flat, name))
            },
        );
    }

    fn minimum_stride(&self, header: Header) -> u32 {
        if self.product == Product::Doppler {
            header.minimum_doppler_stride()
        } else {
            header.minimum_stride()
        }
    }

    fn reload_byte_order(&mut self, order: Option<ByteOrder>) {
        let Some(scan) = &self.scan else { return };
        if order == scan.byte_order {
            return;
        }
        match scan.file.try_clone() {
            Ok(file) => self.import(file, scan.name.clone(), order),
            Err(error) => self.note = Some(Notice::Processing(error.into())),
        }
    }

    fn reconstruct(&mut self) {
        let (Some(scan), Some(trace)) = (&self.scan, self.trace) else {
            return;
        };
        let product = self.product;
        let wing_offset = self.wing_offset;
        let shift = self.shift;
        let wings = if product == Product::Doppler {
            if self.dispersion == 0. || !self.dispersion.is_finite() {
                self.note = Some(Notice::Processing(Error::DopplerDispersion));
                return;
            }
            if !scan.spectrum.supports_wings(trace, shift, wing_offset) {
                self.note = Some(Notice::Processing(Error::DopplerCoverage));
                return;
            }
            Some(wing_offset.copysign(self.dispersion))
        } else {
            None
        };
        let file = match scan.file.try_clone() {
            Ok(file) => file,
            Err(error) => {
                self.note = Some(Notice::Processing(error.into()));
                return;
            }
        };
        let order = scan.byte_order;
        let header = scan.header;
        let correct_disk = self.correct_disk;
        let synthetic_flat = self.flat_mode == FlatMode::Synthetic && wings.is_none();
        let physical_flat = if self.flat_mode == FlatMode::Physical {
            self.physical_flat.clone()
        } else {
            None
        };
        let stride = self.stride.max(self.minimum_stride(header));
        let mut metadata = FitsMetadata {
            observation: scan.observation.clone(),
            source_name: Some(scan.name.clone()),
            source_header: Some(header),
            offset_pixels: Some(shift),
            frame_step: Some(stride),
            line_center_angstrom: (self.wavelength > 0.).then_some(self.wavelength),
            dispersion_angstrom: (self.dispersion != 0.).then_some(self.dispersion),
            fwhm_pixels: self.profile.as_ref().and_then(|p| p.fit).map(|f| f.fwhm),
            geometry: None,
            flat_file: None,
        };
        self.start_job(
            ("Reconstructing scan", "正在重建图像"),
            true,
            move |events, cancel| {
                let mut reader = SerReader::open_with_byte_order(&file.file, order)?;
                if reader.header() != header {
                    return Err(Error::InvalidSer(
                        "recording changed; open the finished file again",
                    ));
                }
                metadata.observation = reader.observation().clone();
                let mut progress = report(events, cancel);
                let geometry_start = if correct_disk { 0.85 } else { 0.98 };
                let flat_start = if synthetic_flat || physical_flat.is_some() {
                    geometry_start - 0.10
                } else {
                    geometry_start
                };
                let mut raw = reconstruct_with_stride(
                    &mut reader,
                    trace,
                    shift + wings.unwrap_or(0.),
                    stride,
                    |p| progress(p * flat_start * if wings.is_some() { 0.5 } else { 1. }),
                )?;
                let mut blue = if let Some(wing) = wings {
                    Some(reconstruct_with_stride(
                        &mut reader,
                        trace,
                        shift - wing,
                        stride,
                        |p| progress((0.5 + p * 0.5) * flat_start),
                    )?)
                } else {
                    None
                };
                drop(reader);
                let mut flat_error = None;
                if synthetic_flat {
                    match raw.correct_synthetic_flat(|p| {
                        progress(flat_start + p * (geometry_start - flat_start))
                    }) {
                        Ok(_) => {}
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(error) => flat_error = Some(error),
                    }
                } else if let Some((flat, name)) = physical_flat {
                    flat.apply(&mut raw)?;
                    if let Some(blue) = &mut blue {
                        flat.apply(blue)?;
                    }
                    metadata.flat_file = Some(name);
                }
                if !progress(geometry_start) {
                    return Err(Error::Cancelled);
                }
                let mut disk_error = None;
                if correct_disk {
                    let corrected = (|| {
                        let geometry = DiskGeometry::fit(&raw)?;
                        let span = if blue.is_some() { 0.065 } else { 0.13 };
                        let red =
                            geometry.correct(&raw, |p| progress(geometry_start + p * span))?;
                        let blue = blue
                            .as_ref()
                            .map(|wing| {
                                geometry
                                    .correct(wing, |p| progress(geometry_start + (1. + p) * span))
                            })
                            .transpose()?;
                        Ok::<_, Error>((geometry, red, blue))
                    })();
                    match corrected {
                        Ok((geometry, corrected_red, corrected_blue)) => {
                            metadata.geometry = Some(geometry);
                            raw = corrected_red;
                            blue = corrected_blue;
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(error) => disk_error = Some(error),
                    }
                }
                let display = DisplayImage::new(&raw, blue.as_ref(), product)?;
                let preview = preview_rgb(raw.width(), raw.height(), |i| display.pixel(i));
                if !progress(1.) {
                    return Err(Error::Cancelled);
                }
                let warning =
                    (flat_error.is_some() || disk_error.is_some()).then_some(Notice::Corrections {
                        flat: flat_error,
                        disk: disk_error,
                    });
                Ok(Work::Reconstructed {
                    image: Arc::new(raw),
                    blue: blue.map(Arc::new),
                    product,
                    wing_offset,
                    preview,
                    metadata: Box::new(metadata),
                    warning,
                })
            },
        );
    }

    fn export(&mut self, request: i32, mut file: DocumentFile, name: String) {
        let Some(output) = &self.output else { return };
        let image = output.image.clone();
        let blue = output.blue.clone();
        let product = output.product;
        let metadata = output.metadata.clone();
        let fits = request == SAVE_FITS;
        self.start_job(
            if fits {
                ("Saving FITS", "正在保存 FITS")
            } else {
                ("Saving PNG", "正在保存 PNG")
            },
            false,
            move |_, _| {
                let result = (|| -> Result<(), Error> {
                    let mut writer = BufWriter::new(&mut file.file);
                    match request {
                        SAVE_FITS => image.write_fits_with_metadata(&mut writer, &metadata)?,
                        SAVE_RAW_PNG => image.write_png(&mut writer)?,
                        _ => DisplayImage::new(&image, blue.as_deref(), product)?
                            .write_png(&mut writer)?,
                    }
                    writer.flush()?;
                    Ok(())
                })();
                let closed = file.finish_export(result.is_err());
                result?;
                closed?;
                Ok(Work::Exported(name))
            },
        );
    }

    fn receive(&mut self, ctx: &Context) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::SharedAvailable => self.shared_available = true,
                Event::Document(request, result) if self.picker == Some(request) => {
                    self.picker = None;
                    match result {
                        Ok(Some((file, name))) if matches!(request, OPEN_SER | OPEN_SHARED) => {
                            self.import(file, name, None)
                        }
                        Ok(Some((file, name))) if request == OPEN_FLAT => {
                            self.import_flat(file, name)
                        }
                        Ok(Some((file, name))) => self.export(request, file, name),
                        Ok(None) => {}
                        Err(error) => self.note = Some(Notice::Document(error)),
                    }
                }
                Event::Progress(progress) => {
                    if let Some(job) = &mut self.job {
                        job.progress = Some(progress);
                    }
                }
                Event::Finished(result) => {
                    self.job = None;
                    match result {
                        Ok(Work::Imported(import)) => {
                            let Imported {
                                file,
                                name,
                                header,
                                observation,
                                byte_order,
                                spectrum,
                                preview,
                                trace,
                            } = *import;
                            self.scan = Some(Scan {
                                file,
                                name,
                                header,
                                observation,
                                byte_order,
                                spectrum: Arc::new(spectrum),
                                texture: ctx.load_texture(
                                    "spectrum",
                                    preview,
                                    TextureOptions::LINEAR,
                                ),
                            });
                            self.trace = None;
                            self.profile = None;
                            self.set_trace(trace);
                            self.shift = 0.;
                            self.wavelength = 0.;
                            self.dispersion = 0.;
                            self.stride = header.minimum_stride();
                            self.physical_flat = None;
                            if self.flat_mode == FlatMode::Physical {
                                self.flat_mode = FlatMode::Off;
                            }
                            self.output = None;
                            self.page = Page::Spectrum;
                        }
                        Ok(Work::Reconstructed {
                            image,
                            blue,
                            product,
                            wing_offset,
                            preview,
                            metadata,
                            warning,
                        }) => {
                            let texture =
                                ctx.load_texture("reconstruction", preview, TextureOptions::LINEAR);
                            self.output = Some(Output {
                                image,
                                blue,
                                product,
                                wing_offset,
                                texture,
                                metadata: *metadata,
                            });
                            self.page = Page::Result;
                            self.note = warning;
                        }
                        Ok(Work::FlatImported(flat, name)) => {
                            self.physical_flat = Some((Arc::new(flat), name));
                            self.flat_mode = FlatMode::Physical;
                        }
                        Ok(Work::Exported(name)) => self.note = Some(Notice::Saved(name)),
                        Err(error) => self.note = Some(Notice::Processing(error)),
                    }
                }
                Event::Document(..) => {}
            }
        }
        self.read_shared();
    }

    fn set_trace(&mut self, result: Result<Trace, Error>) {
        match result {
            Ok(trace) => {
                if self.trace.is_some_and(|previous| previous != trace) {
                    self.wavelength = 0.;
                }
                self.trace = Some(trace);
                self.profile = self
                    .scan
                    .as_ref()
                    .map(|scan| scan.spectrum.line_profile(trace));
                self.note = None;
            }
            Err(error) => self.note = Some(Notice::Processing(error)),
        }
    }
}

impl Drop for Mobile {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Ok(mut route) = PICKER.lock()
            && let Some((_, events)) = route.as_ref()
            && events.context == self.events.context
        {
            *route = None;
        }
    }
}

fn report<'a>(events: &'a Events, cancel: &'a AtomicBool) -> impl FnMut(f32) -> bool + 'a {
    let mut last = None;
    move |progress| {
        let percent = (progress * 100.) as u32;
        if last != Some(percent) {
            events.post(Event::Progress(progress));
            last = Some(percent);
        }
        !cancel.load(Ordering::Relaxed)
    }
}

fn cache_document(input: &mut File, cache: &Path, cancel: &AtomicBool) -> Result<File, Error> {
    let mut file = tempfile::tempfile_in(cache)?;
    let volume = rustix::fs::fstatvfs(&file).map_err(io::Error::from)?;
    // Keep space for processing output and other applications on the shared volume.
    let limit = volume.f_bavail.saturating_mul(volume.f_frsize) / 2;
    let result = cache_recording(PipeReader { input, cancel }, &mut file, limit);
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    result?;
    Ok(file)
}

fn seekable_document(
    mut file: DocumentFile,
    cache: &Path,
    cancel: &AtomicBool,
) -> Result<DocumentFile, Error> {
    match file.file.seek(SeekFrom::Start(0)) {
        Ok(_) => Ok(file),
        Err(error) if error.raw_os_error() == Some(Errno::SPIPE.raw_os_error()) => {
            let cached = cache_document(&mut file.file, cache, cancel)?;
            file.check_error()?;
            Ok(DocumentFile::cached(cached))
        }
        Err(error) => Err(error.into()),
    }
}

struct PipeReader<'a> {
    input: &'a mut File,
    cancel: &'a AtomicBool,
}

impl Read for PipeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        };
        while !self.cancel.load(Ordering::Relaxed) {
            let ready = poll(
                &mut [PollFd::new(&*self.input, PollFlags::IN)],
                Some(&timeout),
            );
            match ready {
                Ok(0) | Err(Errno::INTR) => continue,
                Ok(_) => return self.input.read(buffer),
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::other("processing cancelled"))
    }
}

fn preview(width: usize, height: usize, value: impl Fn(usize) -> u16) -> ColorImage {
    let stretch = Stretch::estimate(width, height, &value);
    preview_rgb(width, height, |i| [stretch.map(value(i)); 3])
}

fn preview_rgb(width: usize, height: usize, pixel: impl Fn(usize) -> [u8; 3]) -> ColorImage {
    let x_step = width.div_ceil(1200);
    let y_step = height.div_ceil(1200);
    let (w, h) = (width.div_ceil(x_step), height.div_ceil(y_step));
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = pixel(y * y_step * width + x * x_step);
            pixels.push(Color32::from_rgb(r, g, b));
        }
    }
    ColorImage::new([w, h], pixels)
}

fn card<T>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> T) -> T {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(14.)
        .inner_margin(14.)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            contents(ui)
        })
        .inner
}

fn action(ui: &mut Ui, text: &str, enabled: bool, primary: bool) -> bool {
    let label = RichText::new(text).size(15.).strong();
    let button = if primary {
        egui::Button::new(label.color(BACKGROUND)).fill(AMBER)
    } else {
        egui::Button::new(label)
    };
    ui.add_enabled(
        enabled,
        button.min_size(Vec2::new(ui.available_width(), 46.)),
    )
    .clicked()
}

fn picture(
    ui: &mut Ui,
    texture: &TextureHandle,
    width: usize,
    height: usize,
    height_range: std::ops::RangeInclusive<f32>,
    sense: Sense,
) -> egui::Response {
    let aspect = width as f32 / height as f32;
    let w = ui.available_width().min(*height_range.end() * aspect);
    let h = (w / aspect).clamp(*height_range.start(), *height_range.end());
    ui.with_layout(egui::Layout::top_down(Align::Center), |ui| {
        ui.add(
            egui::Image::from_texture(texture)
                .fit_to_exact_size(vec2(w, h))
                .maintain_aspect_ratio(false)
                .sense(sense),
        )
    })
    .inner
}

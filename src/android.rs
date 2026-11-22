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
use crate::reconstruct::{MAX_DOPPLER_WING_PIXELS, MAX_PIXELS};
use crate::ser::cache_recording;
use crate::{
    APP_NAME, ByteOrder, DiskGeometry, DisplayImage, Error, FitsMetadata, Header, LineProfile,
    Observation, OpticalSetup, PhysicalFlat, Product, Reconstruction, SerReader, Spectrum, Stretch,
    Trace, reconstruct_offsets, reconstruct_with_stride,
};

mod language;
mod results;
mod ui;

use language::{Language, Notice};
use results::{Export, Images, PALETTES};

const OPEN_SER: i32 = 10;
const SAVE_PNG: i32 = 11;
const SAVE_FITS: i32 = 12;
const OPEN_SHARED: i32 = 13;
const OPEN_FLAT: i32 = 14;
const SAVE_RAW_PNG: i32 = 15;
const SAVE_BATCH: i32 = 16;
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
    Directory(Result<Option<GlobalRef>, String>),
    SharedAvailable,
    Progress(f32),
    Finished(Result<Work, Error>),
}

enum Work {
    Imported(Box<Imported>),
    FlatImported(PhysicalFlat, String),
    Reconstructed {
        images: Arc<Images>,
        preview: ColorImage,
        warning: Option<Notice>,
    },
    Previewed(Product, ColorImage),
    Exported(String),
    BatchExported {
        saved: usize,
        error: Option<Error>,
    },
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
    images: Arc<Images>,
    shown: Product,
    texture: TextureHandle,
}

impl Output {
    fn export(&self, request: i32) -> Export {
        match request {
            SAVE_FITS => Export::Fits,
            SAVE_RAW_PNG => Export::RawPng,
            _ => Export::Display(self.shown),
        }
    }
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

#[derive(Default)]
struct Calibration {
    wavelength: f64,
    custom_wavelength: bool,
    manual_dispersion: f64,
    use_optics: bool,
    optics: OpticalSetup,
    increases_downward: Option<bool>,
}

impl Calibration {
    fn dispersion(&self) -> Option<f64> {
        if self.use_optics {
            let increasing = self.increases_downward?;
            let magnitude = self.optics.dispersion_angstrom(self.wavelength)?;
            return Some(if increasing { magnitude } else { -magnitude });
        }
        let value = self.manual_dispersion;
        (value.is_finite() && value != 0.).then_some(value)
    }

    fn clear_line(&mut self) {
        self.wavelength = 0.;
        self.custom_wavelength = false;
    }
}

struct Mobile {
    vm: Arc<JavaVM>,
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
    calibration: Calibration,
    stride: u32,
    correct_disk: bool,
    flat_mode: FlatMode,
    physical_flat: Option<(Arc<PhysicalFlat>, String)>,
    palettes: [bool; 3],
    doppler: bool,
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
                &descriptor,
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

#[unsafe(no_mangle)]
extern "system" fn Java_org_mjsolex_mobile_MainActivity_onExportDirectory(
    env: JNIEnv<'_>,
    activity: JObject<'_>,
    uri: JObject<'_>,
) {
    if let Some(events) = activity_events(&env, &activity) {
        let directory = if uri.is_null() {
            Ok(None)
        } else {
            env.new_global_ref(uri)
                .map(Some)
                .map_err(|e| format!("access:{e}"))
        };
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        events.post(Event::Directory(directory));
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
            vm: Arc::new(vm),
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
            calibration: Calibration::default(),
            stride: 1,
            correct_disk: false,
            flat_mode: FlatMode::default(),
            physical_flat: None,
            palettes: [true, false, false],
            doppler: false,
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
                } else if request == SAVE_BATCH {
                    env.call_method(activity, "pickExportDirectory", "()V", &[])?;
                } else if let Some(output) = output {
                    let name = env.new_string(output.images.file_name(output.export(request)))?;
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
        let byte_order = scan.byte_order;
        self.start_job(
            ("Reading flat SER", "正在读取平场 SER"),
            true,
            move |events, cancel| {
                let file = seekable_document(file, &cache, cancel)?;
                let mut reader = SerReader::open_with_byte_order(&file.file, byte_order)?;
                let flat = PhysicalFlat::from_ser(&mut reader, &spectrum, report(events, cancel))?;
                file.check_error()?;
                Ok(Work::FlatImported(flat, name))
            },
        );
    }

    fn minimum_stride(&self, header: Header) -> u32 {
        if self.doppler_shift().is_ok_and(|shift| shift.is_some()) {
            header.minimum_doppler_stride()
        } else {
            header.minimum_stride()
        }
    }

    fn doppler_shift(&self) -> Result<Option<f64>, Error> {
        if !self.doppler {
            return Ok(None);
        }
        let dispersion = self
            .calibration
            .dispersion()
            .ok_or(Error::DopplerDispersion)?;
        let (scan, trace) = self
            .scan
            .as_ref()
            .zip(self.trace)
            .ok_or(Error::DopplerCoverage)?;
        if !scan
            .spectrum
            .supports_wings(trace, self.shift, self.wing_offset)
        {
            return Err(Error::DopplerCoverage);
        }
        Ok(Some(self.wing_offset.copysign(dispersion)))
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
        let palettes = self.palettes;
        let wing_offset = self.wing_offset;
        let shift = self.shift;
        let dispersion = self.calibration.dispersion();
        let wavelength = self.calibration.wavelength;
        let (wing_shift, doppler_error) = match self.doppler_shift() {
            Ok(shift) => (shift, None),
            Err(error) => (None, Some(error)),
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
        let synthetic_flat = self.flat_mode == FlatMode::Synthetic;
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
            line_center_angstrom: (wavelength > 0.).then_some(wavelength),
            dispersion_angstrom: dispersion,
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
                let (mut raw, mut wings) = if let Some(wing) = wing_shift {
                    let [center, red, blue] = reconstruct_offsets(
                        &mut reader,
                        trace,
                        [shift, shift + wing, shift - wing],
                        stride,
                        |p| progress(p * flat_start),
                    )?;
                    (center, Some([red, blue]))
                } else {
                    let center = reconstruct_with_stride(&mut reader, trace, shift, stride, |p| {
                        progress(p * flat_start)
                    })?;
                    (center, None)
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
                    if let Some(wings) = &mut wings {
                        for wing in wings {
                            flat.apply(wing)?;
                        }
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
                        let span = if wings.is_some() { 0.13 / 3. } else { 0.13 };
                        let budget = if wings.is_some() {
                            MAX_DOPPLER_WING_PIXELS
                        } else {
                            MAX_PIXELS
                        };
                        let mut correct = |image, step| {
                            geometry.correct_with_budget(image, budget, |p| {
                                progress(geometry_start + (step + p) * span)
                            })
                        };
                        let center = correct(&raw, 0.)?;
                        let wings = wings
                            .as_ref()
                            .map(|[red, blue]| {
                                Ok::<_, Error>([correct(red, 1.)?, correct(blue, 2.)?])
                            })
                            .transpose()?;
                        Ok::<_, Error>((geometry, center, wings))
                    })();
                    match corrected {
                        Ok((geometry, corrected_center, corrected_wings)) => {
                            metadata.geometry = Some(geometry);
                            raw = corrected_center;
                            wings = corrected_wings;
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(error) => disk_error = Some(error),
                    }
                }
                let images = Images {
                    center: raw,
                    wings,
                    palettes,
                    wing_offset,
                    metadata,
                };
                let preview = images.preview(Product::Gray)?;
                if !progress(1.) {
                    return Err(Error::Cancelled);
                }
                let warning =
                    (flat_error.is_some() || disk_error.is_some() || doppler_error.is_some())
                        .then_some(Notice::Corrections {
                            flat: flat_error,
                            disk: disk_error,
                            doppler: doppler_error,
                        });
                Ok(Work::Reconstructed {
                    images: Arc::new(images),
                    preview,
                    warning,
                })
            },
        );
    }

    fn show_product(&mut self, product: Product) {
        let Some(output) = &self.output else { return };
        if output.shown == product || !output.images.products().any(|item| item == product) {
            return;
        }
        let images = output.images.clone();
        self.start_job(
            ("Preparing preview", "正在准备预览"),
            false,
            move |_, _| Ok(Work::Previewed(product, images.preview(product)?)),
        );
    }

    fn export(&mut self, request: i32, file: DocumentFile, name: String) {
        let Some(output) = &self.output else { return };
        let images = output.images.clone();
        let export = output.export(request);
        let fits = request == SAVE_FITS;
        self.start_job(
            if fits {
                ("Saving FITS", "正在保存 FITS")
            } else {
                ("Saving PNG", "正在保存 PNG")
            },
            false,
            move |_, _| {
                results::write_destination(file, &images, export)?;
                Ok(Work::Exported(name))
            },
        );
    }

    fn export_directory(&mut self, directory: GlobalRef) {
        let Some(output) = &self.output else { return };
        let images = output.images.clone();
        let vm = self.vm.clone();
        let activity = self.activity.clone();
        self.start_job(
            ("Saving images", "正在保存图像"),
            false,
            move |events, _| {
                let mut env = vm.attach_current_thread().map_err(io::Error::other)?;
                let total = images.exports().count();
                for (saved, export) in images.exports().enumerate() {
                    events.post(Event::Progress(saved as f32 / total as f32));
                    if let Err(error) = results::write_in_directory(
                        &mut env,
                        activity.as_obj(),
                        directory.as_obj(),
                        &images,
                        export,
                    ) {
                        return Ok(Work::BatchExported {
                            saved,
                            error: Some(error),
                        });
                    }
                }
                Ok(Work::BatchExported {
                    saved: total,
                    error: None,
                })
            },
        );
    }

    fn receive(&mut self, ctx: &Context) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::SharedAvailable => self.shared_available = true,
                Event::Directory(result) if self.picker == Some(SAVE_BATCH) => {
                    self.picker = None;
                    match result {
                        Ok(Some(directory)) => self.export_directory(directory),
                        Ok(None) => {}
                        Err(error) => self.note = Some(Notice::Document(error)),
                    }
                }
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
                            self.calibration.clear_line();
                            self.calibration.manual_dispersion = 0.;
                            self.stride = header.minimum_stride();
                            self.physical_flat = None;
                            if self.flat_mode == FlatMode::Physical {
                                self.flat_mode = FlatMode::Off;
                            }
                            self.output = None;
                            self.page = Page::Spectrum;
                        }
                        Ok(Work::Reconstructed {
                            images,
                            preview,
                            warning,
                        }) => {
                            let texture =
                                ctx.load_texture("reconstruction", preview, TextureOptions::LINEAR);
                            self.output = Some(Output {
                                images,
                                shown: Product::Gray,
                                texture,
                            });
                            self.page = Page::Result;
                            self.note = warning;
                        }
                        Ok(Work::FlatImported(flat, name)) => {
                            self.physical_flat = Some((Arc::new(flat), name));
                            self.flat_mode = FlatMode::Physical;
                        }
                        Ok(Work::Exported(name)) => self.note = Some(Notice::Saved(name)),
                        Ok(Work::Previewed(product, preview)) => {
                            if let Some(output) = &mut self.output {
                                output.shown = product;
                                output.texture.set(preview, TextureOptions::LINEAR);
                            }
                        }
                        Ok(Work::BatchExported { saved, error }) => {
                            self.note = Some(Notice::BatchSaved { saved, error });
                        }
                        Err(error) => self.note = Some(Notice::Processing(error)),
                    }
                }
                Event::Document(..) | Event::Directory(..) => {}
            }
        }
        self.read_shared();
    }

    fn set_trace(&mut self, result: Result<Trace, Error>) {
        match result {
            Ok(trace) => {
                if self.trace.is_some_and(|previous| previous != trace) {
                    self.calibration.clear_line();
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

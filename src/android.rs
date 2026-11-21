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
    ByteOrder, DiskGeometry, Error, FitsMetadata, Header, LineProfile, Observation, Reconstruction,
    SerReader, Spectrum, Trace, reconstruct_with_stride,
};

mod language;
mod ui;

use language::{Language, Notice};

const OPEN_SER: i32 = 10;
const SAVE_PNG: i32 = 11;
const SAVE_FITS: i32 = 12;
const OPEN_SHARED: i32 = 13;
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
    Reconstructed {
        image: Arc<Reconstruction>,
        preview: ColorImage,
        metadata: Box<FitsMetadata>,
        warning: Option<Error>,
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
    spectrum: Spectrum,
    texture: TextureHandle,
}

struct Output {
    image: Arc<Reconstruction>,
    texture: TextureHandle,
    metadata: FitsMetadata,
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
            eprintln!("MJ Sol'Ex could not initialize the Android activity: {error}");
            return;
        }
    };
    let options = eframe::NativeOptions {
        android_app: Some(app),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "MJ Sol'Ex",
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
        eprintln!("MJ Sol'Ex failed to start: {error}");
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
                !matches!(request, OPEN_SER | OPEN_SHARED),
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
        let result = self.vm.attach_current_thread().and_then(|mut env| {
            let result = env.with_local_frame(8, |env| {
                let activity = self.activity.as_obj();
                if request == OPEN_SER {
                    env.call_method(activity, "pickSer", "()V", &[])?;
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
                    let details = self.output.as_ref().map(|output| &output.metadata);
                    let geometry = if details.is_some_and(|m| m.geometry.is_some()) {
                        "-disk"
                    } else {
                        ""
                    };
                    let shift = details
                        .and_then(|m| m.offset_pixels)
                        .filter(|shift| *shift != 0.)
                        .map(|shift| format!("-{shift:+.2}px"))
                        .unwrap_or_default();
                    let name =
                        env.new_string(format!("{stem}-reconstruction{shift}{geometry}.{ext}"))?;
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

    fn import(&mut self, mut file: DocumentFile, name: String, byte_order: Option<ByteOrder>) {
        let cache = self.cache.clone();
        self.start_job(
            ("Reading spectrum", "正在读取光谱"),
            true,
            move |events, cancel| {
                if let Err(error) = file.file.seek(SeekFrom::Start(0)) {
                    if error.raw_os_error() != Some(Errno::SPIPE.raw_os_error()) {
                        return Err(error.into());
                    }
                    let cached = cache_document(&mut file.file, &cache, cancel)?;
                    file.check_error()?;
                    file = DocumentFile::cached(cached);
                }
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
        let mut metadata = FitsMetadata {
            observation: scan.observation.clone(),
            source_name: Some(scan.name.clone()),
            source_header: Some(header),
            offset_pixels: Some(self.shift),
            frame_step: Some(self.stride),
            line_center_angstrom: (self.wavelength > 0.).then_some(self.wavelength),
            dispersion_angstrom: (self.dispersion != 0.).then_some(self.dispersion),
            fwhm_pixels: self.profile.as_ref().and_then(|p| p.fit).map(|f| f.fwhm),
            geometry: None,
        };
        let shift = self.shift;
        let stride = self.stride;
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
                let fraction = if correct_disk { 0.85 } else { 0.98 };
                let raw = reconstruct_with_stride(&mut reader, trace, shift, stride, |p| {
                    progress(p * fraction)
                })?;
                drop(reader);
                let (image, warning) = if correct_disk {
                    match DiskGeometry::fit(&raw).and_then(|geometry| {
                        geometry
                            .correct(&raw, |p| progress(fraction + p * 0.13))
                            .map(|image| (image, geometry))
                    }) {
                        Ok((image, geometry)) => {
                            metadata.geometry = Some(geometry);
                            (image, None)
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(error) => (raw, Some(error)),
                    }
                } else {
                    (raw, None)
                };
                let preview = preview(image.width(), image.height(), |i| image.pixels()[i]);
                if !progress(1.) {
                    return Err(Error::Cancelled);
                }
                Ok(Work::Reconstructed {
                    image: Arc::new(image),
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
                    if fits {
                        image.write_fits_with_metadata(&mut writer, &metadata)?;
                    } else {
                        image.write_png(&mut writer)?;
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
                                spectrum,
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
                            self.output = None;
                            self.page = Page::Spectrum;
                        }
                        Ok(Work::Reconstructed {
                            image,
                            preview,
                            metadata,
                            warning,
                        }) => {
                            let texture =
                                ctx.load_texture("reconstruction", preview, TextureOptions::LINEAR);
                            self.output = Some(Output {
                                image,
                                texture,
                                metadata: *metadata,
                            });
                            self.page = Page::Result;
                            self.note = warning.map(Notice::DiskCorrection);
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
    let x_step = width.div_ceil(1200);
    let y_step = height.div_ceil(1200);
    let (w, h) = (width.div_ceil(x_step), height.div_ceil(y_step));
    let mut pixels = Vec::with_capacity(w * h);
    let (mut min, mut max) = (u16::MAX, 0);
    for y in 0..h {
        for x in 0..w {
            let sample = value(y * y_step * width + x * x_step);
            min = min.min(sample);
            max = max.max(sample);
            pixels.push(sample);
        }
    }
    let span = u32::from((max - min).max(1));
    let mut histogram = [0; 256];
    for &sample in &pixels {
        histogram[(u32::from(sample - min) * 255 / span) as usize] += 1;
    }
    let percentile = |target| {
        let mut seen = 0;
        histogram
            .iter()
            .position(|count| {
                seen += count;
                seen > target
            })
            .unwrap_or(255) as u32
            * span
            / 255
            + u32::from(min)
    };
    let lo = percentile(pixels.len() / 100);
    let hi = percentile(pixels.len() * 99 / 100).max(lo + (span / 128).max(1));
    let gray: Vec<_> = pixels
        .into_iter()
        .map(|sample| (u32::from(sample).saturating_sub(lo) * 255 / (hi - lo)).min(255) as u8)
        .collect();
    ColorImage::from_gray([w, h], &gray)
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

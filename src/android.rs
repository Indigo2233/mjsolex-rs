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
use crate::ser::cache_frames;
use crate::{DiskGeometry, Error, Header, Reconstruction, SerReader, Spectrum, Trace, reconstruct};

const OPEN_SER: i32 = 10;
const SAVE_PNG: i32 = 11;
const SAVE_FITS: i32 = 12;
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
    Progress(f32),
    Finished(Result<Work, Error>),
}

enum Work {
    Imported(Box<Imported>),
    Reconstructed {
        image: Arc<Reconstruction>,
        preview: ColorImage,
        shift: f64,
        corrected: bool,
        warning: Option<String>,
    },
    Exported(String),
}

struct Imported {
    file: DocumentFile,
    name: String,
    header: Header,
    spectrum: Spectrum,
    preview: ColorImage,
    trace: Result<Trace, Error>,
}

struct Scan {
    file: DocumentFile,
    name: String,
    header: Header,
    spectrum: Spectrum,
    texture: TextureHandle,
}

struct Output {
    image: Arc<Reconstruction>,
    texture: TextureHandle,
    shift: f64,
    corrected: bool,
}

struct Job {
    label: &'static str,
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
    job: Option<Job>,
    scan: Option<Scan>,
    trace: Option<Trace>,
    shift: f64,
    correct_disk: bool,
    output: Option<Output>,
    view_output: bool,
    note: Option<(String, bool)>,
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
            Ok::<_, jni::errors::Error>((activity, PathBuf::from(cache)))
        });
        if matches!(&result, Err(jni::errors::Error::JavaException)) {
            let _ = env.exception_clear();
        }
        drop(env);
        result.map(|(activity, cache)| (vm, activity, cache))
    });
    let Ok((vm, activity, cache)) = result else {
        eprintln!("MJ Sol'Ex could not initialize the Android file picker: {result:?}");
        return;
    };
    let options = eframe::NativeOptions {
        android_app: Some(app),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "MJ Sol'Ex",
        options,
        Box::new(move |cc| Ok(Box::new(Mobile::new(vm, activity, cache, &cc.egui_ctx)))),
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
    let Some((owner, events)) = PICKER.lock().ok().and_then(|route| route.clone()) else {
        return JNI_FALSE;
    };
    if !matches!(env.is_same_object(&activity, owner.as_obj()), Ok(true)) {
        return JNI_FALSE;
    }
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
            DocumentFile::from_android(&mut env, descriptor, request != OPEN_SER)
                .map(|file| Some((file, name)))
                .map_err(|error| error.to_string())
        })
    };
    let document =
        document.unwrap_or_else(|error| Err(format!("Cannot access Android document: {error}")));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    let adopted = matches!(document, Ok(Some(_)));
    events.post(Event::Document(request, document));
    if adopted { JNI_TRUE } else { JNI_FALSE }
}

impl Mobile {
    fn new(vm: JavaVM, activity: GlobalRef, cache: PathBuf, context: &Context) -> Self {
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
            style.spacing.item_spacing = vec2(9., 10.);
            style.spacing.interact_size.y = 46.;
        });
        context.set_theme(egui::Theme::Dark);
        Self {
            vm,
            activity,
            cache,
            events,
            receiver,
            picker: None,
            job: None,
            scan: None,
            trace: None,
            shift: 0.,
            correct_disk: false,
            output: None,
            view_output: false,
            note: None,
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
                    let name = env.new_string(format!("{stem}-reconstruction.{ext}"))?;
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
                self.note = Some((format!("Cannot open file picker: {err}"), true));
            }
        }
    }

    fn start_job(
        &mut self,
        label: &'static str,
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
            Err(error) => self.note = Some((error.to_string(), true)),
        }
    }

    fn import(&mut self, mut file: DocumentFile, name: String) {
        let cache = self.cache.clone();
        self.start_job("Reading spectrum", true, move |events, cancel| {
            if let Err(error) = file.file.seek(SeekFrom::Start(0)) {
                if error.raw_os_error() != Some(Errno::SPIPE.raw_os_error()) {
                    return Err(error.into());
                }
                let cached = cache_document(&mut file.file, &cache, cancel)?;
                file.check_error()?;
                file = DocumentFile::cached(cached);
            }
            let mut reader = SerReader::open(&file.file)?;
            let header = reader.header();
            let spectrum = Spectrum::average(&mut reader, report(events, cancel))?;
            let trace = spectrum.fit_line(None);
            let preview = preview(header.width, header.height, |i| spectrum.pixels()[i] as u16);
            drop(reader);
            Ok(Work::Imported(Box::new(Imported {
                file,
                name,
                header,
                spectrum,
                preview,
                trace,
            })))
        });
    }

    fn reconstruct(&mut self) {
        let (Some(scan), Some(trace)) = (&self.scan, self.trace) else {
            return;
        };
        let file = match scan.file.try_clone() {
            Ok(file) => file,
            Err(error) => {
                self.note = Some((error.to_string(), true));
                return;
            }
        };
        let shift = self.shift;
        let correct_disk = self.correct_disk;
        self.start_job("Reconstructing scan", true, move |events, cancel| {
            let mut reader = SerReader::open(&file.file)?;
            let mut progress = report(events, cancel);
            let fraction = if correct_disk { 0.85 } else { 1. };
            let raw = reconstruct(&mut reader, trace, shift, |p| progress(p * fraction))?;
            let (image, corrected, warning) = if correct_disk {
                match DiskGeometry::fit(&raw)
                    .and_then(|geometry| geometry.correct(&raw, |p| progress(fraction + p * 0.15)))
                {
                    Ok(image) => (image, true, None),
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(error) => (
                        raw,
                        false,
                        Some(format!("Disk correction skipped. {error}")),
                    ),
                }
            } else {
                (raw, false, None)
            };
            let preview = preview(image.width(), image.height(), |i| image.pixels()[i]);
            Ok(Work::Reconstructed {
                image: Arc::new(image),
                preview,
                shift,
                corrected,
                warning,
            })
        });
    }

    fn export(&mut self, request: i32, mut file: DocumentFile, name: String) {
        let Some(output) = &self.output else { return };
        let image = output.image.clone();
        let fits = request == SAVE_FITS;
        self.start_job(
            if fits { "Saving FITS" } else { "Saving PNG" },
            false,
            move |_, _| {
                let result = (|| -> Result<(), Error> {
                    let mut writer = BufWriter::new(&mut file.file);
                    if fits {
                        image.write_fits(&mut writer)?;
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
                Event::Document(request, result) if self.picker == Some(request) => {
                    self.picker = None;
                    match result {
                        Ok(Some((file, name))) if request == OPEN_SER => self.import(file, name),
                        Ok(Some((file, name))) => self.export(request, file, name),
                        Ok(None) => {}
                        Err(error) => self.note = Some((error, true)),
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
                                spectrum,
                                preview,
                                trace,
                            } = *import;
                            self.scan = Some(Scan {
                                file,
                                name,
                                header,
                                spectrum,
                                texture: ctx.load_texture(
                                    "spectrum",
                                    preview,
                                    TextureOptions::LINEAR,
                                ),
                            });
                            self.trace = None;
                            self.set_trace(trace);
                            self.shift = 0.;
                            self.output = None;
                            self.view_output = false;
                        }
                        Ok(Work::Reconstructed {
                            image,
                            preview,
                            shift,
                            corrected,
                            warning,
                        }) => {
                            let texture =
                                ctx.load_texture("reconstruction", preview, TextureOptions::LINEAR);
                            self.output = Some(Output {
                                image,
                                texture,
                                shift,
                                corrected,
                            });
                            self.view_output = true;
                            self.note = warning.map(|warning| (warning, true));
                        }
                        Ok(Work::Exported(name)) => {
                            self.note = Some((format!("Saved {name}"), false))
                        }
                        Err(Error::Cancelled) => {
                            self.note = Some(("Processing cancelled.".into(), false))
                        }
                        Err(error) => self.note = Some((error.to_string(), true)),
                    }
                }
                Event::Document(..) => {}
            }
        }
    }

    fn set_trace(&mut self, result: Result<Trace, Error>) {
        match result {
            Ok(trace) => {
                self.trace = Some(trace);
                self.note = None;
            }
            Err(error) => self.note = Some((error.to_string(), true)),
        }
    }

    fn content(&mut self, ui: &mut Ui) {
        let busy = self.picker.is_some() || self.job.is_some();
        ui.label(RichText::new("MJ Sol’Ex").size(36.).strong());
        ui.label(
            RichText::new("Solar scan reconstruction")
                .size(15.)
                .color(MUTED),
        );
        ui.add_space(8.);
        card(ui, |ui| {
            if let Some(scan) = &self.scan {
                ui.add(egui::Label::new(RichText::new(&scan.name).strong()).wrap());
                let h = scan.header;
                ui.label(
                    RichText::new(format!(
                        "{} × {} px  ·  {} frames  ·  {}-bit",
                        h.width, h.height, h.frames, h.depth
                    ))
                    .color(MUTED),
                );
            } else {
                ui.label(RichText::new("Start with a monochrome SER recording from Sol’Ex or another solar spectroheliograph.").color(MUTED));
            }
            if action(
                ui,
                if self.scan.is_some() {
                    "Open another SER"
                } else {
                    "Open SER recording"
                },
                !busy,
                self.scan.is_none(),
            ) {
                self.pick(OPEN_SER);
            }
        });
        if let Some(job) = &self.job {
            card(ui, |ui| {
                ui.label(RichText::new(job.label).strong());
                if let Some(progress) = job.progress {
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .show_percentage()
                            .fill(AMBER),
                    );
                } else {
                    ui.spinner();
                }
                if job.cancellable
                    && action(ui, "Cancel", !job.cancel.load(Ordering::Relaxed), false)
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            });
        }
        if let Some((note, error)) = &self.note {
            ui.label(RichText::new(note).color(if *error { Color32::LIGHT_RED } else { AMBER }));
        }
        if self.output.is_some() {
            ui.horizontal(|ui| {
                if ui.selectable_label(!self.view_output, "Spectrum").clicked() {
                    self.view_output = false;
                }
                if ui
                    .selectable_label(self.view_output, "Reconstruction")
                    .clicked()
                {
                    self.view_output = true;
                }
            });
        }
        if self.view_output {
            self.show_output(ui, busy);
        } else if self.scan.is_some() {
            self.show_spectrum(ui, busy);
        }
    }

    fn show_spectrum(&mut self, ui: &mut Ui, busy: bool) {
        let Some(scan) = &self.scan else { return };
        let header = scan.header;
        let mut trace = None;
        card(ui, |ui| {
            ui.label(RichText::new("Average spectrum").size(18.).strong());
            let response = picture(
                ui,
                &scan.texture,
                header.width,
                header.height,
                112. ..=200.,
                Sense::click(),
            );
            let rect = response.rect;
            if let Some(line) = self.trace {
                let painter = ui.painter().with_clip_rect(rect);
                for x in 0..64 {
                    let pos = |step| {
                        let col = step * (header.width - 1) / 64;
                        pos2(
                            rect.left() + rect.width() * (col as f32 + 0.5) / header.width as f32,
                            rect.top()
                                + rect.height() * ((line.row(col) + self.shift) as f32 + 0.5)
                                    / header.height as f32,
                        )
                    };
                    painter.line_segment([pos(x), pos(x + 1)], Stroke::new(1.5, AMBER));
                }
            }
            if !busy
                && response.clicked()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let x = ((pos.x - rect.left()) / rect.width() * header.width as f32) as usize;
                let y = ((pos.y - rect.top()) / rect.height() * header.height as f32) as usize;
                trace = Some(
                    scan.spectrum
                        .fit_line(Some((x.min(header.width - 1), y.min(header.height - 1)))),
                );
            }
            ui.label(RichText::new("Tap a dark line to change the fit.").color(MUTED));
            if action(ui, "Auto-fit line", !busy, false) {
                trace = Some(scan.spectrum.fit_line(None));
            }
        });
        if let Some(trace) = trace {
            self.set_trace(trace);
        }
        card(ui, |ui| {
            ui.label(RichText::new(format!("Spectral offset  {:+.2} px", self.shift)).strong());
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("−").min_size(vec2(52., 46.)))
                    .clicked()
                {
                    self.shift = (self.shift - 0.25).max(-1024.);
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("+").min_size(vec2(52., 46.)))
                    .clicked()
                {
                    self.shift = (self.shift + 0.25).min(1024.);
                }
                if ui
                    .add_enabled(!busy && self.shift != 0., egui::Button::new("Reset"))
                    .clicked()
                {
                    self.shift = 0.;
                }
            });
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.correct_disk, "Correct full-disk geometry"),
            );
            if action(ui, "Reconstruct scan", !busy && self.trace.is_some(), true) {
                self.reconstruct();
            }
        });
    }

    fn show_output(&mut self, ui: &mut Ui, busy: bool) {
        let Some(output) = &self.output else { return };
        let request = card(ui, |ui| {
            let title = if output.corrected {
                "Geometry-corrected scan"
            } else {
                "Reconstructed scan"
            };
            ui.label(RichText::new(title).size(18.).strong());
            let (w, h) = (output.image.width(), output.image.height());
            ui.label(
                RichText::new(format!("{w} × {h} px  ·  {:+.2} px offset", output.shift))
                    .color(MUTED),
            );
            let png = action(ui, "Save 16-bit PNG", !busy, true);
            let fits = action(ui, "Save FITS", !busy, false);
            picture(ui, &output.texture, w, h, 0. ..=360., Sense::hover());
            ui.label(RichText::new("Preview contrast is stretched.").color(MUTED));
            if png {
                Some(SAVE_PNG)
            } else if fits {
                Some(SAVE_FITS)
            } else {
                None
            }
        });
        if let Some(request) = request {
            self.pick(request);
        }
    }
}

impl eframe::App for Mobile {
    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        self.receive(ui.ctx());
        egui::Frame::new().fill(BACKGROUND).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let padding = ((ui.available_width() - 590.) / 2.).max(17.);
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(padding as i8, 17))
                        .show(ui, |ui| {
                            self.content(ui);
                        });
                });
        });
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
    let result = cache_frames(PipeReader { input, cancel }, &mut file, limit);
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

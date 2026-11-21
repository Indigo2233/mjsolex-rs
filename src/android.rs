use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use eframe::egui::{
    self, Align, Color32, ColorImage, Context, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};
use winit::platform::android::activity::AndroidApp;

use crate::{Error, Header, Reconstruction, SerReader, Spectrum, Trace, reconstruct};

const OPEN_SER: i32 = 10;
const SAVE_PNG: i32 = 11;
const BACKGROUND: Color32 = Color32::from_rgb(17, 21, 27);
const CARD: Color32 = Color32::from_rgb(28, 35, 44);
const TEXT: Color32 = Color32::from_rgb(240, 243, 247);
const MUTED: Color32 = Color32::from_rgb(171, 185, 199);
const AMBER: Color32 = Color32::from_rgb(255, 190, 99);

type Document = Option<(File, String)>;

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
    Reconstructed(Arc<Reconstruction>, ColorImage, f64),
    Exported(String),
}

struct Imported {
    file: File,
    name: String,
    header: Header,
    spectrum: Spectrum,
    preview: ColorImage,
    trace: Result<Trace, Error>,
}

struct Scan {
    file: File,
    name: String,
    header: Header,
    spectrum: Spectrum,
    texture: TextureHandle,
}

struct Output {
    image: Arc<Reconstruction>,
    texture: TextureHandle,
    shift: f64,
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
        // SAFETY: android-activity supplies a global reference to the live Activity.
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let activity = env.new_global_ref(activity)?;
        let cache = env
            .call_method(activity.as_obj(), "getCacheDir", "()Ljava/io/File;", &[])?
            .l()?;
        let cache = env
            .call_method(cache, "getAbsolutePath", "()Ljava/lang/String;", &[])?
            .l()?;
        let cache: String = env.get_string(&JString::from(cache))?.into();
        drop(env);
        Ok((vm, activity, PathBuf::from(cache)))
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

#[unsafe(no_mangle)]
extern "system" fn Java_org_mjsolex_mobile_MainActivity_onDocument(
    mut env: JNIEnv<'_>,
    activity: JObject<'_>,
    request: i32,
    fd: i32,
    name: JString<'_>,
    error: JString<'_>,
) {
    let Ok(route) = PICKER.lock() else { return };
    let Some((owner, events)) = route.as_ref() else {
        return;
    };
    if !matches!(env.is_same_object(&activity, owner.as_obj()), Ok(true)) {
        return;
    }
    let document = if !error.is_null() {
        let message = env
            .get_string(&error)
            .map(Into::into)
            .unwrap_or_else(|_| "Cannot access the Android document.".into());
        Err(message)
    } else if fd < 0 {
        Ok(None)
    } else {
        let name = env
            .get_string(&name)
            .map(Into::into)
            .unwrap_or_else(|_| "scan".into());
        // SAFETY: the Java call holds the ParcelFileDescriptor open until we return.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        borrowed
            .try_clone_to_owned()
            .map(|owned| Some((File::from(owned), name)))
            .map_err(|err| err.to_string())
    };
    events.post(Event::Document(request, document));
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
            output: None,
            view_output: false,
            note: None,
        }
    }

    fn pick(&mut self, request: i32) {
        let result = (|| {
            let mut env = self.vm.attach_current_thread()?;
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
                let name = env.new_string(format!("{stem}-reconstruction.png"))?;
                env.call_method(
                    activity,
                    "createPng",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&name)],
                )?;
            }
            Ok::<_, jni::errors::Error>(())
        })();
        match result {
            Ok(()) => {
                self.picker = Some(request);
                self.note = None;
            }
            Err(err) => {
                if let Ok(env) = self.vm.get_env() {
                    let _ = env.exception_clear();
                }
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

    fn import(&mut self, mut file: File, name: String) {
        let cache = self.cache.clone();
        self.start_job("Reading spectrum", true, move |events, cancel| {
            if let Err(error) = file.seek(SeekFrom::Start(0)) {
                if error.raw_os_error() != Some(29) {
                    // ESPIPE on Android
                    return Err(error.into());
                }
                file = cache_document(file, &cache, cancel)?;
            }
            let mut reader = SerReader::open(&file)?;
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
        self.start_job("Reconstructing scan", true, move |events, cancel| {
            let mut reader = SerReader::open(file)?;
            let image = Arc::new(reconstruct(
                &mut reader,
                trace,
                shift,
                report(events, cancel),
            )?);
            let thumbnail = preview(image.width(), image.height(), |i| image.pixels()[i]);
            Ok(Work::Reconstructed(image, thumbnail, shift))
        });
    }

    fn export(&mut self, file: File, name: String) {
        let Some(output) = &self.output else { return };
        let image = output.image.clone();
        self.start_job("Saving PNG", false, move |_, _| {
            let mut writer = BufWriter::new(file);
            image.write_png(&mut writer)?;
            writer.flush()?;
            Ok(Work::Exported(name))
        });
    }

    fn receive(&mut self, ctx: &Context) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::Document(request, result) if self.picker == Some(request) => {
                    self.picker = None;
                    match result {
                        Ok(Some((file, name))) if request == OPEN_SER => self.import(file, name),
                        Ok(Some((file, name))) => self.export(file, name),
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
                        Ok(Work::Reconstructed(image, preview, shift)) => {
                            let texture =
                                ctx.load_texture("reconstruction", preview, TextureOptions::LINEAR);
                            self.output = Some(Output {
                                image,
                                texture,
                                shift,
                            });
                            self.view_output = true;
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
                200.,
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
            if action(ui, "Reconstruct scan", !busy && self.trace.is_some(), true) {
                self.reconstruct();
            }
        });
    }

    fn show_output(&mut self, ui: &mut Ui, busy: bool) {
        let Some(output) = &self.output else { return };
        let export = card(ui, |ui| {
            ui.label(RichText::new("Reconstructed scan").size(18.).strong());
            let (w, h) = (output.image.width(), output.image.height());
            ui.label(
                RichText::new(format!("{w} × {h} px  ·  {:+.2} px offset", output.shift))
                    .color(MUTED),
            );
            let save = action(ui, "Save 16-bit PNG", !busy, true);
            picture(ui, &output.texture, w, h, 360., Sense::hover());
            ui.label(RichText::new("Preview contrast is stretched.").color(MUTED));
            save
        });
        if export {
            self.pick(SAVE_PNG);
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

fn cache_document(mut input: File, cache: &Path, cancel: &AtomicBool) -> Result<File, Error> {
    let mut file = tempfile::tempfile_in(cache)?;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let size = input.read(&mut buffer)?;
        if size == 0 {
            return Ok(file);
        }
        file.write_all(&buffer[..size])?;
    }
}

fn preview(width: usize, height: usize, value: impl Fn(usize) -> u16) -> ColorImage {
    let step = width.max(height).div_ceil(1200);
    let (w, h) = (width.div_ceil(step), height.div_ceil(step));
    let mut pixels = Vec::with_capacity(w * h);
    let mut histogram = [0; 256];
    for y in 0..h {
        for x in 0..w {
            let sample = value(y * step * width + x * step);
            histogram[usize::from(sample >> 8)] += 1;
            pixels.push(sample);
        }
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
            * 257
    };
    let lo = percentile(pixels.len() / 100);
    let hi = percentile(pixels.len() * 99 / 100).max(lo + 512);
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
    max_height: f32,
    sense: Sense,
) -> egui::Response {
    let aspect = width as f32 / height as f32;
    let mut w = ui.available_width();
    let h = (w / aspect).min(max_height);
    w = (h * aspect).min(w);
    ui.with_layout(egui::Layout::top_down(Align::Center), |ui| {
        ui.add(
            egui::Image::from_texture(texture)
                .fit_to_exact_size(vec2(w, h))
                .sense(sense),
        )
    })
    .inner
}

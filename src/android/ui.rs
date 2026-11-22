use super::*;
use crate::solar_atlas::{ReferenceProfile, reference_profile};

const CYAN: Color32 = Color32::from_rgb(131, 208, 225);
const ATLAS_COLOR: Color32 = Color32::from_rgb(194, 160, 255);
const SPECTRAL_LINES: [(&str, f64); 5] = [
    ("Hα", 6562.81),
    ("Ca II K", 3933.66),
    ("Ca II H", 3968.47),
    ("Hβ", 4861.34),
    ("He I D3", 5875.62),
];

fn product_label(product: Product, lang: Language) -> &'static str {
    match product {
        Product::Gray => lang.text("Grayscale", "灰度图"),
        Product::Hydrogen => lang.text("Hα false color", "Hα 假彩色"),
        Product::Calcium => lang.text("Ca II false color", "Ca II 假彩色"),
        Product::Helium => lang.text("He I false color", "He I 假彩色"),
        Product::Doppler => lang.text("Doppler contrast", "双翼多普勒图"),
    }
}

fn flat_label(mode: FlatMode, lang: Language) -> &'static str {
    match mode {
        FlatMode::Off => lang.text("Off", "关闭"),
        FlatMode::Synthetic => lang.text("Synthetic", "虚拟平场"),
        FlatMode::Physical => lang.text("Captured SER", "实拍平场"),
    }
}

impl eframe::App for Mobile {
    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        self.receive(ui.ctx());
        egui::Panel::top("page_navigation")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| centered(ui, |ui| self.navigation(ui)));
        if self.page != Page::Scan {
            egui::Panel::bottom("page_actions")
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .inner_margin(egui::Margin::symmetric(14, 10)),
                )
                .show(ui, |ui| centered(ui, |ui| self.page_actions(ui)));
        }
        egui::Frame::new()
            .fill(BACKGROUND)
            .inner_margin(egui::Margin::symmetric(14, 0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(match self.page {
                        Page::Scan => "scan",
                        Page::Spectrum => "spectrum",
                        Page::Result => "result",
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        centered(ui, |ui| {
                            self.job_status(ui);
                            match self.page {
                                Page::Scan => self.scan_page(ui),
                                Page::Spectrum => self.spectrum_page(ui),
                                Page::Result => self.result_page(ui),
                            }
                            ui.add_space(8.);
                        });
                    });
            });
    }
}

impl Mobile {
    fn navigation(&mut self, ui: &mut Ui) {
        ui.add_space(12.);
        ui.horizontal(|ui| {
            ui.label(RichText::new(APP_NAME).size(27.).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let label = self.language.text("简体中文", "English");
                if ui.button(label).clicked() {
                    self.switch_language();
                }
            });
        });
        let lang = self.language;
        let tab_width = (ui.available_width() - 2. * ui.spacing().item_spacing.x) / 3.;
        ui.horizontal(|ui| {
            for (page, name, enabled) in [
                (Page::Scan, lang.text("Import", "导入"), true),
                (
                    Page::Spectrum,
                    lang.text("Spectrum", "光谱"),
                    self.scan.is_some(),
                ),
                (
                    Page::Result,
                    lang.text("Result", "结果"),
                    self.output.is_some(),
                ),
            ] {
                let button = egui::Button::new(RichText::new(name).size(14.))
                    .selected(self.page == page)
                    .min_size(vec2(tab_width, 42.));
                if ui.add_enabled(enabled, button).clicked() {
                    self.page = page;
                }
            }
        });
    }

    fn job_status(&self, ui: &mut Ui) {
        let lang = self.language;
        if let Some(job) = &self.job {
            card(ui, |ui| {
                ui.label(RichText::new(lang.text(job.label.0, job.label.1)).strong());
                if let Some(progress) = job.progress {
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .show_percentage()
                            .fill(AMBER)
                            .desired_height(19.),
                    );
                } else {
                    ui.spinner();
                }
                if job.cancellable
                    && action(
                        ui,
                        lang.text("Cancel", "取消"),
                        !job.cancel.load(Ordering::Relaxed),
                        false,
                    )
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            });
        } else if self.picker == Some(OPEN_SHARED) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(lang.text("Opening the shared SER…", "正在打开分享的 SER…"));
            });
        }
        if let Some(notice) = &self.note {
            ui.add(
                egui::Label::new(
                    RichText::new(notice.message(lang)).color(if notice.is_error() {
                        Color32::LIGHT_RED
                    } else {
                        CYAN
                    }),
                )
                .wrap(),
            );
        }
    }

    fn scan_page(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        let lang = self.language;
        card(ui, |ui| {
            heading(ui, lang.text("Solar scan file", "太阳扫描文件"));
            if let Some(scan) = &self.scan {
                ui.add(egui::Label::new(RichText::new(&scan.name).strong()).wrap());
                recording_summary(ui, scan.header, lang);
            }
            if action(ui, lang.text("Open SER file", "打开 SER 文件"), !busy, true) {
                self.pick(OPEN_SER);
            }
        });
        card(ui, |ui| {
            heading(ui, lang.text("Indigo Observatory", "靛空台"));
            ui.label(
                RichText::new(lang.text(
                    "In the library, select one original SER and share it to MJSol'Ex.",
                    "在素材库中选中一份原始 SER，分享至 MJSol'Ex。",
                ))
                .color(MUTED),
            );
            if action(
                ui,
                lang.text("Open Indigo Observatory", "打开靛空台"),
                !busy,
                false,
            ) {
                self.note = match self.activity_flag("openIndigo") {
                    Ok(true) => None,
                    Ok(false) => Some(Notice::IndigoUnavailable),
                    Err(error) => Some(Notice::IndigoLaunch(error.to_string())),
                };
            }
        });
        let Some(scan) = &self.scan else { return };
        let mut chosen_order = scan.byte_order;
        let current_order = scan.byte_order;
        card(ui, |ui| {
            egui::CollapsingHeader::new(
                RichText::new(lang.text("SER details", "SER 详情")).strong(),
            )
            .id_salt("recording_details")
            .show(ui, |ui| {
                for (key, value) in [
                    (
                        lang.text("Start UTC", "开始时间（UTC）"),
                        scan.observation.start_utc.as_deref(),
                    ),
                    (
                        lang.text("End UTC", "结束时间（UTC）"),
                        scan.observation.end_utc.as_deref(),
                    ),
                    (
                        lang.text("Recorder", "录制应用"),
                        scan.observation.recorder.map(|name| match name {
                            "Indigo Observatory" => lang.text("Indigo Observatory", "靛空台"),
                            other => other,
                        }),
                    ),
                    (
                        lang.text("Instrument", "仪器"),
                        scan.observation.instrument.as_deref(),
                    ),
                    (
                        lang.text("Filter", "滤镜"),
                        scan.observation.filter.as_deref(),
                    ),
                    (
                        lang.text("Observer", "观测者"),
                        scan.observation.observer.as_deref(),
                    ),
                    (
                        lang.text("Telescope", "望远镜"),
                        scan.observation.telescope.as_deref(),
                    ),
                ] {
                    if let Some(value) = value {
                        ui.add(egui::Label::new(format!("{key}  {value}")).wrap());
                    }
                }
                if scan.header.depth > 8 {
                    let order = order_label(Some(scan.header.byte_order()), lang);
                    let decoded = match lang {
                        Language::English => format!("Decoded as {order} endian"),
                        Language::Chinese => format!("当前按{order}字节序解码"),
                    };
                    ui.label(RichText::new(decoded).color(MUTED));
                    ui.add_enabled_ui(!busy, |ui| {
                        egui::ComboBox::from_id_salt("byte_order")
                            .selected_text(order_label(chosen_order, lang))
                            .show_ui(ui, |ui| {
                                for order in [None, Some(ByteOrder::Little), Some(ByteOrder::Big)] {
                                    ui.selectable_value(
                                        &mut chosen_order,
                                        order,
                                        order_label(order, lang),
                                    );
                                }
                            });
                    });
                }
            });
        });
        if chosen_order != current_order {
            self.reload_byte_order(chosen_order);
        }
    }

    fn spectrum_page(&mut self, ui: &mut Ui) {
        let Some(scan) = &self.scan else { return };
        let header = scan.header;
        let busy = self.busy();
        let lang = self.language;
        let mut selected = None;
        card(ui, |ui| {
            heading(ui, lang.text("Average spectrum", "平均光谱"));
            let response = picture(
                ui,
                &scan.texture,
                header.width,
                header.height,
                88. ..=152.,
                Sense::click(),
            );
            let rect = response.rect;
            if let Some(trace) = self.trace {
                let painter = ui.painter().with_clip_rect(rect);
                for (offset, color) in [(0., AMBER), (self.shift, CYAN)] {
                    if offset == 0. && color == CYAN {
                        continue;
                    }
                    let points = (0..=64)
                        .map(|step| {
                            let col = step * (header.width - 1) / 64;
                            pos2(
                                rect.left()
                                    + rect.width() * (col as f32 + 0.5) / header.width as f32,
                                rect.top()
                                    + rect.height() * ((trace.row(col) + offset) as f32 + 0.5)
                                        / header.height as f32,
                            )
                        })
                        .collect();
                    painter.add(egui::Shape::line(points, Stroke::new(1.4, color)));
                }
            }
            if !busy
                && response.clicked()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let relative = (pos - rect.min) / rect.size();
                let col = (relative.x * header.width as f32) as usize;
                let row = (relative.y * header.height as f32) as usize;
                selected = Some(scan.spectrum.fit_line(Some((
                    col.min(header.width - 1),
                    row.min(header.height - 1),
                ))));
            }
            ui.label(
                RichText::new(lang.text("Tap a dark line to select it.", "点选图中的暗线。"))
                    .color(MUTED),
            );
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new(lang.text("Auto-fit", "自动拟合")))
                    .clicked()
                {
                    selected = Some(scan.spectrum.fit_line(None));
                }
                ui.label(RichText::new(lang.text("Line", "谱线")).color(AMBER));
                if self.shift != 0. {
                    ui.label(RichText::new(lang.text("Sampling", "采样位置")).color(CYAN));
                }
            });
            offset_controls(ui, &mut self.shift, (header.height - 1) as f64, busy, lang);
        });
        if let Some(selected) = selected {
            self.set_trace(selected);
        }
        if let Some(profile) = &self.profile {
            card(ui, |ui| {
                heading(ui, lang.text("Line profile", "谱线剖面"));
                let wavelength = self.calibration.wavelength;
                let dispersion = self.calibration.dispersion();
                let reference = reference_profile(profile, wavelength, dispersion.unwrap_or(0.));
                if let Some(fit) = profile.fit {
                    let label = lang.text("FWHM", "半高全宽");
                    let width = if let Some(dispersion) = dispersion {
                        format!(
                            "{label}  {:.2} px  ·  {:.3} Å",
                            fit.fwhm,
                            fit.fwhm * dispersion.abs()
                        )
                    } else {
                        format!("{label}  {:.2} px", fit.fwhm)
                    };
                    ui.label(RichText::new(width).size(17.).color(AMBER).strong());
                } else {
                    ui.label(
                        RichText::new(lang.text(
                            "No reliable FWHM. Select a line with both wings.",
                            "无法可靠测量半高全宽。请选择两翼完整的谱线。",
                        ))
                        .color(MUTED),
                    );
                }
                profile_chart(ui, profile, reference.as_ref(), lang);
                if let Some(atlas) = &reference {
                    if let Some(depth) = &atlas.line_depth {
                        let percent = 100. * depth.ratio();
                        let comparison = match lang {
                            Language::English => format!("Line depth · {percent:.0}% of BASS2000"),
                            Language::Chinese => format!("线深为 BASS2000 参考值的 {percent:.0}%"),
                        };
                        ui.label(RichText::new(comparison).color(ATLAS_COLOR).strong());
                        let observed = 100. * depth.observed;
                        let reference = 100. * depth.reference;
                        let depths = match lang {
                            Language::English => {
                                format!("Observed {observed:.1}% · Reference {reference:.1}%")
                            }
                            Language::Chinese => {
                                format!("实测 {observed:.1}% · 参考 {reference:.1}%")
                            }
                        };
                        ui.label(RichText::new(depths).color(MUTED));
                    } else {
                        ui.label(
                            RichText::new(lang.text(
                                "Line depth comparison needs both wings and a distinct line core.",
                                "线深对比需要两侧谱翼和可辨认的线芯。",
                            ))
                            .color(MUTED),
                        );
                    }
                }
                if reference.is_none() {
                    let hint = if wavelength == 0. || dispersion.is_none() {
                        lang.text(
                            "Select a line and set dispersion to compare with BASS2000.",
                            "选择谱线并设定色散后，可与 BASS2000 参考曲线对比。",
                        )
                    } else {
                        lang.text(
                            "No comparable BASS2000 data in this window.",
                            "当前波段没有可用于对比的 BASS2000 数据。",
                        )
                    };
                    ui.label(RichText::new(hint).color(MUTED));
                }
                wavelength_calibration(ui, &mut self.calibration, lang, busy);
            });
        }
        card(ui, |ui| {
            heading(ui, lang.text("Processing options", "处理选项"));
            if self.profile.is_none() {
                wavelength_calibration(ui, &mut self.calibration, lang, busy);
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.label(lang.text("Image product", "图像产品"));
                ui.horizontal_wrapped(|ui| {
                    for product in [
                        Product::Gray,
                        Product::Hydrogen,
                        Product::Calcium,
                        Product::Helium,
                        Product::Doppler,
                    ] {
                        let button = egui::Button::new(product_label(product, lang))
                            .selected(self.product == product);
                        if ui.add(button).clicked() {
                            self.product = product;
                        }
                    }
                });
                if self.product == Product::Doppler {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            lang.text("Wing offset from sampling center", "相对采样中心的谱翼偏移"),
                        );
                        ui.add(
                            egui::DragValue::new(&mut self.wing_offset)
                                .range(0.5..=(header.height as f64 / 2.).max(0.5))
                                .speed(0.1)
                                .prefix("±")
                                .suffix(" px"),
                        );
                    });
                    let (hint, color) = if self.calibration.dispersion().is_none() {
                        (
                            lang.text(
                                "Set dispersion and its direction in Wavelength calibration.",
                                "请在波长标定中设定色散和方向。",
                            ),
                            AMBER,
                        )
                    } else if self
                        .scan
                        .as_ref()
                        .zip(self.trace)
                        .is_some_and(|(scan, trace)| {
                            !scan
                                .spectrum
                                .supports_wings(trace, self.shift, self.wing_offset)
                        })
                    {
                        (
                            lang.text(
                                "The scan does not cover both wings at this offset.",
                                "当前扫描无法覆盖所选偏移处的两侧谱翼。",
                            ),
                            AMBER,
                        )
                    } else {
                        (
                            lang.text(
                                "Red and blue show relative wing contrast.",
                                "红色和蓝色表示两侧谱翼的相对强度差。",
                            ),
                            MUTED,
                        )
                    };
                    ui.label(RichText::new(hint).color(color));
                }
                ui.label(lang.text("Flat correction", "平场校正"));
                ui.horizontal_wrapped(|ui| {
                    for mode in [FlatMode::Off, FlatMode::Synthetic, FlatMode::Physical] {
                        let button = egui::Button::new(flat_label(mode, lang))
                            .selected(self.flat_mode == mode);
                        if ui.add(button).clicked() {
                            if mode == FlatMode::Physical && self.physical_flat.is_none() {
                                self.pick(OPEN_FLAT);
                            } else {
                                self.flat_mode = mode;
                            }
                        }
                    }
                });
                if self.flat_mode == FlatMode::Physical {
                    if let Some((_, name)) = &self.physical_flat {
                        ui.label(RichText::new(name).color(MUTED));
                    }
                    if ui
                        .button(lang.text("Replace flat SER", "更换平场 SER"))
                        .clicked()
                    {
                        self.pick(OPEN_FLAT);
                    }
                }
                if self.flat_mode == FlatMode::Synthetic && self.product == Product::Doppler {
                    ui.label(
                        RichText::new(lang.text(
                            "Doppler skips synthetic flat. Shared slit gain cancels in the ratio.",
                            "双翼多普勒跳过虚拟平场。共同的狭缝增益在谱翼比值中抵消。",
                        ))
                        .color(MUTED),
                    );
                }
            });
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.correct_disk,
                    lang.text("Correct full-disk geometry", "校正太阳盘几何形状"),
                ),
            );
            let minimum = self.minimum_stride(header);
            let mut stride = self.stride.max(minimum);
            if minimum > 1 || stride > 1 {
                let summary = match lang {
                    Language::English => format!("Large scan · using every {stride} frames"),
                    Language::Chinese => format!("大文件 · 每 {stride} 帧取一帧"),
                };
                ui.label(RichText::new(summary).color(MUTED));
            }
            egui::CollapsingHeader::new(
                RichText::new(lang.text("Scan sampling", "扫描采样")).strong(),
            )
            .id_salt("scan_sampling")
            .show(ui, |ui| {
                if ui
                    .add_enabled(
                        !busy,
                        egui::DragValue::new(&mut stride)
                            .range(minimum..=header.frames)
                            .prefix(lang.text("Every ", "每 "))
                            .suffix(lang.text(" frames", " 帧取一帧")),
                    )
                    .changed()
                {
                    self.stride = stride;
                }
                let (width, height) = (header.width, header.frames.div_ceil(stride));
                let size = match lang {
                    Language::English => {
                        format!("{width} × {height} px before geometry correction")
                    }
                    Language::Chinese => format!("校正前尺寸为 {width} × {height} px"),
                };
                ui.label(RichText::new(size).color(MUTED));
            });
        });
    }

    fn result_page(&self, ui: &mut Ui) {
        let Some(output) = &self.output else { return };
        let lang = self.language;
        card(ui, |ui| {
            heading(ui, product_label(output.product, lang));
            let (w, h) = (output.image.width(), output.image.height());
            let shift = output.metadata.offset_pixels.unwrap_or(0.);
            let size = match lang {
                Language::English => format!("{w} × {h} px · {shift:+.2} px offset"),
                Language::Chinese => format!("{w} × {h} px · 偏移 {shift:+.2} px"),
            };
            ui.label(RichText::new(size).color(MUTED));
            if output.product == Product::Doppler {
                let offset = output.wing_offset;
                let label = match lang {
                    Language::English => format!("Wings ±{offset:.2} px · qualitative contrast"),
                    Language::Chinese => format!("谱翼 ±{offset:.2} px · 定性对比"),
                };
                ui.label(RichText::new(label).color(MUTED));
            }
            if let Some(name) = &output.metadata.flat_file {
                let label = lang.text("Captured flat applied", "已应用实拍平场");
                ui.label(RichText::new(format!("{label} · {name}")).color(MUTED));
            } else if output.image.has_synthetic_flat() {
                ui.label(
                    RichText::new(lang.text("Synthetic flat applied", "已应用虚拟平场"))
                        .color(MUTED),
                );
            }
            picture(ui, &output.texture, w, h, 0. ..=430., Sense::hover());
        });
    }

    fn page_actions(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        let lang = self.language;
        match self.page {
            Page::Spectrum => {
                if action(
                    ui,
                    lang.text("Reconstruct scan", "重建图像"),
                    !busy && self.trace.is_some(),
                    true,
                ) {
                    self.reconstruct();
                }
            }
            Page::Result => {
                if action(ui, lang.text("Save PNG", "保存 PNG"), !busy, true) {
                    self.pick(SAVE_PNG);
                }
                if self
                    .output
                    .as_ref()
                    .is_none_or(|output| output.product == Product::Doppler)
                {
                    return;
                }
                let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.;
                ui.horizontal(|ui| {
                    for (label, request) in [
                        (lang.text("Save FITS", "保存 FITS"), SAVE_FITS),
                        (lang.text("16-bit PNG", "16 位 PNG"), SAVE_RAW_PNG),
                    ] {
                        let text = RichText::new(label).size(15.).strong();
                        let button = egui::Button::new(text);
                        if ui
                            .add_enabled(!busy, button.min_size(vec2(width, 44.)))
                            .clicked()
                        {
                            self.pick(request);
                        }
                    }
                });
            }
            Page::Scan => {}
        }
    }
}

fn centered<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> R {
    let width = ui.available_width().min(590.);
    ui.vertical_centered(|ui| {
        ui.allocate_ui_with_layout(vec2(width, 0.), egui::Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            contents(ui)
        })
        .inner
    })
    .inner
}

fn heading(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).size(18.).strong());
}

fn recording_summary(ui: &mut Ui, header: Header, lang: Language) {
    let (width, height, frames, depth) = (header.width, header.height, header.frames, header.depth);
    let color = header.color.label();
    let (dimensions, format) = match lang {
        Language::English => (
            format!("{width} × {height} px · {frames} frames"),
            format!("{depth}-bit · {color}"),
        ),
        Language::Chinese => {
            let color = if header.color == crate::ColorMode::Mono {
                "单色"
            } else {
                color
            };
            (
                format!("{width} × {height} px · {frames} 帧"),
                format!("{depth} 位 · {color}"),
            )
        }
    };
    ui.label(RichText::new(dimensions).color(MUTED));
    ui.label(RichText::new(format).color(MUTED));
}

fn order_label(order: Option<ByteOrder>, lang: Language) -> &'static str {
    match order {
        None => lang.text("Auto", "自动"),
        Some(ByteOrder::Little) => lang.text("Little", "小端"),
        Some(ByteOrder::Big) => lang.text("Big", "大端"),
    }
}

fn offset_controls(ui: &mut Ui, shift: &mut f64, limit: f64, busy: bool, lang: Language) {
    ui.add_enabled_ui(!busy, |ui| {
        ui.horizontal(|ui| {
            ui.label(lang.text("Offset", "偏移"));
            for (text, delta) in [("−", -0.25), ("+", 0.25)] {
                if ui
                    .add(egui::Button::new(text).min_size(vec2(36., 42.)))
                    .clicked()
                {
                    *shift = (*shift + delta).clamp(-limit, limit);
                }
            }
            ui.add(
                egui::DragValue::new(shift)
                    .range(-limit..=limit)
                    .speed(0.25)
                    .fixed_decimals(2)
                    .suffix(" px"),
            );
            if *shift != 0. && ui.button(lang.text("Reset", "重置")).clicked() {
                *shift = 0.;
            }
        });
    });
}

fn wavelength_calibration(ui: &mut Ui, calibration: &mut Calibration, lang: Language, busy: bool) {
    egui::CollapsingHeader::new(
        RichText::new(lang.text("Wavelength calibration", "波长标定")).strong(),
    )
    .id_salt("wavelength_calibration")
    .show(ui, |ui| {
        ui.add_enabled_ui(!busy, |ui| {
            ui.label(lang.text("Selected absorption line", "所选谱线"));
            ui.horizontal_wrapped(|ui| {
                for (label, wavelength) in SPECTRAL_LINES {
                    let selected = !calibration.custom_wavelength && calibration.wavelength == wavelength;
                    if ui.add(egui::Button::new(label).selected(selected)).clicked() {
                        calibration.custom_wavelength = false;
                        calibration.wavelength = wavelength;
                    }
                }
                let custom = egui::Button::new(lang.text("Custom", "自定义"))
                    .selected(calibration.custom_wavelength);
                if ui.add(custom).clicked() {
                    calibration.custom_wavelength = true;
                }
            });
            if calibration.custom_wavelength {
                ui.label(lang.text("Zero-offset wavelength (Å)", "零偏移波长（Å）"));
                ui.add(
                    egui::DragValue::new(&mut calibration.wavelength)
                        .range(0. ..=100_000.)
                        .speed(0.02)
                        .max_decimals(4)
                        .custom_formatter(move |value, _| optional_number(value, lang)),
                );
            } else if calibration.wavelength > 0. {
                let label = lang.text("Reference wavelength", "参考波长");
                ui.label(RichText::new(format!("{label}  {:.2} Å", calibration.wavelength)).color(MUTED));
            }
            ui.collapsing(lang.text("Tell Ca II K from H", "区分 Ca II K 和 H"), |ui| {
                ui.label(lang.text(
                    "Ca II K is at 3933.66 Å and Ca II H is at 3968.47 Å. Look for Hε at 3970.07 Å, about 1.60 Å to the red of Ca II H. For a narrow crop, check the recorded wavelength band.",
                    "Ca II K 在 3933.66 Å，Ca II H 在 3968.47 Å。Hε 在 3970.07 Å，距 Ca II H 红侧约 1.60 Å。窄幅录制时请核对拍摄波段。",
                ));
            });

            ui.label(lang.text("Dispersion source", "色散来源"));
            ui.horizontal_wrapped(|ui| {
                for (label, optical) in [
                    (lang.text("Manual", "手动输入"), false),
                    (lang.text("Optical setup", "光学参数"), true),
                ] {
                    if ui.add(egui::Button::new(label).selected(calibration.use_optics == optical)).clicked() {
                        calibration.use_optics = optical;
                    }
                }
            });
            if calibration.use_optics {
                optical_calibration(ui, calibration, lang);
            } else {
                ui.label(lang.text("Dispersion (Å/px)", "色散（Å/px）"));
                ui.add(
                    egui::DragValue::new(&mut calibration.manual_dispersion)
                        .range(-50. ..=50.)
                        .speed(0.001)
                        .max_decimals(5)
                        .custom_formatter(move |value, _| optional_number(value, lang)),
                );
                ui.label(RichText::new(lang.text(
                    "Use negative dispersion if wavelength decreases down the frame.",
                    "如果波长从画面顶部向底部递减，请输入负色散。",
                )).color(MUTED));
            }
        });
    });
}

fn optical_calibration(ui: &mut Ui, calibration: &mut Calibration, lang: Language) {
    let optics = &mut calibration.optics;
    if ui
        .button(lang.text("Use standard Sol'Ex optics", "填入标准 Sol'Ex 光路"))
        .clicked()
    {
        optics.lines_per_mm = 2400.;
        optics.order = 1;
        optics.included_angle_deg = 34.;
        optics.camera_focal_length_mm = 125.;
    }
    egui::Grid::new("optical_setup")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label(lang.text("Grating (lines/mm)", "光栅刻线（条/mm）"));
            ui.add(
                egui::DragValue::new(&mut optics.lines_per_mm)
                    .range(0. ..=12000.)
                    .speed(10.),
            );
            ui.end_row();
            ui.label(lang.text("Diffraction order", "衍射级次"));
            ui.add(egui::DragValue::new(&mut optics.order).range(1..=16));
            ui.end_row();
            ui.label(lang.text("Angle between arms (°)", "光臂夹角（°）"));
            ui.add(
                egui::DragValue::new(&mut optics.included_angle_deg)
                    .range(0. ..=179.9)
                    .speed(0.1),
            );
            ui.end_row();
            ui.label(lang.text("Camera focal length (mm)", "成像镜焦距（mm）"));
            ui.add(
                egui::DragValue::new(&mut optics.camera_focal_length_mm)
                    .range(0. ..=2000.)
                    .speed(0.5),
            );
            ui.end_row();
            ui.label(lang.text("Sensor pixel (µm)", "相机像元（µm）"));
            ui.add(
                egui::DragValue::new(&mut optics.pixel_size_um)
                    .range(0. ..=100.)
                    .speed(0.02),
            );
            ui.end_row();
            ui.label(lang.text("Spectral binning", "波长方向像元合并"));
            ui.add(egui::DragValue::new(&mut optics.spectral_binning).range(1..=16));
            ui.end_row();
        });
    ui.label(lang.text(
        "Wavelength from top to bottom",
        "从画面顶部到下方的波长方向",
    ));
    ui.horizontal_wrapped(|ui| {
        for (increasing, label) in [
            (true, lang.text("Increases (+)", "递增（+）")),
            (false, lang.text("Decreases (−)", "递减（−）")),
        ] {
            if ui
                .add(
                    egui::Button::new(label)
                        .selected(calibration.increases_downward == Some(increasing)),
                )
                .clicked()
            {
                calibration.increases_downward = Some(increasing);
            }
        }
    });
    let result = if let Some(dispersion) = calibration.dispersion() {
        let label = lang.text("Estimated dispersion", "估算色散");
        format!("{label} {dispersion:+.5} Å/px")
    } else if calibration
        .optics
        .dispersion_angstrom(calibration.wavelength)
        .is_some()
    {
        lang.text("Select the wavelength direction.", "请选择波长方向。")
            .to_owned()
    } else {
        lang.text(
            "Select a line and enter compatible optical parameters.",
            "请选择谱线并填写有效的光学参数。",
        )
        .to_owned()
    };
    ui.label(RichText::new(result).color(AMBER));
}

fn optional_number(value: f64, lang: Language) -> String {
    if value == 0. {
        lang.text("Unset", "未设置").into()
    } else {
        format!("{value:.5}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn profile_chart(
    ui: &mut Ui,
    profile: &LineProfile,
    reference: Option<&ReferenceProfile>,
    lang: Language,
) {
    let (Some(first), Some(last)) = (profile.samples.first(), profile.samples.last()) else {
        return;
    };
    if first[0] == last[0] {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 136.), Sense::hover());
    let left_margin = if reference.is_some() { 39. } else { 7. };
    let plot = egui::Rect::from_min_max(rect.min + vec2(left_margin, 8.), rect.max - vec2(7., 20.));
    let values = profile
        .samples
        .iter()
        .chain(reference.into_iter().flat_map(|atlas| &atlas.samples))
        .map(|p| p[1]);
    let (mut lo, mut hi) = (
        values.clone().fold(f64::INFINITY, f64::min),
        values.fold(f64::NEG_INFINITY, f64::max),
    );
    let model = profile.fit.map(|fit| {
        (0..=96)
            .map(|i| {
                let x = first[0] + (last[0] - first[0]) * f64::from(i) / 96.;
                let y = fit.intensity_at(x);
                lo = lo.min(y);
                hi = hi.max(y);
                [x, y]
            })
            .collect::<Vec<_>>()
    });
    let pad = ((hi - lo) * 0.05).max(1.);
    lo -= pad;
    hi += pad;
    let xy = |x: f64, y: f64| {
        pos2(
            plot.left() + plot.width() * ((x - first[0]) / (last[0] - first[0])) as f32,
            plot.bottom() - plot.height() * ((y - lo) / (hi - lo)) as f32,
        )
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8., BACKGROUND);
    let grid_line = |y| {
        painter.line_segment(
            [pos2(plot.left(), y), pos2(plot.right(), y)],
            Stroke::new(1., CARD),
        );
    };
    if let Some(atlas) = reference {
        let tick_spacing = if lo > 0.8 * atlas.observed_peak {
            5
        } else {
            20
        };
        for percent in (0..=100).step_by(tick_spacing) {
            let value = atlas.observed_peak * f64::from(percent) / 100.;
            if (lo..=hi).contains(&value) {
                let y = xy(first[0], value).y;
                grid_line(y);
                painter.text(
                    pos2(plot.left() - 4., y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{percent}%"),
                    egui::FontId::proportional(10.),
                    MUTED,
                );
            }
        }
    } else {
        for n in 0..=3 {
            grid_line(plot.top() + plot.height() * n as f32 / 3.);
        }
    }
    let points = |samples: &[[f64; 2]]| samples.iter().map(|p| xy(p[0], p[1])).collect();
    painter.add(egui::Shape::line(
        points(&profile.samples),
        Stroke::new(1.7, CYAN),
    ));
    if let Some((fit, model)) = profile.fit.zip(model.as_ref()) {
        painter.add(egui::Shape::line(points(model), Stroke::new(1.6, AMBER)));
        let left = fit.center - fit.fwhm / 2.;
        let right = fit.center + fit.fwhm / 2.;
        let markers = [
            xy(left, fit.half_depth_at(left)),
            xy(right, fit.half_depth_at(right)),
        ];
        painter.line_segment(markers, Stroke::new(1., TEXT));
        for marker in markers {
            painter.circle_filled(marker, 2.5, TEXT);
        }
    }
    if let Some(atlas) = reference {
        painter.add(egui::Shape::line(
            points(&atlas.samples),
            Stroke::new(1.6, ATLAS_COLOR),
        ));
    }
    painter.text(
        rect.center_bottom() - vec2(0., 4.),
        egui::Align2::CENTER_BOTTOM,
        lang.text("Offset (px)", "偏移（px）"),
        egui::FontId::proportional(11.),
        MUTED,
    );
    for (along, offset, alignment) in [
        (plot.left(), first[0], egui::Align2::LEFT_BOTTOM),
        (plot.right(), last[0], egui::Align2::RIGHT_BOTTOM),
    ] {
        painter.text(
            pos2(along, rect.bottom() - 4.),
            alignment,
            format!("{offset:+.0}"),
            egui::FontId::proportional(11.),
            MUTED,
        );
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(lang.text("Observed", "实测"))
                .size(12.)
                .color(CYAN),
        );
        if profile.fit.is_some() {
            ui.label(
                RichText::new(lang.text("Pseudo-Voigt fit", "伪 Voigt 拟合"))
                    .size(12.)
                    .color(AMBER),
            );
        }
        if reference.is_some() {
            ui.label(
                RichText::new(lang.text("BASS2000 reference", "BASS2000 参考"))
                    .size(12.)
                    .color(ATLAS_COLOR),
            );
        }
    });
}

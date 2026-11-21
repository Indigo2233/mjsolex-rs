use super::*;

const CYAN: Color32 = Color32::from_rgb(131, 208, 225);

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
            ui.label(RichText::new("MJ Sol’Ex").size(27.).strong());
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
                    "In the library, select one original SER and share it to MJ Sol’Ex.",
                    "在素材库中选中一份原始 SER，分享至 MJ Sol’Ex。",
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
                if let Some(fit) = profile.fit {
                    let label = lang.text("FWHM", "半高全宽");
                    let width = if self.dispersion != 0. {
                        format!(
                            "{label}  {:.2} px  ·  {:.3} Å",
                            fit.fwhm,
                            fit.fwhm * self.dispersion.abs()
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
                profile_chart(ui, profile, lang);
            });
        }
        card(ui, |ui| {
            heading(ui, lang.text("Processing options", "处理选项"));
            egui::CollapsingHeader::new(
                RichText::new(lang.text("Wavelength calibration", "波长标定")).strong(),
            )
            .id_salt("wavelength_calibration")
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    ui.label(lang.text("Zero-offset wavelength (Å)", "零偏移波长（Å）"));
                    ui.add(
                        egui::DragValue::new(&mut self.wavelength)
                            .range(0. ..=100_000.)
                            .speed(0.02)
                            .max_decimals(4)
                            .custom_formatter(move |value, _| optional_number(value, lang)),
                    );
                    ui.label(lang.text("Dispersion (Å/px)", "色散（Å/px）"));
                    ui.add(
                        egui::DragValue::new(&mut self.dispersion)
                            .range(-50. ..=50.)
                            .speed(0.001)
                            .max_decimals(5)
                            .custom_formatter(move |value, _| optional_number(value, lang)),
                    );
                    ui.label(
                        RichText::new(lang.text(
                            "Use negative dispersion if wavelength decreases down the frame.",
                            "如果波长从画面顶部向底部递减，请输入负色散。",
                        ))
                        .color(MUTED),
                    );
                });
            });
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.correct_disk,
                    lang.text("Correct full-disk geometry", "校正太阳盘几何形状"),
                ),
            );
            if header.minimum_stride() > 1 {
                let stride = self.stride;
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
                ui.add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.stride)
                        .range(header.minimum_stride()..=header.frames)
                        .prefix(lang.text("Every ", "每 "))
                        .suffix(lang.text(" frames", " 帧取一帧")),
                );
                let (width, height) = (header.width, header.frames.div_ceil(self.stride));
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
            heading(
                ui,
                if output.metadata.geometry.is_some() {
                    lang.text("Corrected solar scan", "已校正的太阳图像")
                } else {
                    lang.text("Reconstructed scan", "重建图像")
                },
            );
            let (w, h) = (output.image.width(), output.image.height());
            let shift = output.metadata.offset_pixels.unwrap_or(0.);
            let size = match lang {
                Language::English => format!("{w} × {h} px · {shift:+.2} px offset"),
                Language::Chinese => format!("{w} × {h} px · 偏移 {shift:+.2} px"),
            };
            ui.label(RichText::new(size).color(MUTED));
            picture(ui, &output.texture, w, h, 0. ..=430., Sense::hover());
            ui.label(
                RichText::new(lang.text(
                    "Screen contrast is stretched. Export keeps 16-bit samples.",
                    "预览已拉伸对比度，导出保留 16 位像素。",
                ))
                .color(MUTED),
            );
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
                let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.;
                ui.horizontal(|ui| {
                    for (label, request, primary) in [
                        (lang.text("Save FITS", "保存 FITS"), SAVE_FITS, true),
                        (lang.text("Save PNG", "保存 PNG"), SAVE_PNG, false),
                    ] {
                        let text = RichText::new(label).size(15.).strong();
                        let button = if primary {
                            egui::Button::new(text.color(BACKGROUND)).fill(AMBER)
                        } else {
                            egui::Button::new(text)
                        };
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

fn profile_chart(ui: &mut Ui, profile: &LineProfile, lang: Language) {
    let (Some(first), Some(last)) = (profile.samples.first(), profile.samples.last()) else {
        return;
    };
    if first[0] == last[0] {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 136.), Sense::hover());
    let plot = egui::Rect::from_min_max(rect.min + vec2(7., 8.), rect.max - vec2(7., 20.));
    let values = profile.samples.iter().map(|p| p[1]);
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
    for n in 0..=3 {
        let y = plot.top() + plot.height() * n as f32 / 3.;
        painter.line_segment(
            [pos2(plot.left(), y), pos2(plot.right(), y)],
            Stroke::new(1., CARD),
        );
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
    ui.horizontal(|ui| {
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
    });
}

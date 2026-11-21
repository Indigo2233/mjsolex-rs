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
        ui.label(RichText::new("MJ Sol’Ex").size(27.).strong());
        let tab_width = (ui.available_width() - 2. * ui.spacing().item_spacing.x) / 3.;
        ui.horizontal(|ui| {
            for (page, name, enabled) in [
                (Page::Scan, "Recording", true),
                (Page::Spectrum, "Spectrum", self.scan.is_some()),
                (Page::Result, "Result", self.output.is_some()),
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
        if let Some(job) = &self.job {
            card(ui, |ui| {
                ui.label(RichText::new(job.label).strong());
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
                    && action(ui, "Cancel", !job.cancel.load(Ordering::Relaxed), false)
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            });
        } else if self.picker == Some(OPEN_SHARED) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Opening the shared recording…");
            });
        }
        if let Some((message, error)) = &self.note {
            ui.add(
                egui::Label::new(RichText::new(message).color(if *error {
                    Color32::LIGHT_RED
                } else {
                    CYAN
                }))
                .wrap(),
            );
        }
    }

    fn scan_page(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        card(ui, |ui| {
            heading(ui, "Solar recording");
            if let Some(scan) = &self.scan {
                ui.add(egui::Label::new(RichText::new(&scan.name).strong()).wrap());
                recording_summary(ui, scan.header);
            } else {
                ui.label(
                    RichText::new("Open a Sol’Ex or other solar scan recorded as SER.")
                        .color(MUTED),
                );
            }
            if action(ui, "Open SER file", !busy, true) {
                self.pick(OPEN_SER);
            }
        });
        card(ui, |ui| {
            heading(ui, "Indigo Observatory");
            ui.label(
                RichText::new("In the library, select one original SER and share it to MJ Sol’Ex.")
                    .color(MUTED),
            );
            if action(ui, "Open Indigo Observatory", !busy, false) {
                self.note = match self.activity_flag("openIndigo") {
                    Ok(true) => None,
                    Ok(false) => Some((
                        "Indigo Observatory is unavailable on this phone.".into(),
                        true,
                    )),
                    Err(error) => Some((format!("Cannot open Indigo Observatory: {error}"), true)),
                };
            }
        });
        let Some(scan) = &self.scan else { return };
        let mut chosen_order = scan.byte_order;
        let current_order = scan.byte_order;
        card(ui, |ui| {
            egui::CollapsingHeader::new(RichText::new("Recording details").strong()).show(
                ui,
                |ui| {
                    for (key, value) in [
                        ("Start UTC", scan.observation.start_utc.as_deref()),
                        ("End UTC", scan.observation.end_utc.as_deref()),
                        ("Recorder", scan.observation.recorder),
                        ("Instrument", scan.observation.instrument.as_deref()),
                        ("Filter", scan.observation.filter.as_deref()),
                        ("Observer", scan.observation.observer.as_deref()),
                        ("Telescope", scan.observation.telescope.as_deref()),
                    ] {
                        if let Some(value) = value {
                            ui.add(egui::Label::new(format!("{key}  {value}")).wrap());
                        }
                    }
                    if scan.header.depth > 8 {
                        ui.label(
                            RichText::new(format!(
                                "Decoded as {} endian",
                                order_label(Some(scan.header.byte_order()))
                            ))
                            .color(MUTED),
                        );
                        ui.add_enabled_ui(!busy, |ui| {
                            egui::ComboBox::from_id_salt("byte_order")
                                .selected_text(order_label(chosen_order))
                                .show_ui(ui, |ui| {
                                    for order in
                                        [None, Some(ByteOrder::Little), Some(ByteOrder::Big)]
                                    {
                                        ui.selectable_value(
                                            &mut chosen_order,
                                            order,
                                            order_label(order),
                                        );
                                    }
                                });
                        });
                    }
                },
            );
        });
        if chosen_order != current_order {
            self.reload_byte_order(chosen_order);
        }
    }

    fn spectrum_page(&mut self, ui: &mut Ui) {
        let Some(scan) = &self.scan else { return };
        let header = scan.header;
        let busy = self.busy();
        let mut selected = None;
        card(ui, |ui| {
            heading(ui, "Average spectrum");
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
            ui.label(RichText::new("Tap a dark line to select it.").color(MUTED));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Auto-fit"))
                    .clicked()
                {
                    selected = Some(scan.spectrum.fit_line(None));
                }
                ui.label(RichText::new("Line").color(AMBER));
                if self.shift != 0. {
                    ui.label(RichText::new("Sampling").color(CYAN));
                }
            });
            offset_controls(ui, &mut self.shift, (header.height - 1) as f64, busy);
        });
        if let Some(selected) = selected {
            self.set_trace(selected);
        }
        if let Some(profile) = &self.profile {
            card(ui, |ui| {
                heading(ui, "Line profile");
                if let Some(fit) = profile.fit {
                    let width = if self.dispersion != 0. {
                        format!(
                            "FWHM  {:.2} px  ·  {:.3} Å",
                            fit.fwhm,
                            fit.fwhm * self.dispersion.abs()
                        )
                    } else {
                        format!("FWHM  {:.2} px", fit.fwhm)
                    };
                    ui.label(RichText::new(width).size(17.).color(AMBER).strong());
                } else {
                    ui.label(
                        RichText::new("No reliable FWHM. Select a line with both wings.")
                            .color(MUTED),
                    );
                }
                profile_chart(ui, profile);
            });
        }
        card(ui, |ui| {
            heading(ui, "Processing options");
            egui::CollapsingHeader::new(RichText::new("Wavelength calibration").strong()).show(
                ui,
                |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label("Zero-offset wavelength (Å)");
                        ui.add(
                            egui::DragValue::new(&mut self.wavelength)
                                .range(0. ..=100_000.)
                                .speed(0.02)
                                .max_decimals(4)
                                .custom_formatter(optional_number),
                        );
                        ui.label("Dispersion (Å/px)");
                        ui.add(
                            egui::DragValue::new(&mut self.dispersion)
                                .range(-50. ..=50.)
                                .speed(0.001)
                                .max_decimals(5)
                                .custom_formatter(optional_number),
                        );
                        ui.label(
                            RichText::new(
                                "Use negative dispersion if wavelength decreases down the frame.",
                            )
                            .color(MUTED),
                        );
                    });
                },
            );
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.correct_disk, "Correct full-disk geometry"),
            );
            if header.minimum_stride() > 1 {
                ui.label(
                    RichText::new(format!("Large scan · using every {} frames", self.stride))
                        .color(MUTED),
                );
            }
            egui::CollapsingHeader::new(RichText::new("Scan sampling").strong()).show(ui, |ui| {
                ui.add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.stride)
                        .range(header.minimum_stride()..=header.frames)
                        .prefix("Every ")
                        .suffix(" frames"),
                );
                ui.label(
                    RichText::new(format!(
                        "{} × {} px before geometry correction",
                        header.width,
                        header.frames.div_ceil(self.stride)
                    ))
                    .color(MUTED),
                );
            });
        });
    }

    fn result_page(&self, ui: &mut Ui) {
        let Some(output) = &self.output else { return };
        card(ui, |ui| {
            heading(
                ui,
                if output.metadata.geometry.is_some() {
                    "Corrected solar scan"
                } else {
                    "Reconstructed scan"
                },
            );
            let (w, h) = (output.image.width(), output.image.height());
            let shift = output.metadata.offset_pixels.unwrap_or(0.);
            ui.label(RichText::new(format!("{w} × {h} px · {shift:+.2} px offset")).color(MUTED));
            picture(ui, &output.texture, w, h, 0. ..=430., Sense::hover());
            ui.label(
                RichText::new("Screen contrast is stretched. Export keeps 16-bit samples.")
                    .color(MUTED),
            );
        });
    }

    fn page_actions(&mut self, ui: &mut Ui) {
        let busy = self.busy();
        match self.page {
            Page::Spectrum => {
                if action(ui, "Reconstruct scan", !busy && self.trace.is_some(), true) {
                    self.reconstruct();
                }
            }
            Page::Result => {
                let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.;
                ui.horizontal(|ui| {
                    for (label, request, primary) in [
                        ("Save FITS", SAVE_FITS, true),
                        ("Save PNG", SAVE_PNG, false),
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

fn recording_summary(ui: &mut Ui, header: Header) {
    ui.label(
        RichText::new(format!(
            "{} × {} px · {} frames",
            header.width, header.height, header.frames
        ))
        .color(MUTED),
    );
    ui.label(
        RichText::new(format!("{}-bit · {}", header.depth, header.color.label())).color(MUTED),
    );
}

fn order_label(order: Option<ByteOrder>) -> &'static str {
    match order {
        None => "Auto",
        Some(ByteOrder::Little) => "Little",
        Some(ByteOrder::Big) => "Big",
    }
}

fn offset_controls(ui: &mut Ui, shift: &mut f64, limit: f64, busy: bool) {
    ui.add_enabled_ui(!busy, |ui| {
        ui.horizontal(|ui| {
            ui.label("Offset");
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
            if *shift != 0. && ui.button("Reset").clicked() {
                *shift = 0.;
            }
        });
    });
}

fn optional_number(value: f64, _: std::ops::RangeInclusive<usize>) -> String {
    if value == 0. {
        "Unset".into()
    } else {
        format!("{value:.5}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn profile_chart(ui: &mut Ui, profile: &LineProfile) {
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
        "Offset (px)",
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
        ui.label(RichText::new("Observed").size(12.).color(CYAN));
        if profile.fit.is_some() {
            ui.label(RichText::new("Pseudo-Voigt fit").size(12.).color(AMBER));
        }
    });
}

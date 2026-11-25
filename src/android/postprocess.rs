use super::*;
use crate::scan_correction::ScanPlan;

#[derive(Clone, Copy)]
pub(super) struct ProcessingOptions {
    pub(super) scan: ScanSettings,
    pub(super) correct_disk: bool,
    pub(super) synthetic_strength: u8,
    pub(super) framing: Framing,
    pub(super) enhance: bool,
    pub(super) negative: bool,
    pub(super) eclipse: bool,
    pub(super) invert_prominences: bool,
    pub(super) prominence_strength: u8,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            scan: ScanSettings::default(),
            correct_disk: true,
            synthetic_strength: 100,
            framing: Framing::default(),
            enhance: true,
            negative: false,
            eclipse: false,
            invert_prominences: false,
            prominence_strength: 100,
        }
    }
}

#[derive(Default)]
pub(super) struct CorrectionWarnings {
    pub(super) flat: Option<Error>,
    pub(super) scan: Option<Error>,
    pub(super) disk: Option<Error>,
    pub(super) framing: Option<Error>,
    pub(super) enhancement: Option<Error>,
    pub(super) negative: Option<Error>,
    pub(super) eclipse: Option<Error>,
    pub(super) doppler: Option<Error>,
}

impl CorrectionWarnings {
    pub(super) fn has_errors(&self) -> bool {
        self.entries().iter().any(|(error, ..)| error.is_some())
    }

    pub(super) fn entries(&self) -> [(&Option<Error>, &'static str, &'static str); 8] {
        [
            (&self.flat, "Synthetic flat skipped. ", "已跳过虚拟平场。"),
            (
                &self.scan,
                "Scan corrections skipped. ",
                "已跳过扫描伪影校正。",
            ),
            (
                &self.disk,
                "Disk correction skipped. ",
                "已跳过太阳盘几何校正。",
            ),
            (
                &self.framing,
                "Some framing settings could not be applied. ",
                "部分构图设置未能应用。",
            ),
            (
                &self.enhancement,
                "Enhanced images skipped. ",
                "已跳过增强图。",
            ),
            (
                &self.doppler,
                "Doppler image skipped. ",
                "已跳过双翼多普勒图。",
            ),
            (
                &self.negative,
                "JSol'Ex negative skipped. ",
                "已跳过 JSol'Ex 负片。",
            ),
            (
                &self.eclipse,
                "Simulated eclipse skipped. ",
                "已跳过模拟日食。",
            ),
        ]
    }
}

pub(super) fn doppler_shift(
    spectrum: &Spectrum,
    trace: Trace,
    shift: f64,
    wing_offset: f64,
    dispersion: Option<f64>,
) -> Result<f64, Error> {
    let dispersion = dispersion.ok_or(Error::DopplerDispersion)?;
    if !spectrum.supports_wings(trace, shift, wing_offset) {
        return Err(Error::DopplerCoverage);
    }
    Ok(wing_offset.copysign(dispersion))
}

#[derive(Clone)]
pub(super) struct ReconstructionOptions {
    pub(super) wavelength: f64,
    pub(super) dispersion: Option<f64>,
    pub(super) shift: f64,
    pub(super) stride: u32,
    pub(super) doppler: bool,
    pub(super) wing_offset: f64,
    pub(super) palettes: [bool; 3],
    pub(super) processing: ProcessingOptions,
    pub(super) synthetic_flat: bool,
    pub(super) physical_flat: Option<(Arc<PhysicalFlat>, String)>,
}

impl ReconstructionOptions {
    pub(super) fn wing_shift(
        &self,
        spectrum: &Spectrum,
        trace: Trace,
    ) -> Result<Option<f64>, Error> {
        self.doppler
            .then(|| {
                doppler_shift(
                    spectrum,
                    trace,
                    self.shift,
                    self.wing_offset,
                    self.dispersion,
                )
            })
            .transpose()
    }

    pub(super) fn build(
        &self,
        mut reader: SerReader<&File>,
        name: String,
        trace: Trace,
        fwhm: Option<f64>,
        wing_shift: Result<Option<f64>, Error>,
        mut progress: impl FnMut(f32) -> bool,
    ) -> Result<(Images, CorrectionWarnings), Error> {
        let header = reader.header();
        let (wing_shift, doppler) = match wing_shift {
            Ok(offset) => (offset, None),
            Err(error) => (None, Some(error)),
        };
        let stride = self.stride.max(if wing_shift.is_some() {
            header.minimum_doppler_stride()
        } else {
            header.minimum_stride()
        });
        let mut metadata = FitsMetadata {
            observation: reader.observation().clone(),
            source_name: Some(name),
            source_header: Some(header),
            offset_pixels: Some(self.shift),
            frame_step: Some(stride),
            line_center_angstrom: (self.wavelength > 0.).then_some(self.wavelength),
            dispersion_angstrom: self.dispersion,
            fwhm_pixels: fwhm,
            ..FitsMetadata::default()
        };
        let mut warnings = CorrectionWarnings {
            doppler,
            ..CorrectionWarnings::default()
        };
        let corrections_start = 0.8;
        let flat_start = if self.synthetic_flat || self.physical_flat.is_some() {
            corrections_start - 0.10
        } else {
            corrections_start
        };
        let shift = self.shift;
        let (mut center, mut wings) = if let Some(wing) = wing_shift {
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
        if self.synthetic_flat {
            let strength = self.processing.synthetic_strength;
            match center.correct_synthetic_flat_at_strength(strength, |p| {
                progress(flat_start + p * (corrections_start - flat_start))
            }) {
                Ok(applied) => metadata.synthetic_flat_strength = applied.then_some(strength),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => warnings.flat = Some(error),
            }
        } else if let Some((flat, name)) = &self.physical_flat {
            flat.apply(&mut center)?;
            if let Some(wings) = &mut wings {
                for wing in wings {
                    flat.apply(wing)?;
                }
            }
            metadata.flat_file = Some(name.clone());
        }
        if !progress(corrections_start) {
            return Err(Error::Cancelled);
        }
        let mut images = Images {
            center,
            wings,
            palettes: self.palettes,
            wing_offset: self.wing_offset,
            metadata,
            enhancement: None,
            negative: None,
            eclipse: None,
        };
        finish(&mut images, self.processing, &mut warnings, |p| {
            progress(corrections_start + (1. - corrections_start) * p)
        })?;
        Ok((images, warnings))
    }
}

pub(super) fn finish(
    images: &mut Images,
    options: ProcessingOptions,
    warnings: &mut CorrectionWarnings,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<(), Error> {
    let count = images.count() as f32;
    let budget = if images.wings.is_some() {
        MAX_DOPPLER_WING_PIXELS
    } else {
        MAX_PIXELS
    };
    if options.scan.enabled() {
        match ScanPlan::prepare(&images.center, options.scan, |p| progress(0.18 * p)) {
            Ok(plan) => {
                let report = plan.report;
                if report.banding != BandingAxis::Off
                    || report.jagged
                    || report.oscillation.is_some()
                {
                    let mut apply = |image, index| {
                        plan.apply(image, |p| progress(0.18 + 0.22 * (index + p) / count))
                    };
                    apply(&mut images.center, 0.)?;
                    if let Some([red, blue]) = &mut images.wings {
                        apply(red, 1.)?;
                        apply(blue, 2.)?;
                    }
                    images.metadata.scan_corrections = report;
                }
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.scan = Some(error),
        }
    }
    if options.correct_disk {
        let corrected = DiskGeometry::fit(&images.center).and_then(|geometry| {
            let scale_error = (geometry.slit_radius - geometry.scan_radius).abs();
            let shear_error = geometry.shear.abs() * geometry.scan_radius;
            // Keep the existing sampling grid when the fitted limb barely moves.
            if scale_error.max(shear_error) <= 1.5 {
                return Ok(None);
            }
            images.transform(|image, index| {
                geometry.correct_with_budget(image, budget, |p| {
                    progress(0.4 + 0.26 * (index as f32 + p) / count)
                })
            })?;
            Ok(Some(geometry))
        });
        match corrected {
            Ok(geometry) => images.metadata.geometry = geometry,
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.disk = Some(error),
        }
    }
    let mut apply_frame = |images: &mut Images, framing: Framing| {
        if let Some(transform) = framing.prepare(&images.center, budget)? {
            images.transform(|image, index| {
                transform.apply(image, |p| {
                    progress(0.66 + 0.23 * (index as f32 + p) / count)
                })
            })?;
            images.metadata.framing = framing;
        }
        Ok::<_, Error>(())
    };
    // If crop planning fails, the independently selected orientation can still be applied.
    match apply_frame(images, options.framing) {
        Ok(()) => {}
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(error) => {
            warnings.framing = Some(error);
            if options.framing.crop_factor.is_some() {
                let orientation = Framing {
                    crop_factor: None,
                    ..options.framing
                };
                if let Err(Error::Cancelled) = apply_frame(images, orientation) {
                    return Err(Error::Cancelled);
                }
            }
        }
    }
    let product_count =
        u8::from(options.enhance) + u8::from(options.negative) + u8::from(options.eclipse);
    let step = 0.11 / f32::from(product_count.max(1));
    let mut start = 0.89;
    if options.enhance {
        match LocalContrast::prepare(&images.center, |p| progress(start + step * p)) {
            Ok(contrast) => images.enhancement = Some(contrast),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.enhancement = Some(error),
        }
        start += step;
    }
    if options.negative {
        let negative_options = NegativeOptions {
            invert_prominences: options.invert_prominences,
            prominence_strength: options.prominence_strength,
        };
        match reconstruct_negative_with_options(&images.center, negative_options, |p| {
            progress(start + step * p)
        }) {
            Ok(negative) => images.negative = Some(negative),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.negative = Some(error),
        }
        start += step;
    }
    if options.eclipse {
        match reconstruct_eclipse(&images.center, |p| progress(start + step * p)) {
            Ok(eclipse) => images.eclipse = Some(eclipse),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.eclipse = Some(error),
        }
    }
    Ok(())
}

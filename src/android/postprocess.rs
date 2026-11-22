use super::*;
use crate::scan_correction::ScanPlan;

#[derive(Clone, Copy)]
pub(super) struct ProcessingOptions {
    pub(super) scan: ScanSettings,
    pub(super) correct_disk: bool,
    pub(super) synthetic_strength: u8,
    pub(super) framing: Framing,
    pub(super) enhance: bool,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            scan: ScanSettings::default(),
            correct_disk: true,
            synthetic_strength: 100,
            framing: Framing::default(),
            enhance: true,
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
    pub(super) doppler: Option<Error>,
}

impl CorrectionWarnings {
    pub(super) fn entries(&self) -> [(&Option<Error>, &'static str, &'static str); 6] {
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
        ]
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
    if options.enhance {
        match LocalContrast::prepare(&images.center, |p| progress(0.89 + 0.11 * p)) {
            Ok(contrast) => images.enhancement = Some(contrast),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.enhancement = Some(error),
        }
    }
    Ok(())
}

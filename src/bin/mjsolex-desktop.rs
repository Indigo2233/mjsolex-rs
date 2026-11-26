use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mjsolex::pipeline::{ProcessingMemory, ProcessingOptions, ReconstructionOptions};
use mjsolex::{
    BandingAxis, ByteOrder, DisplaySettings, DisplayTone, PhysicalFlat, SerReader, Spectrum, Trace,
};

const USAGE: &str = "Usage: mjsolex-desktop INPUT.ser NEW_OUTPUT_DIRECTORY [OPTIONS]

Input and sampling:
  --trace-point=X,Y | --trace-row=Y    Select a line seed or a fixed trace
  --shift=PX  --step=N  --byte-order=little|big
  --headroom-mib=N                    Match Android's available-memory input

Corrections:
  --synthetic-flat[=PERCENT] | --flat=PATH
  --banding=off|auto|vertical|horizontal  --band-width=PX
  --align  --oscillation  --no-geometry
  --flip-x  --flip-y  --rotate=DEGREES  --crop=FACTOR

Products:
  --negative  --invert-prominences[=PERCENT]  --eclipse
  --doppler=PX  --dispersion=ANGSTROM_PER_PX  --wavelength=ANGSTROM
  --palette=none|ha|ca|he|all  --no-enhance  --tone=standard|dark|bright
  --strict                           Exit with an error on processing warnings

Writes the same PNG and FITS products as Android's Save All action.";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if first.is_none()
        || first
            .as_deref()
            .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let input = PathBuf::from(first.unwrap());
    let output = PathBuf::from(
        args.next()
            .ok_or("Specify a new output directory. See --help.")?,
    );
    let mut options = ReconstructionOptions {
        wavelength: 0.,
        dispersion: None,
        shift: 0.,
        stride: 1,
        memory: ProcessingMemory::default(),
        doppler: false,
        wing_offset: 2.,
        palettes: [true, false, false],
        processing: ProcessingOptions::default(),
        synthetic_flat: false,
        physical_flat: None,
    };
    let mut seed = None;
    let mut fixed_row = None;
    let mut order = None;
    let mut flat = None;
    let mut strict = false;
    let mut display = DisplaySettings::default();

    for arg in args {
        let arg = arg.to_str().ok_or("Options must be valid UTF-8")?;
        let (flag, value) = arg
            .split_once('=')
            .map_or((arg, None), |(flag, val)| (flag, Some(val)));
        let required = || value.ok_or_else(|| format!("{flag} requires a value"));
        let processing = &mut options.processing;
        match flag {
            "--trace-point" => {
                let (x, y) = required()?.split_once(',').ok_or("Use --trace-point=X,Y")?;
                seed = Some((x.parse()?, y.parse()?));
            }
            "--trace-row" => fixed_row = Some(finite(required()?)?),
            "--shift" => options.shift = finite(required()?)?,
            "--step" => {
                options.stride = required()?.parse()?;
                if options.stride == 0 {
                    return Err("Frame interval must be at least one".into());
                }
            }
            "--byte-order" => {
                order = Some(match required()? {
                    "little" => ByteOrder::Little,
                    "big" => ByteOrder::Big,
                    _ => return Err("Byte order must be little or big".into()),
                })
            }
            "--headroom-mib" => {
                let mib: u64 = required()?.parse()?;
                options.memory = ProcessingMemory::with_headroom(
                    mib.checked_mul(1_048_576)
                        .ok_or("Memory value is too large")?,
                );
            }
            "--synthetic-flat" => {
                options.synthetic_flat = true;
                processing.synthetic_strength = percent(value)?;
            }
            "--flat" => flat = Some(PathBuf::from(required()?)),
            "--banding" => {
                processing.scan.banding = match required()? {
                    "off" => BandingAxis::Off,
                    "auto" => BandingAxis::Both,
                    "vertical" => BandingAxis::Slit,
                    "horizontal" => BandingAxis::Scan,
                    _ => return Err("Banding must be off, auto, vertical or horizontal".into()),
                }
            }
            "--band-width" => {
                let width = required()?.parse()?;
                if !(7..=51).contains(&width) {
                    return Err("Band width must be 7–51 pixels".into());
                }
                processing.scan.banding_width = Some(width);
            }
            "--align" if value.is_none() => processing.scan.jagged = true,
            "--oscillation" if value.is_none() => processing.scan.oscillation = true,
            "--no-geometry" if value.is_none() => processing.correct_disk = false,
            "--flip-x" if value.is_none() => processing.framing.flip_x = true,
            "--flip-y" if value.is_none() => processing.framing.flip_y = true,
            "--rotate" => processing.framing.rotation_deg = finite(required()?)?,
            "--crop" => {
                let factor = finite(required()?)?;
                if !(1. ..=2.).contains(&factor) {
                    return Err("Crop factor must be 1–2".into());
                }
                processing.framing.crop_factor = Some(factor);
            }
            "--negative" if value.is_none() => processing.negative = true,
            "--invert-prominences" => {
                processing.negative = true;
                processing.invert_prominences = true;
                processing.prominence_strength = percent(value)?;
            }
            "--eclipse" if value.is_none() => processing.eclipse = true,
            "--doppler" => {
                options.doppler = true;
                options.wing_offset = finite(required()?)?;
                if options.wing_offset <= 0. {
                    return Err("Doppler wing offset must be positive".into());
                }
            }
            "--dispersion" => {
                let dispersion = finite(required()?)?;
                if dispersion == 0. {
                    return Err("Dispersion must be nonzero".into());
                }
                options.dispersion = Some(dispersion);
            }
            "--wavelength" => {
                options.wavelength = finite(required()?)?;
                if options.wavelength <= 0. {
                    return Err("Wavelength must be positive".into());
                }
            }
            "--palette" => {
                options.palettes = match required()? {
                    "none" => [false; 3],
                    "ha" => [true, false, false],
                    "ca" => [false, true, false],
                    "he" => [false, false, true],
                    "all" => [true; 3],
                    _ => return Err("Palette must be none, ha, ca, he or all".into()),
                }
            }
            "--no-enhance" if value.is_none() => processing.enhance = false,
            "--tone" => {
                display.tone = match required()? {
                    "standard" => DisplayTone::Standard,
                    "dark" => DisplayTone::Darker,
                    "bright" => DisplayTone::Brighter,
                    _ => return Err("Tone must be standard, dark or bright".into()),
                }
            }
            "--strict" if value.is_none() => strict = true,
            _ => return Err(format!("Unknown option {arg}. See --help.").into()),
        }
    }
    if seed.is_some() && fixed_row.is_some() || flat.is_some() && options.synthetic_flat {
        return Err("Select one trace mode and one flat mode".into());
    }
    if output.exists() {
        return Err(format!("Output directory already exists: {}", output.display()).into());
    }

    let start = Instant::now();
    let mut reader = SerReader::open_with_byte_order(File::open(&input)?, order)?;
    let header = reader.header();
    if seed.is_some_and(|(x, y)| x >= header.width || y >= header.height) {
        return Err("Trace seed is outside the frame".into());
    }
    let spectrum = Spectrum::average(&mut reader, |_| true)?;
    let trace = if let Some(row) = fixed_row {
        Trace::constant(header.width, row)
    } else {
        spectrum.fit_line(seed)?
    };
    let fwhm = spectrum.line_profile(trace).fit.map(|fit| fit.fwhm);
    let wings = options.wing_shift(&spectrum, trace);
    if let Some(flat) = flat {
        let model = PhysicalFlat::from_recording(
            File::open(&flat)?,
            &spectrum,
            reader.observation(),
            order,
            |_| true,
        )?;
        options.physical_flat = Some((Arc::new(model), file_name(&flat)));
    }
    drop(spectrum);
    let (images, warnings) =
        options.build(reader, file_name(&input), trace, fwhm, wings, |_| true)?;
    let info = images.metadata();
    let mut report = vec![format!(
        "Arguments: {:?}",
        std::env::args_os().skip(1).collect::<Vec<_>>()
    )];
    report.push(format!(
        "SER {} × {} · {} frames · {} {}-bit {:?}",
        header.width,
        header.height,
        header.frames,
        header.color.label(),
        header.depth,
        header.byte_order()
    ));
    report.push(format!(
        "Sampled every {} frames · center trace {:.3} px · shift {:+.3} px · FWHM {:?} px",
        info.frame_step.unwrap_or(1),
        trace.row(header.width / 2),
        options.shift,
        fwhm
    ));
    report.push(format!(
        "{} × {} px · flat {:?} / {:?} · scan {:?} · disk {:?} · framing {:?} · {:.3}s",
        images.center().width(),
        images.center().height(),
        info.flat_file,
        info.synthetic_flat_strength,
        info.scan_corrections,
        info.geometry,
        info.framing,
        start.elapsed().as_secs_f64()
    ));
    for line in &report {
        println!("{line}");
    }
    for (error, label, _) in warnings.entries() {
        if let Some(error) = error {
            eprintln!("{label}{error}");
            report.push(format!("Warning: {label}{error}"));
        }
    }
    if strict && warnings.has_errors() {
        return Err("A requested processing stage reported a warning".into());
    }
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&output)?;
    let mut log = File::create(output.join("run.log"))?;
    for line in &report {
        writeln!(log, "{line}")?;
    }
    for export in images.exports(display) {
        let path = output.join(images.file_name(export));
        let mut file = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?,
        );
        images.write(export, &mut file)?;
        file.flush()?;
        println!("Saved {}", path.display());
        writeln!(log, "Saved {}", path.display())?;
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn percent(value: Option<&str>) -> Result<u8, Box<dyn Error>> {
    let value = value.unwrap_or("100").parse()?;
    if value > 100 {
        return Err("Strength must be 0–100 percent".into());
    }
    Ok(value)
}

fn finite(value: &str) -> Result<f64, Box<dyn Error>> {
    let number: f64 = value.parse()?;
    if !number.is_finite() {
        return Err("Expected a finite number".into());
    }
    Ok(number)
}

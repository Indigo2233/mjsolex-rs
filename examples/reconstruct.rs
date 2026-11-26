//! Export a single linear reconstruction. Use mjsolex-desktop for all app products.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use mjsolex::{
    ByteOrder, DiskGeometry, FitsMetadata, SerReader, Spectrum, Trace, reconstruct_with_stride,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        return Err("usage: reconstruct INPUT.ser OUTPUT.png|.fits [OFFSET_PIXELS] [--trace-row=ROW] [--step=STEP] [--byte-order=little|big] [--correct-disk] [--synthetic-flat]".into());
    };
    let mut offset = None;
    let mut row = None;
    let mut step = None;
    let mut order = None;
    let mut correct_disk = false;
    let mut synthetic_flat = false;
    for arg in args {
        let arg = arg.to_str().ok_or("options must be valid UTF-8")?;
        if arg == "--correct-disk" && !correct_disk {
            correct_disk = true;
        } else if arg == "--synthetic-flat" && !synthetic_flat {
            synthetic_flat = true;
        } else if let Some(value) = arg.strip_prefix("--trace-row=")
            && row.is_none()
        {
            row = Some(value.parse()?);
        } else if let Some(value) = arg.strip_prefix("--step=")
            && step.is_none()
        {
            step = Some(value.parse()?);
        } else if let Some(value) = arg.strip_prefix("--byte-order=")
            && order.is_none()
        {
            order = Some(match value {
                "little" => ByteOrder::Little,
                "big" => ByteOrder::Big,
                _ => return Err("byte order must be little or big".into()),
            });
        } else if offset.is_none() {
            offset = Some(arg.parse::<f64>()?);
        } else {
            return Err(format!("unexpected option {arg}").into());
        }
    }

    let source_name = Path::new(&input)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let fits = Path::new(&output)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .ok_or("output needs a .png or .fits extension")?;
    if !matches!(fits.as_str(), "fits" | "png") {
        return Err("output needs a .png or .fits extension".into());
    }
    let mut reader = SerReader::open_with_byte_order(File::open(input)?, order)?;
    let header = reader.header();
    let spectrum = Spectrum::average(&mut reader, |_| true)?;
    let trace = match row {
        Some(row) => Trace::constant(header.width, row),
        None => spectrum.fit_line(None)?,
    };
    let trace_center = trace.row(header.width / 2);
    let step = step.unwrap_or_else(|| header.minimum_stride());
    let shift = offset.unwrap_or(0.);
    let fit = spectrum.line_profile(trace).fit;
    drop(spectrum);
    let mut raw = reconstruct_with_stride(&mut reader, trace, shift, step, |_| true)?;
    let mut metadata = FitsMetadata {
        observation: reader.observation().clone(),
        source_header: Some(header),
        source_name,
        frame_step: Some(step),
        offset_pixels: Some(shift),
        fwhm_pixels: fit.map(|fit| fit.fwhm),
        ..FitsMetadata::default()
    };
    drop(reader);
    if synthetic_flat {
        let start = std::time::Instant::now();
        let applied = raw.correct_synthetic_flat(|_| true)?;
        println!(
            "Synthetic flat applied: {applied}, {:.2} ms",
            start.elapsed().as_secs_f64() * 1_000.
        );
    }
    let image = if correct_disk {
        let disk = DiskGeometry::fit(&raw)?;
        metadata.geometry = Some(disk);
        println!(
            "slit radius {:.3}, scan radius {:.3}, shear {:.5}",
            disk.slit_radius, disk.scan_radius, disk.shear
        );
        disk.correct(&raw, |_| true)?
    } else {
        raw
    };
    let mut output = BufWriter::new(File::create(output)?);
    if fits == "fits" {
        image.write_fits_with_metadata(&mut output, &metadata)?;
    } else {
        image.write_png(&mut output)?;
    }
    output.flush()?;
    println!(
        "{} × {} pixels, trace at center row {:.3}",
        image.width(),
        image.height(),
        trace_center
    );
    println!(
        "SER {}, {}-bit {:?}, frame interval {step}",
        header.color.label(),
        header.depth,
        header.byte_order()
    );
    if let Some(fit) = fit {
        println!("FWHM {:.3} px", fit.fwhm);
    }
    Ok(())
}

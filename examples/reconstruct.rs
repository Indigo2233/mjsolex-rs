//! Run the same processing pipeline as the Android app on a local test file.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use mjsolex::{DiskGeometry, SerReader, Spectrum, Trace, reconstruct};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        return Err("usage: reconstruct INPUT.ser OUTPUT.png|.fits [OFFSET_PIXELS] [--trace-row=ROW] [--correct-disk]".into());
    };
    let mut offset = None;
    let mut row = None;
    let mut correct_disk = false;
    for arg in args {
        let arg = arg.to_str().ok_or("options must be valid UTF-8")?;
        if arg == "--correct-disk" && !correct_disk {
            correct_disk = true;
        } else if let Some(value) = arg.strip_prefix("--trace-row=")
            && row.is_none()
        {
            row = Some(value.parse()?);
        } else if offset.is_none() {
            offset = Some(arg.parse::<f64>()?);
        } else {
            return Err(format!("unexpected option {arg}").into());
        }
    }

    let mut reader = SerReader::open(File::open(input)?)?;
    let trace = match row {
        Some(row) => Trace::constant(reader.header().width, row),
        None => Spectrum::average(&mut reader, |_| true)?.fit_line(None)?,
    };
    let trace_center = trace.row(reader.header().width / 2);
    let raw = reconstruct(&mut reader, trace, offset.unwrap_or(0.), |_| true)?;
    let image = if correct_disk {
        let disk = DiskGeometry::fit(&raw)?;
        println!(
            "slit radius {:.3}, scan radius {:.3}, shear {:.5}",
            disk.slit_radius, disk.scan_radius, disk.shear
        );
        disk.correct(&raw, |_| true)?
    } else {
        raw
    };
    let fits = Path::new(&output)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("fits"));
    let mut output = BufWriter::new(File::create(output)?);
    if fits {
        image.write_fits(&mut output)?;
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
    Ok(())
}

//! Run the same processing pipeline as the Android app on a local test file.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

use mjsolex::{SerReader, Spectrum, reconstruct};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        return Err("usage: reconstruct INPUT.ser OUTPUT.png [OFFSET_PIXELS]".into());
    };
    let offset = args
        .next()
        .map(|arg| {
            arg.to_str()
                .ok_or("offset must be valid UTF-8")?
                .parse::<f64>()
                .map_err(|_| "invalid offset")
        })
        .transpose()?
        .unwrap_or(0.);
    if args.next().is_some() {
        return Err("expected INPUT.ser OUTPUT.png [OFFSET_PIXELS]".into());
    }

    let mut reader = SerReader::open(File::open(input)?)?;
    let spectrum = Spectrum::average(&mut reader, |_| true)?;
    let trace = spectrum.fit_line(None)?;
    let image = reconstruct(&mut reader, trace, offset, |_| true)?;
    let mut output = BufWriter::new(File::create(output)?);
    image.write_png(&mut output)?;
    output.flush()?;
    println!(
        "{} × {} pixels, trace at center row {:.3}",
        image.width(),
        image.height(),
        trace.row(image.width() / 2)
    );
    Ok(())
}

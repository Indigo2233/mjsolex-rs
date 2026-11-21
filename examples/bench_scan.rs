//! Measure average spectrum and raw reconstruction separately against a local SER file.

use std::error::Error;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use mjsolex::{SerReader, Spectrum, Trace, reconstruct};

#[derive(Default)]
struct Counts {
    bytes: u64,
    seeks: u64,
}

struct Counted<'a> {
    file: File,
    counts: &'a mut Counts,
}

impl Read for Counted<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buffer)?;
        self.counts.bytes += n as u64;
        Ok(n)
    }
}

impl Seek for Counted<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.counts.seeks += 1;
        self.file.seek(position)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: bench_scan INPUT.ser [REPEATS] [TRACE_ROW]")?;
    let repeats: usize = args
        .next()
        .map(|arg| arg.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(5);
    let trace_row = args
        .next()
        .map(|arg| arg.to_string_lossy().parse())
        .transpose()?;
    if repeats == 0 || args.next().is_some() {
        return Err(
            "expected INPUT.ser, a positive repeat count, and an optional trace row".into(),
        );
    }

    let mut average_times = Vec::new();
    let mut reconstruction_times = Vec::new();
    let mut final_counts = Counts::default();
    let mut checksum = 0;
    for run in 0..=repeats {
        let mut counts = Counts::default();
        let mut reader = SerReader::open(Counted {
            file: File::open(&path)?,
            counts: &mut counts,
        })?;

        let start = Instant::now();
        let spectrum = Spectrum::average(&mut reader, |_| true)?;
        let trace = match trace_row {
            Some(row) => Trace::constant(reader.header().width, row),
            None => spectrum.fit_line(None)?,
        };
        if run > 0 {
            average_times.push(start.elapsed());
        }
        drop(reader);

        let mut counts = Counts::default();
        let mut reader = SerReader::open(Counted {
            file: File::open(&path)?,
            counts: &mut counts,
        })?;
        let start = Instant::now();
        let image = reconstruct(&mut reader, trace, 0., |_| true)?;
        if run > 0 {
            reconstruction_times.push(start.elapsed());
        }
        if run == repeats {
            checksum = image.pixels().iter().fold(0u64, |hash, pixel| {
                hash.wrapping_mul(1_099_511_628_211) ^ u64::from(*pixel)
            });
            drop(reader);
            final_counts = counts;
        }
    }
    let median_ms = |mut times: Vec<Duration>| {
        times.sort_unstable();
        times[times.len() / 2].as_secs_f64() * 1000.
    };
    println!(
        "average and trace median ms: {:.2}",
        median_ms(average_times)
    );
    println!(
        "reconstruction median ms: {:.2}",
        median_ms(reconstruction_times)
    );
    println!(
        "reconstruction input bytes (including header): {}",
        final_counts.bytes
    );
    println!(
        "reconstruction seeks (including header): {}",
        final_counts.seeks
    );
    println!("pixel checksum: {checksum:016x}");
    Ok(())
}

#![deny(unsafe_op_in_unsafe_fn)]

mod reconstruct;
mod ser;
mod spectrum;

#[cfg(target_os = "android")]
mod android;

pub use reconstruct::{Reconstruction, reconstruct};
pub use ser::{Header, SerReader};
pub use spectrum::{Spectrum, Trace};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid SER: {0}")]
    InvalidSer(&'static str),
    #[error("SER color mode {0} is unsupported. Use a monochrome recording.")]
    Color(u32),
    #[error("No reliable absorption line found. Select a dark line in the spectrum.")]
    NoLine,
    #[error("The spectral line or offset lies outside the recording.")]
    InvalidTrace,
    #[error("The reconstruction exceeds the supported 32 megapixels.")]
    OutputTooLarge,
    #[error("Processing cancelled.")]
    Cancelled,
    #[error("PNG export failed: {0}")]
    Png(#[from] png::EncodingError),
}

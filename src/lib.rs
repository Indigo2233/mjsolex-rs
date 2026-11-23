#![deny(unsafe_op_in_unsafe_fn)]

mod contrast;
mod display;
mod fits;
mod flat;
mod framing;
mod geometry;
mod negative;
mod observation;
mod optics;
mod profile;
mod reconstruct;
mod regression;
mod scan_correction;
mod ser;
mod spectrum;

#[cfg(any(target_os = "android", test))]
mod solar_atlas;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
mod android_document;

pub use contrast::LocalContrast;
pub use display::{DisplayImage, DisplaySettings, DisplayTone, Product, Stretch};
pub use fits::FitsMetadata;
pub use flat::PhysicalFlat;
pub use framing::Framing;
pub use geometry::DiskGeometry;
pub use negative::{NegativeOptions, reconstruct_negative, reconstruct_negative_with_options};
pub use observation::Observation;
pub use optics::OpticalSetup;
pub use profile::{LineFit, LineProfile};
pub use reconstruct::{Reconstruction, reconstruct, reconstruct_offsets, reconstruct_with_stride};
pub use scan_correction::{BandingAxis, ScanReport, ScanSettings};
pub use ser::{ByteOrder, ColorMode, Header, SerReader};
pub use spectrum::{Spectrum, Trace};

const APP_NAME: &str = "MJSol'Ex";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid SER: {0}")]
    InvalidSer(&'static str),
    #[error("SER color mode {0} is unsupported.")]
    Color(u32),
    #[error("No reliable absorption line found. Select a dark line in the spectrum.")]
    NoLine,
    #[error("The spectral line or offset lies outside the recording.")]
    InvalidTrace,
    #[error("A complete solar limb could not be fitted.")]
    NoSolarLimb,
    #[error("The rotation or crop settings are invalid.")]
    InvalidFraming,
    #[error("Too few usable disk samples for synthetic flat correction.")]
    NoFlatSamples,
    #[error("The flat SER must match the scan dimensions, color mode and bit depth.")]
    FlatMismatch,
    #[error(
        "The scan dimensions, color mode, bit depth and trimming must match the reference SER."
    )]
    SequenceMismatch,
    #[error("The folder contains no SER files.")]
    EmptySequenceFolder,
    #[error("No SER in the folder could be opened as a reference scan.")]
    UnreadableSequenceFolder,
    #[error("The scan uses slit columns outside the captured flat calibration.")]
    FlatCoverage,
    #[error("The flat SER does not illuminate the full slit reliably.")]
    InvalidPhysicalFlat,
    #[error("The scan must cover both wings at the selected Doppler offset.")]
    DopplerCoverage,
    #[error("Set the wavelength dispersion and direction before creating a Doppler image.")]
    DopplerDispersion,
    #[error("The reconstruction exceeds the supported pixel budget.")]
    OutputTooLarge,
    #[error("The frame interval must be at least one.")]
    InvalidStride,
    #[error("Insufficient memory for this scan.")]
    Allocation(#[from] std::collections::TryReserveError),
    #[error("Processing cancelled.")]
    Cancelled,
    #[error(
        "The SER needs {required} bytes of temporary storage, but the cache limit is {limit} bytes."
    )]
    CacheLimit { required: u64, limit: u64 },
    #[error("PNG export failed: {0}")]
    Png(#[from] png::EncodingError),
}

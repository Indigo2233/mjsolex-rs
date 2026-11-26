use crate::Header;
use crate::reconstruct::{MAX_DOPPLER_WING_PIXELS, MAX_PIXELS, transform_image_count};

const MIB: u64 = 1024 * 1024;
const SCRATCH_BYTES: u64 = 16 * MIB;

/// Available system memory above Android's low-memory threshold, sampled with
/// any existing result still resident. Missing readings use the legacy limits.
#[derive(Clone, Copy, Default)]
pub(crate) struct ProcessingMemory(pub(crate) Option<u64>);

impl ProcessingMemory {
    pub(crate) fn image_pixels(self, header: Header, wings: bool, products: usize) -> usize {
        let Some(headroom) = self.0 else {
            return if wings {
                MAX_DOPPLER_WING_PIXELS
            } else {
                MAX_PIXELS
            };
        };
        // Leave the rest to the OS and other work. Reserve a source frame and
        // workspace for resampling, histograms and previews before image data.
        let bytes = (headroom / 4).saturating_sub(SCRATCH_BYTES + header.frame_bytes() as u64);
        let images = transform_image_count(wings) as u64;
        let transforms = 4 * images; // Old and new 16-bit grids coexist.
        let derived = 2 * images + u64::from(products != 0) * (4 + 2 * products as u64);
        let pixels = bytes / transforms.max(derived);
        // Keep dimension casts and u32 pixel histograms within their range.
        pixels.min(u64::from(u32::MAX) / 4) as usize
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::reconstruct::reconstruct_offsets_with_budget;
    use crate::{Error, SerReader, Trace, reconstruct_offsets};

    #[test]
    fn admits_full_sampling_above_the_old_limit_when_memory_allows_it() {
        let (width, frames) = (4096_usize, 9000_u32);
        let mut ser = vec![0; 178 + width * frames as usize];
        ser[..14].copy_from_slice(b"LUCAM-RECORDER");
        ser[26..30].copy_from_slice(&(width as u32).to_le_bytes());
        ser[30..34].copy_from_slice(&1_u32.to_le_bytes());
        ser[34..38].copy_from_slice(&8_u32.to_le_bytes());
        ser[38..42].copy_from_slice(&frames.to_le_bytes());
        let mut reader = SerReader::open(Cursor::new(ser)).unwrap();
        let header = reader.header();
        let trace = Trace::constant(width, 0.);
        let limit = ProcessingMemory(Some(4 * 1024 * MIB)).image_pixels(header, true, 2);
        assert_eq!(header.stride_for_budget(limit), 1);
        assert!(header.minimum_doppler_stride() > 1);
        assert!(matches!(
            reconstruct_offsets(&mut reader, trace, [0.], 1, |_| false),
            Err(Error::OutputTooLarge)
        ));
        assert!(matches!(
            reconstruct_offsets_with_budget(&mut reader, trace, [0.; 3], 1, limit * 3, |_| false),
            Err(Error::Cancelled)
        ));
        let pressured = ProcessingMemory(Some(256 * MIB)).image_pixels(header, true, 2);
        assert!(header.stride_for_budget(pressured) > 1);
        assert_eq!(ProcessingMemory(Some(0)).image_pixels(header, false, 0), 0);
        assert_eq!(
            ProcessingMemory::default().image_pixels(header, true, 2),
            MAX_DOPPLER_WING_PIXELS
        );
    }
}

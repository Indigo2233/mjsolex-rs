use std::io::Write;

use crate::Reconstruction;

const CARD: usize = 80;
const BLOCK: usize = 2880;

impl Reconstruction {
    /// Write a FITS primary image with the reconstructed unsigned 16-bit samples.
    /// The first FITS row corresponds to the first row stored in the image.
    pub fn write_fits(&self, mut output: impl Write) -> std::io::Result<()> {
        let mut header = [b' '; BLOCK];
        let width = format!("NAXIS1  = {:>20}", self.width());
        let height = format!("NAXIS2  = {:>20}", self.height());
        let cards = [
            "SIMPLE  =                    T",
            "BITPIX  =                   16",
            "NAXIS   =                    2",
            &width,
            &height,
            "BSCALE  =                    1",
            "BZERO   =                32768",
            "ORIGIN  = 'MJ SolEx'",
            "END",
        ];
        for (slot, card) in header.as_chunks_mut::<CARD>().0.iter_mut().zip(cards) {
            slot[..card.len()].copy_from_slice(card.as_bytes());
        }
        output.write_all(&header)?;

        let mut bytes = [0; BLOCK];
        for samples in self.pixels().chunks(BLOCK / 2) {
            for (word, sample) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(samples) {
                *word = (sample ^ 0x8000).to_be_bytes();
            }
            bytes[samples.len() * 2..].fill(0);
            output.write_all(&bytes)?;
        }
        Ok(())
    }
}

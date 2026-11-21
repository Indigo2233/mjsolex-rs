use std::io::Write;

use crate::{APP_NAME, DiskGeometry, Header, Observation, Reconstruction};

const CARD: usize = 80;
const BLOCK: usize = 2880;

#[derive(Clone, Debug, Default)]
pub struct FitsMetadata {
    pub observation: Observation,
    pub source_name: Option<String>,
    pub source_header: Option<Header>,
    pub frame_step: Option<u32>,
    pub offset_pixels: Option<f64>,
    pub line_center_angstrom: Option<f64>,
    pub dispersion_angstrom: Option<f64>,
    pub fwhm_pixels: Option<f64>,
    pub geometry: Option<DiskGeometry>,
}

impl Reconstruction {
    /// Write a FITS primary image with the reconstructed unsigned 16-bit samples.
    /// The first FITS row corresponds to the first row stored in the image.
    pub fn write_fits(&self, output: impl Write) -> std::io::Result<()> {
        self.write_fits_with_metadata(output, &FitsMetadata::default())
    }

    pub fn write_fits_with_metadata(
        &self,
        mut output: impl Write,
        metadata: &FitsMetadata,
    ) -> std::io::Result<()> {
        let mut cards = vec![
            "SIMPLE  =                    T".to_owned(),
            "BITPIX  =                   16".to_owned(),
            "NAXIS   =                    2".to_owned(),
            format!("NAXIS1  = {:>20}", self.width()),
            format!("NAXIS2  = {:>20}", self.height()),
            "BSCALE  =                    1".to_owned(),
            "BZERO   =                32768".to_owned(),
            text_card("ORIGIN", APP_NAME),
        ];
        metadata.cards(&mut cards);
        cards.push("END".to_owned());
        for block in cards.chunks(BLOCK / CARD) {
            let mut header = [b' '; BLOCK];
            for (slot, card) in header.as_chunks_mut::<CARD>().0.iter_mut().zip(block) {
                slot[..card.len()].copy_from_slice(card.as_bytes());
            }
            output.write_all(&header)?;
        }

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

impl FitsMetadata {
    fn cards(&self, cards: &mut Vec<String>) {
        let obs = &self.observation;
        for (key, text) in [
            ("DATE-OBS", obs.start_utc.as_deref()),
            ("DATE-END", obs.end_utc.as_deref()),
            ("OBSERVER", obs.observer.as_deref()),
            ("INSTRUME", obs.instrument.as_deref()),
            ("TELESCOP", obs.telescope.as_deref()),
            ("FILTER", obs.filter.as_deref()),
            ("RECORDER", obs.recorder),
            ("SERFILE", self.source_name.as_deref()),
        ] {
            if let Some(text) = text {
                cards.push(text_card(key, text));
            }
        }
        if obs.start_utc.is_some() || obs.end_utc.is_some() {
            cards.push(text_card("TIMESYS", "UTC"));
        }
        if let Some(header) = self.source_header {
            cards.push(format!("FRMCOUNT= {:>20}", header.frames));
            cards.push(format!("SERDEPTH= {:>20}", header.depth));
            cards.push(format!("COLORID = {:>20}", header.color as u32));
            cards.push(if self.geometry.is_some() {
                "GEOMFIX =                    T".to_owned()
            } else {
                "GEOMFIX =                    F".to_owned()
            });
        }
        if let Some(stride) = self.frame_step {
            cards.push(format!("SCNSTEP = {stride:>20}"));
        }
        let wavelength =
            self.line_center_angstrom
                .zip(self.offset_pixels)
                .and_then(|(center, shift)| {
                    if shift == 0. {
                        Some(center)
                    } else {
                        self.dispersion_angstrom.map(|disp| center + shift * disp)
                    }
                });
        let width_angstrom = self
            .fwhm_pixels
            .zip(self.dispersion_angstrom)
            .map(|(width, disp)| width * disp.abs());
        if wavelength.is_some_and(f64::is_finite) {
            cards.push(text_card("WAVEUNIT", "Angstrom"));
        }
        for (key, value) in [
            ("PXSHIFT", self.offset_pixels),
            ("LINEANG", self.line_center_angstrom),
            ("ANGPERPX", self.dispersion_angstrom),
            ("WAVELNTH", wavelength),
            ("FWHMPIX", self.fwhm_pixels),
            ("FWHMANG", width_angstrom),
            ("SLITRAD", self.geometry.map(|g| g.slit_radius)),
            ("SCANRAD", self.geometry.map(|g| g.scan_radius)),
            ("SHEAR", self.geometry.map(|g| g.shear)),
        ] {
            if let Some(value) = value.filter(|n| n.is_finite()) {
                cards.push(format!("{key:<8}= {value:>20.12E}"));
            }
        }
    }
}

fn text_card(key: &str, text: &str) -> String {
    let mut card = format!("{key:<8}= '");
    for character in text.chars() {
        let quote = character == '\'';
        if card.len() + usize::from(quote) + 2 > CARD {
            break;
        }
        if quote {
            card.push('\'');
        }
        card.push(if character.is_ascii_graphic() || character == ' ' {
            character
        } else {
            '?'
        });
    }
    card.push('\'');
    card
}

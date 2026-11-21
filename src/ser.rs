#[cfg(any(target_os = "android", test))]
use std::io::{self, Write};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use crate::{Error, Observation};

const HEADER_LEN: u64 = 178;
const MAX_FRAME_PIXELS: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: usize = 16_384;
type HeaderBytes = [u8; HEADER_LEN as usize];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ColorMode {
    Mono = 0,
    Rggb = 8,
    Grbg = 9,
    Gbrg = 10,
    Bggr = 11,
    Rgb = 100,
    Bgr = 101,
}

impl ColorMode {
    fn parse(value: u32) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::Mono,
            8 => Self::Rggb,
            9 => Self::Grbg,
            10 => Self::Gbrg,
            11 => Self::Bggr,
            100 => Self::Rgb,
            101 => Self::Bgr,
            _ => return Err(Error::Color(value)),
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Rggb => "Bayer RGGB",
            Self::Grbg => "Bayer GRBG",
            Self::Gbrg => "Bayer GBRG",
            Self::Bggr => "Bayer BGGR",
            Self::Rgb => "RGB",
            Self::Bgr => "BGR",
        }
    }

    fn planes(self) -> usize {
        match self {
            Self::Rgb | Self::Bgr => 3,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub depth: u8,
    pub frames: u32,
    pub trimmed: bool,
    pub color: ColorMode,
    byte_order: ByteOrder,
}

impl Header {
    pub fn pixel_count(self) -> usize {
        self.width * self.height
    }

    pub fn byte_order(self) -> ByteOrder {
        self.byte_order
    }

    pub(crate) fn frame_bytes(self) -> usize {
        self.row_bytes() * self.height
    }

    fn data_end(self) -> u64 {
        // The validated header limits frames to 48 MiB each with a u32 frame count.
        HEADER_LEN + self.frame_bytes() as u64 * u64::from(self.frames)
    }

    fn trailer_bytes(self) -> u64 {
        8 * u64::from(self.frames)
    }

    pub(crate) fn row_bytes(self) -> usize {
        self.width * self.color.planes() * if self.depth > 8 { 2 } else { 1 }
    }

    fn sample_plane(self, frame: &[u8], index: usize) -> u16 {
        if self.depth <= 8 {
            let mask = u8::MAX << (8 - self.depth);
            u16::from(frame[index] & mask) << 8
        } else {
            let offset = index * 2;
            let bytes = [frame[offset], frame[offset + 1]];
            let value = if self.byte_order == ByteOrder::Little {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            };
            value << (16 - self.depth)
        }
    }

    pub(crate) fn sample(self, frame: &[u8], index: usize, first_row: usize) -> u16 {
        match self.color {
            ColorMode::Mono => self.sample_plane(frame, index),
            ColorMode::Rgb | ColorMode::Bgr => {
                let first = 3 * index;
                let green = u32::from(self.sample_plane(frame, first + 1));
                let a = u32::from(self.sample_plane(frame, first));
                let b = u32::from(self.sample_plane(frame, first + 2));
                let (red, blue) = if self.color == ColorMode::Rgb {
                    (a, b)
                } else {
                    (b, a)
                };
                ((2126 * red + 7152 * green + 722 * blue + 5000) / 10000) as u16
            }
            _ => {
                // Average matching green samples along the slit. Never mix rows
                // across the wavelength axis during Bayer conversion.
                let row = first_row + index / self.width;
                let x = index % self.width;
                let even_row_green = matches!(self.color, ColorMode::Grbg | ColorMode::Gbrg);
                if x.is_multiple_of(2) == (row.is_multiple_of(2) == even_row_green) {
                    return self.sample_plane(frame, index);
                }
                let left = if x > 0 { index - 1 } else { index + 1 };
                let right = if x + 1 < self.width { index + 1 } else { left };
                self.sample_plane(frame, left)
                    .midpoint(self.sample_plane(frame, right))
            }
        }
    }

    pub(crate) fn accumulate(self, frame: &[u8], sums: &mut [f32]) {
        if self.color != ColorMode::Mono {
            for (index, sum) in sums.iter_mut().enumerate() {
                *sum += f32::from(self.sample(frame, index, 0));
            }
            return;
        }
        if self.depth <= 8 {
            let mask = u8::MAX << (8 - self.depth);
            for (sum, sample) in sums.iter_mut().zip(frame) {
                *sum += f32::from(u16::from(sample & mask) << 8);
            }
        } else {
            let shift = 16 - self.depth;
            let samples = frame.as_chunks::<2>().0.iter().copied();
            if self.byte_order == ByteOrder::Little {
                for (sum, bytes) in sums.iter_mut().zip(samples) {
                    *sum += f32::from(u16::from_le_bytes(bytes) << shift);
                }
            } else {
                for (sum, bytes) in sums.iter_mut().zip(samples) {
                    *sum += f32::from(u16::from_be_bytes(bytes) << shift);
                }
            }
        }
    }
}

fn read_header(input: &mut impl Read) -> Result<HeaderBytes, Error> {
    let mut bytes = [0; HEADER_LEN as usize];
    input.read_exact(&mut bytes).map_err(|err| {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::InvalidSer("incomplete header")
        } else {
            err.into()
        }
    })?;
    Ok(bytes)
}

impl Header {
    fn parse(
        bytes: &HeaderBytes,
        observation: &Observation,
        order: Option<ByteOrder>,
    ) -> Result<Self, Error> {
        let trimmed = match &bytes[..14] {
            b"LUCAM-RECORDER" => false,
            b"JSOLEX-TRIMMED" => true,
            _ => return Err(Error::InvalidSer("unrecognized recorder signature")),
        };
        let field = |offset| {
            let [a, b, c, d] = bytes[offset..][..4] else {
                unreachable!("fixed offsets in the SER header");
            };
            u32::from_le_bytes([a, b, c, d])
        };
        let color = ColorMode::parse(field(18))?;
        // Indigo's writer stores little-endian samples with a zero flag. For
        // other recorders retain the default used by Astro4j and older imports.
        let default_order = match field(22) {
            0 if observation.recorder.is_some() => ByteOrder::Little,
            0 => ByteOrder::Big,
            1 => ByteOrder::Little,
            _ => return Err(Error::InvalidSer("invalid byte order flag")),
        };
        let width = field(26) as usize;
        let height = field(30) as usize;
        let depth = field(34);
        let frames = field(38);
        if width == 0 || height == 0 || frames == 0 {
            return Err(Error::InvalidSer("empty frame or recording"));
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::InvalidSer("frame dimensions exceed 16384 pixels"));
        }
        if width < 2
            && matches!(
                color,
                ColorMode::Rggb | ColorMode::Grbg | ColorMode::Gbrg | ColorMode::Bggr
            )
        {
            return Err(Error::InvalidSer(
                "a Bayer frame needs at least two columns",
            ));
        }
        if width
            .checked_mul(height)
            .is_none_or(|n| n > MAX_FRAME_PIXELS)
        {
            return Err(Error::InvalidSer("frame exceeds 8 Mi pixels"));
        }
        if !(1..=16).contains(&depth) {
            return Err(Error::InvalidSer("invalid pixel depth"));
        }
        Ok(Header {
            width,
            height,
            depth: depth as u8,
            frames,
            trimmed,
            color,
            byte_order: order.unwrap_or(default_order),
        })
    }
}

/// Stage the frame data and any complete timestamp trailer that fits the limit.
/// Some document providers deliver a stream without a size or seek support.
#[cfg(any(target_os = "android", test))]
pub(crate) fn cache_recording(
    mut input: impl Read,
    output: &mut impl Write,
    limit: u64,
) -> Result<(), Error> {
    let bytes = read_header(&mut input)?;
    let header = Header::parse(&bytes, &Observation::from_header(&bytes), None)?;
    let required = header.data_end();
    if required > limit {
        return Err(Error::CacheLimit { required, limit });
    }
    output.write_all(&bytes)?;
    let remaining = required - HEADER_LEN;
    if io::copy(&mut input.by_ref().take(remaining), output)? != remaining {
        return Err(Error::InvalidSer("frame data is truncated"));
    }
    if header.trailer_bytes() <= limit - required {
        // Incomplete trailers are ignored when the cached recording is opened.
        io::copy(&mut input.take(header.trailer_bytes()), output)?;
    }
    Ok(())
}

pub struct SerReader<R> {
    input: R,
    header: Header,
    observation: Observation,
    next_offset: Option<u64>,
    buffer: Vec<u8>,
}

impl<R: Read + Seek> SerReader<R> {
    pub fn open(input: R) -> Result<Self, Error> {
        Self::open_with_byte_order(input, None)
    }

    pub fn open_with_byte_order(mut input: R, order: Option<ByteOrder>) -> Result<Self, Error> {
        input.seek(SeekFrom::Start(0))?;
        let bytes = read_header(&mut input)?;
        let mut observation = Observation::from_header(&bytes);
        let header = Header::parse(&bytes, &observation, order)?;
        let length = input.seek(SeekFrom::End(0))?;
        let data_end = header.data_end();
        if length < data_end {
            return Err(Error::InvalidSer("frame data is truncated"));
        }
        let trailer_end = data_end + header.trailer_bytes();
        if length >= trailer_end {
            let mut timestamp = [0; 8];
            input.seek(SeekFrom::Start(data_end))?;
            input.read_exact(&mut timestamp)?;
            let first = u64::from_le_bytes(timestamp);
            input.seek(SeekFrom::Start(trailer_end - 8))?;
            input.read_exact(&mut timestamp)?;
            observation.set_trailer(first, u64::from_le_bytes(timestamp));
        }
        input.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Self {
            input,
            header,
            observation,
            next_offset: Some(HEADER_LEN),
            buffer: Vec::new(),
        })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn observation(&self) -> &Observation {
        &self.observation
    }

    pub(crate) fn frame(&mut self, index: u32) -> Result<&[u8], Error> {
        self.frame_rows(index, 0..self.header.height)
    }

    pub(crate) fn frame_rows(&mut self, index: u32, rows: Range<usize>) -> Result<&[u8], Error> {
        if index >= self.header.frames {
            return Err(Error::InvalidSer("frame index outside the recording"));
        }
        debug_assert!(rows.start < rows.end && rows.end <= self.header.height);
        let row_bytes = self.header.row_bytes();
        let frame_offset = HEADER_LEN + self.header.frame_bytes() as u64 * u64::from(index);
        let offset = frame_offset + (rows.start * row_bytes) as u64;
        let length = rows.len() * row_bytes;
        let next_offset = self.next_offset.take();
        if next_offset != Some(offset) {
            self.input.seek(SeekFrom::Start(offset))?;
        }
        if length > self.buffer.capacity() {
            self.buffer.try_reserve_exact(length - self.buffer.len())?;
        }
        self.buffer.resize(length, 0);
        self.input.read_exact(&mut self.buffer)?;
        self.next_offset = Some(offset + length as u64);
        Ok(&self.buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_cache_checks_header_and_limit_before_writing() {
        let mut input = [0; 192];
        let mut output = Vec::new();
        assert!(matches!(
            cache_recording(&input[..], &mut output, 192),
            Err(Error::InvalidSer(_))
        ));
        assert!(output.is_empty());

        input[..14].copy_from_slice(b"LUCAM-RECORDER");
        for (offset, value) in [(26, 3_u32), (30, 2), (34, 8), (38, 2)] {
            input[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut source = &input[..];
        assert!(matches!(
            cache_recording(&mut source, &mut output, 189),
            Err(Error::CacheLimit {
                required: 190,
                limit: 189
            })
        ));
        assert_eq!(source.len(), 14);
        assert!(output.is_empty());

        let mut source = &input[..];
        cache_recording(&mut source, &mut output, 190).unwrap();
        assert_eq!(output, input[..190]);
        assert_eq!(source.len(), 2);
        assert!(matches!(
            cache_recording(&input[..189], &mut Vec::new(), 190),
            Err(Error::InvalidSer(_))
        ));
    }

    #[test]
    fn stream_cache_preserves_complete_timestamps_within_its_budget() {
        use std::io::Cursor;

        let mut input = vec![0; 182];
        input[..14].copy_from_slice(b"LUCAM-RECORDER");
        for (offset, value) in [(26, 2_u32), (30, 1), (34, 8), (38, 2)] {
            input[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        const UNIX: u64 = 621_355_968_000_000_000;
        input.extend(UNIX.to_le_bytes());
        input.extend((UNIX + 10_000_000).to_le_bytes());
        let cases = [
            (&input[..], 198, Some("1970-01-01T00:00:01.0000000")),
            (&input[..], 197, None),
            (&input[..197], 198, None),
        ];
        for (bytes, limit, end) in cases {
            let mut output = Vec::new();
            cache_recording(bytes, &mut output, limit).unwrap();
            assert!(output.len() as u64 <= limit);
            let reader = SerReader::open(Cursor::new(output)).unwrap();
            assert_eq!(reader.observation().end_utc.as_deref(), end);
        }
    }
}

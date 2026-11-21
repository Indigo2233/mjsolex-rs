#[cfg(any(target_os = "android", test))]
use std::io::{self, Write};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

use crate::Error;

const HEADER_LEN: u64 = 178;
const MAX_FRAME_PIXELS: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: usize = 16_384;
type HeaderBytes = [u8; HEADER_LEN as usize];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub depth: u8,
    pub frames: u32,
    pub trimmed: bool,
    little_endian: bool,
}

impl Header {
    pub fn pixel_count(self) -> usize {
        self.width * self.height
    }

    fn frame_bytes(self) -> usize {
        self.row_bytes() * self.height
    }

    fn data_end(self) -> u64 {
        // The validated header limits frames to 16 MiB each with a u32 frame count.
        HEADER_LEN + self.frame_bytes() as u64 * u64::from(self.frames)
    }

    pub(crate) fn row_bytes(self) -> usize {
        self.width * if self.depth > 8 { 2 } else { 1 }
    }

    pub(crate) fn sample(self, frame: &[u8], index: usize) -> u16 {
        if self.depth <= 8 {
            let mask = u8::MAX << (8 - self.depth);
            u16::from(frame[index] & mask) << 8
        } else {
            let offset = index * 2;
            let bytes = [frame[offset], frame[offset + 1]];
            let value = if self.little_endian {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            };
            value << (16 - self.depth)
        }
    }

    pub(crate) fn accumulate(self, frame: &[u8], sums: &mut [f32]) {
        if self.depth <= 8 {
            let mask = u8::MAX << (8 - self.depth);
            for (sum, sample) in sums.iter_mut().zip(frame) {
                *sum += f32::from(u16::from(sample & mask) << 8);
            }
        } else {
            let shift = 16 - self.depth;
            let samples = frame.as_chunks::<2>().0.iter().copied();
            if self.little_endian {
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
    fn parse(bytes: &HeaderBytes) -> Result<Self, Error> {
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
        let color = field(18);
        if color != 0 {
            return Err(Error::Color(color));
        }
        // Match Astro4j's interpretation of the historically inverted SER field.
        let little_endian = match field(22) {
            0 => false,
            1 => true,
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
            little_endian,
        })
    }
}

/// Stage only the frame data used by reconstruction. Some providers stream
/// documents and cannot report a size or seek to the SER frames.
#[cfg(any(target_os = "android", test))]
pub(crate) fn cache_frames(
    mut input: impl Read,
    output: &mut impl Write,
    limit: u64,
) -> Result<(), Error> {
    let bytes = read_header(&mut input)?;
    let required = Header::parse(&bytes)?.data_end();
    if required > limit {
        return Err(Error::CacheLimit { required, limit });
    }
    output.write_all(&bytes)?;
    let remaining = required - HEADER_LEN;
    if io::copy(&mut input.take(remaining), output)? != remaining {
        return Err(Error::InvalidSer("frame data is truncated"));
    }
    Ok(())
}

pub struct SerReader<R> {
    input: R,
    header: Header,
    next_offset: Option<u64>,
    buffer: Vec<u8>,
}

impl<R: Read + Seek> SerReader<R> {
    pub fn open(mut input: R) -> Result<Self, Error> {
        input.seek(SeekFrom::Start(0))?;
        let header = Header::parse(&read_header(&mut input)?)?;
        if input.seek(SeekFrom::End(0))? < header.data_end() {
            return Err(Error::InvalidSer("frame data is truncated"));
        }
        input.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Self {
            input,
            header,
            next_offset: Some(HEADER_LEN),
            buffer: Vec::new(),
        })
    }

    pub fn header(&self) -> Header {
        self.header
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
            cache_frames(&input[..], &mut output, 192),
            Err(Error::InvalidSer(_))
        ));
        assert!(output.is_empty());

        input[..14].copy_from_slice(b"LUCAM-RECORDER");
        for (offset, value) in [(26, 3_u32), (30, 2), (34, 8), (38, 2)] {
            input[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut source = &input[..];
        assert!(matches!(
            cache_frames(&mut source, &mut output, 189),
            Err(Error::CacheLimit {
                required: 190,
                limit: 189
            })
        ));
        assert_eq!(source.len(), 14);
        assert!(output.is_empty());

        let mut source = &input[..];
        cache_frames(&mut source, &mut output, 190).unwrap();
        assert_eq!(output, input[..190]);
        assert_eq!(source.len(), 2);
        assert!(matches!(
            cache_frames(&input[..189], &mut Vec::new(), 190),
            Err(Error::InvalidSer(_))
        ));
    }
}

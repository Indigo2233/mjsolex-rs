use std::io::{Read, Seek, SeekFrom};

use crate::Error;

const HEADER_LEN: u64 = 178;
const MAX_FRAME_PIXELS: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: usize = 16_384;

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
        self.pixel_count() * if self.depth > 8 { 2 } else { 1 }
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
}

pub struct SerReader<R> {
    input: R,
    header: Header,
    next: u32,
    buffer: Vec<u8>,
}

impl<R: Read + Seek> SerReader<R> {
    pub fn open(mut input: R) -> Result<Self, Error> {
        input.seek(SeekFrom::Start(0))?;
        let mut bytes = [0; HEADER_LEN as usize];
        input.read_exact(&mut bytes).map_err(|err| {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::InvalidSer("incomplete header")
            } else {
                err.into()
            }
        })?;
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
        let header = Header {
            width,
            height,
            depth: depth as u8,
            frames,
            trimmed,
            little_endian,
        };
        let frame_bytes = header.frame_bytes();
        let required = HEADER_LEN + (frame_bytes as u64) * u64::from(frames);
        if input.seek(SeekFrom::End(0))? < required {
            return Err(Error::InvalidSer("frame data is truncated"));
        }
        input.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Self {
            input,
            header,
            next: 0,
            buffer: vec![0; frame_bytes],
        })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub(crate) fn frame(&mut self, index: u32) -> Result<&[u8], Error> {
        if index >= self.header.frames {
            return Err(Error::InvalidSer("frame index outside the recording"));
        }
        if index != self.next {
            let offset = HEADER_LEN + self.buffer.len() as u64 * u64::from(index);
            self.input.seek(SeekFrom::Start(offset))?;
        }
        self.next = u32::MAX;
        self.input.read_exact(&mut self.buffer)?;
        self.next = index + 1;
        Ok(&self.buffer)
    }
}

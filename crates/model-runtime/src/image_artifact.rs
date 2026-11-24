//! Validate the bounded, single-image RGB8 PNG produced by the fixed SDXL template.
//! Successful decoding establishes content identity only. Job ownership, delivery and pricing
//! require independent server-side facts.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Cursor;

pub const MAX_PNG: usize = 16 << 20;
const EDGE: u32 = 1024;
const PIXELS: usize = EDGE as usize * EDGE as usize * 3;

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct ImageContent {
    pub width: u32,
    pub height: u32,
    pub media_type: &'static str,
    pub bytes: usize,
    pub sha256: String,
    pub rgb_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("Invalid fixed-template PNG container")]
    Container,
    #[error("Invalid fixed-template PNG pixels")]
    Pixels,
    #[error("PNG decoder rejected the artifact")]
    Decode(#[from] png::DecodingError),
}

pub fn inspect(data: &[u8]) -> Result<ImageContent, ImageError> {
    if data.len() > MAX_PNG || !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(ImageError::Container);
    }
    // The template emits three chunk types only. Check every CRC, including the terminal one,
    // and require IEND to be the final bytes even when a decoder would return a valid frame.
    let mut pos = 8;
    let mut has_data = false;
    let mut ended = false;
    while pos < data.len() {
        let header = data.get(pos..pos + 8).ok_or(ImageError::Container)?;
        let size = u32::from_be_bytes(header[..4].try_into().expect("four bytes")) as usize;
        let crc_at = (pos + 8).checked_add(size).ok_or(ImageError::Container)?;
        let next = crc_at.checked_add(4).ok_or(ImageError::Container)?;
        let crc = data.get(crc_at..next).ok_or(ImageError::Container)?;
        if crc != crc32fast::hash(&data[pos + 4..crc_at]).to_be_bytes() {
            return Err(ImageError::Container);
        }
        match &header[4..] {
            b"IHDR" if pos == 8 && size == 13 => {
                let expected = [0, 0, 4, 0, 0, 0, 4, 0, 8, 2, 0, 0];
                if data[pos + 8..crc_at - 1] != expected || data[crc_at - 1] > 1 {
                    return Err(ImageError::Pixels);
                }
            }
            b"IDAT" if pos > 8 => has_data = true,
            b"IEND" if size == 0 && has_data && next == data.len() => ended = true,
            _ => return Err(ImageError::Container),
        }
        pos = next;
    }
    if !ended {
        return Err(ImageError::Container);
    }

    let mut reader = png::Decoder::new(Cursor::new(data)).read_info()?;
    let mut pixels = vec![0; PIXELS];
    let frame = reader.next_frame(&mut pixels)?;
    if frame.buffer_size() != PIXELS || pixels.chunks_exact(3).all(|pixel| pixel == &pixels[..3]) {
        return Err(ImageError::Pixels);
    }
    reader.finish()?;
    Ok(ImageContent {
        width: EDGE,
        height: EDGE,
        media_type: "image/png",
        bytes: data.len(),
        sha256: hex::encode(Sha256::digest(data)),
        rgb_sha256: hex::encode(Sha256::digest(pixels)),
    })
}

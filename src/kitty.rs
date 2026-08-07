use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;

const PAYLOAD_CHUNK_SIZE: usize = 4096;

pub struct Placement {
    pub image_id: u32,
    pub columns: u16,
    pub rows: u16,
    /// Source rectangle to display, in image pixels. `None` shows the whole image.
    pub crop: Option<Crop>,
}

/// A pixel rectangle within a source image, used to display a scrolled crop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn compress_rgba(rgba: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
    compressor.write_all(rgba)?;
    compressor.finish()
}

pub fn transmit_compressed_rgba(
    output: &mut impl Write,
    compressed_rgba: &[u8],
    width: u32,
    height: u32,
    placement: Placement,
) -> io::Result<()> {
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image dimensions must be non-zero",
        ));
    }

    let crop = match placement.crop {
        Some(crop) => format!(",x={},y={},w={},h={}", crop.x, crop.y, crop.width, crop.height),
        None => String::new(),
    };
    let encoded = STANDARD.encode(compressed_rgba);
    let chunks = encoded.as_bytes().chunks(PAYLOAD_CHUNK_SIZE);
    let chunk_count = chunks.len();

    for (index, chunk) in chunks.enumerate() {
        let more = u8::from(index + 1 < chunk_count);
        if index == 0 {
            write!(
                output,
                "\x1b_Ga=T,f=32,s={width},v={height},i={},p=1,o=z,c={},r={}{crop},C=1,q=2,m={more};",
                placement.image_id, placement.columns, placement.rows
            )?;
        } else {
            write!(output, "\x1b_Gq=2,m={more};")?;
        }
        output.write_all(chunk)?;
        output.write_all(b"\x1b\\")?;
    }
    output.flush()
}

pub fn delete_image(output: &mut impl Write, image_id: u32) -> io::Result<()> {
    write!(output, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")?;
    output.flush()
}

pub fn delete_all(output: &mut impl Write) -> io::Result<()> {
    output.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_sized_image() {
        let error = transmit_compressed_rgba(
            &mut Vec::new(),
            &[0; 3],
            0,
            1,
            Placement {
                image_id: 1,
                columns: 1,
                rows: 1,
                crop: None,
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn crop_adds_source_rectangle_to_the_command() {
        let mut output = Vec::new();
        let compressed = compress_rgba(&[0; 64]).unwrap();

        transmit_compressed_rgba(
            &mut output,
            &compressed,
            8,
            8,
            Placement {
                image_id: 3,
                columns: 8,
                rows: 4,
                crop: Some(Crop {
                    x: 0,
                    y: 16,
                    width: 8,
                    height: 4,
                }),
            },
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(",c=8,r=4,x=0,y=16,w=8,h=4,C=1"));
    }

    #[test]
    fn chunks_payload_at_kitty_limit() {
        let mut output = Vec::new();
        let pixels = (0..16_384)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let compressed = compress_rgba(&pixels).unwrap();

        transmit_compressed_rgba(
            &mut output,
            &compressed,
            64,
            64,
            Placement {
                image_id: 7,
                columns: 8,
                rows: 4,
                crop: None,
            },
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b_Ga=T,f=32,s=64,v=64,i=7,p=1,o=z,c=8,r=4,C=1,q=2,m="));
        assert!(output.ends_with("\x1b\\"));
        for command in output.split("\x1b\\").filter(|command| !command.is_empty()) {
            let payload = command.split_once(';').unwrap().1;
            assert!(payload.len() <= PAYLOAD_CHUNK_SIZE);
            assert_eq!(payload.len() % 4, 0);
        }
    }
}

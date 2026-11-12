use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;

const PAYLOAD_CHUNK_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    pub image_id: u32,
    /// Both zero selects native pixel size instead of scaling into terminal cells.
    pub columns: u16,
    pub rows: u16,
    /// Pixel offset from the top of the cursor cell (Kitty `Y`).
    pub offset_y: u32,
    pub z_index: i32,
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
    let mut compressor = ZlibEncoder::new(Vec::with_capacity(rgba.len() / 16), Compression::fast());
    compressor.write_all(rgba)?;
    compressor.finish()
}

pub fn compress_rgba_as_rgb(rgba: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if expected != Some(rgba.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RGBA raster length does not match dimensions",
        ));
    }
    let mut compressor = ZlibEncoder::new(Vec::with_capacity(rgba.len() / 16), Compression::fast());
    let mut rgb = [0u8; 6144];
    for block in rgba.chunks(8192) {
        for (dst, src) in rgb.chunks_exact_mut(3).zip(block.chunks_exact(4)) {
            dst.copy_from_slice(&src[..3]);
        }
        compressor.write_all(&rgb[..(block.len() / 4) * 3])?;
    }
    compressor.finish()
}

fn write_cell_extent(output: &mut impl Write, placement: Placement) -> io::Result<()> {
    // Omission means native pixel size. Some terminals interpret explicit zeros
    // as a placement occupying zero cells.
    if placement.columns != 0 || placement.rows != 0 {
        write!(output, ",c={},r={}", placement.columns, placement.rows)?;
    }
    Ok(())
}

fn write_crop(output: &mut impl Write, crop: Option<Crop>) -> io::Result<()> {
    if let Some(c) = crop {
        write!(output, ",x={},y={},w={},h={}", c.x, c.y, c.width, c.height)?;
    }
    Ok(())
}

pub fn transmit_compressed_rgb(
    output: &mut impl Write,
    compressed_rgb: &[u8],
    width: u32,
    height: u32,
    placement: Placement,
) -> io::Result<()> {
    transmit_compressed(output, 24, compressed_rgb, width, height, placement)
}

pub fn transmit_compressed_rgba(
    output: &mut impl Write,
    compressed_rgba: &[u8],
    width: u32,
    height: u32,
    placement: Placement,
) -> io::Result<()> {
    transmit_compressed(output, 32, compressed_rgba, width, height, placement)
}

fn transmit_compressed(
    output: &mut impl Write,
    format_bits: u8,
    compressed: &[u8],
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
    const RAW_CHUNK_SIZE: usize = (PAYLOAD_CHUNK_SIZE / 4) * 3;
    let chunk_count = compressed.len().div_ceil(RAW_CHUNK_SIZE).max(1);
    let mut encoded = [0u8; PAYLOAD_CHUNK_SIZE];

    for index in 0..chunk_count {
        let start = index * RAW_CHUNK_SIZE;
        let end = (start + RAW_CHUNK_SIZE).min(compressed.len());
        let encoded_len = STANDARD
            .encode_slice(&compressed[start..end], &mut encoded)
            .expect("chunk fits in base64 buffer");
        let more = u8::from(index + 1 < chunk_count);
        if index == 0 {
            write!(
                output,
                "\x1b_Ga=T,f={format_bits},s={width},v={height},i={},p=1,o=z",
                placement.image_id
            )?;
            write_cell_extent(output, placement)?;
            write!(output, ",Y={},z={}", placement.offset_y, placement.z_index)?;
            write_crop(output, placement.crop)?;
            write!(output, ",C=1,q=2,m={more};")?;
        } else {
            write!(output, "\x1b_Gq=2,m={more};")?;
        }
        output.write_all(&encoded[..encoded_len])?;
        output.write_all(b"\x1b\\")?;
    }
    Ok(())
}

pub fn delete_image(output: &mut impl Write, image_id: u32) -> io::Result<()> {
    write!(output, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

pub fn place_image(output: &mut impl Write, placement: Placement) -> io::Result<()> {
    if (placement.columns == 0) != (placement.rows == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image placement dimensions must both be zero or both be non-zero",
        ));
    }
    write!(output, "\x1b_Ga=p,i={},p=1", placement.image_id)?;
    write_cell_extent(output, placement)?;
    write!(output, ",Y={},z={}", placement.offset_y, placement.z_index)?;
    write_crop(output, placement.crop)?;
    output.write_all(b",C=1,q=2\x1b\\")
}

pub fn delete_all(output: &mut impl Write) -> io::Result<()> {
    output.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")
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
                offset_y: 0,
                z_index: 0,
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
                offset_y: 0,
                z_index: -3,
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
        assert!(output.contains(",c=8,r=4,Y=0,z=-3,x=0,y=16,w=8,h=4,C=1"));
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
                offset_y: 0,
                z_index: 0,
                crop: None,
            },
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(
            output.starts_with("\x1b_Ga=T,f=32,s=64,v=64,i=7,p=1,o=z,c=8,r=4,Y=0,z=0,C=1,q=2,m=")
        );
        assert!(output.ends_with("\x1b\\"));
        for command in output.split("\x1b\\").filter(|command| !command.is_empty()) {
            let payload = command.split_once(';').unwrap().1;
            assert!(payload.len() <= PAYLOAD_CHUNK_SIZE);
            assert_eq!(payload.len() % 4, 0);
        }
    }

    #[test]
    fn native_pixel_placement_omits_cell_extent() {
        let placement = Placement {
            image_id: 12,
            columns: 0,
            rows: 0,
            offset_y: 3,
            z_index: -4,
            crop: Some(Crop {
                x: 0,
                y: 1,
                width: 2,
                height: 1,
            }),
        };
        let mut transfer = Vec::new();
        transmit_compressed_rgba(
            &mut transfer,
            &compress_rgba(&[0; 16]).unwrap(),
            2,
            2,
            placement,
        )
        .unwrap();
        assert!(transfer.starts_with(
            b"\x1b_Ga=T,f=32,s=2,v=2,i=12,p=1,o=z,Y=3,z=-4,x=0,y=1,w=2,h=1,C=1,q=2,m=0;"
        ));

        let mut move_image = Vec::new();
        place_image(&mut move_image, placement).unwrap();
        assert_eq!(
            move_image,
            b"\x1b_Ga=p,i=12,p=1,Y=3,z=-4,x=0,y=1,w=2,h=1,C=1,q=2\x1b\\"
        );
    }

    #[test]
    fn replaces_an_existing_placement_without_retransmitting_pixels() {
        let mut output = Vec::new();

        place_image(
            &mut output,
            Placement {
                image_id: 12,
                columns: 40,
                rows: 20,
                offset_y: 0,
                z_index: -3,
                crop: Some(Crop {
                    x: 4,
                    y: 8,
                    width: 400,
                    height: 600,
                }),
            },
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b_Ga=p,i=12,p=1,c=40,r=20,Y=0,z=-3,x=4,y=8,w=400,h=600,C=1,q=2\x1b\\"
        );
    }

    #[test]
    fn transmits_rgb_frames_with_f24_and_rejects_zero_dimensions() {
        let placement = Placement {
            image_id: 1,
            columns: 2,
            rows: 1,
            offset_y: 0,
            z_index: -3,
            crop: None,
        };
        let mut out = Vec::new();
        let compressed = compress_rgba(&[7; 12]).unwrap();
        super::transmit_compressed_rgb(&mut out, &compressed, 2, 2, placement).unwrap();
        assert!(out.starts_with(b"\x1b_Ga=T,f=24,s=2,v=2,i=1,p=1,o=z,"));
        assert!(
            super::transmit_compressed_rgb(&mut Vec::new(), &compressed, 0, 2, placement).is_err()
        );
    }
}

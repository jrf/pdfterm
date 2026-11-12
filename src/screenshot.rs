use crate::pdf::Frame;
use crate::terminal::{ImagePlacement, Viewport};
use flate2::read::ZlibDecoder;
use fontdue::{Font, FontSettings, Metrics};
use png::{BitDepth, ColorType, Encoder};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Page<'a> {
    pub frame: &'a Frame,
    pub placement: ImagePlacement,
    /// Page's first terminal row relative to the content viewport.
    pub row: u16,
}

pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub color: [u8; 4],
}

pub struct Badge<'a> {
    pub x: u32,
    pub y: u32,
    pub layout: BadgeLayout,
    pub text: &'a str,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
}

pub struct Overlay {
    pub col: u16,
    pub row: u16,
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
pub struct BadgeLayout {
    pub width: u32,
    pub height: u32,
    glyph_size: u32,
    origin_x: i64,
    baseline: i64,
}

struct Glyph {
    metrics: Metrics,
    coverage: Vec<u8>,
}

pub struct FlashFont {
    font: Font,
    glyphs: HashMap<(u32, char), Glyph>,
}

impl FlashFont {
    pub fn load(family: &str) -> io::Result<Self> {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        let generic = family == "monospace";
        #[cfg(target_os = "macos")]
        let family = if family == "monospace" {
            "Menlo"
        } else {
            family
        };
        let family = if family == "monospace" {
            database.family_name(&fontdb::Family::Monospace)
        } else {
            family
        };
        let families = [fontdb::Family::Name(family)];
        let query = fontdb::Query {
            families: &families,
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        let id = database
            .query(&query)
            .or_else(|| {
                generic
                    .then(|| {
                        database
                            .faces()
                            .find(|face| face.monospaced)
                            .map(|face| face.id)
                    })
                    .flatten()
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("flash-label font family {family:?} is not installed"),
                )
            })?;
        let face = database.face(id).expect("queried font face exists");
        if !generic && !face.families.iter().any(|(name, _)| name == family) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("flash-label font family {family:?} is not installed"),
            ));
        }
        let font = database
            .with_face_data(id, |data, index| {
                Font::from_bytes(
                    data,
                    FontSettings {
                        collection_index: index,
                        ..FontSettings::default()
                    },
                )
            })
            .ok_or_else(|| io::Error::other("selected system font data is unavailable"))?
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("cannot parse flash-label font {family:?}: {error}"),
                )
            })?;
        Ok(Self {
            font,
            glyphs: HashMap::new(),
        })
    }

    pub fn badge_layout(&mut self, text: &str, size: u32) -> io::Result<BadgeLayout> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "badge glyph size must be positive",
            ));
        }
        let px = size as f32;
        let line = self.font.horizontal_line_metrics(px).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "flash-label font has no horizontal metrics",
            )
        })?;
        let baseline = 1 + line.ascent.round() as i64;
        let mut cursor = 2.0f32;
        let mut ink_left = 2i64;
        let mut ink_top = 1i64;
        let mut ink_right = 0i64;
        let mut ink_bottom = 0i64;
        let mut previous = None;
        for character in text.chars() {
            if let Some(left) = previous {
                cursor += self
                    .font
                    .horizontal_kern(left, character, px)
                    .unwrap_or(0.0);
            }
            let metrics = &self.glyph(character, size)?.metrics;
            if metrics.width > 0 && metrics.height > 0 {
                let x = cursor.round() as i64 + i64::from(metrics.xmin);
                let y = baseline - i64::from(metrics.ymin) - metrics.height as i64;
                ink_left = ink_left.min(x);
                ink_top = ink_top.min(y);
                ink_right = ink_right.max(x + metrics.width as i64);
                ink_bottom = ink_bottom.max(y + metrics.height as i64);
            }
            cursor += metrics.advance_width;
            previous = Some(character);
        }
        let extra_x = (2 - ink_left).max(0);
        let extra_y = (1 - ink_top).max(0);
        Ok(BadgeLayout {
            width: ((cursor + 2.0).ceil() as i64 + extra_x).max(ink_right + extra_x + 2) as u32,
            height: ((line.ascent - line.descent).ceil() as i64 + 2 + extra_y)
                .max(ink_bottom + extra_y + 1) as u32,
            glyph_size: size,
            origin_x: 2 + extra_x,
            baseline: baseline + extra_y,
        })
    }

    fn glyph(&mut self, character: char, size: u32) -> io::Result<&Glyph> {
        let key = (size, character);
        if !self.glyphs.contains_key(&key) {
            if !self.font.chars().contains_key(&character) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("flash-label font does not contain {character:?}"),
                ));
            }
            let (metrics, coverage) = self.font.rasterize(character, size as f32);
            self.glyphs.insert(key, Glyph { metrics, coverage });
        }
        Ok(self.glyphs.get(&key).expect("glyph was inserted"))
    }
}

fn pixel_count(viewport: Viewport) -> io::Result<usize> {
    if viewport.pixel_width == 0
        || viewport.pixel_height == 0
        || viewport.columns == 0
        || viewport.rows == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "viewport dimensions must be non-zero",
        ));
    }
    usize::from(viewport.pixel_width)
        .checked_mul(usize::from(viewport.pixel_height))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "viewport is too large"))
}

fn blank_overlay(viewport: Viewport) -> io::Result<Vec<u8>> {
    Ok(vec![0; pixel_count(viewport)?])
}

pub fn cropped_overlay(
    font: &mut FlashFont,
    viewport: Viewport,
    rects: &[Rect],
    badges: &[Badge<'_>],
) -> io::Result<Option<Overlay>> {
    pixel_count(viewport)?;
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    let (cw, ch) = viewport.cell_size();
    let mut left = width;
    let mut top = height;
    let mut right = 0;
    let mut bottom = 0;
    for (x, y, w, h) in rects.iter().map(|r| (r.x, r.y, r.width, r.height)).chain(
        badges
            .iter()
            .map(|b| (b.x, b.y, b.layout.width, b.layout.height)),
    ) {
        let x1 = x.saturating_add(w).min(width);
        let y1 = y.saturating_add(h).min(height);
        if x < x1 && y < y1 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x1);
            bottom = bottom.max(y1);
        }
    }
    if left >= right || top >= bottom {
        return Ok(None);
    }
    let col = (left / cw).min(u32::from(viewport.columns - 1)) as u16;
    let row = (top / ch).min(u32::from(viewport.rows - 1)) as u16;
    let end_col = right.div_ceil(cw).min(u32::from(viewport.columns)) as u16;
    let end_row = bottom.div_ceil(ch).min(u32::from(viewport.rows)) as u16;
    let x = u32::from(col) * cw;
    let y = u32::from(row) * ch;
    right = if end_col == viewport.columns {
        width
    } else {
        u32::from(end_col) * cw
    };
    bottom = if end_row == viewport.rows {
        height
    } else {
        u32::from(end_row) * ch
    };
    let cropped = Viewport {
        columns: end_col - col,
        rows: end_row - row,
        pixel_width: (right - x) as u16,
        pixel_height: (bottom - y) as u16,
        ..viewport
    };
    let rects: Vec<_> = rects
        .iter()
        .filter(|r| r.x < right && r.y < bottom && r.width > 0 && r.height > 0)
        .map(|r| Rect {
            x: r.x.saturating_sub(x),
            y: r.y.saturating_sub(y),
            width: r.width,
            height: r.height,
            color: r.color,
        })
        .collect();
    let badges: Vec<_> = badges
        .iter()
        .filter(|b| b.x < right && b.y < bottom && b.layout.width > 0 && b.layout.height > 0)
        .map(|b| Badge {
            x: b.x.saturating_sub(x),
            y: b.y.saturating_sub(y),
            layout: b.layout,
            text: b.text,
            foreground: b.foreground,
            background: b.background,
        })
        .collect();
    let rgba = overlay(font, cropped, &rects, &badges)?;
    Ok(Some(Overlay {
        col,
        row,
        columns: cropped.columns,
        rows: cropped.rows,
        pixel_width: cropped.pixel_width,
        pixel_height: cropped.pixel_height,
        rgba,
    }))
}

pub fn overlay(
    font: &mut FlashFont,
    viewport: Viewport,
    rects: &[Rect],
    badges: &[Badge<'_>],
) -> io::Result<Vec<u8>> {
    let mut pixels = blank_overlay(viewport)?;
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    for rect in rects {
        let right = rect.x.saturating_add(rect.width).min(width);
        let bottom = rect.y.saturating_add(rect.height).min(height);
        for y in rect.y.min(height)..bottom {
            for x in rect.x.min(width)..right {
                let offset = ((y * width + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&rect.color);
            }
        }
    }
    for badge in badges {
        let layout = badge.layout;
        for y in badge.y.min(height)..badge.y.saturating_add(layout.height).min(height) {
            for x in badge.x.min(width)..badge.x.saturating_add(layout.width).min(width) {
                let offset = ((y * width + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&[
                    badge.background[0],
                    badge.background[1],
                    badge.background[2],
                    255,
                ]);
            }
        }
        let baseline = badge.y as i64 + layout.baseline;
        let mut cursor = badge.x as f32 + layout.origin_x as f32;
        let mut previous = None;
        for character in badge.text.chars() {
            if let Some(left) = previous {
                cursor += font
                    .font
                    .horizontal_kern(left, character, layout.glyph_size as f32)
                    .unwrap_or(0.0);
            }
            let glyph = font.glyph(character, layout.glyph_size)?;
            let x0 = cursor.round() as i64 + i64::from(glyph.metrics.xmin);
            let y0 = baseline - i64::from(glyph.metrics.ymin) - glyph.metrics.height as i64;
            for gy in 0..glyph.metrics.height {
                let y = y0 + gy as i64;
                if !(0..i64::from(height)).contains(&y) {
                    continue;
                }
                for gx in 0..glyph.metrics.width {
                    let x = x0 + gx as i64;
                    if !(0..i64::from(width)).contains(&x) {
                        continue;
                    }
                    let alpha = u32::from(glyph.coverage[gy * glyph.metrics.width + gx]);
                    if alpha == 0 {
                        continue;
                    }
                    let offset = ((y as u32 * width + x as u32) * 4) as usize;
                    for channel in 0..3 {
                        pixels[offset + channel] = ((u32::from(badge.foreground[channel]) * alpha
                            + u32::from(pixels[offset + channel]) * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
            }
            cursor += glyph.metrics.advance_width;
            previous = Some(character);
        }
    }
    Ok(pixels)
}

fn blend(dst: &mut [u8], src: &[u8]) {
    let alpha = u32::from(src[3]);
    if alpha == 0 {
        return;
    }
    let inverse = 255 - alpha;
    for i in 0..3 {
        dst[i] = ((u32::from(src[i]) * alpha + u32::from(dst[i]) * inverse + 127) / 255) as u8;
    }
    dst[3] = 255;
}

fn decompress(frame: &Frame) -> io::Result<Vec<u8>> {
    let expected = usize::try_from(frame.width)
        .ok()
        .and_then(|w| {
            usize::try_from(frame.height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame dimensions overflow"))?;
    let decoder = ZlibDecoder::new(frame.compressed_rgba.as_slice());
    let mut rgb = Vec::with_capacity(expected);
    decoder.take(expected as u64 + 1).read_to_end(&mut rgb)?;
    if rgb.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cached frame has invalid RGB length",
        ));
    }
    Ok(rgb)
}

fn compose(
    viewport: Viewport,
    pages: &[Page<'_>],
    overlay: Option<&Overlay>,
    background: [u8; 3],
) -> io::Result<Vec<u8>> {
    let expected = pixel_count(viewport)?;
    if let Some(overlay) = overlay
        && overlay.rgba.len()
            != usize::from(overlay.pixel_width) * usize::from(overlay.pixel_height) * 4
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "overlay dimensions do not match RGBA length",
        ));
    }
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    let mut output = vec![0; expected];
    for pixel in output.as_chunks_mut::<4>().0 {
        pixel[..3].copy_from_slice(&background);
        pixel[3] = 255;
    }
    let (cell_w, cell_h) = viewport.cell_size();
    for page in pages {
        let frame = page.frame;
        let rgb = decompress(frame)?;
        let crop = page.placement.crop.unwrap_or(crate::kitty::Crop {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        });
        let sx0 = crop.x.min(frame.width);
        let sy0 = crop.y.min(frame.height);
        let sx1 = crop.x.saturating_add(crop.width).min(frame.width);
        let sy1 = crop.y.saturating_add(crop.height).min(frame.height);
        if sx0 >= sx1 || sy0 >= sy1 {
            continue;
        }
        let native = page.placement.native_cell.is_some();
        let dst_x = u32::from(page.placement.left).saturating_mul(cell_w);
        let dst_y = u32::from(page.row).saturating_mul(cell_h);
        let dst_w = if native {
            sx1 - sx0
        } else {
            u32::from(page.placement.columns).saturating_mul(cell_w)
        };
        let dst_h = if native {
            sy1 - sy0
        } else {
            u32::from(page.placement.rows).saturating_mul(cell_h)
        };
        let dst_y = dst_y.saturating_add(if native { page.placement.offset_y } else { 0 });
        for dy in 0..dst_h {
            let y = dst_y.saturating_add(dy);
            if y >= height {
                break;
            }
            let sy = sy0 + (u64::from(dy) * u64::from(sy1 - sy0) / u64::from(dst_h)) as u32;
            for dx in 0..dst_w {
                let x = dst_x.saturating_add(dx);
                if x >= width {
                    break;
                }
                let sx = sx0 + (u64::from(dx) * u64::from(sx1 - sx0) / u64::from(dst_w)) as u32;
                let source = ((sy * frame.width + sx) * 3) as usize;
                let target = ((y * width + x) * 4) as usize;
                output[target..target + 3].copy_from_slice(&rgb[source..source + 3]);
                output[target + 3] = 255;
            }
        }
    }
    if let Some(overlay) = overlay {
        let x = u32::from(overlay.col) * cell_w;
        let y = u32::from(overlay.row) * cell_h;
        for dy in 0..u32::from(overlay.pixel_height).min(height.saturating_sub(y)) {
            for dx in 0..u32::from(overlay.pixel_width).min(width.saturating_sub(x)) {
                let dst = (((y + dy) * width + x + dx) * 4) as usize;
                let src = ((dy * u32::from(overlay.pixel_width) + dx) * 4) as usize;
                blend(&mut output[dst..dst + 4], &overlay.rgba[src..src + 4]);
            }
        }
    }
    Ok(output)
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn save(
    path: &Path,
    viewport: Viewport,
    pages: &[Page<'_>],
    overlay: Option<&Overlay>,
    background: [u8; 3],
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::ipc::validate_screenshot_path(path)?;
    let rgba = compose(viewport, pages, overlay, background)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output has no parent",
        )
    })?;
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output has no filename",
        )
    })?;
    let temp = loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".pdfterm-shot-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let (temp_path, file) = temp;
    let encoded = (|| -> io::Result<()> {
        let mut encoder = Encoder::new(
            file,
            u32::from(viewport.pixel_width),
            u32::from(viewport.pixel_height),
        );
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;
        Ok(())
    })();
    if let Err(error) = encoded {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::hard_link(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    let _ = fs::remove_file(temp_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::{DarkModeStyle, FitMode, RenderKey};
    use crate::synctex::DocumentRevision;
    use std::time::Duration;

    #[test]
    fn cropped_overlay_preserves_viewport_pixels_and_cell_origin() {
        let viewport = Viewport {
            columns: 20,
            rows: 8,
            pixel_width: 160,
            pixel_height: 96,
            top: 3,
            status_row: 11,
        };
        let mut font = FlashFont::load("monospace").unwrap();
        let rects = [Rect {
            x: 35,
            y: 28,
            width: 19,
            height: 7,
            color: [20, 40, 80, 96],
        }];
        let badges = [Badge {
            x: 54,
            y: 25,
            layout: font.badge_layout("qz", 10).unwrap(),
            text: "qz",
            foreground: [240, 220, 200],
            background: [80, 30, 20],
        }];
        let full = overlay(&mut font, viewport, &rects, &badges).unwrap();
        let cropped = cropped_overlay(&mut font, viewport, &rects, &badges)
            .unwrap()
            .unwrap();
        let right = (54 + badges[0].layout.width).max(54).div_ceil(8) * 8;
        let bottom = (25 + badges[0].layout.height).max(35).div_ceil(12) * 12;
        assert_eq!((cropped.col, cropped.row), (4, 2));
        assert_eq!(
            (
                u32::from(cropped.pixel_width),
                u32::from(cropped.pixel_height)
            ),
            (right - 32, bottom - 24)
        );
        assert!(cropped.rgba.len() < full.len() / 4);
        let composed = compose(viewport, &[], Some(&cropped), [5, 10, 15]).unwrap();
        for (index, source) in full.as_chunks::<4>().0.iter().enumerate() {
            let mut expected = [5, 10, 15, 255];
            blend(&mut expected, source);
            assert_eq!(&composed[index * 4..index * 4 + 4], &expected);
        }
    }

    #[test]
    fn cropped_overlay_clips_viewport_edges_and_skips_invisible_rects() {
        let viewport = Viewport {
            columns: 480,
            rows: 135,
            pixel_width: 3840,
            pixel_height: 2160,
            top: 0,
            status_row: 135,
        };
        let mut font = FlashFont::load("monospace").unwrap();
        let rects = [
            Rect {
                x: 3837,
                y: 2157,
                width: 100,
                height: 100,
                color: [1, 2, 3, 255],
            },
            Rect {
                x: 4000,
                y: 20,
                width: 10,
                height: 10,
                color: [1, 2, 3, 255],
            },
        ];
        let zero_badge = [Badge {
            x: 0,
            y: 0,
            layout: BadgeLayout {
                width: 0,
                height: 10,
                glyph_size: 10,
                origin_x: 0,
                baseline: 0,
            },
            text: "a",
            foreground: [1, 2, 3],
            background: [4, 5, 6],
        }];
        let cropped = cropped_overlay(&mut font, viewport, &rects, &zero_badge)
            .unwrap()
            .unwrap();
        assert_eq!(
            (cropped.col, cropped.row, cropped.columns, cropped.rows),
            (479, 134, 1, 1)
        );
        assert_eq!((cropped.pixel_width, cropped.pixel_height), (8, 16));
        assert_eq!(cropped.rgba.len(), 8 * 16 * 4);
        assert!(
            cropped_overlay(&mut font, viewport, &rects[1..], &zero_badge)
                .unwrap()
                .is_none()
        );
        assert!(
            cropped_overlay(&mut font, viewport, &[], &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn compose_rejects_invalid_cropped_overlay_length() {
        let viewport = Viewport {
            columns: 2,
            rows: 2,
            pixel_width: 16,
            pixel_height: 32,
            top: 0,
            status_row: 2,
        };
        let overlay = Overlay {
            col: 1,
            row: 1,
            columns: 1,
            rows: 1,
            pixel_width: 8,
            pixel_height: 16,
            rgba: vec![0; 4],
        };
        assert_eq!(
            compose(viewport, &[], Some(&overlay), [0; 3])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn screenshot_uses_page_crop_and_viewport_origin() {
        let mut source = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                source.extend_from_slice(&[x * 40, y * 40, 0]);
            }
        }
        let compressed_rgba = crate::kitty::compress_rgba(&source).unwrap();
        let revision_file = tempfile::NamedTempFile::new().unwrap();
        let frame = Frame {
            key: RenderKey {
                document_id: 0,
                page: 0,
                width: 4,
                height: 4,
                zoom: 100,
                fit: FitMode::Page,
                invert: false,
                dark_mode_style: DarkModeStyle::new([0, 0, 0], [255, 255, 255]),
                search_request_id: 0,
                search_highlight: [255, 255, 0],
                link_mode: false,
                link_highlight: [0, 255, 255],
                selected_link_ordinal: None,
            },
            revision: DocumentRevision::read(revision_file.path()).unwrap(),
            width: 4,
            height: 4,
            page_width_pt: 4.0,
            page_height_pt: 4.0,
            content_bounds: [0.0, 0.0, 1.0, 1.0],
            compressed_rgba,
            render_elapsed: Duration::ZERO,
            dark_mode_elapsed: None,
            highlight_elapsed: None,
            compression_elapsed: Duration::ZERO,
            generation: 0,
            links: Vec::new(),
            flash: None,
        };
        let viewport = Viewport {
            columns: 2,
            rows: 2,
            pixel_width: 4,
            pixel_height: 4,
            top: 0,
            status_row: 2,
        };
        let placement = ImagePlacement {
            left: 0,
            columns: 2,
            rows: 2,
            crop: Some(crate::kitty::Crop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }),
            scroll_x: 1,
            scroll_y: 1,
            native_cell: Some((1, 1)),
            offset_y: 1,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("viewport.png");
        save(
            &path,
            viewport,
            &[Page {
                frame: &frame,
                placement,
                row: 0,
            }],
            None,
            [0, 0, 0],
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().expect("bounded PNG dimensions")];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (4, 4));
        let pixels = &pixels[..info.buffer_size()];
        assert_eq!(&pixels[..4], &[0, 0, 0, 255]);
        assert_eq!(&pixels[4..8], &[0, 0, 0, 255]);
        assert_eq!(&pixels[4 * 4..4 * 4 + 4], &[40, 40, 0, 255]);
        assert_eq!(&pixels[4 * 4 + 4..4 * 4 + 8], &[80, 40, 0, 255]);
        assert_eq!(&pixels[2 * 4 * 4..2 * 4 * 4 + 4], &[40, 80, 0, 255]);
        assert_eq!(&pixels[3 * 4 * 4..3 * 4 * 4 + 4], &[0, 0, 0, 255]);

        let mut bad_frame = Frame {
            compressed_rgba: crate::kitty::compress_rgba(&[0; 64]).unwrap(),
            key: frame.key,
            revision: frame.revision,
            width: 4,
            height: 4,
            page_width_pt: 4.0,
            page_height_pt: 4.0,
            content_bounds: [0.0, 0.0, 1.0, 1.0],
            render_elapsed: Duration::ZERO,
            dark_mode_elapsed: None,
            highlight_elapsed: None,
            compression_elapsed: Duration::ZERO,
            generation: 0,
            links: Vec::new(),
            flash: None,
        };
        assert!(
            compose(
                viewport,
                &[Page {
                    frame: &bad_frame,
                    placement,
                    row: 0,
                }],
                None,
                [0, 0, 0]
            )
            .is_err()
        );
        bad_frame.compressed_rgba.clear();
    }

    #[test]
    fn flash_labels_are_antialiased_and_respect_selected_font() {
        let viewport = Viewport {
            columns: 40,
            rows: 10,
            pixel_width: 160,
            pixel_height: 40,
            top: 0,
            status_row: 10,
        };
        let badge = |y, layout| Badge {
            x: 4,
            y,
            layout,
            text: "Data",
            foreground: [192, 202, 245],
            background: [255, 0, 124],
        };
        let mut mono = FlashFont::load("monospace").expect("system monospace font");
        let fit_badge = badge(2, mono.badge_layout("Data", 10).unwrap());
        let fit = overlay(&mut mono, viewport, &[], &[fit_badge]).unwrap();
        let zoom_badge = badge(2, mono.badge_layout("Data", 22).unwrap());
        let zoom = overlay(&mut mono, viewport, &[], &[zoom_badge]).unwrap();
        assert_ne!(fit, zoom, "zoom-sized glyphs must rasterize differently");
        assert!(
            fit.as_chunks::<4>().0.iter().any(|pixel| {
                pixel[3] == 255 && pixel[..3] != [255, 0, 124] && pixel[..3] != [192, 202, 245]
            }),
            "glyph edges should blend against the opaque label background"
        );

        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        let monospace = database.family_name(&fontdb::Family::Monospace);
        let mut families = std::collections::HashSet::new();
        for face in database.faces() {
            for (family, _) in &face.families {
                if family != monospace {
                    families.insert(family.clone());
                }
            }
        }
        let mut other_pixels = None;
        for family in families {
            if let Ok(mut other) = FlashFont::load(&family)
                && let Ok(layout) = other.badge_layout("Data", 10)
                && let Ok(pixels) = overlay(&mut other, viewport, &[], &[badge(2, layout)])
                && pixels != fit
            {
                other_pixels = Some(pixels);
                break;
            }
        }
        assert!(
            other_pixels.is_some(),
            "an installed alternative face should change rendered pixels"
        );
    }

    #[test]
    fn flash_label_rejects_unknown_font_family() {
        assert!(
            FlashFont::load("pdfterm-font-family-that-does-not-exist")
                .err()
                .is_some()
        );
    }
}

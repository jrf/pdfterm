use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::style::{ResetColor, SetBackgroundColor, SetForegroundColor, force_color_output};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};

use crate::kitty;
use crate::theme::Palette;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Viewport {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub top: u16,
    pub status_row: u16,
}

impl Viewport {
    pub fn detect(header_rows: u16) -> io::Result<Self> {
        let size = terminal::window_size()?;
        let status_row = size.rows.saturating_sub(1);
        let top = header_rows.min(status_row);
        let content_rows = status_row.saturating_sub(top).max(1);
        let cell_width = if size.columns == 0 {
            8
        } else {
            size.width
                .checked_div(size.columns)
                .filter(|value| *value > 0)
                .unwrap_or(8)
        };
        let cell_height = if size.rows == 0 {
            16
        } else {
            size.height
                .checked_div(size.rows)
                .filter(|value| *value > 0)
                .unwrap_or(16)
        };

        Ok(Self {
            columns: size.columns.max(1),
            rows: content_rows,
            // Exclude window padding/remainders: native images occupy the cell grid.
            pixel_width: size.columns.max(1).saturating_mul(cell_width).max(1),
            pixel_height: content_rows.saturating_mul(cell_height).max(1),
            top,
            status_row,
        })
    }

    /// The largest scroll offsets that keep the image edge aligned with the
    /// viewport, in image pixels. Zero on an axis means the image fits.
    pub fn max_scroll(self, image_width: u32, image_height: u32) -> (u32, u32) {
        (
            image_width.saturating_sub(u32::from(self.pixel_width)),
            image_height.saturating_sub(u32::from(self.pixel_height)),
        )
    }

    /// Places an image into the viewport, cropping to the visible region when it
    /// overflows and centering it on any axis where it fits. `scroll_x` and
    /// `scroll_y` are clamped to the valid range before use.
    pub fn place(
        self,
        image_width: u32,
        image_height: u32,
        scroll_x: u32,
        scroll_y: u32,
    ) -> ImagePlacement {
        let cell_width = (u32::from(self.pixel_width) / u32::from(self.columns)).max(1);
        let cell_height = (u32::from(self.pixel_height) / u32::from(self.rows)).max(1);
        let (max_scroll_x, max_scroll_y) = self.max_scroll(image_width, image_height);
        let scroll_x = scroll_x.min(max_scroll_x);
        let scroll_y = scroll_y.min(max_scroll_y);
        let visible_width = image_width.min(u32::from(self.pixel_width));
        let visible_height = image_height.min(u32::from(self.pixel_height));

        let columns = visible_width
            .div_ceil(cell_width)
            .min(u32::from(self.columns))
            .max(1) as u16;
        let rows = visible_height
            .div_ceil(cell_height)
            .min(u32::from(self.rows))
            .max(1) as u16;
        let left = self.columns.saturating_sub(columns) / 2;
        let crop = if max_scroll_x == 0 && max_scroll_y == 0 {
            None
        } else {
            Some(kitty::Crop {
                x: scroll_x,
                y: scroll_y,
                width: visible_width,
                height: visible_height,
            })
        };

        ImagePlacement {
            left,
            columns,
            rows,
            crop,
            scroll_x,
            scroll_y,
            native_cell: None,
            offset_y: 0,
        }
    }

    /// A continuous page crop at native pixel size, including a partial top cell.
    pub fn place_continuous(
        self,
        image_width: u32,
        image_height: u32,
        scroll_x: u32,
        source_y: u32,
        top: u32,
    ) -> Option<ImagePlacement> {
        let cell_width = (u32::from(self.pixel_width) / u32::from(self.columns)).max(1);
        let cell_height = (u32::from(self.pixel_height) / u32::from(self.rows)).max(1);
        let height = image_height
            .saturating_sub(source_y)
            .min((u32::from(self.rows) * cell_height).saturating_sub(top));
        if height == 0 {
            return None;
        }
        let mut placement = self.place(image_width, image_height, scroll_x, 0);
        // Window pixel sizes may include a remainder outside the cell grid.
        let width = image_width.min(u32::from(self.columns) * cell_width);
        placement.scroll_x = scroll_x.min(image_width.saturating_sub(width));
        placement.offset_y = top % cell_height;
        placement.rows = (placement.offset_y + height).div_ceil(cell_height) as u16;
        placement.scroll_y = source_y;
        placement.native_cell = Some((cell_width, cell_height));
        placement.crop = Some(kitty::Crop {
            x: placement.scroll_x,
            y: source_y,
            width,
            height,
        });
        Some(placement)
    }
}

/// The result of fitting an image into the viewport for a given scroll offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImagePlacement {
    pub left: u16,
    pub columns: u16,
    pub rows: u16,
    pub crop: Option<kitty::Crop>,
    pub scroll_x: u32,
    pub scroll_y: u32,
    /// Native pixel placement uses these cell dimensions for pointer mapping.
    pub native_cell: Option<(u32, u32)>,
    pub offset_y: u32,
}

impl ImagePlacement {
    /// Visible source pixels under a terminal cell; `row` is relative to image top.
    pub fn source_cell(
        self,
        column: u16,
        row: u16,
        image_width: u32,
        image_height: u32,
    ) -> Option<kitty::Crop> {
        let column = column.checked_sub(self.left)?;
        if column >= self.columns || row >= self.rows {
            return None;
        }
        let crop = self.crop.unwrap_or(kitty::Crop {
            x: 0,
            y: 0,
            width: image_width,
            height: image_height,
        });
        let (x0, x1, y0, y1) = if let Some((width, height)) = self.native_cell {
            (
                u32::from(column) * width,
                (u32::from(column) + 1) * width,
                (u32::from(row) * height).saturating_sub(self.offset_y),
                ((u32::from(row) + 1) * height).saturating_sub(self.offset_y),
            )
        } else {
            let boundary = |cell: u32, cells: u16, pixels: u32| {
                (u64::from(cell) * u64::from(pixels) / u64::from(cells)) as u32
            };
            (
                boundary(u32::from(column), self.columns, crop.width),
                boundary(u32::from(column) + 1, self.columns, crop.width),
                boundary(u32::from(row), self.rows, crop.height),
                boundary(u32::from(row) + 1, self.rows, crop.height),
            )
        };
        let x1 = x1.min(crop.width);
        let y1 = y1.min(crop.height);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some(kitty::Crop {
            x: crop.x + x0,
            y: crop.y + y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter(output: &mut impl Write, theme: Palette) -> io::Result<Self> {
        force_color_output(true);
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            output,
            EnterAlternateScreen,
            EnableMouseCapture,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg),
            DisableLineWrap,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut output = io::stdout();
        let _ = kitty::delete_all(&mut output);
        let _ = execute!(
            output,
            DisableMouseCapture,
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_viewport() -> Viewport {
        Viewport {
            columns: 100,
            rows: 40,
            pixel_width: 1000,
            pixel_height: 800,
            top: 0,
            status_row: 40,
        }
    }

    #[test]
    fn centers_image_without_exceeding_viewport() {
        let placement = sample_viewport().place(600, 800, 0, 0);

        assert_eq!(
            (placement.left, placement.columns, placement.rows),
            (20, 60, 40)
        );
        assert_eq!(placement.crop, None);
    }

    #[test]
    fn crops_and_clamps_scroll_when_image_overflows() {
        let viewport = sample_viewport();

        // A page fit to width that is twice as tall as the viewport.
        let placement = viewport.place(1000, 1600, 0, 5000);

        assert_eq!(placement.left, 0);
        assert_eq!(placement.columns, 100);
        assert_eq!(placement.scroll_y, 800);
        assert_eq!(
            placement.crop,
            Some(kitty::Crop {
                x: 0,
                y: 800,
                width: 1000,
                height: 800,
            })
        );
    }

    #[test]
    fn continuous_crops_keep_single_pixel_motion_and_partial_cell_hits() {
        let viewport = sample_viewport();
        let before = viewport.place_continuous(1000, 1600, 0, 19, 0).unwrap();
        let after = viewport.place_continuous(1000, 1600, 0, 20, 0).unwrap();
        assert_eq!(before.crop.unwrap().y, 19);
        assert_eq!(after.crop.unwrap().y, 20);
        assert_eq!(before.crop.unwrap().height, after.crop.unwrap().height);
        assert_eq!(before.source_cell(0, 0, 1000, 1600).unwrap().y, 19);

        // A short page starts 7 pixels into a cell, with a partial bottom row.
        let page = viewport.place_continuous(995, 26, 0, 0, 7).unwrap();
        let top = page.source_cell(0, 0, 995, 26).unwrap();
        let bottom = page.source_cell(0, 1, 995, 26).unwrap();
        assert_eq!((top.y, top.height), (0, 13));
        assert_eq!((bottom.y, bottom.height), (13, 13));
        assert_eq!(page.source_cell(99, 1, 995, 26).unwrap().width, 5);
        assert!(page.source_cell(0, 2, 995, 26).is_none());
        assert!(viewport.place_continuous(1000, 26, 0, 27, 0).is_none());

        // The last viewport row clips the page, not its placement scale.
        let clipped = viewport.place_continuous(1000, 1600, 0, 0, 793).unwrap();
        assert_eq!(clipped.crop.unwrap().height, 7);
        assert_eq!(clipped.source_cell(0, 0, 1000, 1600).unwrap().height, 7);

        let padded = Viewport {
            pixel_width: 1007,
            ..viewport
        };
        let edge = padded.place_continuous(1007, 1600, 7, 0, 0).unwrap();
        assert_eq!(edge.crop.unwrap().width, 1000);
        assert_eq!(edge.source_cell(99, 0, 1007, 1600).unwrap().x, 997);
    }
}

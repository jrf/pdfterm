use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::execute;
use crossterm::style::{ResetColor, SetBackgroundColor, SetForegroundColor, force_color_output};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};

use crate::kitty;
use crate::theme::TOKYO_NIGHT_MOON;

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
            pixel_width: size
                .width
                .max(size.columns.saturating_mul(cell_width))
                .max(1),
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
        }
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
}

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter(output: &mut impl Write) -> io::Result<Self> {
        force_color_output(true);
        terminal::enable_raw_mode()?;
        let theme = TOKYO_NIGHT_MOON;
        if let Err(error) = execute!(
            output,
            EnterAlternateScreen,
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

        assert_eq!((placement.left, placement.columns, placement.rows), (20, 60, 40));
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
}

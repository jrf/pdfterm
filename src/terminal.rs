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
}

impl Viewport {
    pub fn detect() -> io::Result<Self> {
        let size = terminal::window_size()?;
        let content_rows = size.rows.saturating_sub(1).max(1);
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
        })
    }

    pub fn placement_for(self, image_width: u32, image_height: u32) -> (u16, u16, u16) {
        let cell_width = (u32::from(self.pixel_width) / u32::from(self.columns)).max(1);
        let cell_height = (u32::from(self.pixel_height) / u32::from(self.rows)).max(1);
        let columns = image_width
            .div_ceil(cell_width)
            .min(u32::from(self.columns)) as u16;
        let rows = image_height.div_ceil(cell_height).min(u32::from(self.rows)) as u16;
        let left = self.columns.saturating_sub(columns) / 2;
        (left, columns.max(1), rows.max(1))
    }
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

    #[test]
    fn centers_image_without_exceeding_viewport() {
        let viewport = Viewport {
            columns: 100,
            rows: 40,
            pixel_width: 1000,
            pixel_height: 800,
        };

        assert_eq!(viewport.placement_for(600, 800), (20, 60, 40));
    }
}

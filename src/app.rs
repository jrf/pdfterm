use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::style::{Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use thiserror::Error;

use crate::kitty::{self, Placement};
use crate::pdf::{Frame, RenderKey, RenderRequest, RenderWorker, WorkerMessage};
use crate::terminal::{TerminalGuard, Viewport};
use crate::theme::TOKYO_NIGHT_MOON;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("pdfterm requires an interactive terminal")]
    NotInteractive,
    #[error("{0}")]
    Renderer(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn run(
    path: PathBuf,
    pdfium_library: Option<PathBuf>,
    start_page: u32,
) -> Result<(), AppError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::NotInteractive);
    }

    let worker = RenderWorker::spawn(path, pdfium_library);
    let page_count = worker.wait_until_ready().map_err(AppError::Renderer)?;
    let mut output = io::stdout();
    let _terminal = TerminalGuard::enter(&mut output)?;
    let mut app = App::new(worker, page_count, start_page.min(page_count - 1));
    app.request_current(&mut output)?;

    loop {
        while let Ok(message) = app.worker.try_recv() {
            match message {
                WorkerMessage::Frame(frame) => app.receive_frame(frame, &mut output)?,
                WorkerMessage::Error(error) => return Err(AppError::Renderer(error)),
                WorkerMessage::Ready { .. } => {}
            }
        }

        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && app.handle_key(key, &mut output)? =>
                {
                    break;
                }
                Event::Resize(_, _) => app.request_current(&mut output)?,
                _ => {}
            }
        }
    }

    Ok(())
}

struct App {
    worker: RenderWorker,
    page_count: u32,
    page: u32,
    generation: u64,
    desired_key: Option<RenderKey>,
    cache: HashMap<RenderKey, Arc<Frame>>,
    pending: HashSet<RenderKey>,
    visible_image_id: Option<u32>,
    next_image_id: u32,
}

impl App {
    fn new(worker: RenderWorker, page_count: u32, page: u32) -> Self {
        Self {
            worker,
            page_count,
            page,
            generation: 0,
            desired_key: None,
            cache: HashMap::new(),
            pending: HashSet::new(),
            visible_image_id: None,
            next_image_id: 1,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::PageDown
            | KeyCode::Char('j')
            | KeyCode::Char('l')
            | KeyCode::Char(' ') => self.set_page(self.page.saturating_add(1), output)?,
            KeyCode::Left
            | KeyCode::Up
            | KeyCode::PageUp
            | KeyCode::Backspace
            | KeyCode::Char('h')
            | KeyCode::Char('k') => self.set_page(self.page.saturating_sub(1), output)?,
            KeyCode::Char('g') | KeyCode::Home => self.set_page(0, output)?,
            KeyCode::Char('G') | KeyCode::End => self.set_page(self.page_count - 1, output)?,
            _ => {}
        }
        Ok(false)
    }

    fn set_page(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        let page = page.min(self.page_count - 1);
        if page != self.page {
            self.page = page;
            self.request_current(output)?;
        }
        Ok(())
    }

    fn request_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = Viewport::detect()?;
        let key = RenderKey {
            page: self.page,
            width: viewport.pixel_width,
            height: viewport.pixel_height,
        };
        self.desired_key = Some(key);
        self.generation = self.generation.wrapping_add(1);
        self.worker.begin_generation(self.generation);
        self.pending.clear();

        if let Some(frame) = self.cache.get(&key).cloned() {
            self.draw_frame(&frame, viewport, output)?;
            self.prefetch_neighbors(key);
        } else {
            self.draw_status(output, viewport, "rendering")?;
            if self.pending.insert(key) {
                self.worker
                    .render(RenderRequest {
                        key,
                        generation: self.generation,
                    })
                    .map_err(AppError::Renderer)?;
            }
        }
        Ok(())
    }

    fn receive_frame(&mut self, frame: Frame, output: &mut impl Write) -> Result<(), AppError> {
        let key = frame.key;
        self.pending.remove(&key);
        let frame = Arc::new(frame);
        self.cache.insert(key, Arc::clone(&frame));
        self.prune_cache(key);

        if self.desired_key == Some(key) {
            let viewport = Viewport::detect()?;
            let current_key = RenderKey {
                page: self.page,
                width: viewport.pixel_width,
                height: viewport.pixel_height,
            };
            if current_key == key {
                self.draw_frame(&frame, viewport, output)?;
                self.prefetch_neighbors(key);
            }
        }
        Ok(())
    }

    fn draw_frame(
        &mut self,
        frame: &Frame,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let (left, columns, rows) = viewport.placement_for(frame.width, frame.height);
        let image_id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);

        execute!(output, MoveTo(left, 0))?;
        let transfer_started = Instant::now();
        kitty::transmit_compressed_rgba(
            output,
            &frame.compressed_rgba,
            frame.width,
            frame.height,
            Placement {
                image_id,
                columns,
                rows,
            },
        )?;
        let transfer_elapsed = transfer_started.elapsed();
        if let Some(previous) = self.visible_image_id.replace(image_id) {
            kitty::delete_image(output, previous)?;
        }
        let state = format!(
            "render {}ms  compress {}ms  transfer {}ms",
            frame.render_elapsed.as_millis(),
            frame.compression_elapsed.as_millis(),
            transfer_elapsed.as_millis()
        );
        self.draw_status(output, viewport, &state)?;
        Ok(())
    }

    fn draw_status(
        &self,
        output: &mut impl Write,
        viewport: Viewport,
        state: &str,
    ) -> io::Result<()> {
        let theme = TOKYO_NIGHT_MOON;
        execute!(
            output,
            MoveTo(0, viewport.rows),
            SetBackgroundColor(theme.bg_dark),
            SetForegroundColor(theme.fg),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.blue),
            Print(self.page + 1),
            SetForegroundColor(theme.fg_dark),
            Print("/"),
            SetForegroundColor(theme.magenta),
            Print(self.page_count),
            Print("  "),
            SetForegroundColor(theme.green),
            Print(state),
            SetForegroundColor(theme.comment),
            Print("  j/k: page  g/G: first/last  q: quit"),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn prefetch_neighbors(&mut self, key: RenderKey) {
        for page in [key.page.checked_sub(1), key.page.checked_add(1)]
            .into_iter()
            .flatten()
            .filter(|page| *page < self.page_count)
        {
            let neighbor = RenderKey { page, ..key };
            if !self.cache.contains_key(&neighbor) && self.pending.insert(neighbor) {
                self.worker.prefetch(RenderRequest {
                    key: neighbor,
                    generation: self.generation,
                });
            }
        }
    }

    fn prune_cache(&mut self, current: RenderKey) {
        self.cache.retain(|key, _| {
            key.width == current.width
                && key.height == current.height
                && key.page.abs_diff(self.page) <= 1
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn page_navigation_bounds_are_saturating() {
        assert_eq!(0_u32.saturating_sub(1), 0);
        assert_eq!(u32::MAX.saturating_add(1), u32::MAX);
    }
}

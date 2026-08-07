use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use ratatui::Frame as RatatuiFrame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RatatuiColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear as RatatuiClear, Paragraph};
use thiserror::Error;

use crate::browser::BrowserState;
use crate::config::Config;
use crate::kitty::{self, Placement};
use crate::pdf::{
    DocumentId, FitMode, Frame, OutlineItem, RenderKey, RenderRequest, RenderWorker, WorkerMessage,
};
use crate::terminal::{TerminalGuard, Viewport};
use crate::theme::TOKYO_NIGHT_MOON;

const FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_STABLE_FOR: Duration = Duration::from_millis(150);
const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(500);
const INITIAL_DOCUMENT_ID: DocumentId = 1;

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
    path: Option<PathBuf>,
    pdfium_library: Option<PathBuf>,
    start_page: u32,
    config: &Config,
) -> Result<(), AppError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::NotInteractive);
    }

    let mut output = io::stdout();
    let _terminal = TerminalGuard::enter(&mut output)?;
    let path = match path {
        Some(path) => path.canonicalize()?,
        None => match pick_pdf(std::env::current_dir()?, &mut output)? {
            Some(path) => path,
            None => return Ok(()),
        },
    };
    let worker = RenderWorker::spawn(INITIAL_DOCUMENT_ID, path.clone(), pdfium_library);
    let (page_count, outline) = worker.wait_until_ready().map_err(AppError::Renderer)?;
    crate::recent::record(&path);
    let watcher = FileWatcher::new(&path)?;
    let mut app = App::new(
        worker,
        page_count,
        start_page.min(page_count - 1),
        path,
        watcher,
        outline,
        config,
    );
    app.request_current(&mut output)?;

    loop {
        while let Ok(message) = app.worker.try_recv() {
            match message {
                WorkerMessage::Frame(frame) => app.receive_frame(frame, &mut output)?,
                WorkerMessage::Error(error) => return Err(AppError::Renderer(error)),
                WorkerMessage::Ready { .. } => {}
                WorkerMessage::Opened {
                    document_id,
                    pages,
                    outline,
                } => app.finish_open(document_id, pages, outline, &mut output)?,
                WorkerMessage::OpenError { document_id, error } => {
                    app.fail_open(document_id, &error, &mut output)?
                }
                WorkerMessage::Text { content, .. } => app.copy_text(&content, &mut output)?,
            }
        }
        app.poll_file_change(&mut output)?;

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
    pending_open: Option<PendingOpen>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_document_id: DocumentId,
    generation: u64,
    desired_key: Option<RenderKey>,
    pending: HashSet<RenderKey>,
    visible_image_id: Option<u32>,
    next_image_id: u32,
    last_status_row: Option<u16>,
    default_fit: FitMode,
    default_invert: bool,
    goto_input: Option<String>,
}

struct Tab {
    document_id: DocumentId,
    path: PathBuf,
    watcher: FileWatcher,
    page_count: u32,
    page: u32,
    fit: FitMode,
    invert: bool,
    scroll_x: u32,
    scroll_y: u32,
    outline: Arc<Vec<OutlineItem>>,
    cache: HashMap<RenderKey, Arc<Frame>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

impl Tab {
    fn render_key(&self, viewport: Viewport) -> RenderKey {
        RenderKey {
            document_id: self.document_id,
            page: self.page,
            width: viewport.pixel_width,
            height: viewport.pixel_height,
            fit: self.fit,
            invert: self.invert,
        }
    }
}

enum PendingOpen {
    Reload {
        document_id: DocumentId,
        fingerprint: FileFingerprint,
    },
    Selection {
        document_id: DocumentId,
        path: PathBuf,
    },
}

impl App {
    fn new(
        worker: RenderWorker,
        page_count: u32,
        page: u32,
        path: PathBuf,
        watcher: FileWatcher,
        outline: Vec<OutlineItem>,
        config: &Config,
    ) -> Self {
        let default_fit = config.fit_mode();
        let default_invert = config.invert();
        Self {
            worker,
            pending_open: None,
            tabs: vec![Tab {
                document_id: INITIAL_DOCUMENT_ID,
                path,
                watcher,
                page_count,
                page,
                fit: default_fit,
                invert: default_invert,
                scroll_x: 0,
                scroll_y: 0,
                outline: Arc::new(outline),
                cache: HashMap::new(),
            }],
            active_tab: 0,
            next_document_id: INITIAL_DOCUMENT_ID + 1,
            generation: 0,
            desired_key: None,
            pending: HashSet::new(),
            visible_image_id: None,
            next_image_id: 1,
            last_status_row: None,
            default_fit,
            default_invert,
            goto_input: None,
        }
    }

    fn poll_file_change(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        let change = self.tabs.iter_mut().find_map(|tab| {
            tab.watcher
                .poll(&tab.path)
                .map(|fingerprint| (tab.document_id, tab.path.clone(), fingerprint))
        });
        let Some((document_id, path, fingerprint)) = change else {
            return Ok(());
        };

        self.worker
            .open(document_id, path)
            .map_err(AppError::Renderer)?;
        self.pending_open = Some(PendingOpen::Reload {
            document_id,
            fingerprint,
        });
        if self.tab().document_id == document_id {
            let viewport = self.prepare_viewport(output)?;
            self.draw_status(output, viewport, "reloading")?;
        }
        Ok(())
    }

    fn finish_open(
        &mut self,
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(pending) = self.pending_open.take() else {
            return Ok(());
        };
        match pending {
            PendingOpen::Reload {
                document_id: expected,
                fingerprint,
            } if expected == document_id => {
                let Some(index) = self.tab_index(document_id) else {
                    return Ok(());
                };
                let tab = &mut self.tabs[index];
                tab.watcher.accept(fingerprint);
                tab.page_count = pages;
                tab.page = tab.page.min(pages - 1);
                tab.scroll_x = 0;
                tab.scroll_y = 0;
                tab.outline = Arc::new(outline);
                tab.cache.clear();
                if index == self.active_tab {
                    self.reset_render_state();
                    self.request_current(output)?;
                }
            }
            PendingOpen::Selection {
                document_id: expected,
                path,
            } if expected == document_id => {
                crate::recent::record(&path);
                let watcher = FileWatcher::new(&path)?;
                self.tabs.push(Tab {
                    document_id,
                    path,
                    watcher,
                    page_count: pages,
                    page: 0,
                    fit: self.default_fit,
                    invert: self.default_invert,
                    scroll_x: 0,
                    scroll_y: 0,
                    outline: Arc::new(outline),
                    cache: HashMap::new(),
                });
                self.active_tab = self.tabs.len() - 1;
                self.reset_render_state();
                self.request_current(output)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn fail_open(
        &mut self,
        document_id: DocumentId,
        error: &str,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let state = match self.pending_open.take() {
            Some(PendingOpen::Reload {
                document_id: expected,
                ..
            }) if expected == document_id => {
                if let Some(index) = self.tab_index(document_id) {
                    self.tabs[index].watcher.defer(RELOAD_RETRY_DELAY);
                }
                format!("reload failed: {error}; retrying")
            }
            Some(PendingOpen::Selection {
                document_id: expected,
                ..
            }) if expected == document_id => format!("open failed: {error}"),
            _ => return Ok(()),
        };
        self.request_current(output)?;
        self.draw_status(output, self.viewport()?, &state)?;
        Ok(())
    }

    fn open_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let directory = self
            .tab()
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        match pick_pdf(directory, output)? {
            Some(path) => {
                let path = path.canonicalize()?;
                if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
                    self.active_tab = index;
                    self.reset_render_state();
                    self.request_current(output)?;
                } else {
                    let document_id = self.next_document_id;
                    self.next_document_id = self.next_document_id.wrapping_add(1).max(1);
                    self.worker
                        .open(document_id, path.clone())
                        .map_err(AppError::Renderer)?;
                    self.pending_open = Some(PendingOpen::Selection { document_id, path });
                    let viewport = self.prepare_viewport(output)?;
                    self.draw_status(output, viewport, "opening")?;
                }
            }
            None => self.request_current(output)?,
        }
        Ok(())
    }

    fn open_outline(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        let outline = Arc::clone(&self.tab().outline);
        if outline.is_empty() {
            let viewport = self.prepare_viewport(output)?;
            self.draw_status(output, viewport, "no outline in this document")?;
            return Ok(());
        }
        self.clear_viewer(output)?;
        let selection = pick_outline(&outline, self.tab().page, output)?;
        if let Some(page) = selection {
            let page = page.min(self.tab().page_count - 1);
            self.tab_mut().page = page;
            self.tab_mut().scroll_x = 0;
            self.tab_mut().scroll_y = 0;
        }
        self.reset_render_state();
        self.request_current(output)
    }

    fn begin_goto(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        self.goto_input = Some(String::new());
        let viewport = self.viewport()?;
        self.draw_goto(output, viewport)?;
        Ok(())
    }

    fn handle_goto_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<(), AppError> {
        match key.code {
            KeyCode::Esc => {
                self.goto_input = None;
                self.redraw_current(output)?;
            }
            KeyCode::Enter => {
                let input = self.goto_input.take().unwrap_or_default();
                let target = input
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|number| *number >= 1)
                    .map(|number| (number - 1).min(self.tab().page_count - 1));
                match target {
                    Some(page) if page != self.tab().page => self.set_page(page, output)?,
                    _ => self.redraw_current(output)?,
                }
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.goto_input.as_mut() {
                    buffer.pop();
                }
                let viewport = self.viewport()?;
                self.draw_goto(output, viewport)?;
            }
            KeyCode::Char(character) if character.is_ascii_digit() => {
                if let Some(buffer) = self.goto_input.as_mut().filter(|buffer| buffer.len() < 9) {
                    buffer.push(character);
                }
                let viewport = self.viewport()?;
                self.draw_goto(output, viewport)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_goto(&self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        let theme = TOKYO_NIGHT_MOON;
        let input = self.goto_input.as_deref().unwrap_or_default();
        let hint = format!(
            "  (1-{}, enter to jump, esc to cancel)",
            self.tab().page_count
        );
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.yellow),
            Print("go to page: "),
            SetForegroundColor(theme.fg),
            Print(input),
            SetForegroundColor(theme.comment),
            Print(hint),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
        if self.goto_input.is_some() {
            self.handle_goto_key(key, output)?;
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return self.close_current(output),
            KeyCode::Char(':') => self.begin_goto(output)?,
            KeyCode::Esc => return Ok(true),
            KeyCode::Char('f') => self.open_picker(output)?,
            KeyCode::Tab => self.switch_tab(1, output)?,
            KeyCode::BackTab => self.switch_tab(-1, output)?,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_view(Axis::Vertical, true, false, output)?
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_view(Axis::Vertical, false, false, output)?
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.move_view(Axis::Vertical, true, true, output)?
            }
            KeyCode::PageUp | KeyCode::Backspace => {
                self.move_view(Axis::Vertical, false, true, output)?
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_view(Axis::Horizontal, true, false, output)?
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_view(Axis::Horizontal, false, false, output)?
            }
            KeyCode::Char('g') | KeyCode::Home => self.set_page(0, output)?,
            KeyCode::Char('G') | KeyCode::End => {
                self.set_page(self.tab().page_count - 1, output)?
            }
            KeyCode::Char('m') => self.cycle_fit(output)?,
            KeyCode::Char('i') => self.toggle_invert(output)?,
            KeyCode::Char('t') => self.open_outline(output)?,
            KeyCode::Char('y') => self.request_copy(output)?,
            _ => {}
        }
        Ok(false)
    }

    fn set_page(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        let page = page.min(self.tab().page_count - 1);
        if page != self.tab().page {
            self.tab_mut().page = page;
            self.tab_mut().scroll_x = 0;
            self.tab_mut().scroll_y = 0;
            self.request_current(output)?;
        }
        Ok(())
    }

    /// Moves along one axis: scrolls the rendered page when it overflows the
    /// viewport on that axis, and changes page at the far edge (or immediately
    /// when the page already fits).
    fn move_view(
        &mut self,
        axis: Axis,
        forward: bool,
        large: bool,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        let key = self.tab().render_key(viewport);
        let Some(frame) = self.tab().cache.get(&key).cloned() else {
            return self.page_step(forward, output);
        };
        let (max_x, max_y) = viewport.max_scroll(frame.width, frame.height);
        let (axis_max, current) = match axis {
            Axis::Vertical => (max_y, self.tab().scroll_y),
            Axis::Horizontal => (max_x, self.tab().scroll_x),
        };
        if axis_max == 0 {
            return self.page_step(forward, output);
        }

        let span = match axis {
            Axis::Vertical => u32::from(viewport.pixel_height),
            Axis::Horizontal => u32::from(viewport.pixel_width),
        };
        let step = if large {
            (span * 85 / 100).max(1)
        } else {
            (span / 8).max(1)
        };

        let next = if forward {
            if current >= axis_max {
                return self.page_step(true, output);
            }
            (current + step).min(axis_max)
        } else {
            if current == 0 {
                return self.page_step(false, output);
            }
            current.saturating_sub(step)
        };
        match axis {
            Axis::Vertical => self.tab_mut().scroll_y = next,
            Axis::Horizontal => self.tab_mut().scroll_x = next,
        }
        self.redraw_current(output)
    }

    fn page_step(&mut self, forward: bool, output: &mut impl Write) -> Result<(), AppError> {
        let page = if forward {
            self.tab().page.saturating_add(1)
        } else {
            self.tab().page.saturating_sub(1)
        };
        self.set_page(page, output)
    }

    /// Redraws the current page from cache with the current scroll offset,
    /// without asking the worker to render again.
    fn redraw_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        let key = self.tab().render_key(viewport);
        if let Some(frame) = self.tab().cache.get(&key).cloned() {
            self.draw_frame(&frame, viewport, output)?;
        }
        Ok(())
    }

    fn request_copy(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        let (document_id, page) = {
            let tab = self.tab();
            (tab.document_id, tab.page)
        };
        self.worker.extract_text(document_id, page);
        let viewport = self.viewport()?;
        self.draw_status(output, viewport, "copying page text...")?;
        Ok(())
    }

    fn copy_text(&mut self, content: &str, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        if content.trim().is_empty() {
            self.draw_status(output, viewport, "no selectable text on this page")?;
            return Ok(());
        }
        write_clipboard_osc52(output, content)?;
        let message = format!(
            "copied {} characters to the clipboard",
            content.chars().count()
        );
        self.draw_status(output, viewport, &message)?;
        Ok(())
    }

    fn cycle_fit(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let next = self.tab().fit.cycle();
        self.tab_mut().fit = next;
        self.tab_mut().scroll_x = 0;
        self.tab_mut().scroll_y = 0;
        self.request_current(output)
    }

    fn toggle_invert(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let inverted = !self.tab().invert;
        self.tab_mut().invert = inverted;
        self.request_current(output)
    }

    fn request_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.prepare_viewport(output)?;
        self.draw_tab_bar(output)?;
        let key = self.tab().render_key(viewport);
        self.desired_key = Some(key);
        self.generation = self.generation.wrapping_add(1);
        self.worker.begin_generation(self.generation);
        self.pending.clear();

        if let Some(frame) = self.tab().cache.get(&key).cloned() {
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
        let Some(index) = self.tab_index(key.document_id) else {
            return Ok(());
        };
        let current_page = self.tabs[index].page;
        self.tabs[index].cache.insert(key, Arc::clone(&frame));
        self.tabs[index].cache.retain(|cached, _| {
            cached.width == key.width
                && cached.height == key.height
                && cached.fit == key.fit
                && cached.invert == key.invert
                && cached.page.abs_diff(current_page) <= 1
        });

        if self.desired_key == Some(key) {
            let viewport = self.viewport()?;
            let current_key = self.tab().render_key(viewport);
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
        let tab = self.tab();
        let placement = viewport.place(frame.width, frame.height, tab.scroll_x, tab.scroll_y);
        self.tab_mut().scroll_x = placement.scroll_x;
        self.tab_mut().scroll_y = placement.scroll_y;
        let image_id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);

        execute!(output, MoveTo(placement.left, viewport.top))?;
        let transfer_started = Instant::now();
        kitty::transmit_compressed_rgba(
            output,
            &frame.compressed_rgba,
            frame.width,
            frame.height,
            Placement {
                image_id,
                columns: placement.columns,
                rows: placement.rows,
                crop: placement.crop,
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
        let tab = self.tab();
        let mut mode = String::new();
        if tab.fit != FitMode::Page {
            mode.push_str(tab.fit.label());
        }
        if tab.invert {
            if !mode.is_empty() {
                mode.push(' ');
            }
            mode.push_str("invert");
        }
        if !mode.is_empty() {
            mode.push_str("  ");
        }
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            SetForegroundColor(theme.fg),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.blue),
            Print(tab.page + 1),
            SetForegroundColor(theme.fg_dark),
            Print("/"),
            SetForegroundColor(theme.magenta),
            Print(tab.page_count),
            Print("  "),
            SetForegroundColor(theme.yellow),
            Print(&mode),
            SetForegroundColor(theme.green),
            Print(state),
            SetForegroundColor(theme.comment),
            Print("  t: toc  :: goto  m: fit  i: invert  y: copy  f: tab  q: close"),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn prefetch_neighbors(&mut self, key: RenderKey) {
        let page_count = self.tab().page_count;
        for page in [key.page.checked_sub(1), key.page.checked_add(1)]
            .into_iter()
            .flatten()
            .filter(|page| *page < page_count)
        {
            let neighbor = RenderKey { page, ..key };
            let cached = self.tab().cache.contains_key(&neighbor);
            if !cached && self.pending.insert(neighbor) {
                self.worker.prefetch(RenderRequest {
                    key: neighbor,
                    generation: self.generation,
                });
            }
        }
    }

    fn switch_tab(&mut self, direction: i32, output: &mut impl Write) -> Result<(), AppError> {
        if self.tabs.len() < 2 || self.pending_open.is_some() {
            return Ok(());
        }
        self.active_tab = cycled_tab_index(self.active_tab, self.tabs.len(), direction);
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)
    }

    fn close_current(&mut self, output: &mut impl Write) -> Result<bool, AppError> {
        if self.pending_open.is_some() {
            return Ok(false);
        }
        if self.tabs.len() == 1 {
            return Ok(true);
        }
        let removed = self.tabs.remove(self.active_tab);
        self.worker.close(removed.document_id);
        if self.active_tab == self.tabs.len() {
            self.active_tab -= 1;
        }
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)?;
        Ok(false)
    }

    fn clear_viewer(&mut self, output: &mut impl Write) -> io::Result<()> {
        let theme = TOKYO_NIGHT_MOON;
        kitty::delete_all(output)?;
        self.visible_image_id = None;
        self.last_status_row = None;
        execute!(
            output,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg),
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        output.flush()
    }

    fn prepare_viewport(&mut self, output: &mut impl Write) -> io::Result<Viewport> {
        let viewport = self.viewport()?;
        if let Some(row) = stale_status_row(self.last_status_row, viewport.status_row) {
            let theme = TOKYO_NIGHT_MOON;
            execute!(
                output,
                MoveTo(0, row),
                SetBackgroundColor(theme.bg),
                SetForegroundColor(theme.fg),
                Clear(ClearType::CurrentLine)
            )?;
        }
        self.last_status_row = Some(viewport.status_row);
        Ok(viewport)
    }

    fn draw_tab_bar(&self, output: &mut impl Write) -> io::Result<()> {
        if self.tabs.len() < 2 {
            return Ok(());
        }
        let theme = TOKYO_NIGHT_MOON;
        let columns = usize::from(crossterm::terminal::size()?.0);
        execute!(
            output,
            MoveTo(0, 0),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine)
        )?;
        let mut used = 0;
        for (index, tab) in self.tabs.iter().enumerate() {
            let name = tab
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| tab.path.to_string_lossy());
            let label = format!(" {}:{} ", index + 1, name);
            let label: String = label.chars().take(columns.saturating_sub(used)).collect();
            if label.is_empty() {
                break;
            }
            if index == self.active_tab {
                execute!(
                    output,
                    SetBackgroundColor(theme.bg_highlight),
                    SetForegroundColor(theme.fg),
                    Print(&label)
                )?;
            } else {
                execute!(
                    output,
                    SetBackgroundColor(theme.bg_dark),
                    SetForegroundColor(theme.fg_dark),
                    Print(&label)
                )?;
            }
            used += label.chars().count();
        }
        execute!(
            output,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn reset_render_state(&mut self) {
        self.desired_key = None;
        self.pending.clear();
    }

    fn viewport(&self) -> io::Result<Viewport> {
        Viewport::detect(u16::from(self.tabs.len() > 1))
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    fn tab_index(&self, document_id: DocumentId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.document_id == document_id)
    }
}

/// Writes text to the system clipboard using the OSC 52 escape sequence, which
/// works over SSH because the terminal emulator performs the copy locally.
fn write_clipboard_osc52(output: &mut impl Write, text: &str) -> io::Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(output, "\x1b]52;c;{encoded}\x07")?;
    output.flush()
}

fn stale_status_row(previous: Option<u16>, current: u16) -> Option<u16> {
    previous.filter(|row| *row < current)
}

fn cycled_tab_index(active: usize, len: usize, direction: i32) -> usize {
    if direction > 0 {
        (active + 1) % len
    } else if active == 0 {
        len - 1
    } else {
        active - 1
    }
}

fn pick_pdf(directory: PathBuf, output: &mut impl Write) -> Result<Option<PathBuf>, AppError> {
    let mut browser = BrowserState::new(directory);
    browser.set_recents(crate::recent::load());
    browser.preload_recursive();
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;

    let selection = loop {
        if browser.poll_recursive() {
            redraw = true;
        }
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(4).max(1));
            browser.adjust_scroll(visible_height);
            terminal.draw(|frame| draw_picker(frame, &browser))?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if apply_picker_navigation(&mut browser, key, visible_height) {
            redraw = true;
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Enter => {
                if let Some(path) = browser.enter_selected() {
                    break Some(path);
                }
            }
            KeyCode::Backspace => {
                browser.filter.pop();
                browser.rebuild_filter();
                browser.select_first();
                browser.scroll_offset = 0;
            }
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                browser.filter.push(character);
                browser.rebuild_filter();
                browser.select_first();
                browser.scroll_offset = 0;
            }
            _ => {}
        }
        redraw = true;
    };
    drop(terminal);
    clear_picker(output)?;
    Ok(selection)
}

fn clear_picker(output: &mut impl Write) -> io::Result<()> {
    let theme = TOKYO_NIGHT_MOON;
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    output.flush()
}

fn pick_outline(
    items: &[OutlineItem],
    current_page: u32,
    output: &mut impl Write,
) -> Result<Option<u32>, AppError> {
    let mut filter = String::new();
    let mut filtered: Vec<usize> = (0..items.len()).collect();
    let mut selected = outline_start_index(items, current_page);
    let mut scroll_offset = 0usize;
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;

    let selection = loop {
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(4).max(1));
            if selected < scroll_offset {
                scroll_offset = selected;
            } else if selected >= scroll_offset + visible_height {
                scroll_offset = selected - visible_height + 1;
            }
            terminal.draw(|frame| {
                draw_outline(frame, items, &filtered, selected, scroll_offset, &filter)
            })?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                redraw = true;
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Enter => {
                if let Some(index) = filtered.get(selected) {
                    break Some(items[*index].page);
                }
            }
            KeyCode::Down => selected = (selected + 1).min(last),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('j') if control => selected = (selected + 1).min(last),
            KeyCode::Char('k') if control => selected = selected.saturating_sub(1),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = last,
            KeyCode::PageDown => selected = (selected + visible_height).min(last),
            KeyCode::PageUp => selected = selected.saturating_sub(visible_height),
            KeyCode::Backspace => {
                filter.pop();
                filtered = filter_outline(items, &filter);
                selected = 0;
                scroll_offset = 0;
            }
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                filter.push(character);
                filtered = filter_outline(items, &filter);
                selected = 0;
                scroll_offset = 0;
            }
            _ => {}
        }
        redraw = true;
    };
    drop(terminal);
    clear_picker(output)?;
    Ok(selection)
}

fn outline_start_index(items: &[OutlineItem], current_page: u32) -> usize {
    items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| item.page <= current_page)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn filter_outline(items: &[OutlineItem], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let haystack = Utf32Str::new(&item.title, &mut buffer);
            pattern
                .score(haystack, &mut matcher)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(index, _)| index).collect()
}

fn draw_outline(
    frame: &mut RatatuiFrame,
    items: &[OutlineItem],
    filtered: &[usize],
    selected: usize,
    scroll_offset: usize,
    filter: &str,
) {
    let theme = TOKYO_NIGHT_MOON;
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(picker_color(theme.bg))),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(picker_color(theme.blue)))
        .title(" Outline ")
        .title_style(
            Style::default()
                .fg(picker_color(theme.blue))
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter_line = if filter.is_empty() {
        Line::from(Span::styled(
            " type to filter...",
            Style::default().fg(picker_color(theme.comment)),
        ))
    } else {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(picker_color(theme.blue))),
            Span::styled(filter, Style::default().fg(picker_color(theme.fg))),
        ])
    };
    frame.render_widget(Paragraph::new(filter_line), rows[0]);

    let visible_height = usize::from(rows[1].height);
    let width = usize::from(rows[1].width);
    let mut lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(position, item_index)| {
            let item = &items[*item_index];
            let is_selected = position == selected;
            let style = if is_selected {
                Style::default()
                    .fg(picker_color(theme.fg))
                    .bg(picker_color(theme.bg_highlight))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(picker_color(theme.fg))
            };
            let indent = "  ".repeat(usize::from(item.depth).min(6) + 1);
            let page_label = format!(" {} ", item.page + 1);
            let mut line = Line::from(vec![
                Span::styled(indent, style),
                Span::styled(item.title.clone(), style),
            ]);
            let used = line.width();
            let page_width = page_label.chars().count();
            if used + page_width < width {
                let page_style = if is_selected {
                    style
                } else {
                    Style::default().fg(picker_color(theme.comment))
                };
                line.spans
                    .push(Span::styled(" ".repeat(width - used - page_width), style));
                line.spans.push(Span::styled(page_label, page_style));
            }
            line
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No matches",
            Style::default().fg(picker_color(theme.comment)),
        )));
    }
    frame.render_widget(Paragraph::new(lines), rows[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " enter:jump  esc:close",
            Style::default().fg(picker_color(theme.comment)),
        ))),
        rows[2],
    );
}

fn apply_picker_navigation(
    browser: &mut BrowserState,
    key: KeyEvent,
    visible_height: usize,
) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Down => browser.select_down(),
        KeyCode::Up => browser.select_up(),
        KeyCode::Char('j') if control => browser.select_down(),
        KeyCode::Char('k') if control => browser.select_up(),
        KeyCode::Home => browser.select_first(),
        KeyCode::End => browser.select_last(),
        KeyCode::PageDown => browser.page_down(visible_height),
        KeyCode::PageUp => browser.page_up(visible_height),
        _ => return false,
    }
    true
}

fn draw_picker(frame: &mut RatatuiFrame, browser: &BrowserState) {
    let theme = TOKYO_NIGHT_MOON;
    let entries: Vec<_> = browser.filtered_entries().collect();
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(picker_color(theme.bg))),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let directory = shorten_path(&browser.current_dir.display().to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(picker_color(theme.blue)))
        .title(format!(" {directory} "))
        .title_style(
            Style::default()
                .fg(picker_color(theme.blue))
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter = if browser.filter.is_empty() {
        Line::from(Span::styled(
            " type to filter...",
            Style::default().fg(picker_color(theme.comment)),
        ))
    } else {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(picker_color(theme.blue))),
            Span::styled(
                browser.filter.as_str(),
                Style::default().fg(picker_color(theme.fg)),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(filter), rows[0]);

    let visible_height = usize::from(rows[1].height);
    let mut lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(browser.scroll_offset)
        .take(visible_height)
        .map(|(index, entry)| {
            let selected = index == browser.selected;
            let style = if selected {
                Style::default()
                    .fg(picker_color(theme.fg))
                    .bg(picker_color(theme.bg_highlight))
                    .add_modifier(Modifier::BOLD)
            } else if entry.name == "../" {
                Style::default().fg(picker_color(theme.fg_dark))
            } else if entry.is_dir {
                Style::default().fg(picker_color(theme.blue))
            } else {
                Style::default().fg(picker_color(theme.fg))
            };
            let icon = if entry.name == "../" {
                "^ "
            } else if entry.is_dir {
                "/ "
            } else {
                "  "
            };
            let mut line = Line::from(vec![
                Span::styled("   ", style),
                Span::styled(icon, style),
                Span::styled(entry.name.as_str(), style),
            ]);
            if selected {
                let used = line.width();
                let width = usize::from(rows[1].width);
                if used < width {
                    line.spans
                        .push(Span::styled(" ".repeat(width - used), style));
                }
            }
            line
        })
        .collect();
    if lines.is_empty() {
        let message = if browser.filter.is_empty() {
            "   No PDF files found"
        } else {
            "   No matches"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(picker_color(theme.comment)),
        )));
    }
    frame.render_widget(Paragraph::new(lines), rows[1]);

    let hint = if browser.recursive_loading() {
        " enter:open  esc:close  (loading...)"
    } else {
        " enter:open  esc:close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(picker_color(theme.comment)),
        ))),
        rows[2],
    );
}

fn picker_rect(area: Rect) -> Rect {
    let width = if area.width > 4 {
        (area.width * 3 / 4).max(50).min(area.width - 4)
    } else {
        area.width.max(1)
    };
    let height = if area.height > 4 {
        (area.height * 3 / 4).max(6).min(area.height - 2)
    } else {
        area.height.max(1)
    };
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn shorten_path(path: &str) -> String {
    std::env::var_os("HOME")
        .and_then(|home| {
            path.strip_prefix(home.to_string_lossy().as_ref())
                .map(|suffix| format!("~{suffix}"))
        })
        .unwrap_or_else(|| path.to_string())
}

fn picker_color(color: crossterm::style::Color) -> RatatuiColor {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => RatatuiColor::Rgb(r, g, b),
        _ => RatatuiColor::Reset,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileFingerprint {
    length: u64,
    modified: SystemTime,
}

impl FileFingerprint {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

struct FileWatcher {
    accepted: FileFingerprint,
    candidate: Option<(FileFingerprint, Instant)>,
    next_poll: Instant,
}

impl FileWatcher {
    fn new(path: &Path) -> io::Result<Self> {
        Ok(Self {
            accepted: FileFingerprint::read(path)?,
            candidate: None,
            next_poll: Instant::now() + FILE_POLL_INTERVAL,
        })
    }

    fn poll(&mut self, path: &Path) -> Option<FileFingerprint> {
        let now = Instant::now();
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + FILE_POLL_INTERVAL;

        let fingerprint = match FileFingerprint::read(path) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                self.candidate = None;
                return None;
            }
        };
        self.observe(fingerprint, now)
    }

    fn observe(&mut self, fingerprint: FileFingerprint, now: Instant) -> Option<FileFingerprint> {
        if fingerprint == self.accepted {
            self.candidate = None;
            return None;
        }

        match self.candidate {
            Some((candidate, since))
                if candidate == fingerprint && now.duration_since(since) >= FILE_STABLE_FOR =>
            {
                Some(fingerprint)
            }
            Some((candidate, _)) if candidate == fingerprint => None,
            _ => {
                self.candidate = Some((fingerprint, now));
                None
            }
        }
    }

    fn accept(&mut self, fingerprint: FileFingerprint) {
        self.accepted = fingerprint;
        if self
            .candidate
            .is_some_and(|(candidate, _)| candidate == fingerprint)
        {
            self.candidate = None;
        }
    }

    fn defer(&mut self, duration: Duration) {
        self.next_poll = Instant::now() + duration;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserState, FILE_STABLE_FOR, FileFingerprint, FileWatcher, apply_picker_navigation,
        clear_picker, cycled_tab_index, draw_picker, filter_outline, outline_start_index,
        picker_rect, stale_status_row, write_clipboard_osc52,
    };
    use crate::pdf::OutlineItem;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::fs;
    use std::time::{Duration, Instant, SystemTime};

    #[test]
    fn page_navigation_bounds_are_saturating() {
        assert_eq!(0_u32.saturating_sub(1), 0);
        assert_eq!(u32::MAX.saturating_add(1), u32::MAX);
    }

    #[test]
    fn tab_switching_wraps_in_both_directions() {
        assert_eq!(cycled_tab_index(0, 3, 1), 1);
        assert_eq!(cycled_tab_index(2, 3, 1), 0);
        assert_eq!(cycled_tab_index(2, 3, -1), 1);
        assert_eq!(cycled_tab_index(0, 3, -1), 2);
    }

    #[test]
    fn growing_viewport_clears_the_previous_status_row() {
        assert_eq!(stale_status_row(Some(20), 40), Some(20));
        assert_eq!(stale_status_row(Some(40), 20), None);
        assert_eq!(stale_status_row(Some(40), 40), None);
        assert_eq!(stale_status_row(None, 40), None);
    }

    #[test]
    fn file_changes_must_stabilize_before_reload() {
        let initial = FileFingerprint {
            length: 10,
            modified: SystemTime::UNIX_EPOCH,
        };
        let changed = FileFingerprint {
            length: 20,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        };
        let started = Instant::now();
        let mut watcher = FileWatcher {
            accepted: initial,
            candidate: None,
            next_poll: started,
        };

        assert_eq!(watcher.observe(changed, started), None);
        assert_eq!(
            watcher.observe(changed, started + FILE_STABLE_FOR),
            Some(changed)
        );
        watcher.accept(changed);
        assert_eq!(watcher.observe(changed, started + FILE_STABLE_FOR), None);
    }

    fn outline_fixture() -> Vec<OutlineItem> {
        vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 0,
                depth: 0,
            },
            OutlineItem {
                title: "Background".into(),
                page: 4,
                depth: 1,
            },
            OutlineItem {
                title: "Results".into(),
                page: 9,
                depth: 0,
            },
        ]
    }

    #[test]
    fn clipboard_write_uses_base64_osc52_sequence() {
        let mut output = Vec::new();

        write_clipboard_osc52(&mut output, "hi").expect("clipboard write");

        assert_eq!(output, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn outline_start_index_selects_nearest_preceding_entry() {
        let items = outline_fixture();

        assert_eq!(outline_start_index(&items, 0), 0);
        assert_eq!(outline_start_index(&items, 6), 1);
        assert_eq!(outline_start_index(&items, 20), 2);
    }

    #[test]
    fn filter_outline_matches_titles_and_passes_all_when_empty() {
        let items = outline_fixture();

        assert_eq!(filter_outline(&items, ""), vec![0, 1, 2]);
        assert_eq!(filter_outline(&items, "result"), vec![2]);
        assert!(filter_outline(&items, "zzz").is_empty());
    }

    #[test]
    fn picker_plain_arrow_keys_change_selection() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pdf"), b"synthetic").expect("first PDF");
        let mut browser = BrowserState::new(directory.path().to_path_buf());

        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            10,
        ));
        assert_eq!(browser.selected, 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            10,
        ));
        assert_eq!(browser.selected, 0);
    }

    #[test]
    fn picker_uses_mdr_three_quarter_layout() {
        let area = Rect::new(0, 0, 100, 40);

        assert_eq!(picker_rect(area), Rect::new(12, 5, 75, 30));
    }

    #[test]
    fn picker_preserves_all_four_border_corners_with_long_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let nested = directory.path().join("a".repeat(100));
        fs::create_dir(&nested).expect("nested directory");
        let browser = BrowserState::new(nested);
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_picker(frame, &browser))
            .expect("draw picker");
        let buffer = terminal.backend().buffer();
        let right = popup.x + popup.width - 1;
        let bottom = popup.y + popup.height - 1;

        assert_eq!(buffer[(popup.x, popup.y)].symbol(), "┌");
        assert_eq!(buffer[(right, popup.y)].symbol(), "┐");
        assert_eq!(buffer[(popup.x, bottom)].symbol(), "└");
        assert_eq!(buffer[(right, bottom)].symbol(), "┘");
    }

    #[test]
    fn closing_picker_clears_its_terminal_buffer() {
        let mut output = Vec::new();

        clear_picker(&mut output).expect("clear picker");

        assert!(output.windows(4).any(|window| window == b"\x1b[2J"));
    }
}

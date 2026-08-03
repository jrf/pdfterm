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
use crate::kitty::{self, Placement};
use crate::pdf::{Frame, RenderKey, RenderRequest, RenderWorker, WorkerMessage};
use crate::terminal::{TerminalGuard, Viewport};
use crate::theme::TOKYO_NIGHT_MOON;

const FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_STABLE_FOR: Duration = Duration::from_millis(150);
const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(500);

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
    let worker = RenderWorker::spawn(path.clone(), pdfium_library);
    let page_count = worker.wait_until_ready().map_err(AppError::Renderer)?;
    let watcher = FileWatcher::new(&path)?;
    let mut app = App::new(
        worker,
        page_count,
        start_page.min(page_count - 1),
        path,
        watcher,
    );
    app.request_current(&mut output)?;

    loop {
        while let Ok(message) = app.worker.try_recv() {
            match message {
                WorkerMessage::Frame(frame) => app.receive_frame(frame, &mut output)?,
                WorkerMessage::Error(error) => return Err(AppError::Renderer(error)),
                WorkerMessage::Ready { .. } => {}
                WorkerMessage::Opened { pages } => app.finish_open(pages, &mut output)?,
                WorkerMessage::OpenError(error) => app.fail_open(&error, &mut output)?,
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
    path: PathBuf,
    watcher: FileWatcher,
    page_count: u32,
    page: u32,
    generation: u64,
    desired_key: Option<RenderKey>,
    cache: HashMap<RenderKey, Arc<Frame>>,
    pending: HashSet<RenderKey>,
    visible_image_id: Option<u32>,
    next_image_id: u32,
}

enum PendingOpen {
    Reload(FileFingerprint),
    Selection(PathBuf),
}

impl App {
    fn new(
        worker: RenderWorker,
        page_count: u32,
        page: u32,
        path: PathBuf,
        watcher: FileWatcher,
    ) -> Self {
        Self {
            worker,
            pending_open: None,
            path,
            watcher,
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

    fn poll_file_change(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        let Some(fingerprint) = self.watcher.poll(&self.path) else {
            return Ok(());
        };

        self.worker
            .open(self.path.clone())
            .map_err(AppError::Renderer)?;
        self.pending_open = Some(PendingOpen::Reload(fingerprint));
        self.draw_status(output, Viewport::detect()?, "reloading")?;
        Ok(())
    }

    fn finish_open(&mut self, pages: u32, output: &mut impl Write) -> Result<(), AppError> {
        match self.pending_open.take() {
            Some(PendingOpen::Reload(fingerprint)) => self.watcher.accept(fingerprint),
            Some(PendingOpen::Selection(path)) => {
                self.path = path;
                self.watcher = FileWatcher::new(&self.path)?;
                self.page = 0;
            }
            None => {}
        }
        self.page_count = pages;
        self.page = self.page.min(pages - 1);
        self.desired_key = None;
        self.cache.clear();
        self.pending.clear();
        self.request_current(output)?;
        Ok(())
    }

    fn fail_open(&mut self, error: &str, output: &mut impl Write) -> Result<(), AppError> {
        let state = match self.pending_open.take() {
            Some(PendingOpen::Reload(_)) => {
                self.watcher.defer(RELOAD_RETRY_DELAY);
                format!("reload failed: {error}; retrying")
            }
            Some(PendingOpen::Selection(_)) | None => format!("open failed: {error}"),
        };
        self.request_current(output)?;
        self.draw_status(output, Viewport::detect()?, &state)?;
        Ok(())
    }

    fn open_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        kitty::delete_all(output)?;
        self.visible_image_id = None;
        let directory = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        match pick_pdf(directory, output)? {
            Some(path) if path != self.path => {
                self.worker.open(path.clone()).map_err(AppError::Renderer)?;
                self.pending_open = Some(PendingOpen::Selection(path));
                self.draw_status(output, Viewport::detect()?, "opening")?;
            }
            Some(_) | None => self.request_current(output)?,
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('f') => self.open_picker(output)?,
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
            Print("  j/k: page  g/G: first/last  f: files  q: quit"),
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

fn pick_pdf(directory: PathBuf, output: &mut impl Write) -> Result<Option<PathBuf>, AppError> {
    let mut browser = BrowserState::new(directory);
    browser.preload_recursive();
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;

    loop {
        if browser.poll_recursive() {
            redraw = true;
        }
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(
                picker_rect(area.into(), browser.filtered_indices.len())
                    .height
                    .saturating_sub(4)
                    .max(1),
            );
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
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => {
                if let Some(path) = browser.enter_selected() {
                    return Ok(Some(path));
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
    }
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
    let popup = picker_rect(area, entries.len());
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

fn picker_rect(area: Rect, entry_count: usize) -> Rect {
    let width = if area.width > 4 {
        (area.width * 3 / 4).max(50).min(area.width - 4)
    } else {
        area.width.max(1)
    };
    let height = if area.height > 4 {
        let maximum = (area.height * 3 / 4).max(6).min(area.height - 2);
        u16::try_from(entry_count)
            .unwrap_or(u16::MAX)
            .saturating_add(4)
            .max(6)
            .min(maximum)
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
        draw_picker, picker_rect,
    };
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
    fn picker_height_tracks_content_and_stays_capped() {
        let area = Rect::new(0, 0, 100, 40);

        assert_eq!(picker_rect(area, 7), Rect::new(12, 14, 75, 11));
        assert_eq!(picker_rect(area, 100), Rect::new(12, 5, 75, 30));
    }

    #[test]
    fn picker_preserves_all_four_border_corners_with_long_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let nested = directory.path().join("a".repeat(100));
        fs::create_dir(&nested).expect("nested directory");
        let browser = BrowserState::new(nested);
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area, browser.filtered_indices.len());
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
}

use crate::screenshot::{Badge as ScreenshotBadge, Page as ScreenshotPage, Rect as ScreenshotRect};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::Frame as RatatuiFrame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RatatuiColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear as RatatuiClear, Paragraph, Wrap};
use thiserror::Error;

use crate::browser::{BrowserEntry, BrowserEntrySource, BrowserState};
use crate::config::{Config, LinkPickerLayout, ViewerSettings};
use crate::kitty::{self, Placement};
use crate::navigation::{
    Coordinator, ForwardStage, InverseTask, PendingFlash, PendingForward, PendingInverse,
};
use crate::pdf::{
    DarkModeStyle, DocumentId, DocumentLink, FitMode, Frame, LinkTarget, OutlineItem, PageLink,
    RenderKey, RenderRequest, RenderWorker, SearchPageMatch, VisibleMatch, WorkerMessage,
};
use crate::synctex::{ForwardRequest, PdfRevision};
use crate::terminal::{ImagePlacement, TerminalGuard, Viewport};
use crate::theme::Palette;
mod session;
use session::{FileFingerprint, FileWatcher, Session, Tab};

const FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_STABLE_FOR: Duration = Duration::from_millis(150);
const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(500);
const LINK_PREVIEW_DELAY: Duration = Duration::from_millis(120);
const INITIAL_DOCUMENT_ID: DocumentId = 1;
const ZOOM_DEFAULT: u16 = 100;
const ZOOM_MIN: u16 = 100;
const ZOOM_MAX: u16 = 400;
const ZOOM_STEP: u16 = 25;
const PAGE_IMAGE_Z_INDEX: i32 = i32::MIN / 2 - 2;
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

/// Defer inner flushes until synchronized_output has closed the frame.
struct FrameOutput<'a, W>(&'a mut W);

impl<W: Write> Write for FrameOutput<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn synchronized_output<W, T, E>(
    output: &mut W,
    operation: impl FnOnce(&mut FrameOutput<'_, W>) -> Result<T, E>,
) -> Result<T, E>
where
    W: Write,
    E: From<io::Error>,
{
    output
        .write_all(BEGIN_SYNCHRONIZED_UPDATE)
        .map_err(E::from)?;
    let operation_result = operation(&mut FrameOutput(output));
    let finish_result = output
        .write_all(END_SYNCHRONIZED_UPDATE)
        .and_then(|()| output.flush());
    match operation_result {
        Ok(value) => {
            finish_result.map_err(E::from)?;
            Ok(value)
        }
        Err(error) => {
            let _ = finish_result;
            Err(error)
        }
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("viewer quit requested")]
    Quit,
    #[error("pdfterm requires an interactive terminal")]
    NotInteractive,
    #[error("{0}")]
    Renderer(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn read_event() -> Result<Event, AppError> {
    let event = match event::read()? {
        Event::Key(key) => Event::Key(normalize_shifted_key(key)),
        other => other,
    };
    if matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Err(AppError::Quit);
    }
    Ok(event)
}

fn new_focus_token() -> io::Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut token = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

pub fn run(
    path: Option<PathBuf>,
    pdfium_library: Option<PathBuf>,
    start_page: u32,
    config: &Config,
    focus_token: Option<String>,
    synctex_best_guess: bool,
) -> Result<(), AppError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::NotInteractive);
    }

    let focus_token = match focus_token {
        Some(token) => token,
        None => new_focus_token()?,
    };
    let defaults = AppDefaults::from_config(config, Some(focus_token));
    let theme = defaults.theme;
    let _terminal_guard = TerminalGuard::enter(&mut io::stdout().lock(), theme)?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    let path = match path {
        Some(path) => path.canonicalize()?,
        None => match pick_pdf(std::env::current_dir()?, &mut output, theme, || Ok(false))? {
            Some(path) => path,
            None => return Ok(()),
        },
    };
    let worker = RenderWorker::spawn(INITIAL_DOCUMENT_ID, path.clone(), pdfium_library);
    let (page_count, outline, revision) = worker.wait_until_ready().map_err(AppError::Renderer)?;
    crate::recent::record(&path);
    let watcher = FileWatcher::from_revision(revision);
    let mut app = App::new(
        worker,
        page_count,
        start_page.min(page_count - 1),
        path,
        watcher,
        (outline, revision.pdf),
        defaults,
    );
    if synctex_best_guess {
        app.navigation.best_guess.insert(app.tab().path.clone());
    }
    app.request_current(&mut output)?;
    if let Some(warning) = app.startup_warning.clone() {
        let viewport = app.viewport()?;
        app.draw_status(&mut output, viewport, &warning)?;
    }

    loop {
        while let Ok(message) = app.worker.try_recv() {
            match message {
                WorkerMessage::VisibleMatches {
                    document_id,
                    request_id,
                    revision,
                    matches,
                } => app.receive_visible_matches(
                    document_id,
                    request_id,
                    revision,
                    matches,
                    &mut output,
                )?,
                WorkerMessage::VisibleMatchesError {
                    document_id,
                    request_id,
                    revision,
                    error,
                } => app.receive_visible_matches_error(
                    document_id,
                    request_id,
                    revision,
                    error,
                    &mut output,
                )?,
                WorkerMessage::Frame(frame) => app.receive_frame(frame, &mut output)?,
                WorkerMessage::RenderError {
                    key,
                    generation,
                    error,
                } => app.receive_render_error(key, generation, &error, &mut output)?,
                WorkerMessage::Error(error) => {
                    app.finish_forward(Some(format!("render failed: {error}")));
                    return Err(AppError::Renderer(error));
                }
                WorkerMessage::Ready { .. } => {}
                WorkerMessage::Opened {
                    document_id,
                    pages,
                    outline,
                    revision,
                } => app.finish_open(document_id, pages, outline, revision, &mut output)?,
                WorkerMessage::OpenError { document_id, error } => {
                    app.fail_open(document_id, &error, &mut output)?
                }
                WorkerMessage::Text { content, .. } => app.copy_text(&content, &mut output)?,
                WorkerMessage::PagePoint {
                    document_id,
                    page,
                    request_id,
                    revision,
                    result,
                } => {
                    app.receive_page_point(
                        SynctexClick {
                            document_id,
                            page,
                            request_id,
                            revision,
                            result,
                        },
                        &mut output,
                    )?;
                }
                WorkerMessage::SearchProgress {
                    document_id,
                    request_id,
                    scanned,
                    total,
                    matches,
                    total_occurrences,
                } => app.receive_search_progress(
                    document_id,
                    SearchProgressUpdate {
                        request_id,
                        scanned,
                        total,
                        matches,
                        total_occurrences,
                    },
                    &mut output,
                )?,
                WorkerMessage::SearchResults {
                    document_id,
                    request_id,
                    matches,
                    total_occurrences,
                } => app.receive_search_results(
                    document_id,
                    request_id,
                    matches,
                    total_occurrences,
                    &mut output,
                )?,
                WorkerMessage::LinkIndexProgress {
                    document_id,
                    request_id,
                    links,
                    scanned,
                    total,
                    complete,
                } => app.receive_link_index_progress(
                    document_id,
                    LinkIndexUpdate {
                        request_id,
                        links,
                        scanned,
                        total,
                        complete,
                    },
                    &mut output,
                )?,
            }
        }
        app.poll_file_change(&mut output)?;
        app.poll_link_preview(&mut output)?;
        app.poll_search_preview(&mut output)?;
        app.poll_forward_socket(&mut output)?;
        app.poll_pending_forward(&mut output)?;
        app.poll_inverse_search(&mut output)?;
        app.poll_flash_expiry()?;
        app.poll_smooth_scroll(&mut output)?;

        if event::poll(app.input_wait())? {
            let event = read_event()?;
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.pending_scale = None;
                    app.cancel_forward("forward search cancelled by keyboard input")?;
                    if app.handle_key(key, &mut output)? {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    app.tab_mut().center_content = true;
                    app.request_current(&mut output)?;
                }
                Event::Mouse(mouse) => {
                    if !matches!(mouse.kind, MouseEventKind::Moved | MouseEventKind::Up(_)) {
                        app.pending_scale = None;
                        app.cancel_forward("forward search cancelled by mouse input")?;
                    }
                    app.handle_mouse(mouse, &mut output)?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkPickerGeometry {
    split_percent: u16,
    layout: LinkPickerLayout,
}

impl LinkPickerGeometry {
    const fn new(split_percent: u16, layout: LinkPickerLayout) -> Self {
        Self {
            split_percent,
            layout,
        }
    }
}

struct App {
    worker: RenderWorker,
    session: Session,
    generation: u64,
    view_generation: u64,
    last_render_view: Option<RenderKey>,
    desired_key: Option<RenderKey>,
    pending: HashMap<RenderKey, bool>,
    visible_page: Option<VisiblePage>,
    submitted_key: Option<RenderKey>,
    visible_pages: Vec<VisiblePage>,
    missing_visible_page: Option<RenderKey>,
    pending_scale: Option<(RenderKey, FitMode, u16)>,
    canvas_viewport: Option<Viewport>,
    pending_vertical_scroll: i64,
    smooth_scroll_remaining: i64,
    smooth_scroll_tick: Instant,
    title_document: Option<DocumentId>,
    flash_font: Option<crate::screenshot::FlashFont>,
    viewer: ViewerSettings,
    next_image_id: u32,
    last_status_row: Option<u16>,
    status_line: Vec<u8>,
    status_buffer: Vec<u8>,
    performance_snapshot: Option<PerformanceSnapshot>,
    default_fit: FitMode,
    default_invert: bool,
    goto_input: Option<String>,
    search_input: Option<String>,
    search_picker: Option<SearchPickerState>,
    next_search_request_id: u64,
    next_link_request_id: u64,
    link_mode: bool,
    navigation: Coordinator,
    synctex_enabled: bool,
    editor: crate::editor::Editor,
    forward_socket: Option<String>,
    session_name: Option<String>,
    forward_listener: Option<crate::ipc::ForwardListener>,
    focus_token: Option<String>,
    focus_task: Option<FocusTask>,
    focus_waiting: Option<crate::ipc::ForwardReply>,
    pending_link_picker_open: bool,
    link_picker: Option<LinkPickerState>,
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
    show_performance: bool,
    theme: Palette,
    label_input: String,
    themes: Vec<(String, Palette)>,
    label_query: Option<String>,
    label_loading: bool,
    label_request_id: u64,
    label_matches: Vec<LabeledMatch>,
    label_overlay_id: Option<u32>,
    theme_index: usize,
    startup_warning: Option<String>,
}

struct FocusTask {
    operation: crate::process::Operation,
    worker: std::thread::JoinHandle<io::Result<()>>,
    reply: crate::ipc::ForwardReply,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.focus_task.take() {
            task.operation.cancel();
            let _ = task.worker.join();
            let mut reply = task.reply;
            reply.finish(Some("viewer stopped before focus completed".into()));
        }
    }
}

struct AppDefaults {
    fit: FitMode,
    invert: bool,
    theme: Palette,
    dark_mode_style: DarkModeStyle,
    search_highlight: [u8; 3],
    link_highlight: [u8; 3],
    themes: Vec<(String, Palette)>,
    theme_index: usize,
    theme_warning: Option<String>,
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
    synctex_enabled: bool,
    editor: crate::editor::Editor,
    forward_socket: Option<String>,
    session: Option<String>,
    focus_token: Option<String>,
    viewer: ViewerSettings,
}

impl AppDefaults {
    fn from_config(config: &Config, focus_token: Option<String>) -> Self {
        let resolved = crate::theme::resolve(config.theme_catalog(), config.theme());
        let theme = resolved.active;
        Self {
            fit: config.fit_mode(),
            invert: config.dark_mode(),
            dark_mode_style: DarkModeStyle::new(
                theme.document.background,
                theme.document.foreground,
            ),
            search_highlight: terminal_color_rgb(theme.yellow),
            link_highlight: terminal_color_rgb(theme.cyan),
            persistent_link_picker: config.persistent_link_picker(),
            link_picker_geometry: LinkPickerGeometry::new(
                config.link_picker_split_percent(),
                config.link_picker_layout(),
            ),
            synctex_enabled: config.synctex_enabled(),
            editor: config.editor.clone(),
            forward_socket: config.forward_socket().map(str::to_owned),
            session: config.session().map(str::to_owned),
            focus_token,
            viewer: config.viewer.clone(),
            theme,
            themes: resolved.themes,
            theme_index: resolved.index,
            theme_warning: resolved.warning,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewPosition {
    page: u32,
    scroll_x: i64,
    scroll_y: u32,
    center_content: bool,
}

impl ViewPosition {
    const fn top(page: u32) -> Self {
        Self {
            page,
            scroll_x: 0,
            scroll_y: 0,
            center_content: true,
        }
    }
}

pub(super) fn normalize_shifted_key(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(character) = key.code
        && key.modifiers == KeyModifiers::SHIFT
        && character.is_ascii_lowercase()
    {
        key.code = KeyCode::Char(character.to_ascii_uppercase());
    }
    key
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct JumpList {
    entries: Vec<ViewPosition>,
    cursor: usize,
    pending_origin: Option<ViewPosition>,
}

impl JumpList {
    fn push(&mut self, position: ViewPosition) {
        self.pending_origin = None;
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        if self.entries.last() != Some(&position) {
            if self.entries.len() == 100 {
                self.entries.remove(0);
            }
            self.entries.push(position);
        }
        self.cursor = self.entries.len().saturating_sub(1);
    }

    fn jump(&mut self, origin: ViewPosition, provisional: ViewPosition) {
        self.push(origin);
        if provisional == origin {
            self.pending_origin = Some(origin);
        } else {
            self.push(provisional);
        }
    }

    fn update_tip(&mut self, position: ViewPosition) {
        if let Some(origin) = self.pending_origin.take() {
            if self.cursor + 1 == self.entries.len() && self.entries.last() == Some(&origin) {
                self.push(position);
            }
            return;
        }
        if self.cursor + 1 == self.entries.len()
            && let Some(last) = self.entries.last_mut()
        {
            *last = position;
            if self.entries.len() >= 2 && self.entries[self.entries.len() - 2] == position {
                self.entries.pop();
                self.cursor = self.entries.len() - 1;
            }
        }
    }

    fn clamp_pages(&mut self, pages: u32) {
        self.pending_origin = None;
        let max_page = pages.saturating_sub(1);
        for entry in &mut self.entries {
            entry.page = entry.page.min(max_page);
        }
        let cursor = self.cursor;
        let mut write = 0usize;
        for read in 0..self.entries.len() {
            if write == 0 || self.entries[read] != self.entries[write - 1] {
                self.entries[write] = self.entries[read];
                write += 1;
            } else if read <= cursor {
                self.cursor = self.cursor.saturating_sub(1);
            }
        }
        self.entries.truncate(write);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }

    fn backward(&mut self, current: ViewPosition) -> Option<ViewPosition> {
        self.pending_origin = None;
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor + 1 == self.entries.len() && self.entries[self.cursor] != current {
            if self.entries.len() == 100 {
                self.entries.remove(0);
                self.cursor = self.entries.len().saturating_sub(1);
            }
            let target = self.entries[self.cursor];
            self.entries.push(current);
            return Some(target);
        }
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        Some(self.entries[self.cursor])
    }

    fn forward(&mut self) -> Option<ViewPosition> {
        self.pending_origin = None;
        if self.cursor + 1 >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        Some(self.entries[self.cursor])
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LinkDestination {
    content_pan_x: Option<i64>,
    page: u32,
    top_ratio: Option<f32>,
    left_ratio: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkPickerState {
    page: u32,
    selected: usize,
    number_input: String,
    filter: String,
    filtering: bool,
    focus: LinkPickerFocus,
    selection_key: Option<(u32, u32)>,
    awaiting_current_page: bool,
    pending_preview: Option<PendingLinkPreview>,
    preview_origin: Option<ViewPosition>,
    rows: RefCell<Option<LinkPickerRowsCache>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkPickerRowsCache {
    document_id: DocumentId,
    revision: u64,
    links: (usize, usize),
    outline: (usize, usize),
    filter: String,
    rows: Arc<LinkPickerRows>,
}

#[derive(Debug, Eq, PartialEq)]
struct LinkPickerRows {
    filtered: Vec<usize>,
    display: Vec<LinkPickerDisplayRow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingLinkPreview {
    selection_key: (u32, u32),
    ready_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkPickerFocus {
    Document,
    Links,
}

#[derive(Default)]
struct LinkIndexState {
    request_id: u64,
    links: Vec<DocumentLink>,
    scanned: u32,
    total_pages: u32,
    indexing: bool,
}

impl LinkIndexState {
    fn new(total_pages: u32) -> Self {
        Self {
            total_pages,
            ..Self::default()
        }
    }

    fn started(&self) -> bool {
        self.request_id != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkIndexProgress {
    scanned: u32,
    total_pages: u32,
    indexing: bool,
}

#[derive(Clone, Copy)]
struct LinkPickerDocument<'a> {
    links: &'a [DocumentLink],
    outline: &'a [OutlineItem],
    document_id: DocumentId,
    revision: u64,
}

impl<'a> LinkPickerDocument<'a> {
    const fn new(links: &'a [DocumentLink], outline: &'a [OutlineItem]) -> Self {
        Self {
            links,
            outline,
            document_id: 0,
            revision: 0,
        }
    }

    fn from_tab(tab: &'a Tab) -> Self {
        Self {
            document_id: tab.document_id,
            revision: tab.link_index.request_id,
            ..Self::new(&tab.link_index.links, &tab.outline)
        }
    }
}

struct LinkIndexUpdate {
    request_id: u64,
    links: Vec<DocumentLink>,
    scanned: u32,
    total: u32,
    complete: bool,
}

struct SearchProgressUpdate {
    request_id: u64,
    scanned: u32,
    total: u32,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
}

#[derive(Clone, Copy)]
struct PickerFilter<'a> {
    query: &'a str,
    active: bool,
}

impl From<&LinkIndexState> for LinkIndexProgress {
    fn from(index: &LinkIndexState) -> Self {
        Self {
            scanned: index.scanned,
            total_pages: index.total_pages,
            indexing: index.indexing,
        }
    }
}

impl LinkPickerState {
    fn new(page: u32) -> Self {
        Self {
            page,
            selected: 0,
            number_input: String::new(),
            filter: String::new(),
            filtering: false,
            focus: LinkPickerFocus::Links,
            selection_key: None,
            awaiting_current_page: true,
            pending_preview: None,
            preview_origin: None,
            rows: RefCell::new(None),
        }
    }

    fn rows(&self, document: LinkPickerDocument<'_>) -> Arc<LinkPickerRows> {
        // Index updates append links; replacing an index changes its request ID.
        // Outlines are immutable and replaced together with document resets.
        let links = (document.links.as_ptr() as usize, document.links.len());
        let outline = (document.outline.as_ptr() as usize, document.outline.len());
        let mut cache = self.rows.borrow_mut();
        if let Some(cached) = cache.as_ref()
            && cached.document_id == document.document_id
            && cached.revision == document.revision
            && cached.links == links
            && cached.outline == outline
            && cached.filter == self.filter
        {
            return Arc::clone(&cached.rows);
        }
        let filtered = filter_document_links(document.links, &self.filter);
        let display = link_picker_display_rows(document.links, &filtered, document.outline);
        let rows = Arc::new(LinkPickerRows { filtered, display });
        *cache = Some(LinkPickerRowsCache {
            document_id: document.document_id,
            revision: document.revision,
            links,
            outline,
            filter: self.filter.clone(),
            rows: Arc::clone(&rows),
        });
        rows
    }

    fn reset_for_document(&mut self, page: u32) {
        self.page = page;
        self.selected = 0;
        self.number_input.clear();
        self.selection_key = None;
        self.awaiting_current_page = true;
        self.pending_preview = None;
        self.preview_origin = None;
        *self.rows.get_mut() = None;
    }

    fn sync(&mut self, page: u32, links: &[DocumentLink], indexing: bool) {
        if self.page != page {
            self.page = page;
            self.selected = 0;
            self.number_input.clear();
            self.selection_key = None;
            self.awaiting_current_page = true;
        }

        if self.awaiting_current_page {
            let target = links
                .iter()
                .position(|link| link.source_page == page)
                .or_else(|| {
                    (!indexing)
                        .then(|| links.iter().position(|link| link.source_page > page))
                        .flatten()
                });
            if let Some(index) = target {
                self.select(index, links);
            } else if !indexing {
                self.select(0, links);
            }
            return;
        }

        if let Some(key) = self.selection_key
            && let Some(index) = links.iter().position(|link| link_key(link) == key)
        {
            self.selected = index;
            return;
        }
        self.select(self.selected.min(links.len().saturating_sub(1)), links);
    }

    fn select(&mut self, selected: usize, links: &[DocumentLink]) {
        self.selected = selected.min(links.len().saturating_sub(1));
        self.selection_key = links.get(self.selected).map(link_key);
        self.awaiting_current_page = false;
    }
}

fn link_key(link: &DocumentLink) -> (u32, u32) {
    (link.source_page, link.ordinal)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PerformanceSnapshot {
    render_ms: u128,
    dark_mode_ms: Option<u128>,
    highlight_ms: Option<u128>,
    compression_ms: u128,
    transfer_ms: u128,
    link_count: usize,
}

impl PerformanceSnapshot {
    fn from_frame(frame: &Frame, transfer_elapsed: Duration) -> Self {
        Self {
            render_ms: frame.render_elapsed.as_millis(),
            dark_mode_ms: frame.dark_mode_elapsed.map(|elapsed| elapsed.as_millis()),
            highlight_ms: frame.highlight_elapsed.map(|elapsed| elapsed.as_millis()),
            compression_ms: frame.compression_elapsed.as_millis(),
            transfer_ms: transfer_elapsed.as_millis(),
            link_count: frame.links.len(),
        }
    }

    fn status(self, detailed: bool, link_mode: bool) -> String {
        let mut status = render_timing_status(
            self.render_ms,
            self.dark_mode_ms,
            self.highlight_ms,
            self.compression_ms,
            self.transfer_ms,
            detailed,
        );
        if link_mode {
            status.push_str(&format!("  {} page links", self.link_count));
        }
        status
    }
}

#[derive(Clone, Default)]
struct SearchState {
    query: String,
    request_id: u64,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
    scanned: u32,
    total_pages: u32,
    searching: bool,
    initial_navigation_pending: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchPickerState {
    page: u32,
    selected: usize,
    focus: LinkPickerFocus,
    awaiting_initial_result: bool,
    selection_page: Option<u32>,
    pending_preview: Option<(u32, Instant)>,
    preview_origin: Option<ViewPosition>,
    rows: RefCell<Option<SearchPickerRowsCache>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchPickerRowsCache {
    request_id: u64,
    matches_len: usize,
    outline: (usize, usize),
    rows: Vec<SearchPickerDisplayRow>,
}

impl SearchPickerState {
    const fn new(page: u32) -> Self {
        Self {
            page,
            selected: 0,
            focus: LinkPickerFocus::Links,
            awaiting_initial_result: true,
            selection_page: None,
            pending_preview: None,
            preview_origin: None,
            rows: RefCell::new(None),
        }
    }

    pub(super) fn display_rows(
        &self,
        search: &SearchState,
        outline: &[OutlineItem],
    ) -> std::cell::Ref<'_, [SearchPickerDisplayRow]> {
        let outline_key = (outline.as_ptr() as usize, outline.len());
        {
            let mut cache = self.rows.borrow_mut();
            let rebuild = cache.as_ref().is_none_or(|cached| {
                cached.request_id != search.request_id
                    || cached.outline != outline_key
                    || cached.matches_len > search.matches.len()
            });
            if rebuild {
                let mut rows = Vec::new();
                append_search_picker_display_rows(&mut rows, &search.matches, 0, outline);
                *cache = Some(SearchPickerRowsCache {
                    request_id: search.request_id,
                    matches_len: search.matches.len(),
                    outline: outline_key,
                    rows,
                });
            } else if let Some(cached) = cache.as_mut()
                && cached.matches_len < search.matches.len()
            {
                append_search_picker_display_rows(
                    &mut cached.rows,
                    &search.matches,
                    cached.matches_len,
                    outline,
                );
                cached.matches_len = search.matches.len();
            }
        }
        std::cell::Ref::map(self.rows.borrow(), |cached| {
            cached
                .as_ref()
                .expect("search picker rows cached")
                .rows
                .as_slice()
        })
    }

    fn sync(&mut self, page: u32, matches: &[SearchPageMatch]) {
        self.page = page;
        if matches.is_empty() {
            self.selected = 0;
            self.selection_page = None;
            return;
        }
        if self.awaiting_initial_result {
            self.selected = matches
                .iter()
                .position(|result| result.page >= page)
                .unwrap_or(0);
            self.awaiting_initial_result = false;
        } else {
            self.selected = self.selected.min(matches.len() - 1);
        }
        self.selection_page = matches.get(self.selected).map(|result| result.page);
    }
}

impl SearchState {
    fn active_highlight_id(&self) -> u64 {
        if !self.searching && !self.matches.is_empty() {
            self.request_id
        } else {
            0
        }
    }

    fn highlight_request_id(&self, page: u32) -> u64 {
        if !self.searching && self.matches.iter().any(|result| result.page == page) {
            self.request_id
        } else {
            0
        }
    }

    fn status_label(&self, current_page: u32) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        let query = truncated_search_query(&self.query, 28);
        if self.searching {
            return Some(format!(
                "  search {}/{}  /{}",
                self.scanned, self.total_pages, query
            ));
        }
        if self.matches.is_empty() {
            return Some(format!("  no matches  /{query}"));
        }
        let position = self
            .matches
            .iter()
            .position(|result| result.page == current_page)
            .map(|index| format!("{}/{}", index + 1, self.matches.len()))
            .unwrap_or_else(|| format!("{} pages", self.matches.len()));
        Some(format!(
            "  search {position} · {} hits  /{query}",
            self.total_occurrences
        ))
    }
}

fn truncated_search_query(query: &str, max_chars: usize) -> String {
    let mut characters = query.chars();
    let mut truncated: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn search_target_page(
    matches: &[SearchPageMatch],
    current_page: u32,
    forward: bool,
) -> Option<u32> {
    if forward {
        matches
            .iter()
            .find(|result| result.page > current_page)
            .or_else(|| matches.first())
    } else {
        matches
            .iter()
            .rev()
            .find(|result| result.page < current_page)
            .or_else(|| matches.last())
    }
    .map(|result| result.page)
}

#[derive(Clone)]
struct VisiblePage {
    frame: Arc<Frame>,
    placement: ImagePlacement,
    top: u16,
    image_id: u32,
}

fn write_page_image(
    output: &mut impl Write,
    frame: &Frame,
    placement: Placement,
    retained: bool,
) -> io::Result<()> {
    if retained {
        kitty::place_image(output, placement)
    } else {
        kitty::transmit_compressed_rgb(
            output,
            &frame.compressed_rgba,
            frame.width,
            frame.height,
            placement,
        )
    }
}

struct LabeledMatch {
    visible: VisibleMatch,
    label: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

fn terminal_color_rgb(color: crossterm::style::Color) -> [u8; 3] {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => [r, g, b],
        _ => [0xff, 0xc7, 0x77],
    }
}

fn render_timing_status(
    render_ms: u128,
    dark_mode_ms: Option<u128>,
    highlight_ms: Option<u128>,
    compression_ms: u128,
    transfer_ms: u128,
    detailed: bool,
) -> String {
    if detailed {
        let dark_mode = dark_mode_ms
            .map(|elapsed| format!("  dark {elapsed}ms"))
            .unwrap_or_default();
        let highlight = highlight_ms
            .map(|elapsed| format!("  highlight {elapsed}ms"))
            .unwrap_or_default();
        return format!(
            "render {render_ms}ms{dark_mode}{highlight}  compress {compression_ms}ms  transfer {transfer_ms}ms"
        );
    }

    let total_ms = render_ms
        + dark_mode_ms.unwrap_or_default()
        + highlight_ms.unwrap_or_default()
        + compression_ms
        + transfer_ms;
    format!("render {total_ms}ms")
}

struct SynctexClick {
    document_id: DocumentId,
    page: u32,
    request_id: u64,
    revision: crate::synctex::DocumentRevision,
    result: Result<crate::pdf::ResolvedClick, String>,
}

impl App {
    fn new(
        worker: RenderWorker,
        page_count: u32,
        page: u32,
        path: PathBuf,
        watcher: FileWatcher,
        document: (Vec<OutlineItem>, PdfRevision),
        defaults: AppDefaults,
    ) -> Self {
        let (outline, revision) = document;
        let default_fit = defaults.fit;
        let default_invert = defaults.invert;
        Self {
            worker,
            session: Session {
                pending_open: None,
                tabs: vec![Tab::new(
                    INITIAL_DOCUMENT_ID,
                    path,
                    revision,
                    watcher,
                    page_count,
                    page,
                    default_fit,
                    default_invert,
                    defaults.dark_mode_style,
                    defaults.search_highlight,
                    defaults.link_highlight,
                    outline,
                )],
                active_tab: 0,
                next_document_id: INITIAL_DOCUMENT_ID + 1,
            },
            generation: 0,
            view_generation: 0,
            last_render_view: None,
            desired_key: None,
            pending: HashMap::new(),
            visible_page: None,
            submitted_key: None,
            visible_pages: Vec::new(),
            missing_visible_page: None,
            pending_scale: None,
            canvas_viewport: None,
            pending_vertical_scroll: 0,
            smooth_scroll_remaining: 0,
            smooth_scroll_tick: Instant::now(),
            title_document: None,
            next_image_id: 1,
            last_status_row: None,
            status_line: Vec::new(),
            status_buffer: Vec::new(),
            performance_snapshot: None,
            default_fit,
            default_invert,
            goto_input: None,
            search_input: None,
            search_picker: None,
            next_search_request_id: 1,
            next_link_request_id: 1,
            link_mode: false,
            navigation: Coordinator::new(),
            synctex_enabled: defaults.synctex_enabled,
            editor: defaults.editor,
            forward_socket: defaults.forward_socket,
            session_name: defaults.session,
            forward_listener: None,
            focus_token: defaults.focus_token,
            focus_task: None,
            focus_waiting: None,
            pending_link_picker_open: false,
            link_picker: None,
            persistent_link_picker: defaults.persistent_link_picker,
            link_picker_geometry: defaults.link_picker_geometry,
            show_performance: false,
            theme: defaults.theme,
            themes: defaults.themes,
            label_input: String::new(),
            theme_index: defaults.theme_index,
            label_query: None,
            label_loading: false,
            label_request_id: 1,
            label_matches: Vec::new(),
            label_overlay_id: None,
            flash_font: None,
            viewer: defaults.viewer,
            startup_warning: defaults.theme_warning,
        }
    }

    fn open_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let directory = self
            .tab()
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        match pick_pdf(directory, output, self.theme, || self.forward_waiting())? {
            Some(path) => match path.canonicalize() {
                Ok(path) => {
                    if let Some(index) = self.tab_index_for_path(&path) {
                        if index == self.session.active_tab {
                            self.reset_render_state();
                            self.request_current(output)?;
                        } else {
                            self.select_tab(index, output)?;
                        }
                    } else {
                        self.begin_open(path, output)?;
                    }
                }
                Err(error) => {
                    self.reset_render_state();
                    self.request_current(output)?;
                    let viewport = self.viewport()?;
                    self.draw_status(output, viewport, &format!("open failed: {error}"))?;
                }
            },
            None => self.request_current(output)?,
        }
        Ok(())
    }

    fn open_outline(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let outline = Arc::clone(&self.tab().outline);
        if outline.is_empty() {
            let viewport = self.prepare_viewport(output)?;
            self.draw_status(output, viewport, "no outline in this document")?;
            return Ok(());
        }
        self.clear_viewer(output)?;
        let selection = pick_outline(&outline, self.tab().page, output, self.theme, || {
            self.forward_waiting()
        })?;
        if let Some(page) = selection {
            let page = page.min(self.tab().page_count - 1);
            let tab = self.tab_mut();
            tab.search.initial_navigation_pending = false;
            tab.set_view_position(ViewPosition::top(page));
        }
        self.reset_render_state();
        self.request_current(output)
    }

    fn open_theme_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let selection = pick_theme(&self.themes, self.theme_index, output, || {
            self.forward_waiting()
        })?;
        if let Some(index) = selection {
            self.apply_theme(index);
        }
        clear_picker(output, self.theme)?;
        self.reset_render_state();
        self.request_current(output)
    }

    fn open_help(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        show_help(output, self.theme, || self.forward_waiting())?;
        self.reset_render_state();
        self.request_current(output)
    }

    fn apply_theme(&mut self, index: usize) {
        let Some((_, theme)) = self.themes.get(index) else {
            return;
        };
        let theme = *theme;
        self.theme_index = index;
        self.set_palette(theme);
    }

    fn set_palette(&mut self, theme: Palette) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        let style = DarkModeStyle::new(theme.document.background, theme.document.foreground);
        let search_highlight = terminal_color_rgb(theme.yellow);
        let link_highlight = terminal_color_rgb(theme.cyan);
        for tab in &mut self.session.tabs {
            tab.dark_mode_style = style;
            tab.search_highlight = search_highlight;
            tab.link_highlight = link_highlight;
            tab.cache.clear();
        }
    }

    fn begin_goto(
        &mut self,
        initial: impl Into<String>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.goto_input = Some(initial.into());
        let viewport = self.viewport()?;
        self.draw_goto(output, viewport)?;
        Ok(())
    }

    fn finish_goto(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let input = self.goto_input.take().unwrap_or_default();
        let target = input
            .trim()
            .trim_end_matches('g')
            .parse::<u32>()
            .ok()
            .filter(|number| *number >= 1)
            .map(|number| (number - 1).min(self.tab().page_count - 1));
        match target {
            Some(page) if page != self.tab().page => self.set_page(page, output)?,
            _ => self.redraw_current(output)?,
        }
        Ok(())
    }

    fn handle_goto_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<(), AppError> {
        match normalize_shifted_key(key).code {
            KeyCode::Esc => {
                self.goto_input = None;
                self.redraw_current(output)?;
            }
            KeyCode::Enter | KeyCode::Char('G') => self.finish_goto(output)?,
            KeyCode::Char('g') if key.modifiers == KeyModifiers::NONE => {
                if self
                    .goto_input
                    .as_deref()
                    .is_some_and(|buffer| buffer.ends_with('g'))
                {
                    self.finish_goto(output)?;
                } else {
                    if let Some(buffer) = self.goto_input.as_mut() {
                        buffer.push('g');
                    }
                    let viewport = self.viewport()?;
                    self.draw_goto(output, viewport)?;
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
                if let Some(buffer) = self
                    .goto_input
                    .as_mut()
                    .filter(|buffer| !buffer.ends_with('g') && buffer.len() < 9)
                {
                    buffer.push(character);
                }
                let viewport = self.viewport()?;
                self.draw_goto(output, viewport)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_input_prompt(
        &mut self,
        output: &mut impl Write,
        viewport: Viewport,
        prompt: &str,
        input: &str,
        hint: &str,
    ) -> io::Result<()> {
        self.status_line.clear();
        let theme = self.theme;
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.yellow),
            Print(prompt),
            SetForegroundColor(theme.fg),
            Print(input),
            SetForegroundColor(theme.comment),
            Print(hint),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
    }

    fn draw_goto(&mut self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        let input = self.goto_input.clone().unwrap_or_default();
        let hint = format!(
            "  (1-{}, enter/G/gg to jump, esc to cancel)",
            self.tab().page_count
        );
        self.draw_input_prompt(output, viewport, "go to page: ", &input, &hint)
    }

    fn reset_label_matches(&mut self, output: &mut impl Write) -> io::Result<()> {
        self.worker.cancel_visible();
        self.label_loading = false;
        self.label_input.clear();
        self.label_matches.clear();
        self.clear_label_overlay(output)
    }

    fn begin_label_mode(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.flash_font.is_none() {
            match crate::screenshot::FlashFont::load(&self.viewer.flash_label_font) {
                Ok(font) => self.flash_font = Some(font),
                Err(error) => {
                    self.draw_status(output, self.viewport()?, &format!("jump labels: {error}"))?;
                    return Ok(());
                }
            }
        }
        self.reset_label_matches(output)?;
        self.label_query = Some(String::new());
        self.label_request_id = self.label_request_id.wrapping_add(1).max(1);
        self.draw_label_status(output)
    }

    fn handle_label_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<(), AppError> {
        if let Some(index) = numbered_tab_index(key) {
            self.select_tab(index, output)?;
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                self.label_query = None;
                self.reset_label_matches(output)?;
                self.redraw_current(output)?;
            }
            KeyCode::Backspace if !self.label_input.is_empty() => {
                self.label_input.pop();
                self.draw_label_status(output)?;
            }
            KeyCode::Backspace => {
                if let Some(query) = self.label_query.as_mut() {
                    query.pop();
                }
                self.refresh_visible_matches(output)?;
                self.draw_label_status(output)?;
            }
            KeyCode::Char('+') | KeyCode::Char('=')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.zoom_in(output)?
            }
            KeyCode::Char('-') | KeyCode::Char('_')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.zoom_out(output)?
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let candidate = format!("{}{}", self.label_input, character);
                if let Some(target) = self
                    .label_matches
                    .iter()
                    .find(|item| item.label == candidate)
                {
                    let target = (target.visible.key, target.visible.hit);
                    self.label_query = None;
                    self.reset_label_matches(output)?;
                    self.begin_inverse_at(target.0, target.1, self.viewport()?, output)?;
                } else if self
                    .label_matches
                    .iter()
                    .any(|item| item.label.starts_with(&candidate))
                {
                    self.label_input = candidate;
                    self.draw_label_status(output)?;
                } else {
                    if let Some(query) = self.label_query.as_mut().filter(|query| query.len() < 256)
                    {
                        query.push(character);
                    }
                    self.refresh_visible_matches(output)?;
                    self.draw_label_status(output)?;
                }
            }
            KeyCode::Down => self.move_view(Axis::Vertical, true, false, true, output)?,
            KeyCode::Up => self.move_view(Axis::Vertical, false, false, true, output)?,
            KeyCode::PageDown => self.move_view(Axis::Vertical, true, true, true, output)?,
            KeyCode::PageUp => self.move_view(Axis::Vertical, false, true, true, output)?,
            KeyCode::Right => self.move_view(Axis::Horizontal, true, false, true, output)?,
            KeyCode::Left => self.move_view(Axis::Horizontal, false, false, true, output)?,
            KeyCode::Tab => self.switch_tab(1, output)?,
            KeyCode::BackTab => self.switch_tab(-1, output)?,
            _ => {}
        }
        Ok(())
    }

    fn draw_label_status(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.viewport()?;
        let query = self.label_query.as_deref().unwrap_or_default();
        let status = format!(
            "find: {query}  label: {}  (type to search, label to jump, esc to cancel)",
            self.label_input
        );
        self.draw_status(output, viewport, &status)?;
        Ok(())
    }

    fn refresh_visible_matches(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.reset_label_matches(output)?;
        let request_id = self.label_request_id;
        self.label_request_id = request_id.wrapping_add(1).max(1);
        let Some(query) = self.label_query.clone().filter(|query| !query.is_empty()) else {
            return Ok(());
        };
        let viewport = self.viewport()?;
        let document_id = self.tab().document_id;
        let visible = self.visible_view_frames(viewport);
        let Some((_, revision, _, _, _, _)) = visible.first().copied() else {
            return Ok(());
        };
        if visible
            .iter()
            .any(|(_, candidate_revision, _, _, _, _)| *candidate_revision != revision)
        {
            return Ok(());
        }
        let keys = visible
            .into_iter()
            .map(|(key, _, _, _, width, height)| (key, width, height))
            .collect::<Vec<_>>();
        self.worker
            .find_visible(document_id, request_id, revision, query, keys)
            .map_err(AppError::Renderer)?;
        self.label_loading = true;
        self.draw_label_status(output)
    }

    fn visible_view_frames(
        &self,
        viewport: Viewport,
    ) -> Vec<(
        RenderKey,
        crate::synctex::DocumentRevision,
        ImagePlacement,
        u16,
        u32,
        u32,
    )> {
        let current_key = self.render_key(viewport);
        if self.continuous_view() {
            self.visible_pages
                .iter()
                .filter(|page| same_render_view(page.frame.key, current_key))
                .map(|page| {
                    (
                        page.frame.key,
                        page.frame.revision,
                        page.placement,
                        page.top,
                        page.frame.width,
                        page.frame.height,
                    )
                })
                .collect()
        } else {
            self.tab()
                .cache
                .get(&current_key)
                .map(|frame| {
                    vec![(
                        current_key,
                        frame.revision,
                        viewport.place(
                            frame.width,
                            frame.height,
                            self.tab().scroll_x.max(0) as u32,
                            self.tab().scroll_y,
                        ),
                        viewport.top,
                        frame.width,
                        frame.height,
                    )]
                })
                .unwrap_or_default()
        }
    }

    fn assign_jump_labels(matches: &[VisibleMatch]) -> Option<Vec<String>> {
        if matches.is_empty() {
            return Some(Vec::new());
        }
        let continuations = matches
            .iter()
            .filter_map(|matched| matched.next_char)
            .map(|c| c.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let alphabet: Vec<char> =
            "asdfghjklqwertyuiopzxcvbnmASDFGHJKLQWERTYUIOPZXCVBNM!@#$%^&*()[]{};:,.?/\\|"
                .chars()
                .filter(|c| !continuations.contains(&c.to_ascii_lowercase()))
                .collect();
        if matches.len() == 1 {
            let next_label_char = matches[0]
                .next_char
                .filter(|character| character.is_ascii_alphanumeric() || *character == '_');
            let label = next_label_char
                .or_else(|| alphabet.first().copied())
                .map(|c| c.to_string())?;
            return Some(vec![label]);
        }
        if alphabet.len() < 2 {
            return None;
        }
        let width = (1..)
            .find(|width| alphabet.len().saturating_pow(*width as u32) >= matches.len())
            .unwrap_or(1);
        Some(
            (0..matches.len())
                .map(|mut index| {
                    let mut label = vec![alphabet[0]; width];
                    for slot in label.iter_mut().rev() {
                        *slot = alphabet[index % alphabet.len()];
                        index /= alphabet.len();
                    }
                    label.into_iter().collect()
                })
                .collect(),
        )
    }

    fn receive_visible_matches(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        revision: crate::synctex::DocumentRevision,
        mut matches: Vec<VisibleMatch>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.label_query.is_none()
            || document_id != self.tab().document_id
            || request_id != self.label_request_id.wrapping_sub(1).max(1)
        {
            return Ok(());
        }
        self.label_loading = false;
        let viewport = self.viewport()?;
        let visible = self.visible_view_frames(viewport);
        if visible.is_empty()
            || visible
                .iter()
                .any(|(_, frame_revision, _, _, _, _)| *frame_revision != revision)
            || matches.iter().any(|matched| {
                !visible.iter().any(|(key, frame_revision, _, _, _, _)| {
                    *key == matched.key && *frame_revision == revision
                })
            })
        {
            return Ok(());
        }
        let cell_size = viewport.cell_size();
        matches.retain_mut(|matched| {
            visible
                .iter()
                .find(|(key, _, _, _, _, _)| *key == matched.key)
                .and_then(|(_, _, placement, top, width, height)| {
                    visible_match_points(
                        &matched.rects,
                        *placement,
                        *top,
                        *width,
                        *height,
                        viewport,
                        cell_size,
                    )
                })
                .is_some_and(|(hit, anchor)| {
                    matched.hit = hit;
                    matched.anchor = anchor;
                    true
                })
        });
        let Some(labels) = Self::assign_jump_labels(&matches) else {
            self.reset_label_matches(output)?;
            self.draw_status(
                output,
                viewport,
                "matches found, but no label keys avoid query continuations",
            )?;
            return Ok(());
        };
        self.label_matches = matches
            .into_iter()
            .zip(labels)
            .map(|(visible, label)| LabeledMatch { visible, label })
            .collect();
        self.draw_label_overlay(output)?;
        self.draw_label_status(output)
    }
    fn receive_visible_matches_error(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        revision: crate::synctex::DocumentRevision,
        error: String,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.label_query.is_none()
            || document_id != self.tab().document_id
            || request_id != self.label_request_id.wrapping_sub(1).max(1)
        {
            return Ok(());
        }
        self.label_loading = false;
        let viewport = self.viewport()?;
        let visible = self.visible_view_frames(viewport);
        if visible.is_empty()
            || visible
                .iter()
                .any(|(_, current, _, _, _, _)| *current != revision)
        {
            return Ok(());
        }
        self.reset_label_matches(output)?;
        self.draw_status(output, viewport, &format!("visible search: {error}"))?;
        Ok(())
    }

    fn clear_label_overlay(&mut self, output: &mut impl Write) -> io::Result<()> {
        if let Some(id) = self.label_overlay_id.take() {
            kitty::delete_image(output, id)?;
            output.flush()?;
        }
        Ok(())
    }

    fn render_label_overlay(
        &self,
        font: &mut crate::screenshot::FlashFont,
        viewport: Viewport,
    ) -> io::Result<Option<crate::screenshot::Overlay>> {
        let mut rects = Vec::new();
        let mut badges = Vec::new();
        let cell_size = viewport.cell_size();
        for labeled in &self.label_matches {
            let page = self
                .visible_pages
                .iter()
                .find(|page| page.frame.key == labeled.visible.key)
                .map(|page| {
                    (
                        page.placement,
                        page.top,
                        page.frame.width,
                        page.frame.height,
                    )
                })
                .or_else(|| {
                    self.tab().cache.get(&labeled.visible.key).map(|frame| {
                        (
                            viewport.place(
                                frame.width,
                                frame.height,
                                self.tab().scroll_x.max(0) as u32,
                                self.tab().scroll_y,
                            ),
                            viewport.top,
                            frame.width,
                            frame.height,
                        )
                    })
                });
            let Some((placement, top, frame_width, frame_height)) = page else {
                continue;
            };
            let mut badge_position = None;
            let mut match_height = 0;
            for rect in &labeled.visible.rects {
                let Some(projected) = project_match_rect(
                    rect,
                    placement,
                    top,
                    (frame_width, frame_height),
                    viewport,
                    cell_size,
                ) else {
                    continue;
                };
                let ProjectedMatch {
                    x: pixel_x,
                    y: pixel_y,
                    width,
                    height,
                    ..
                } = projected;
                rects.push(ScreenshotRect {
                    x: pixel_x,
                    y: pixel_y,
                    width,
                    height,
                    color: [0x3d, 0x59, 0xa1, 96],
                });
                badge_position = Some((pixel_x, pixel_x.saturating_add(width), pixel_y));
                match_height = match_height.max(height);
            }
            if let Some((word_x, x, rect_y)) = badge_position {
                let glyph_size = badge_glyph_size(match_height);
                let layout = font.badge_layout(&labeled.label, glyph_size)?;
                let (badge_width, badge_height) = (layout.width, layout.height);
                let x = if x.saturating_add(badge_width) > u32::from(viewport.pixel_width)
                    && word_x >= badge_width
                {
                    word_x - badge_width
                } else {
                    x.min(u32::from(viewport.pixel_width).saturating_sub(badge_width))
                };
                let y = rect_y.min(u32::from(viewport.pixel_height).saturating_sub(badge_height));
                badges.push(ScreenshotBadge {
                    x,
                    y,
                    layout,
                    text: &labeled.label,
                    foreground: self.viewer.flash_label_foreground,
                    background: self.viewer.flash_label_background,
                });
            }
        }
        crate::screenshot::cropped_overlay(font, viewport, &rects, &badges)
    }

    fn label_overlay_pixels(
        &mut self,
        viewport: Viewport,
    ) -> io::Result<Option<crate::screenshot::Overlay>> {
        let mut font = self
            .flash_font
            .take()
            .ok_or_else(|| io::Error::other("jump-label font was not initialized"))?;
        let result = self.render_label_overlay(&mut font, viewport);
        self.flash_font = Some(font);
        result
    }

    fn draw_label_overlay(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.clear_label_overlay(output)?;
        if self.label_matches.is_empty() {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let Some(overlay) = self.label_overlay_pixels(viewport)? else {
            return Ok(());
        };
        let compressed = kitty::compress_rgba(&overlay.rgba)?;
        let id = self.allocate_image_id();
        let placement = Placement {
            image_id: id,
            columns: overlay.columns,
            rows: overlay.rows,
            offset_y: 0,
            z_index: PAGE_IMAGE_Z_INDEX + 10,
            crop: None,
        };
        execute!(output, MoveTo(overlay.col, viewport.top + overlay.row))?;
        kitty::transmit_compressed_rgba(
            output,
            &compressed,
            u32::from(overlay.pixel_width),
            u32::from(overlay.pixel_height),
            placement,
        )?;
        output.flush()?;
        self.label_overlay_id = Some(id);
        Ok(())
    }

    fn begin_search(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.search_input = Some(String::new());
        self.draw_search(output, self.viewport()?)?;
        Ok(())
    }

    fn handle_search_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.search_input = None;
                self.redraw_current(output)?;
            }
            KeyCode::Enter => {
                let query = self.search_input.take().unwrap_or_default();
                let query = query.trim();
                if query.is_empty() {
                    self.redraw_current(output)?;
                } else {
                    self.start_search(query.to_string(), output)?;
                }
            }
            KeyCode::Backspace => {
                if let Some(buffer) = self.search_input.as_mut() {
                    buffer.pop();
                }
                self.draw_search(output, self.viewport()?)?;
            }
            KeyCode::Char('u') if control => {
                if let Some(buffer) = self.search_input.as_mut() {
                    buffer.clear();
                }
                self.draw_search(output, self.viewport()?)?;
            }
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(buffer) = self
                    .search_input
                    .as_mut()
                    .filter(|buffer| buffer.len() < 256)
                {
                    buffer.push(character);
                }
                self.draw_search(output, self.viewport()?)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_search(&mut self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        let input = self.search_input.clone().unwrap_or_default();
        self.draw_input_prompt(
            output,
            viewport,
            "search: ",
            &input,
            "  (enter to search, esc to cancel)",
        )
    }

    fn start_search(&mut self, query: String, output: &mut impl Write) -> Result<(), AppError> {
        let request_id = self.next_search_request_id;
        self.next_search_request_id = self.next_search_request_id.wrapping_add(1).max(1);
        let (document_id, total_pages) = {
            let tab = self.tab();
            (tab.document_id, tab.page_count)
        };
        self.tab_mut().search = SearchState {
            query: query.clone(),
            request_id,
            matches: Vec::new(),
            total_occurrences: 0,
            scanned: 0,
            total_pages,
            searching: true,
            initial_navigation_pending: true,
        };
        self.worker.search(document_id, request_id, query);
        self.search_picker = Some(SearchPickerState::new(self.tab().page));
        self.open_search_picker(output)
    }

    fn receive_search_progress(
        &mut self,
        document_id: DocumentId,
        update: SearchProgressUpdate,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.tab_index(document_id) else {
            return Ok(());
        };
        if self.session.tabs[index].search.request_id != update.request_id {
            return Ok(());
        }
        let search = &mut self.session.tabs[index].search;
        search.scanned = update.scanned;
        search.total_pages = update.total;
        search.matches.extend(update.matches);
        search.total_occurrences = update.total_occurrences;
        if index == self.session.active_tab {
            if self.search_picker.is_some() {
                self.redraw_search_picker(output)?;
            }
            self.draw_status(output, self.viewport()?, "")?;
        }
        Ok(())
    }

    fn receive_search_results(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.tab_index(document_id) else {
            return Ok(());
        };
        if self.session.tabs[index].search.request_id != request_id {
            return Ok(());
        }
        let current_page = self.session.tabs[index].page;
        let search = &mut self.session.tabs[index].search;
        let should_jump = std::mem::take(&mut search.initial_navigation_pending);
        search.matches.extend(matches);
        let target_page = search
            .matches
            .iter()
            .find(|result| result.page >= current_page)
            .or_else(|| search.matches.first())
            .map(|result| result.page);
        search.total_occurrences = total_occurrences;
        search.scanned = search.total_pages;
        search.searching = false;

        if index != self.session.active_tab {
            return Ok(());
        }
        if let Some(state) = &mut self.search_picker {
            state.sync(current_page, &self.session.tabs[index].search.matches);
        }
        if should_jump && let Some(page) = target_page {
            self.session.tabs[index].set_view_position(ViewPosition::top(page));
            self.request_current(output)?;
        } else if !self.session.tabs[index].search.matches.is_empty()
            || self.search_picker.is_some()
        {
            self.request_current(output)?;
        } else {
            self.draw_status(output, self.viewport()?, "")?;
        }
        Ok(())
    }

    fn receive_link_index_progress(
        &mut self,
        document_id: DocumentId,
        update: LinkIndexUpdate,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.tab_index(document_id) else {
            return Ok(());
        };
        let link_index = &mut self.session.tabs[index].link_index;
        if link_index.request_id != update.request_id {
            return Ok(());
        }
        link_index.links.extend(update.links);
        link_index.scanned = update.scanned;
        link_index.total_pages = update.total;
        link_index.indexing = !update.complete;

        if index == self.session.active_tab && self.link_picker.is_some() {
            self.redraw_link_picker(output)?;
        }
        Ok(())
    }

    fn navigate_search(&mut self, forward: bool, output: &mut impl Write) -> Result<(), AppError> {
        let tab = self.tab();
        if tab.search.query.is_empty() {
            self.draw_status(output, self.viewport()?, "no active search")?;
            return Ok(());
        }
        if tab.search.searching {
            self.draw_status(output, self.viewport()?, "searching")?;
            return Ok(());
        }
        let Some(page) = search_target_page(&tab.search.matches, tab.page, forward) else {
            self.draw_status(output, self.viewport()?, "")?;
            return Ok(());
        };
        self.tab_mut().set_view_position(ViewPosition::top(page));
        self.request_current(output)
    }

    fn clear_search(&mut self, output: &mut impl Write) -> Result<bool, AppError> {
        if self.tab().search.query.is_empty() {
            return Ok(false);
        }
        let document_id = self.tab().document_id;
        let request_id = self.tab().search.request_id;
        self.worker.cancel_search(document_id, request_id);
        if self.search_picker.is_some() {
            self.close_search_picker(output)?;
        }
        self.tab_mut().search = SearchState::default();
        self.request_current(output)?;
        Ok(true)
    }

    fn toggle_link_mode(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.set_link_mode(!self.link_mode, output)
    }

    fn set_link_mode(&mut self, enabled: bool, output: &mut impl Write) -> Result<(), AppError> {
        if enabled == self.link_mode {
            return Ok(());
        }
        self.link_mode = enabled;
        self.pending_link_picker_open = enabled;
        self.ensure_link_index();
        self.request_current(output)?;
        self.open_pending_link_picker(output)
    }

    fn ensure_link_index(&mut self) {
        if self.link_mode || self.link_picker.is_some() {
            self.start_link_index();
        }
    }

    pub(super) fn reset_active_tab_pickers(&mut self) {
        self.search_picker = None;
        let page = self.tab().page;
        if let Some(state) = &mut self.link_picker {
            state.reset_for_document(page);
        }
    }

    fn start_link_index(&mut self) {
        if self.tab().link_index.started() {
            return;
        }
        let request_id = self.next_link_request_id;
        self.next_link_request_id = self.next_link_request_id.wrapping_add(1).max(1);
        let document_id = self.tab().document_id;
        let total_pages = self.tab().page_count;
        self.tab_mut().link_index = LinkIndexState {
            request_id,
            links: Vec::new(),
            scanned: 0,
            total_pages,
            indexing: true,
        };
        self.worker.index_links(document_id, request_id);
    }

    fn schedule_link_preview(&mut self) {
        let Some(state) = &mut self.link_picker else {
            return;
        };
        let Some(selection_key) = state.selection_key else {
            return;
        };
        state.pending_preview = Some(PendingLinkPreview {
            selection_key,
            ready_at: Instant::now() + LINK_PREVIEW_DELAY,
        });
    }

    fn poll_link_preview(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let origin = self.tab().view_position();
        let Some(state) = &mut self.link_picker else {
            return Ok(());
        };
        let Some(pending) = state.pending_preview else {
            return Ok(());
        };
        if Instant::now() < pending.ready_at {
            return Ok(());
        }
        state.pending_preview = None;
        if state.selection_key != Some(pending.selection_key) {
            return Ok(());
        }
        let Some(link) = self.session.tabs[self.session.active_tab]
            .link_index
            .links
            .get(state.selected)
            .cloned()
        else {
            return Ok(());
        };
        state.preview_origin.get_or_insert(origin);
        state.page = link.source_page;
        state.awaiting_current_page = false;
        self.apply_preview_jump(
            link.source_page,
            Some((link.source_top_ratio - 0.08).max(0.0)),
            output,
        )
    }

    fn schedule_search_preview(&mut self) {
        let Some(state) = &mut self.search_picker else {
            return;
        };
        let Some(page) = state.selection_page else {
            return;
        };
        state.pending_preview = Some((page, Instant::now() + LINK_PREVIEW_DELAY));
    }

    fn apply_preview_jump(
        &mut self,
        page: u32,
        top_ratio: Option<f32>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let tab = self.tab_mut();
        tab.set_view_position(ViewPosition::top(page));
        tab.pending_destination = top_ratio.map(|top_ratio| LinkDestination {
            content_pan_x: None,
            page: tab.page,
            top_ratio: Some(top_ratio),
            left_ratio: None,
        });
        self.request_current(output)
    }

    fn take_picker_preview_origin(&mut self) -> Option<ViewPosition> {
        let mut origin = None;
        if let Some(state) = &mut self.link_picker {
            state.pending_preview = None;
            origin = state.preview_origin.take();
        }
        if let Some(state) = &mut self.search_picker {
            state.pending_preview = None;
            origin = origin.or(state.preview_origin.take());
        }
        origin
    }

    fn clear_picker_previews(&mut self) {
        let _ = self.take_picker_preview_origin();
    }

    pub(super) fn restore_active_picker_preview(&mut self) {
        if let Some(origin) = self.take_picker_preview_origin() {
            let tab = self.tab_mut();
            tab.set_view_position(origin);
            tab.pending_destination = None;
        }
    }

    fn forward_waiting(&self) -> io::Result<bool> {
        self.forward_listener
            .as_ref()
            .map_or(Ok(false), crate::ipc::ForwardListener::has_pending)
    }

    fn poll_forward_socket(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.forward_listener.is_none() {
            let Some(path) = self.forward_socket.as_deref() else {
                return Ok(());
            };
            self.forward_listener = Some(crate::ipc::ForwardListener::bind(Path::new(path))?);
        }
        let ready = self
            .forward_listener
            .as_mut()
            .expect("listener bound")
            .poll()?;
        let newer_forward = ready
            .iter()
            .any(|(result, _)| matches!(result, Ok(crate::ipc::ViewerRequest::Forward(_))));
        if newer_forward {
            if let Some(task) = self.focus_task.as_ref() {
                task.operation.cancel();
            }
            if let Some(mut waiting) = self.focus_waiting.take() {
                waiting.finish(Some("focus superseded by a newer forward request".into()));
            }
        }
        for (result, mut reply) in ready {
            match result {
                Ok(crate::ipc::ViewerRequest::Forward(request)) => {
                    self.cancel_forward("forward search superseded by a newer request")?;
                    self.navigation.inverse = None;
                    self.navigation.forward = Some(PendingForward {
                        request,
                        reply,
                        deadline: Instant::now() + crate::ipc::FORWARD_TIMEOUT,
                        stage: ForwardStage::AwaitingDocument,
                    });
                }
                Ok(crate::ipc::ViewerRequest::Screenshot(path)) => {
                    let result = self.save_screenshot(&path);
                    let error = result.as_ref().err().map(ToString::to_string);
                    reply.finish(error.clone());
                    if let Some(error) = error {
                        self.draw_status(
                            output,
                            self.viewport()?,
                            &format!("screenshot: {error}"),
                        )?;
                    } else {
                        self.draw_status(output, self.viewport()?, "screenshot saved")?;
                    }
                }
                Ok(crate::ipc::ViewerRequest::Focus(viewer_token)) => {
                    let error = if newer_forward || self.navigation.forward.is_some() {
                        Some("focus rejected because a forward request is pending".to_owned())
                    } else if self.focus_token.as_deref() != Some(viewer_token.as_str()) {
                        Some("focus rejected because viewer token does not match".to_owned())
                    } else {
                        None
                    };
                    if let Some(error) = error {
                        reply.finish(Some(error.clone()));
                        self.draw_status(
                            output,
                            self.viewport()?,
                            &format!("viewer focus: {error}"),
                        )?;
                    } else {
                        if let Some(task) = self.focus_task.as_ref() {
                            task.operation.cancel();
                        }
                        if let Some(mut waiting) = self.focus_waiting.replace(reply) {
                            waiting
                                .finish(Some("focus superseded by a newer focus request".into()));
                        }
                    }
                }
                Ok(crate::ipc::ViewerRequest::Quit) => {
                    reply.finish_with_token(None, self.focus_token.as_deref());
                    return Err(AppError::Quit);
                }
                Err(error) => {
                    reply.finish(Some(error.to_string()));
                    self.draw_status(
                        output,
                        self.viewport()?,
                        &format!("forward search: {error}"),
                    )?;
                }
            }
        }
        self.poll_focus_task(output)?;
        Ok(())
    }
    fn poll_focus_task(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if let Some(task) = self.focus_task.as_ref() {
            if task.reply.disconnected()? {
                task.operation.cancel();
            }
            if task.worker.is_finished() {
                let task = self.focus_task.take().expect("finished focus task");
                let result = task
                    .worker
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("viewer focus worker panicked")));
                let error = if task.operation.is_cancelled() {
                    Some("focus superseded or client disconnected".to_owned())
                } else {
                    result.err().map(|error| error.to_string())
                };
                let mut reply = task.reply;
                reply.finish(error.clone());
                if let Some(error) = error {
                    self.draw_status(output, self.viewport()?, &format!("viewer focus: {error}"))?;
                }
            }
        }
        if self.focus_task.is_none()
            && let Some(mut reply) = self.focus_waiting.take()
        {
            if reply.disconnected()? {
                reply.finish(Some("focus request disconnected".into()));
            } else if self.navigation.forward.is_some() {
                reply.finish(Some(
                    "focus rejected because a forward request is pending".into(),
                ));
            } else {
                let operation = crate::process::Operation::new(crate::focus::FOCUS_TIMEOUT);
                let worker_operation = operation.clone();
                match std::thread::Builder::new()
                    .name("viewer-focus".into())
                    .spawn(move || crate::focus::focus_self(&worker_operation))
                {
                    Ok(worker) => {
                        self.focus_task = Some(FocusTask {
                            operation,
                            worker,
                            reply,
                        })
                    }
                    Err(error) => {
                        reply.finish(Some(format!("could not start viewer focus: {error}")));
                        self.draw_status(
                            output,
                            self.viewport()?,
                            &format!("viewer focus: {error}"),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn save_screenshot(&mut self, path: &Path) -> io::Result<()> {
        if self.link_picker.is_some() || self.search_picker.is_some() {
            return Err(io::Error::other(
                "screenshot is unavailable while a link or search picker is open",
            ));
        }
        let viewport = self
            .viewport()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let overlay = if self.label_overlay_id.is_some() && !self.label_matches.is_empty() {
            self.label_overlay_pixels(viewport)?
        } else {
            None
        };
        let pages = self.screenshot_pages(viewport)?;
        crate::screenshot::save(
            path,
            viewport,
            &pages,
            overlay.as_ref(),
            terminal_color_rgb(self.theme.bg),
        )
    }

    fn current_visible_page(&self, viewport: Viewport) -> Option<&VisiblePage> {
        let page = self.visible_page.as_ref()?;
        (self.canvas_viewport == Some(viewport)
            && self.submitted_key == Some(self.render_key(viewport))
            && page.frame.revision.pdf == self.tab().revision
            && same_render_view(page.frame.key, self.render_key(viewport)))
        .then_some(page)
    }

    fn picker_image(&self, viewport: Viewport) -> Option<LinkPickerImage> {
        let page = self.current_visible_page(viewport)?;
        let mut image = LinkPickerImage::new(page.image_id, &page.frame, page.placement, viewport);
        image.original.top = page.top;
        Some(image)
    }

    fn screenshot_pages(&self, viewport: Viewport) -> io::Result<Vec<ScreenshotPage<'_>>> {
        let unavailable =
            || io::Error::other("viewer frame is still rendering; retry the screenshot");
        if self.missing_visible_page.is_some()
            || self.canvas_viewport != Some(viewport)
            || self.submitted_key != Some(self.render_key(viewport))
        {
            return Err(unavailable());
        }
        let mut pages = Vec::new();
        if self.visible_pages.is_empty() {
            let page = self
                .current_visible_page(viewport)
                .ok_or_else(unavailable)?;
            pages.push(ScreenshotPage {
                frame: &page.frame,
                placement: page.placement,
                row: page.top.saturating_sub(viewport.top),
            });
        } else {
            let current_key = self.render_key(viewport);
            for page in &self.visible_pages {
                if page.frame.revision.pdf != self.tab().revision
                    || !same_render_view(page.frame.key, current_key)
                {
                    return Err(unavailable());
                }
                pages.push(ScreenshotPage {
                    frame: &page.frame,
                    placement: page.placement,
                    row: page.top.saturating_sub(viewport.top),
                });
            }
        }
        Ok(pages)
    }

    fn finish_forward(&mut self, error: Option<String>) {
        if let Some(mut pending) = self.navigation.forward.take() {
            if error.is_none() {
                let path = fs::canonicalize(&pending.request.pdf)
                    .unwrap_or_else(|_| self.session.tabs[self.session.active_tab].path.clone());
                if let Some(endpoint) = pending.request.inverse_search {
                    self.navigation.source_maps.insert(path.clone(), endpoint);
                } else {
                    self.navigation.source_maps.remove(&path);
                }
                if pending.request.best_guess {
                    self.navigation.best_guess.insert(path);
                } else {
                    self.navigation.best_guess.remove(&path);
                }
            }
            pending
                .reply
                .finish_with_token(error, self.focus_token.as_deref());
        }
    }

    fn cancel_forward(&mut self, reason: &str) -> Result<(), AppError> {
        if self.navigation.forward.is_some() {
            self.finish_forward(Some(reason.into()));
            if let Some(flash) = self.navigation.flash.as_mut() {
                flash.expires_at = Some(Instant::now());
            }
            self.poll_flash_expiry()?;
        }
        Ok(())
    }

    fn poll_pending_forward(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let result = (|| -> Result<(), AppError> {
            let Some(pending) = self.navigation.forward.as_ref() else {
                return Ok(());
            };
            if pending.reply.disconnected()? {
                return Err(io::Error::other("forward-search client disconnected").into());
            }
            if Instant::now() >= pending.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "forward frame submission timed out",
                )
                .into());
            }
            pending.check_revision()?;
            if pending.stage == ForwardStage::AwaitingFrame || self.session.pending_open.is_some() {
                return Ok(());
            }
            let pdf = fs::canonicalize(&pending.request.pdf)?;
            let Some(index) = self.tab_index_for_path(&pdf) else {
                self.begin_open(pdf, output)?;
                return Ok(());
            };
            if self.session.tabs[index].revision != pending.request.revision {
                let document_id = self.session.tabs[index].document_id;
                let fingerprint = FileFingerprint::read(&pdf)?;
                self.begin_reload(document_id, pdf, fingerprint, output)?;
                return Ok(());
            }
            // Take the connection while positioning so internal tab/scroll transitions
            // cannot acknowledge an older cached frame.
            let mut pending = self.navigation.forward.take().unwrap();
            match self.apply_forward_request(&pending.request, index, output) {
                Ok(()) => {
                    pending.stage = ForwardStage::AwaitingFrame;
                    self.navigation.forward = Some(pending);
                }
                Err(error) => {
                    return {
                        pending.reply.finish(Some(error.to_string()));
                        Err(error)
                    };
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.cancel_forward(&error.to_string())?;
            self.draw_status(
                output,
                self.viewport()?,
                &format!("forward search: {error}"),
            )?;
        }
        Ok(())
    }

    fn apply_forward_request(
        &mut self,
        request: &ForwardRequest,
        index: usize,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if request.page > self.session.tabs[index].page_count {
            return Err(io::Error::other("forward page is outside the document").into());
        }
        let rect = request.rect();
        self.pending_scale = None;
        self.restore_active_picker_preview();
        self.select_tab(index, output)?;
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
        if let Some(previous) = self.navigation.flash.take() {
            self.clear_flash_state(&previous);
        }
        let tab = self.tab_mut();
        tab.search.initial_navigation_pending = false;
        let document_id = tab.document_id;
        let current = tab.view_position();
        let page = request.page.saturating_sub(1).min(tab.page_count - 1);
        if page != tab.page {
            tab.page = page;
            tab.scroll_y = 0;
        }
        let provisional = tab.view_position();
        tab.link_history.jump(current, provisional);
        self.navigation.flash = Some(PendingFlash {
            document_id,
            revision: request.revision,
            page,
            positioning_pending: true,
            expires_at: None,
        });
        self.worker
            .flash(document_id, page, rect, request.word.clone());
        self.bump_generation(true);
        // A cached unhighlighted target is not a submitted forward-search frame.
        self.tab_mut().cache.retain(|key, _| key.page != page);
        let viewport = self.viewport()?;
        let key = self.render_key(viewport);
        self.desired_key = Some(key);
        self.sync_worker_view_window(key.page);
        self.pending.insert(key, false);
        self.performance_snapshot = None;
        self.worker
            .render(RenderRequest {
                key,
                generation: self.generation,
            })
            .map_err(AppError::Renderer)?;
        Ok(())
    }

    fn clear_flash_state(&mut self, flash: &PendingFlash) {
        self.worker.clear_flash(flash.document_id);
        if let Some(index) = self.tab_index(flash.document_id) {
            self.session.tabs[index]
                .cache
                .retain(|key, _| key.page != flash.page);
        }
    }

    fn clear_document_flash(&mut self, document_id: DocumentId) {
        if self
            .navigation
            .flash
            .as_ref()
            .is_some_and(|flash| flash.document_id == document_id)
        {
            let flash = self.navigation.flash.take().unwrap();
            self.clear_flash_state(&flash);
        }
    }

    fn poll_flash_expiry(&mut self) -> Result<(), AppError> {
        if !self.navigation.flash.as_ref().is_some_and(|flash| {
            flash
                .expires_at
                .is_some_and(|deadline| Instant::now() >= deadline)
        }) {
            return Ok(());
        }
        let flash = self.navigation.flash.take().unwrap();
        self.clear_flash_state(&flash);
        // Clearing an overlay must not cancel an unrelated in-flight page render.
        if self.tab().document_id == flash.document_id
            && self.tab().revision == flash.revision
            && flash.page < self.tab().page_count
            && (self.tab().page == flash.page
                || self.visible_pages.iter().any(|page| {
                    page.frame.key.document_id == flash.document_id
                        && page.frame.key.page == flash.page
                        && page.frame.revision.pdf == flash.revision
                }))
        {
            let key = self.page_key(flash.page, self.viewport()?);
            self.request_visible_page(key)?;
        }
        Ok(())
    }

    fn poll_search_preview(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let origin = self.tab().view_position();
        let Some(state) = &mut self.search_picker else {
            return Ok(());
        };
        let Some((page, ready_at)) = state.pending_preview else {
            return Ok(());
        };
        if Instant::now() < ready_at {
            return Ok(());
        }
        state.pending_preview = None;
        if state.selection_page != Some(page) || page == origin.page {
            return Ok(());
        }
        state.preview_origin.get_or_insert(origin);
        state.page = page;
        self.apply_preview_jump(page, None, output)
    }

    fn handle_link_picker_pointer(
        &mut self,
        mouse: MouseEvent,
        output: &mut impl Write,
    ) -> Result<bool, AppError> {
        let area = link_picker_area(self.viewport()?);
        let tab = &self.session.tabs[self.session.active_tab];
        let links = &tab.link_index.links;
        let Some(state) = self.link_picker.as_ref() else {
            return Ok(false);
        };
        let Some(selected) = link_picker_link_at_position(
            area,
            LinkPickerDocument::from_tab(tab),
            state,
            self.link_picker_geometry,
            mouse.column,
            mouse.row,
        ) else {
            return Ok(false);
        };
        if state.selected == selected && state.focus == LinkPickerFocus::Links {
            if state.pending_preview.is_none() && state.preview_origin.is_none() {
                self.schedule_link_preview();
            }
            return Ok(true);
        }
        let state = self.link_picker.as_mut().expect("link picker state");
        state.focus = LinkPickerFocus::Links;
        state.select(selected, links);
        self.schedule_link_preview();
        self.redraw_link_picker(output)?;
        Ok(true)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let scroll = match mouse.kind {
            MouseEventKind::ScrollUp => Some((Axis::Vertical, false)),
            MouseEventKind::ScrollDown => Some((Axis::Vertical, true)),
            MouseEventKind::ScrollLeft => Some((Axis::Horizontal, false)),
            MouseEventKind::ScrollRight => Some((Axis::Horizontal, true)),
            _ => None,
        };
        if let Some((axis, forward)) = scroll {
            if self.link_picker.is_none()
                && self.search_picker.is_none()
                && self.search_input.is_none()
                && self.goto_input.is_none()
            {
                self.move_view(axis, forward, false, axis == Axis::Vertical, output)?;
            }
            return Ok(());
        }
        if self.link_picker.is_some()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.handle_link_picker_pointer(mouse, output)?
        {
            return Ok(());
        }
        if self.synctex_enabled
            && mouse.modifiers.contains(KeyModifiers::ALT)
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            self.begin_inverse_search(mouse, output)?;
            return Ok(());
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let Some((frame, placement, image_top)) = self.page_at_mouse(mouse, viewport) else {
            return Ok(());
        };
        let target = link_at_cell(
            &frame.links,
            placement,
            frame.width,
            frame.height,
            image_top,
            mouse.column,
            mouse.row,
        );
        let Some(target) = target else {
            self.draw_status(output, viewport, "no link here")?;
            return Ok(());
        };
        if matches!(&target, LinkTarget::Internal { .. })
            && let Some(origin) = self.take_picker_preview_origin()
        {
            self.tab_mut().link_history.push(origin);
        }
        if self.link_picker.is_some() && !self.persistent_link_picker {
            self.close_link_picker(output)?;
        }
        self.follow_link(target, output)
    }

    fn follow_link(&mut self, target: LinkTarget, output: &mut impl Write) -> Result<(), AppError> {
        match target {
            LinkTarget::Internal {
                page,
                top_ratio,
                left_ratio,
            } => {
                let current = self.tab().view_position();
                let page = page.min(self.tab().page_count - 1);
                let content_pan_x = self.tab().pending_destination.map_or_else(
                    || {
                        if self.tab().fit != FitMode::Content || current.center_content {
                            return None;
                        }
                        self.viewport()
                            .ok()
                            .and_then(|viewport| {
                                let key = self.page_key(current.page, viewport);
                                let from = self.tab().cache.get(&key)?;
                                Some(current.scroll_x - content_scroll(from, viewport).0)
                            })
                            .or_else(|| (current.page != page).then_some(0))
                    },
                    |pending| pending.content_pan_x,
                );
                let tab = self.tab_mut();
                tab.search.initial_navigation_pending = false;
                tab.page = page;
                if top_ratio.is_some() {
                    tab.scroll_y = 0;
                }
                let provisional = tab.view_position();
                tab.link_history.jump(current, provisional);
                tab.pending_destination = Some(LinkDestination {
                    content_pan_x,
                    page,
                    top_ratio,
                    left_ratio,
                });
                self.request_current(output)?;
            }
            LinkTarget::Uri(uri) => {
                if uri.trim().is_empty() {
                    self.draw_status(output, self.viewport()?, "empty link")?;
                } else {
                    write_clipboard_osc52(output, &uri)?;
                    self.draw_status(output, self.viewport()?, "copied link to clipboard")?;
                }
            }
        }
        Ok(())
    }

    fn jump_history(
        &mut self,
        target: Option<ViewPosition>,
        empty_msg: &str,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(pos) = target else {
            self.draw_status(output, self.viewport()?, empty_msg)?;
            return Ok(());
        };
        let tab = self.tab_mut();
        tab.search.initial_navigation_pending = false;
        tab.set_view_position(pos);
        tab.pending_destination = None;
        self.request_current(output)
    }

    fn follow_link_back(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let current = self.tab().view_position();
        let previous = self.tab_mut().link_history.backward(current);
        self.jump_history(previous, "no link history", output)
    }

    fn follow_link_forward(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let next = self.tab_mut().link_history.forward();
        self.jump_history(next, "no forward link history", output)
    }

    fn open_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.start_link_index();
        let viewport = self.viewport()?;
        let Some(image) = self.picker_image(viewport) else {
            self.draw_status(output, viewport, "page is still rendering")?;
            return Ok(());
        };
        let page = self
            .visible_page
            .as_ref()
            .expect("picker has a visible image")
            .frame
            .key
            .page;
        self.retain_primary_image(output)?;

        self.pending_link_picker_open = false;
        self.link_picker = Some(LinkPickerState::new(page));
        show_link_picker_split(
            output,
            link_picker_area(viewport),
            image,
            self.link_picker_geometry,
            self.theme,
        )?;
        self.redraw_link_picker(output)
    }

    fn open_pending_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if !self.pending_link_picker_open
            || !self.link_mode
            || self.link_picker.is_some()
            || self.session.pending_open.is_some()
        {
            return Ok(());
        }
        let viewport = self.viewport()?;
        if self.picker_image(viewport).is_none() {
            return Ok(());
        }
        self.open_link_picker(output)
    }

    fn paint_link_picker(
        &mut self,
        page: u32,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let Some(state) = &mut self.link_picker else {
            return Ok(());
        };
        let tab = &self.session.tabs[self.session.active_tab];
        let progress = LinkIndexProgress::from(&tab.link_index);
        let selection_before = state.selection_key;
        state.sync(page, &tab.link_index.links, progress.indexing);
        let schedule_preview =
            selection_before != state.selection_key && state.focus == LinkPickerFocus::Links;
        let document = LinkPickerDocument::from_tab(tab);
        let area = link_picker_area(viewport);
        let (geometry, theme) = (self.link_picker_geometry, self.theme);
        draw_side_picker_terminal(output, area, |frame| {
            draw_link_picker(frame, area, document, state, progress, geometry, theme)
        })?;
        if schedule_preview {
            self.schedule_link_preview();
        }
        Ok(())
    }

    fn redraw_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.is_some() {
            let viewport = self.viewport()?;
            let page = self
                .current_visible_page(viewport)
                .map_or(self.tab().page, |page| page.frame.key.page);
            synchronized_output(output, |output| {
                self.paint_link_picker(page, viewport, output)
            })?;
        }
        Ok(())
    }

    fn set_link_picker_layout(
        &mut self,
        layout: LinkPickerLayout,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.link_picker.is_none() && self.search_picker.is_none() {
            return Ok(());
        }
        if self.link_picker_geometry.layout == layout {
            return self.redraw_active_side_picker(output);
        }
        let viewport = self.viewport()?;
        let area = link_picker_area(viewport);
        let previous = self.link_picker_geometry;
        let image = self.picker_image(viewport);
        self.link_picker_geometry.layout = layout;
        if layout == LinkPickerLayout::Floating
            && let Some(state) = &mut self.link_picker
        {
            state.focus = LinkPickerFocus::Links;
        }
        if layout == LinkPickerLayout::Floating
            && let Some(state) = &mut self.search_picker
        {
            state.focus = LinkPickerFocus::Links;
        }
        if let Some(image) = image {
            restore_link_picker_split(output, area, image, previous, self.theme)?;
            show_link_picker_split(output, area, image, self.link_picker_geometry, self.theme)?;
            self.redraw_active_side_picker(output)
        } else {
            self.reset_render_state();
            self.request_current(output)
        }
    }

    fn cycle_link_picker_layout(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let area = link_picker_area(self.viewport()?);
        let layout = next_link_picker_layout(area, self.link_picker_geometry.layout);
        self.set_link_picker_layout(layout, output)
    }

    fn redraw_active_side_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.is_some() {
            self.redraw_link_picker(output)
        } else {
            self.redraw_search_picker(output)
        }
    }

    fn restore_preview_origin(
        &mut self,
        origin: Option<ViewPosition>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if let Some(origin) = origin {
            let tab = self.tab_mut();
            tab.set_view_position(origin);
            tab.pending_destination = None;
            self.clear_viewer(output)?;
            self.reset_render_state();
        }
        self.request_current(output)
    }

    fn close_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(state) = self.link_picker.take() else {
            return Ok(());
        };
        self.restore_preview_origin(state.preview_origin, output)
    }

    fn close_link_picker_and_exit_link_mode(
        &mut self,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.close_link_picker(output)?;
        if self.link_mode {
            self.set_link_mode(false, output)?;
        }
        Ok(())
    }

    fn open_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        self.retain_primary_image(output)?;
        let viewport = self.viewport()?;
        if let Some(image) = self.picker_image(viewport) {
            show_link_picker_split(
                output,
                link_picker_area(viewport),
                image,
                self.link_picker_geometry,
                self.theme,
            )?;
            self.redraw_search_picker(output)
        } else {
            self.reset_render_state();
            self.request_current(output)
        }
    }

    fn paint_search_picker(
        &mut self,
        page: u32,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let Some(state) = &mut self.search_picker else {
            return Ok(());
        };
        let tab = &self.session.tabs[self.session.active_tab];
        state.sync(page, &tab.search.matches);
        let area = link_picker_area(viewport);
        let (geometry, theme) = (self.link_picker_geometry, self.theme);
        draw_side_picker_terminal(output, area, |frame| {
            draw_search_picker(
                frame,
                area,
                &tab.search,
                &tab.outline,
                state,
                geometry,
                theme,
            )
        })
    }

    fn redraw_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.search_picker.is_some() {
            let viewport = self.viewport()?;
            let page = self
                .current_visible_page(viewport)
                .map_or(self.tab().page, |page| page.frame.key.page);
            synchronized_output(output, |output| {
                self.paint_search_picker(page, viewport, output)
            })?;
        }
        Ok(())
    }

    fn close_search_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(state) = self.search_picker.take() else {
            return Ok(());
        };
        self.tab_mut().search.initial_navigation_pending = false;
        self.restore_preview_origin(state.preview_origin, output)
    }

    fn handle_side_picker_chrome_key(
        &mut self,
        area: Rect,
        key: KeyEvent,
        is_link: bool,
        output: &mut impl Write,
    ) -> Result<bool, AppError> {
        let layout = resolved_link_picker_layout(area, self.link_picker_geometry.layout);
        let current_focus = if is_link {
            self.link_picker.as_ref().expect("link picker state").focus
        } else {
            self.search_picker
                .as_ref()
                .expect("search picker state")
                .focus
        };
        if let Some(focus) = link_picker_focus_for_key(current_focus, layout, key) {
            if is_link {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.focus = focus;
                if focus == LinkPickerFocus::Document {
                    state.pending_preview = None;
                } else {
                    self.schedule_link_preview();
                }
            } else {
                let state = self.search_picker.as_mut().expect("search picker state");
                state.focus = focus;
                if focus == LinkPickerFocus::Document {
                    state.pending_preview = None;
                } else {
                    self.schedule_search_preview();
                }
            }
            self.redraw_active_side_picker(output)?;
            return Ok(true);
        }
        if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            match normalize_shifted_key(key).code {
                KeyCode::Char('s') => {
                    self.cycle_link_picker_layout(output)?;
                    return Ok(true);
                }
                KeyCode::Char('a') => {
                    self.set_link_picker_layout(LinkPickerLayout::Auto, output)?;
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_search_picker_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let key = normalize_shifted_key(key);
        let area = link_picker_area(self.viewport()?);
        if self.handle_side_picker_chrome_key(area, key, false, output)? {
            return Ok(());
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return self.close_search_picker(output),
            KeyCode::Char('/') if plain => {
                self.close_search_picker(output)?;
                return self.begin_search(output);
            }
            _ => {}
        }
        if self
            .search_picker
            .as_ref()
            .is_some_and(|state| state.focus == LinkPickerFocus::Document)
        {
            self.dispatch_document_navigation_key(key, true, output)?;
            return Ok(());
        }

        let visible_height = search_picker_visible_height(area, self.link_picker_geometry);
        let tab = &mut self.session.tabs[self.session.active_tab];
        let matches = &tab.search.matches;
        let selected = self
            .search_picker
            .as_ref()
            .expect("search picker state")
            .selected;
        if let Some(next) = picker_navigation_index(
            selected,
            matches.len(),
            key,
            visible_height,
            true,
            ListEdge::Wrap,
        ) {
            tab.search.initial_navigation_pending = false;
            let state = self.search_picker.as_mut().expect("search picker state");
            state.selected = next;
            state.selection_page = matches.get(next).map(|result| result.page);
            self.schedule_search_preview();
            self.redraw_search_picker(output)?;
        } else if key.code == KeyCode::Enter
            && let Some(result) = matches.get(selected)
        {
            let page = result.page;
            self.clear_picker_previews();
            self.set_page(page, output)?;
        }
        Ok(())
    }

    fn handle_link_picker_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let key = normalize_shifted_key(key);
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain = !control && !alt;
        let (filtering, has_filter) = self
            .link_picker
            .as_ref()
            .map_or((false, false), |s| (s.filtering, !s.filter.is_empty()));
        let area = link_picker_area(self.viewport()?);
        if !filtering && self.handle_side_picker_chrome_key(area, key, true, output)? {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc if self.link_mode => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Esc if filtering || has_filter => {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.filter.clear();
                state.filtering = false;
                state.number_input.clear();
                state.awaiting_current_page = true;
                return self.redraw_link_picker(output);
            }
            KeyCode::Esc => return self.close_link_picker(output),
            KeyCode::Char('/') if !filtering && plain => {
                let state = self.link_picker.as_mut().expect("link picker state");
                state.filtering = true;
                state.focus = LinkPickerFocus::Links;
                state.number_input.clear();
                return self.redraw_link_picker(output);
            }
            KeyCode::Char('L') if !filtering && plain => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Char('q') if plain && (self.link_mode || !filtering) => {
                return self.close_link_picker_and_exit_link_mode(output);
            }
            KeyCode::Char('B') if !filtering && plain => {
                return self.follow_link_forward(output).map(|_| ());
            }
            KeyCode::Char('b') if !filtering && plain => {
                return self.follow_link_back(output).map(|_| ());
            }
            KeyCode::Char('o') if control && !alt => {
                return self.follow_link_back(output).map(|_| ());
            }
            KeyCode::Char('i') if control && !alt => {
                return self.follow_link_forward(output).map(|_| ());
            }
            _ => {}
        }

        if self
            .link_picker
            .as_ref()
            .is_some_and(|state| state.focus == LinkPickerFocus::Document)
        {
            self.dispatch_document_navigation_key(key, true, output)?;
            return Ok(());
        }

        let viewport = self.viewport()?;
        let page = self
            .current_visible_page(viewport)
            .map_or(self.tab().page, |page| page.frame.key.page);
        let visible_height =
            link_picker_visible_height(link_picker_area(viewport), self.link_picker_geometry);
        let tab = &self.session.tabs[self.session.active_tab];
        let links = &tab.link_index.links;
        let indexing = tab.link_index.indexing;
        let state = self.link_picker.as_mut().expect("link picker state");
        state.sync(page, links, indexing);
        let selected_before = state.selected;

        let input_consumed = if !state.filtering {
            false
        } else if key.code == KeyCode::Enter {
            state.filtering = false;
            false
        } else {
            edit_picker_filter(&mut state.filter, key)
        };
        let rows = state.rows(LinkPickerDocument::from_tab(tab));
        let filtered = &rows.filtered;
        if !filtered.contains(&state.selected)
            && let Some(first) = filtered.first().copied()
        {
            state.select(first, links);
        }
        let selected_position = filtered
            .iter()
            .position(|index| *index == state.selected)
            .unwrap_or(0);
        let navigation = (!input_consumed)
            .then(|| {
                picker_navigation_index(
                    selected_position,
                    filtered.len(),
                    key,
                    visible_height,
                    true,
                    ListEdge::Wrap,
                )
            })
            .flatten();
        let mut explicit_selection = navigation.is_some();
        let mut redraw = navigation.is_some();
        let mut target = None;
        if input_consumed {
            redraw = true;
        } else if let Some(selected) = navigation {
            state.select(filtered[selected], links);
            state.number_input.clear();
        } else {
            match key.code {
                KeyCode::Enter => {
                    target = filtered
                        .get(selected_position)
                        .and_then(|index| links.get(*index))
                        .map(|link| link.target.clone());
                    state.number_input.clear();
                    redraw = false;
                }
                KeyCode::Backspace => {
                    state.number_input.pop();
                    if let Some(index) = link_number_index(&state.number_input, filtered.len()) {
                        state.select(filtered[index], links);
                        explicit_selection = true;
                    }
                    redraw = true;
                }
                KeyCode::Char(digit) if !control && !alt && digit.is_ascii_digit() => {
                    if let Some(index) =
                        update_link_number_selection(&mut state.number_input, digit, filtered.len())
                    {
                        state.select(filtered[index], links);
                        explicit_selection = true;
                    }
                    redraw = true;
                }
                _ => redraw = false,
            }
        }
        let selection_changed = state.selected != selected_before;

        if let Some(target) = target {
            if matches!(&target, LinkTarget::Internal { .. })
                && let Some(origin) = self.take_picker_preview_origin()
            {
                self.tab_mut().link_history.push(origin);
            }
            if !self.persistent_link_picker {
                self.close_link_picker(output)?;
            }
            self.follow_link(target, output)?;
        } else if redraw {
            if selection_changed || explicit_selection {
                self.schedule_link_preview();
            }
            self.redraw_link_picker(output)?;
        }
        Ok(())
    }

    fn dispatch_document_navigation_key(
        &mut self,
        key: KeyEvent,
        from_side_picker: bool,
        output: &mut impl Write,
    ) -> Result<bool, AppError> {
        if key.modifiers == KeyModifiers::CONTROL {
            let forward = match key.code {
                KeyCode::Char('b' | 'u') => false,
                KeyCode::Char('f' | 'd') => true,
                _ => return Ok(false),
            };
            if from_side_picker {
                self.clear_picker_previews();
            }
            self.move_view(Axis::Vertical, forward, true, true, output)?;
            return Ok(true);
        }
        let code = normalize_shifted_key(key).code;
        if matches!(code, KeyCode::Char(_))
            && !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
        {
            return Ok(false);
        }
        let matched = matches!(
            code,
            KeyCode::Char('J' | 'K' | 'j' | 'k' | ' ' | 'd' | 'u' | 'g' | 'G' | 'm' | 'i')
                | KeyCode::Down
                | KeyCode::Up
                | KeyCode::PageDown
                | KeyCode::PageUp
                | KeyCode::Backspace
                | KeyCode::Right
                | KeyCode::Left
                | KeyCode::Home
                | KeyCode::End
        ) || (!from_side_picker && matches!(code, KeyCode::Char('h' | 'l')));
        if !matched {
            return Ok(false);
        }
        if from_side_picker {
            self.clear_picker_previews();
        }
        match code {
            KeyCode::Char('J') => self.page_step(true, output)?,
            KeyCode::Char('K') => self.page_step(false, output)?,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_view(Axis::Vertical, true, false, true, output)?
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_view(Axis::Vertical, false, false, true, output)?
            }
            KeyCode::PageDown | KeyCode::Char(' ' | 'd') => {
                self.move_view(Axis::Vertical, true, true, true, output)?
            }
            KeyCode::PageUp | KeyCode::Backspace | KeyCode::Char('u') => {
                self.move_view(Axis::Vertical, false, true, true, output)?
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_view(Axis::Horizontal, true, false, true, output)?
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_view(Axis::Horizontal, false, false, true, output)?
            }
            KeyCode::Char('g') | KeyCode::Home => self.set_page_vertically(0, output)?,
            KeyCode::Char('G') | KeyCode::End => {
                self.set_page_vertically(self.tab().page_count - 1, output)?
            }
            KeyCode::Char('m') => self.cycle_fit(output)?,
            KeyCode::Char('i') => self.toggle_invert(output)?,
            _ => unreachable!(),
        }
        Ok(true)
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
        let key = normalize_shifted_key(key);
        if self.search_picker.is_some() {
            self.handle_search_picker_key(key, output)?;
            return Ok(false);
        }
        if self.link_picker.is_some() {
            self.handle_link_picker_key(key, output)?;
            return Ok(false);
        }
        if self.search_input.is_some() {
            self.handle_search_key(key, output)?;
            return Ok(false);
        }
        if self.goto_input.is_some() {
            self.handle_goto_key(key, output)?;
            return Ok(false);
        }
        if self.label_query.is_some() {
            self.handle_label_key(key, output)?;
            return Ok(false);
        }
        if let Some(index) = numbered_tab_index(key) {
            self.select_tab(index, output)?;
            return Ok(false);
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('o') => {
                    self.follow_link_back(output)?;
                    return Ok(false);
                }
                KeyCode::Char('i') => {
                    self.follow_link_forward(output)?;
                    return Ok(false);
                }
                _ => {}
            }
        }
        if self.dispatch_document_navigation_key(key, false, output)? {
            return Ok(false);
        }
        let shift = key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char(_)
                if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) => {}
            KeyCode::Char('x') if self.session.pending_open.is_none() && !self.link_mode => {
                self.begin_label_mode(output)?
            }
            KeyCode::Char('?') => self.open_help(output)?,
            KeyCode::Char('q') if self.link_mode => self.set_link_mode(false, output)?,
            KeyCode::Char('q') => return self.close_current(output),
            KeyCode::Char(':') => self.begin_goto(String::new(), output)?,
            KeyCode::Char(digit @ '1'..='9') if !shift => {
                self.begin_goto(digit.to_string(), output)?
            }
            KeyCode::Char('/') => self.begin_search(output)?,
            KeyCode::Char('n') => self.navigate_search(true, output)?,
            KeyCode::Char('N') => self.navigate_search(false, output)?,
            KeyCode::Char('L') => self.toggle_link_mode(output)?,
            KeyCode::Char('B') => self.follow_link_forward(output)?,
            KeyCode::Char('b') => self.follow_link_back(output)?,
            KeyCode::Enter => self.open_link_picker(output)?,
            KeyCode::Esc if self.link_mode => self.set_link_mode(false, output)?,
            KeyCode::Esc if self.clear_search(output)? => {}
            KeyCode::Esc => return Ok(true),
            KeyCode::Char('f') => self.open_picker(output)?,
            KeyCode::Char('D') => self.duplicate_tab(output)?,
            KeyCode::Tab => self.switch_tab(1, output)?,
            KeyCode::BackTab => self.switch_tab(-1, output)?,
            KeyCode::Char('+' | '=') => self.zoom_in(output)?,
            KeyCode::Char('-' | '_') => self.zoom_out(output)?,
            KeyCode::Char('0') => self.reset_zoom(output)?,
            KeyCode::Char('S') => self.toggle_smooth_scroll(output)?,
            KeyCode::Char('p') => self.toggle_performance(output)?,
            KeyCode::Char('t') => self.open_outline(output)?,
            KeyCode::Char('T') => self.open_theme_picker(output)?,
            KeyCode::Char('y') => self.request_copy(output)?,
            KeyCode::Char('R') => self.reload_config(output)?,
            _ => {}
        }
        Ok(false)
    }

    fn set_page(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        self.tab_mut().scroll_x = 0;
        self.set_page_vertically(page, output)
    }

    fn set_page_vertically(&mut self, page: u32, output: &mut impl Write) -> Result<(), AppError> {
        let page = page.min(self.tab().page_count - 1);
        let tab = self.tab_mut();
        tab.search.initial_navigation_pending = false;
        tab.page = page;
        tab.scroll_y = 0;
        tab.pending_destination = None;
        tab.center_content = true;
        self.request_current(output)
    }

    /// Vertical movement crosses page boundaries without discarding the remainder.
    fn move_view(
        &mut self,
        axis: Axis,
        forward: bool,
        large: bool,
        allow_page_step: bool,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.tab_mut().search.initial_navigation_pending = false;
        let viewport = self.viewport()?;
        if self.continuous_view() && axis == Axis::Vertical {
            let pixels = if large {
                u32::from(viewport.pixel_height) * self.viewer.page_scroll_percent as u32 / 100
            } else {
                u32::from(viewport.pixel_height) * self.viewer.scroll_step_percent as u32 / 100
            };
            let step = i64::from(pixels.max(1));
            self.smooth_scroll_remaining += if forward { step } else { -step };
            if !self.viewer.smooth_scroll {
                self.pending_vertical_scroll += std::mem::take(&mut self.smooth_scroll_remaining);
                if self.apply_vertical_scroll(viewport)? {
                    self.request_current(output)?;
                }
            }
            return Ok(());
        }
        let key = self.render_key(viewport);
        let Some(frame) = self.tab().cache.get(&key).cloned() else {
            return if allow_page_step {
                self.page_step(forward, output)
            } else {
                Ok(())
            };
        };
        let (max_x, max_y) = viewport.max_scroll(frame.width, frame.height);
        let mut min_x = 0_i64;
        let mut max_x = i64::from(max_x);
        if self.continuous_view() {
            for page in &self.visible_pages {
                let origin = content_horizontal_scroll(0, &frame, &page.frame, viewport);
                min_x = min_x.min(-origin);
                max_x = max_x.max(
                    i64::from(
                        page.frame
                            .width
                            .saturating_sub(u32::from(viewport.pixel_width)),
                    ) - origin,
                );
            }
        }
        let (axis_min, axis_max, current) = match axis {
            Axis::Vertical => (0, i64::from(max_y), i64::from(self.tab().scroll_y)),
            Axis::Horizontal => (min_x, max_x, self.tab().scroll_x),
        };
        if axis_min == axis_max {
            return if allow_page_step {
                self.page_step(forward, output)
            } else {
                Ok(())
            };
        }

        let span = match axis {
            Axis::Vertical => u32::from(viewport.pixel_height),
            Axis::Horizontal => u32::from(viewport.pixel_width),
        };
        let step = if large {
            (span * self.viewer.page_scroll_percent as u32 / 100).max(1)
        } else {
            (span * self.viewer.scroll_step_percent as u32 / 100).max(1)
        };

        let next = if forward {
            if current >= axis_max {
                return if allow_page_step {
                    self.page_step(true, output)
                } else {
                    Ok(())
                };
            }
            (current + i64::from(step)).min(axis_max)
        } else {
            if current <= axis_min {
                return if allow_page_step {
                    self.page_step(false, output)
                } else {
                    Ok(())
                };
            }
            (current - i64::from(step)).max(axis_min)
        };
        match axis {
            Axis::Vertical => self.tab_mut().scroll_y = next as u32,
            Axis::Horizontal => self.tab_mut().scroll_x = next,
        }
        self.redraw_current(output)
    }

    fn input_wait(&self) -> Duration {
        let fast_wait = Duration::from_millis(10);
        if self.smooth_scroll_remaining != 0 && self.pending_vertical_scroll == 0 {
            return Duration::from_millis(self.viewer.scroll_frame_ms)
                .saturating_sub(self.smooth_scroll_tick.elapsed())
                .min(fast_wait);
        }
        if !self.pending.is_empty()
            || self.session.pending_open.is_some()
            || self.navigation.forward.is_some()
            || self.navigation.inverse.is_some()
            || self.focus_task.is_some()
            || self.focus_waiting.is_some()
            || self.pending_vertical_scroll != 0
            || self.label_loading
            || self
                .forward_listener
                .as_ref()
                .is_some_and(crate::ipc::ForwardListener::has_in_flight_clients)
            || self
                .session
                .tabs
                .iter()
                .any(|tab| tab.search.searching || tab.link_index.indexing)
        {
            return fast_wait;
        }
        let mut wait = if self.forward_socket.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(250)
        };
        let now = Instant::now();
        for deadline in [
            self.link_picker
                .as_ref()
                .and_then(|state| state.pending_preview.map(|pending| pending.ready_at)),
            self.search_picker
                .as_ref()
                .and_then(|state| state.pending_preview.map(|(_, ready_at)| ready_at)),
            self.navigation
                .flash
                .as_ref()
                .and_then(|flash| flash.expires_at),
        ]
        .into_iter()
        .flatten()
        {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        for tab in &self.session.tabs {
            if tab.watcher.candidate.is_some() {
                wait = wait.min(tab.watcher.next_poll.saturating_duration_since(now));
            }
        }
        wait.max(Duration::from_millis(1))
    }

    fn poll_smooth_scroll(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.is_some()
            || self.search_picker.is_some()
            || self.search_input.is_some()
            || self.goto_input.is_some()
        {
            self.smooth_scroll_remaining = 0;
            return Ok(());
        }
        let now = Instant::now();
        if self.smooth_scroll_remaining == 0
            || self.pending_vertical_scroll != 0
            || now.duration_since(self.smooth_scroll_tick)
                < Duration::from_millis(self.viewer.scroll_frame_ms)
        {
            return Ok(());
        }
        self.smooth_scroll_tick = now;
        let viewport = self.viewport()?;
        // Ease toward the accumulated wheel/key target without queuing animations.
        let remaining = self.smooth_scroll_remaining;
        let step = remaining
            .unsigned_abs()
            .div_ceil(self.viewer.scroll_ease_divisor) as i64
            * remaining.signum();
        self.smooth_scroll_remaining -= step;
        self.pending_vertical_scroll = step;
        if self.apply_vertical_scroll(viewport)? {
            let key = self.render_key(viewport);
            if self.desired_key == Some(key)
                && let Some(frame) = self.tab().cache.get(&key).cloned()
            {
                // An offset-only tick needs placement, not a new render generation
                // or another scan of the prefetch window.
                self.draw_frame(&frame, viewport, output)?;
            } else {
                let rest = self.smooth_scroll_remaining;
                self.request_current(output)?;
                self.smooth_scroll_remaining = rest;
            }
        }
        Ok(())
    }

    fn page_key(&self, page: u32, viewport: Viewport) -> RenderKey {
        RenderKey {
            page,
            search_request_id: self.tab().search.highlight_request_id(page),
            selected_link_ordinal: None,
            ..self.render_key(viewport)
        }
    }

    fn request_visible_page(&mut self, key: RenderKey) -> Result<(), AppError> {
        if self.pending.insert(key, false) != Some(false) {
            self.worker
                .render(RenderRequest {
                    key,
                    generation: self.generation,
                })
                .map_err(AppError::Renderer)?;
        }
        Ok(())
    }

    fn apply_vertical_scroll(&mut self, viewport: Viewport) -> Result<bool, AppError> {
        let cell = viewport.cell_size().1;
        while self.pending_vertical_scroll != 0 {
            let page = self.tab().page;
            let key = self.page_key(page, viewport);
            let Some(frame) = self.tab().cache.get(&key) else {
                self.request_visible_page(key)?;
                return Ok(false);
            };
            let offset = i64::from(self.tab().scroll_y);
            let delta = self.pending_vertical_scroll;
            if delta > 0 {
                if page + 1 == self.tab().page_count {
                    self.tab_mut().scroll_y = (offset + delta).min(i64::from(
                        frame
                            .height
                            .saturating_sub(u32::from(viewport.pixel_height)),
                    )) as u32;
                    self.pending_vertical_scroll = 0;
                } else {
                    let span = i64::from(frame.height) + i64::from(cell);
                    if offset + delta < span {
                        self.tab_mut().scroll_y = (offset + delta) as u32;
                        self.pending_vertical_scroll = 0;
                    } else {
                        if frame.key.fit == FitMode::Content {
                            let next = self.page_key(page + 1, viewport);
                            let Some(next) = self.tab().cache.get(&next) else {
                                self.request_visible_page(next)?;
                                return Ok(false);
                            };
                            let scroll_x = content_horizontal_scroll(
                                self.tab().scroll_x,
                                frame,
                                next,
                                viewport,
                            );
                            self.tab_mut().scroll_x = scroll_x;
                        }
                        self.pending_vertical_scroll = offset + delta - span;
                        self.tab_mut().page += 1;
                        self.tab_mut().scroll_y = 0;
                    }
                }
            } else if offset + delta >= 0 || page == 0 {
                self.tab_mut().scroll_y = (offset + delta).max(0) as u32;
                self.pending_vertical_scroll = 0;
            } else {
                let previous = self.page_key(page - 1, viewport);
                let Some(previous) = self.tab().cache.get(&previous) else {
                    self.request_visible_page(previous)?;
                    return Ok(false);
                };
                let span = previous.height + cell;
                if frame.key.fit == FitMode::Content {
                    let scroll_x =
                        content_horizontal_scroll(self.tab().scroll_x, frame, previous, viewport);
                    self.tab_mut().scroll_x = scroll_x;
                }
                self.pending_vertical_scroll += offset;
                self.tab_mut().page -= 1;
                self.tab_mut().scroll_y = span;
            }
        }
        Ok(true)
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
        let key = self.render_key(viewport);
        if let Some(frame) = self.tab().cache.get(&key).cloned() {
            self.draw_frame(&frame, viewport, output)?;
        }
        Ok(())
    }

    fn request_copy(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
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
        self.set_view_scale(self.tab().fit.cycle(), self.tab().zoom, output)
    }

    fn set_zoom(&mut self, zoom: u16, output: &mut impl Write) -> Result<(), AppError> {
        self.set_view_scale(self.tab().fit, zoom, output)
    }

    fn set_view_scale(
        &mut self,
        fit: FitMode,
        zoom: u16,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let old_zoom = self.tab().zoom;
        if zoom == old_zoom && fit == self.tab().fit {
            return Ok(());
        }
        // Anchor to the displayed position, not an unfinished scroll target.
        self.smooth_scroll_remaining = 0;
        self.pending_vertical_scroll = 0;
        if fit == FitMode::Content && self.tab().fit != FitMode::Content {
            let tab = self.tab_mut();
            tab.fit = fit;
            tab.zoom = zoom;
            tab.scroll_x = 0;
            tab.scroll_y = 0;
            tab.center_content = true;
            return self.request_current(output);
        }
        let viewport = self.viewport()?;
        let tab = self.tab();
        let current_key = self.render_key(viewport);
        let page_frame = |page| {
            let key = self.page_key(page, viewport);
            tab.cache
                .get(&key)
                .map(Arc::as_ref)
                .or_else(|| {
                    self.visible_pages
                        .iter()
                        .find(|visible| {
                            visible.frame.key.document_id == tab.document_id
                                && visible.frame.key.page == page
                                && visible.frame.key.fit == tab.fit
                                && visible.frame.key.width == viewport.pixel_width
                                && visible.frame.key.height == viewport.pixel_height
                                && visible.frame.revision.pdf == tab.revision
                        })
                        .map(|visible| visible.frame.as_ref())
                })
                .or_else(|| {
                    tab.cache
                        .values()
                        .find(|frame| {
                            frame.key.page == page
                                && frame.key.width == current_key.width
                                && frame.key.height == current_key.height
                                && frame.key.fit == current_key.fit
                                && frame.revision.pdf == tab.revision
                        })
                        .map(Arc::as_ref)
                })
        };
        let page_size = |page| {
            page_frame(page).map(|frame| {
                let (width, height) = if tab.fit == FitMode::Content {
                    fitted_page_size(
                        viewport,
                        frame.page_width_pt,
                        frame.page_height_pt,
                        frame.content_bounds,
                        tab.fit,
                        old_zoom,
                    )
                } else {
                    (
                        scale_zoom(frame.width, frame.key.zoom, old_zoom),
                        scale_zoom(frame.height, frame.key.zoom, old_zoom),
                    )
                };
                let origin = (tab.fit == FitMode::Content).then(|| {
                    content_axis_scroll(
                        frame.content_bounds[0],
                        frame.content_bounds[2],
                        width,
                        viewport.pixel_width,
                    )
                });
                (width, height, origin)
            })
        };
        let position = centered_scaled_view(
            viewport,
            tab.page,
            tab.page_count,
            (tab.scroll_x, tab.scroll_y),
            |page, width, height| {
                let frame = page_frame(page)?;
                let (width, height) = if fit == tab.fit && fit != FitMode::Content {
                    (
                        scale_zoom(width, old_zoom, zoom),
                        scale_zoom(height, old_zoom, zoom),
                    )
                } else {
                    fitted_page_size(
                        viewport,
                        frame.page_width_pt,
                        frame.page_height_pt,
                        frame.content_bounds,
                        fit,
                        zoom,
                    )
                };
                let origin = (fit == FitMode::Content).then(|| {
                    content_axis_scroll(
                        frame.content_bounds[0],
                        frame.content_bounds[2],
                        width,
                        viewport.pixel_width,
                    )
                });
                Some((width, height, origin))
            },
            self.continuous_view(),
            page_size,
        );
        let (page, scroll_x, scroll_y) = match position {
            Ok(position) => position,
            Err(missing_page) => {
                let key = self.page_key(missing_page, viewport);
                self.pending_scale = Some((key, fit, zoom));
                self.request_visible_page(key)?;
                self.draw_status(output, viewport, "loading page geometry")?;
                return Ok(());
            }
        };
        let tab = self.tab_mut();
        tab.page = page;
        tab.scroll_x = scroll_x;
        tab.scroll_y = scroll_y;
        tab.zoom = zoom;
        tab.fit = fit;
        self.request_current(output)
    }

    fn zoom_in(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let zoom = stepped_zoom(self.tab().zoom, true);
        self.set_zoom(zoom, output)
    }

    fn zoom_out(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let zoom = stepped_zoom(self.tab().zoom, false);
        self.set_zoom(zoom, output)
    }

    fn reset_zoom(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.set_zoom(ZOOM_DEFAULT, output)
    }

    fn toggle_invert(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let inverted = !self.tab().invert;
        self.tab_mut().invert = inverted;
        self.request_current(output)
    }

    fn toggle_smooth_scroll(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.viewer.smooth_scroll = !self.viewer.smooth_scroll;
        if !self.viewer.smooth_scroll && self.smooth_scroll_remaining != 0 {
            self.pending_vertical_scroll += std::mem::take(&mut self.smooth_scroll_remaining);
            if self.apply_vertical_scroll(self.viewport()?)? {
                self.request_current(output)?;
            }
        }
        let state = if self.viewer.smooth_scroll {
            "smooth scroll on"
        } else {
            "smooth scroll off"
        };
        self.draw_status(output, self.viewport()?, state)?;
        Ok(())
    }

    fn toggle_performance(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.show_performance = !self.show_performance;
        let Some(snapshot) = self.performance_snapshot else {
            return Ok(());
        };
        let state = snapshot.status(self.show_performance, self.link_mode);
        self.draw_status(output, self.viewport()?, &state)?;
        Ok(())
    }

    fn reload_config(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let config = match Config::load().and_then(|mut config| {
            if let Some(session) = self.session_name.as_deref() {
                config.select_session(Some(session))?;
            }
            Ok(config)
        }) {
            Ok(config) => config,
            Err(error) => {
                let viewport = self.viewport()?;
                self.draw_status(output, viewport, &format!("config error: {error}"))?;
                return Ok(());
            }
        };
        let warning = self.apply_config(&config);
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)?;
        let viewport = self.viewport()?;
        let status = match warning {
            Some(warning) => format!("config reloaded ({warning})"),
            None => "config reloaded".to_string(),
        };
        self.draw_status(output, viewport, &status)?;
        Ok(())
    }

    fn apply_config(&mut self, config: &Config) -> Option<String> {
        let mut defaults = AppDefaults::from_config(config, self.focus_token.clone());
        if config.session().is_none()
            && let Some(session) = self.session_name.as_deref()
            && let Some(socket) = defaults.editor.socket_mut()
            && let Ok(Some(prefixed)) = crate::config::prefixed_session_socket(session, socket)
        {
            *socket = prefixed;
        }
        let fit_changed = self.default_fit != defaults.fit;
        let invert_changed = self.default_invert != defaults.invert;
        self.default_fit = defaults.fit;
        self.default_invert = defaults.invert;
        self.persistent_link_picker = defaults.persistent_link_picker;
        self.link_picker_geometry = defaults.link_picker_geometry;
        if self.link_picker_geometry.layout == LinkPickerLayout::Floating {
            if let Some(state) = &mut self.link_picker {
                state.focus = LinkPickerFocus::Links;
            }
            if let Some(state) = &mut self.search_picker {
                state.focus = LinkPickerFocus::Links;
            }
        }
        self.synctex_enabled = defaults.synctex_enabled;
        self.editor = defaults.editor;
        self.viewer = defaults.viewer;
        if !self.viewer.set_window_title {
            self.title_document = None;
        }
        if !self.viewer.smooth_scroll {
            self.smooth_scroll_remaining = 0;
        }
        self.themes = defaults.themes;
        self.flash_font = None;
        self.theme_index = defaults.theme_index;
        self.set_palette(defaults.theme);
        for tab in &mut self.session.tabs {
            if fit_changed {
                tab.fit = defaults.fit;
                if defaults.fit == FitMode::Content {
                    tab.center_content = true;
                }
            }
            if invert_changed {
                tab.invert = defaults.invert;
            }
        }
        defaults.theme_warning
    }

    pub(super) fn bump_generation(&mut self, invalidate_view: bool) {
        self.generation = self.generation.wrapping_add(1);
        if invalidate_view {
            self.view_generation = self.generation;
            self.last_render_view = None;
            self.pending.clear();
            self.worker.invalidate_view(self.view_generation);
        } else {
            self.pending.retain(|_, is_prefetch| *is_prefetch);
        }
        self.worker.begin_generation(self.generation);
    }

    fn sync_worker_view_window(&mut self, page: u32) {
        let radius = u32::try_from(self.viewer.prefetch_pages)
            .expect("validated prefetch_pages exceeds u32")
            .max(1);
        let active_doc = self.tab().document_id;
        let max_visible = self
            .visible_pages
            .last()
            .filter(|visible| visible.frame.key.document_id == active_doc)
            .map_or(page, |visible| visible.frame.key.page)
            .max(page);
        let min_page = page.saturating_sub(radius);
        let max_page = max_visible
            .saturating_add(radius)
            .min(self.tab().page_count.saturating_sub(1));
        self.pending.retain(|key, is_prefetch| {
            !*is_prefetch
                || (key.document_id == active_doc && key.page >= min_page && key.page <= max_page)
        });
        self.worker
            .set_view_state(self.view_generation, min_page, max_page);
    }

    fn request_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
        self.pending_scale = None;
        self.missing_visible_page = None;
        self.status_line.clear();
        if self.label_query.is_some() {
            self.reset_label_matches(output)?;
            self.label_request_id = self.label_request_id.wrapping_add(1).max(1);
        }
        if self.viewer.set_window_title && self.title_document != Some(self.tab().document_id) {
            let title: String = self
                .tab()
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            execute!(output, crossterm::terminal::SetTitle(title))?;
            self.title_document = Some(self.tab().document_id);
        }
        let viewport = self.prepare_viewport(output)?;
        self.draw_tab_bar(output, viewport.columns)?;
        let key = self.render_key(viewport);
        self.desired_key = Some(key);
        let signature = RenderKey {
            page: 0,
            search_request_id: self.tab().search.active_highlight_id(),
            selected_link_ordinal: None,
            ..key
        };
        let invalidate_view = self.last_render_view != Some(signature);
        self.bump_generation(invalidate_view);
        self.last_render_view = Some(signature);
        self.sync_worker_view_window(key.page);
        self.performance_snapshot = None;

        if let Some(frame) = self.tab().cache.get(&key).cloned() {
            self.position_content_frame(&frame, viewport);
            self.draw_frame(&frame, viewport, output)?;
            self.prefetch_neighbors(key);
        } else {
            self.draw_status(output, viewport, "rendering")?;
            self.pending.insert(key, false);
            self.worker
                .render(RenderRequest {
                    key,
                    generation: self.generation,
                })
                .map_err(AppError::Renderer)?;
        }
        Ok(())
    }

    fn receive_render_error(
        &mut self,
        key: RenderKey,
        generation: u64,
        error: &str,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.pending.remove(&key);
        if generation < self.view_generation {
            return Ok(());
        }
        let mut active_failure = self.desired_key == Some(key);
        if self
            .pending_scale
            .is_some_and(|(waiting, _, _)| waiting == key)
        {
            self.pending_scale = None;
            active_failure = true;
        }
        if self.missing_visible_page == Some(key) {
            self.missing_visible_page = None;
            active_failure = true;
        }
        if key.document_id == self.tab().document_id && self.pending_vertical_scroll != 0 {
            self.pending_vertical_scroll = 0;
            self.smooth_scroll_remaining = 0;
            active_failure = true;
        }
        if self.desired_key == Some(key) {
            self.finish_forward(Some(format!("render failed: {error}")));
        }
        if active_failure {
            let viewport = self.viewport()?;
            self.draw_status(output, viewport, &format!("render failed: {error}"))?;
        }
        Ok(())
    }

    fn receive_frame(&mut self, frame: Frame, output: &mut impl Write) -> Result<(), AppError> {
        if frame.generation < self.view_generation {
            return Ok(());
        }
        let was_pending = self.pending.remove(&frame.key).is_some();
        let Some(index) = self.tab_index(frame.key.document_id) else {
            return Ok(());
        };
        let viewport = self.viewport()?;
        let tab = &self.session.tabs[index];
        let expected_key = tab.render_key(viewport, self.link_mode, None);
        if frame.key.page >= tab.page_count
            || frame.revision.pdf != tab.revision
            || !same_render_view(frame.key, expected_key)
            || frame.key.link_mode != self.link_mode
            || frame.key.link_highlight != expected_key.link_highlight
            || frame.key.search_highlight != expected_key.search_highlight
            || frame.key.search_request_id != tab.search.highlight_request_id(frame.key.page)
        {
            return Ok(());
        }
        if frame.flash.is_some()
            && !self.navigation.flash.as_ref().is_some_and(|flash| {
                flash.document_id == frame.key.document_id && flash.page == frame.key.page
            })
        {
            // ClearFlash may have arrived while this frame was rendering.
            if was_pending {
                self.pending.insert(frame.key, false);
                self.worker
                    .render(RenderRequest {
                        key: frame.key,
                        generation: self.generation,
                    })
                    .map_err(AppError::Renderer)?;
            }
            return Ok(());
        }
        let key = frame.key;
        let frame_generation = frame.generation;
        let frame = if frame.flash.is_none()
            && let Some(existing) = self.session.tabs[index]
                .cache
                .get(&key)
                .filter(|existing| existing.revision == frame.revision && existing.flash.is_none())
        {
            Arc::clone(existing)
        } else {
            Arc::new(frame)
        };
        let current_page = self.session.tabs[index].page;
        // Keep one on-demand neighbor even with prefetch disabled: continuous
        // scrolling may need its dimensions before it becomes visible.
        // Config validation bounds prefetch_pages at u32::MAX.
        let cache_radius = u32::try_from(self.viewer.prefetch_pages)
            .expect("validated prefetch_pages exceeds u32")
            .max(1)
            .max(
                self.pending_scale
                    .filter(|(wanted, _, _)| wanted.document_id == key.document_id)
                    .map_or(0, |(wanted, _, _)| wanted.page.abs_diff(current_page)),
            );
        let visible_end = self
            .visible_pages
            .last()
            .filter(|page| page.frame.key.document_id == key.document_id)
            .map_or(current_page, |page| page.frame.key.page)
            .max(current_page)
            .saturating_add(cache_radius);
        let selected_ordinal = self.render_key(viewport).selected_link_ordinal;
        let link_mode = self.link_mode;
        let tab = &mut self.session.tabs[index];
        tab.cache.insert(key, Arc::clone(&frame));
        let search = &tab.search;
        tab.cache.retain(|cached, _| {
            same_render_view(*cached, key)
                && cached.link_mode == link_mode
                && cached.link_highlight == key.link_highlight
                && cached.search_highlight == key.search_highlight
                && cached.search_request_id == search.highlight_request_id(cached.page)
                && cached.page >= current_page.saturating_sub(cache_radius)
                && cached.page <= visible_end
                && (cached.selected_link_ordinal.is_none()
                    || cached.selected_link_ordinal == selected_ordinal)
        });
        if let Some((waiting, fit, zoom)) = self.pending_scale
            && waiting == key
        {
            self.pending_scale = None;
            self.set_view_scale(fit, zoom, output)?;
            return Ok(());
        }
        if self.desired_key == Some(key) {
            self.position_content_frame(&frame, viewport);
        }

        // Center across page boundaries only when the active display is continuous.
        let flash_scroll = match self.navigation.flash.as_mut() {
            Some(flash)
                if flash.document_id == key.document_id
                    && flash.page == key.page
                    && flash.revision == frame.revision.pdf
                    && frame.flash.is_some() =>
            {
                if std::mem::take(&mut flash.positioning_pending) {
                    frame
                        .flash
                        .as_ref()
                        .and_then(|highlight| highlight.pixel_bounds)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((left, right, top, bottom)) = flash_scroll {
            self.tab_mut().scroll_x = horizontal_scroll_to_reveal(
                self.tab().scroll_x.max(0) as u32,
                frame.width,
                u32::from(viewport.pixel_width),
                left as f32,
                right as f32,
            ) as i64;
            let center = if self.viewer.center_forward_search {
                (i64::from(top) + i64::from(bottom)) / 2
            } else {
                i64::from(top)
            };
            self.tab_mut().pending_destination = None;
            self.tab_mut().scroll_y = 0;
            let target = center
                - if self.viewer.center_forward_search {
                    i64::from(viewport.pixel_height) / 2
                } else {
                    i64::from(viewport.pixel_height) * 8 / 100
                };
            if self.continuous_view() {
                self.pending_vertical_scroll = target;
            } else {
                let tab = self.tab_mut();
                tab.scroll_y = target.max(0) as u32;
                let updated = tab.view_position();
                tab.link_history.update_tip(updated);
            }
        }

        if key.document_id == self.tab().document_id && self.pending_vertical_scroll != 0 {
            if self.apply_vertical_scroll(viewport)? {
                if flash_scroll.is_some() {
                    let tab = self.tab_mut();
                    let updated = tab.view_position();
                    tab.link_history.update_tip(updated);
                }
                let rest = self.smooth_scroll_remaining;
                self.request_current(output)?;
                self.smooth_scroll_remaining = rest;
            }
            return Ok(());
        }
        if self.continuous_view()
            && self.desired_key != Some(key)
            && (self.missing_visible_page == Some(key)
                || self.visible_pages.iter().any(|page| page.frame.key == key))
        {
            self.redraw_current(output)?;
        }

        if self.desired_key == Some(key) {
            let current_key = self.render_key(viewport);
            if current_key == key {
                if was_pending && frame_generation < self.generation {
                    // Cancel the duplicate priority render now satisfied by an in-flight prefetch.
                    self.bump_generation(false);
                    if let Some((waiting, _, _)) = self.pending_scale {
                        self.request_visible_page(waiting)?;
                    }
                }
                self.draw_frame(&frame, viewport, output)?;
                if flash_scroll.is_some() {
                    let tab = self.tab_mut();
                    let updated = tab.view_position();
                    tab.link_history.update_tip(updated);
                }
                self.prefetch_neighbors(key);
                self.open_pending_link_picker(output)?;
            }
        }
        Ok(())
    }

    fn position_content_frame(&mut self, frame: &Frame, viewport: Viewport) {
        if self.tab().fit != FitMode::Content || !self.tab().center_content {
            return;
        }
        let (scroll_x, scroll_y) = content_scroll(frame, viewport);
        let tab = self.tab_mut();
        tab.scroll_x = scroll_x;
        tab.scroll_y = scroll_y;
        tab.center_content = false;
    }

    fn allocate_image_id(&mut self) -> u32 {
        let id = self.next_image_id;
        self.next_image_id = id.wrapping_add(1).max(1);
        id
    }

    fn draw_frame(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if let Some(pending) = self.navigation.forward.as_ref()
            && let Err(error) = pending.request.revision.check(&pending.request.pdf)
        {
            self.cancel_forward(&error.to_string())?;
            return Ok(());
        }
        synchronized_output(output, |output| {
            self.draw_frame_unsynchronized(frame, viewport, output)
        })?;
        if self.label_query.is_some() {
            self.refresh_visible_matches(output)?;
        }
        let submitted = self.navigation.flash.as_ref().and_then(|flash| {
            let matches = |rendered: &Frame| {
                rendered.key.document_id == flash.document_id
                    && rendered.key.page == flash.page
                    && rendered.revision.pdf == flash.revision
                    && rendered.flash.is_some()
            };
            if flash.positioning_pending || self.pending_vertical_scroll != 0 {
                return None;
            }
            if self.continuous_view() {
                self.visible_pages
                    .iter()
                    .find(|page| matches(&page.frame))
                    .and_then(|page| page.frame.flash.clone())
            } else {
                matches(frame).then(|| frame.flash.clone()).flatten()
            }
        });
        if let Some(highlight) = submitted {
            let flash = self.navigation.flash.as_mut().unwrap();
            flash.expires_at.get_or_insert_with(|| {
                Instant::now() + Duration::from_millis(self.viewer.flash_duration_ms)
            });
            let error = self
                .navigation
                .forward
                .as_ref()
                .and_then(|pending| pending.check_revision().err())
                .map(|error| error.to_string())
                .or_else(|| {
                    highlight
                        .error
                        .as_ref()
                        .map(|error| format!("word highlight failed: {error}"))
                });
            if self
                .navigation
                .forward
                .as_ref()
                .is_some_and(|pending| pending.stage == ForwardStage::AwaitingFrame)
            {
                if error.is_none() {
                    if let Some(warning) = self
                        .navigation
                        .forward
                        .as_mut()
                        .and_then(|pending| pending.request.warning.take())
                    {
                        let label =
                            if warning == "assuming saved source is unchanged since compilation" {
                                if highlight.word_precise {
                                    "forward search"
                                } else {
                                    "forward search (SyncTeX region; no unique source word)"
                                }
                            } else {
                                "warning"
                            };
                        self.draw_status(output, viewport, &format!("{label}: {warning}"))?;
                    } else if !highlight.word_precise {
                        self.draw_status(
                            output,
                            viewport,
                            "forward search: SyncTeX region; no unique source word",
                        )?;
                    }
                }
                self.finish_forward(error);
            }
        }
        Ok(())
    }

    fn draw_frame_unsynchronized(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let mut resolved_destination = false;
        if let Some(destination) = self.tab_mut().pending_destination.take()
            && destination.page == frame.key.page
        {
            resolved_destination = self
                .link_picker
                .as_ref()
                .is_none_or(|state| state.preview_origin.is_none());
            if let Some(left_ratio) = destination.left_ratio {
                let target_x = left_ratio * frame.width as f32;
                let base_x = if self.tab().fit == FitMode::Content {
                    content_scroll(frame, viewport).0
                } else {
                    self.tab().scroll_x
                };
                self.tab_mut().scroll_x = horizontal_scroll_to_reveal(
                    base_x.max(0) as u32,
                    frame.width,
                    u32::from(viewport.pixel_width),
                    target_x,
                    target_x,
                ) as i64;
            } else if self.tab().fit == FitMode::Content
                && let Some(pan_x) = destination.content_pan_x
            {
                self.tab_mut().scroll_x = content_scroll(frame, viewport).0 + pan_x;
            }
            if let Some(top_ratio) = destination.top_ratio {
                self.tab_mut().scroll_y = (top_ratio * frame.height as f32).round() as u32;
            }
        }
        if self.continuous_view() {
            let result = self.draw_continuous(frame, viewport, output);
            if resolved_destination {
                let tab = self.tab_mut();
                let updated = tab.view_position();
                tab.link_history.update_tip(updated);
            }
            return result;
        }
        self.retain_primary_image(output)?;
        let tab = self.tab();
        let placement = viewport.place(
            frame.width,
            frame.height,
            tab.scroll_x.max(0) as u32,
            tab.scroll_y,
        );
        self.tab_mut().scroll_x = i64::from(placement.scroll_x);
        self.tab_mut().scroll_y = placement.scroll_y;
        if resolved_destination {
            let tab = self.tab_mut();
            let updated = tab.view_position();
            tab.link_history.update_tip(updated);
        }
        let retained_id = self
            .visible_page
            .as_ref()
            .filter(|previous| Arc::ptr_eq(&previous.frame, frame))
            .map(|previous| previous.image_id);
        let image_id = retained_id.unwrap_or_else(|| self.allocate_image_id());
        let original = PositionedImage {
            left: placement.left,
            top: viewport.top,
            placement: Placement {
                image_id,
                columns: placement.columns,
                rows: placement.rows,
                offset_y: 0,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: placement.crop,
            },
        };
        let positioned = if self.link_picker.is_some() || self.search_picker.is_some() {
            let area = link_picker_area(viewport);
            let (preview, _) = link_picker_panes(area, self.link_picker_geometry);
            position_link_picker_image(
                LinkPickerImage::new(image_id, frame, placement, viewport),
                preview,
                self.link_picker_geometry.layout,
            )
        } else {
            original
        };

        self.prepare_image_canvas(output, viewport)?;
        execute!(output, MoveTo(positioned.left, positioned.top))?;
        let transfer_started = Instant::now();
        write_page_image(output, frame, positioned.placement, retained_id.is_some())?;
        let transfer_elapsed = transfer_started.elapsed();
        if let Some(previous) = self.visible_page.replace(VisiblePage {
            frame: Arc::clone(frame),
            placement,
            top: viewport.top,
            image_id,
        }) && previous.image_id != image_id
        {
            kitty::delete_image(output, previous.image_id)?;
        }
        self.submitted_key = Some(frame.key);
        let snapshot = PerformanceSnapshot::from_frame(frame, transfer_elapsed);
        self.performance_snapshot = Some(snapshot);
        let state = self
            .startup_warning
            .take()
            .unwrap_or_else(|| snapshot.status(self.show_performance, self.link_mode));
        if self.link_picker.is_some() {
            self.paint_link_picker(frame.key.page, viewport, output)?;
        } else if self.search_picker.is_some() {
            self.paint_search_picker(frame.key.page, viewport, output)?;
        }
        self.draw_status(output, viewport, &state)?;
        Ok(())
    }

    fn retain_primary_image(&mut self, output: &mut impl Write) -> io::Result<()> {
        for page in self.visible_pages.drain(..) {
            if Some(page.image_id) != self.visible_page.as_ref().map(|page| page.image_id) {
                kitty::delete_image(output, page.image_id)?;
            }
        }
        Ok(())
    }

    fn draw_continuous(
        &mut self,
        frame: &Arc<Frame>,
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let cell = viewport.cell_size().1;
        let max_y = if self.tab().page + 1 == self.tab().page_count {
            frame
                .height
                .saturating_sub(u32::from(viewport.pixel_height))
        } else {
            // The inter-page gap belongs to this page; the next page starts at
            // height + cell. This is the inclusive clamp for that half-open span.
            frame.height + cell - 1
        };
        self.tab_mut().scroll_y = self.tab().scroll_y.min(max_y);
        let old_pages = std::mem::take(&mut self.visible_pages);
        if old_pages.is_empty()
            && let Some(previous) = self.visible_page.take()
        {
            kitty::delete_image(output, previous.image_id)?;
        }
        // Kitty (image id, p=1) replaces the old placement, including its crop.
        // Only text/UI or geometry changes require clearing the canvas.
        if old_pages
            .first()
            .is_none_or(|old| old.frame.key.document_id != frame.key.document_id)
            || self.canvas_viewport != Some(viewport)
        {
            self.prepare_image_canvas(output, viewport)?;
        }
        let started = Instant::now();
        self.missing_visible_page = None;
        let mut page = self.tab().page;
        let mut y = -i64::from(self.tab().scroll_y);
        while y < i64::from(viewport.pixel_height) && page < self.tab().page_count {
            let key = self.page_key(page, viewport);
            let rendered = if let Some(rendered) = self.tab().cache.get(&key).cloned() {
                rendered
            } else {
                self.missing_visible_page.get_or_insert(key);
                self.request_visible_page(key)?;
                // Overlay expiry can evict a frame that is still on screen. Keep
                // its image until the replacement arrives, but only for this
                // document revision and exact render settings. Run placement
                // normally so scrolling still updates crops and removes pages.
                let Some(old) = old_pages.iter().find(|old| {
                    old.frame.key == key && old.frame.revision.pdf == self.tab().revision
                }) else {
                    break;
                };
                Arc::clone(&old.frame)
            };
            let offset = (-y).max(0) as u32;
            let top = y.max(0) as u32;
            let span = i64::from(rendered.height) + i64::from(cell);
            if let Some(placement) = viewport.place_continuous(
                rendered.width,
                rendered.height,
                content_horizontal_scroll(self.tab().scroll_x, frame, &rendered, viewport).clamp(
                    0,
                    i64::from(
                        rendered
                            .width
                            .saturating_sub(u32::from(viewport.pixel_width)),
                    ),
                ) as u32,
                offset,
                top,
            ) {
                let row = (top / cell) as u16;
                let retained_id = old_pages
                    .iter()
                    .find(|old| Arc::ptr_eq(&old.frame, &rendered))
                    .map(|old| old.image_id);
                let image_id = retained_id.unwrap_or_else(|| self.allocate_image_id());
                let kitty_placement = Placement {
                    image_id,
                    columns: 0,
                    rows: 0,
                    offset_y: placement.offset_y,
                    z_index: PAGE_IMAGE_Z_INDEX,
                    crop: placement.crop,
                };
                execute!(output, MoveTo(placement.left, viewport.top + row))?;
                write_page_image(output, &rendered, kitty_placement, retained_id.is_some())?;
                self.visible_pages.push(VisiblePage {
                    frame: rendered,
                    placement,
                    top: viewport.top + row,
                    image_id,
                });
            }
            y += span; // Keep a one-cell-high gap, without rounding page heights.
            page += 1;
        }
        // A narrow preceding page must not hide a wider visible jump or zoom target.
        // Each placement clamps independently, including retained overlay frames.
        // An uncached visible page may be wider; retain the requested offset until it renders.
        if frame.key.fit != FitMode::Content
            && self.missing_visible_page.is_none()
            && let Some(scroll_x) = self
                .visible_pages
                .iter()
                .map(|page| page.placement.scroll_x)
                .max()
        {
            self.tab_mut().scroll_x = i64::from(scroll_x);
        }
        for old in old_pages {
            if !self
                .visible_pages
                .iter()
                .any(|page| page.image_id == old.image_id)
            {
                kitty::delete_image(output, old.image_id)?;
            }
        }
        self.visible_page = self.visible_pages.first().cloned();
        self.submitted_key = Some(frame.key);
        let snapshot = PerformanceSnapshot::from_frame(frame, started.elapsed());
        self.performance_snapshot = Some(snapshot);
        let state = self
            .startup_warning
            .take()
            .unwrap_or_else(|| snapshot.status(self.show_performance, self.link_mode));
        self.draw_status(output, viewport, &state)?;
        Ok(())
    }

    fn page_at_mouse(
        &self,
        mouse: MouseEvent,
        viewport: Viewport,
    ) -> Option<(Arc<Frame>, ImagePlacement, u16)> {
        if self.continuous_view() {
            return self
                .visible_pages
                .iter()
                .filter(|page| {
                    same_render_view(page.frame.key, self.render_key(viewport))
                        && page.frame.revision.pdf == self.tab().revision
                })
                .find(|page| {
                    mouse.row.checked_sub(page.top).is_some_and(|row| {
                        page.placement
                            .source_cell(mouse.column, row, page.frame.width, page.frame.height)
                            .is_some()
                    })
                })
                .map(|page| (Arc::clone(&page.frame), page.placement, page.top));
        }
        let page = self.current_visible_page(viewport)?;
        let frame = Arc::clone(&page.frame);
        if (self.link_picker.is_none() && self.search_picker.is_none())
            || self.link_picker_geometry.layout == LinkPickerLayout::Floating
        {
            return Some((frame, page.placement, page.top));
        }
        let (preview, _) = link_picker_panes(link_picker_area(viewport), self.link_picker_geometry);
        let positioned = position_link_picker_image(
            self.picker_image(viewport)?,
            preview,
            self.link_picker_geometry.layout,
        );
        Some((
            frame,
            ImagePlacement {
                left: positioned.left,
                columns: positioned.placement.columns,
                rows: positioned.placement.rows,
                offset_y: 0,
                native_cell: None,
                crop: positioned.placement.crop,
                ..page.placement
            },
            positioned.top,
        ))
    }

    fn prepare_image_canvas(
        &mut self,
        output: &mut impl Write,
        viewport: Viewport,
    ) -> io::Result<()> {
        clear_image_canvas(output, viewport)?;
        self.canvas_viewport = Some(viewport);
        self.status_line.clear();
        Ok(())
    }

    fn draw_status(
        &mut self,
        output: &mut impl Write,
        viewport: Viewport,
        state: &str,
    ) -> io::Result<()> {
        let theme = self.theme;
        let tab = self.tab();
        let mut mode = String::new();
        if tab.fit != FitMode::Page {
            mode.push_str(tab.fit.label());
        }
        if tab.invert {
            if !mode.is_empty() {
                mode.push(' ');
            }
            mode.push_str("dark");
        }
        if tab.zoom != ZOOM_DEFAULT {
            if !mode.is_empty() {
                mode.push(' ');
            }
            mode.push_str(&format!("{}%", tab.zoom));
        }
        if !mode.is_empty() {
            mode.push_str("  ");
        }
        let search_status = tab.search.status_label(tab.page);
        let link_status = self.link_mode.then_some("  click/enter: open  esc: close");
        let page = tab.page + 1;
        let page_count = tab.page_count;
        self.status_buffer.clear();
        // Leave the last cell unused even if a terminal has re-enabled wrapping.
        let columns = Cell::new(usize::from(viewport.columns.saturating_sub(1)));
        execute!(
            self.status_buffer,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            SetForegroundColor(theme.fg),
            Clear(ClearType::CurrentLine),
            Print(StatusText(" ", &columns)),
            SetForegroundColor(theme.blue),
            Print(StatusText(page, &columns)),
            SetForegroundColor(theme.fg_dark),
            Print(StatusText("/", &columns)),
            SetForegroundColor(theme.magenta),
            Print(StatusText(page_count, &columns)),
            Print(StatusText("  ", &columns)),
            SetForegroundColor(theme.yellow),
            Print(StatusText(&mode, &columns)),
            SetForegroundColor(theme.green),
            Print(StatusText(state, &columns)),
            SetForegroundColor(theme.magenta),
            Print(StatusText(
                search_status.as_deref().unwrap_or_default(),
                &columns
            )),
            SetForegroundColor(theme.cyan),
            Print(StatusText(link_status.unwrap_or_default(), &columns)),
            SetForegroundColor(theme.comment),
            Print(StatusText("  ?: help", &columns)),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        if self.status_buffer != self.status_line {
            output.write_all(&self.status_buffer)?;
            output.flush()?;
            std::mem::swap(&mut self.status_line, &mut self.status_buffer);
        }
        Ok(())
    }

    fn prefetch_neighbors(&mut self, key: RenderKey) {
        self.sync_worker_view_window(key.page);
        let page_count = self.tab().page_count;
        let max_distance = u32::try_from(self.viewer.prefetch_pages)
            .expect("validated prefetch_pages exceeds u32")
            .min(page_count.saturating_sub(1));
        for page in (1..=max_distance)
            .flat_map(|distance| {
                [
                    key.page.checked_sub(distance),
                    key.page.checked_add(distance),
                ]
            })
            .flatten()
            .filter(|page| *page < page_count)
        {
            let neighbor = RenderKey {
                page,
                search_request_id: self.tab().search.highlight_request_id(page),
                selected_link_ordinal: None,
                ..key
            };
            let cached = self.tab().cache.contains_key(&neighbor);
            if !cached && !self.pending.contains_key(&neighbor) {
                self.pending.insert(neighbor, true);
                self.worker.prefetch(RenderRequest {
                    key: neighbor,
                    generation: self.generation,
                });
            }
        }
    }

    fn clear_viewer(&mut self, output: &mut impl Write) -> io::Result<()> {
        let theme = self.theme;
        kitty::delete_all(output)?;
        self.visible_page = None;
        self.submitted_key = None;
        self.visible_pages.clear();
        self.missing_visible_page = None;
        self.canvas_viewport = None;
        self.status_line.clear();
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
        self.last_status_row = None;
        execute!(
            output,
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg),
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        output.flush()
    }

    fn prepare_viewport(&mut self, output: &mut impl Write) -> io::Result<Viewport> {
        let viewport = self.viewport()?;
        if let Some(row) = stale_status_row(self.last_status_row, viewport.status_row) {
            let theme = self.theme;
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

    fn draw_tab_bar(&self, output: &mut impl Write, columns: u16) -> io::Result<()> {
        if self.session.tabs.len() < 2 {
            return Ok(());
        }
        let theme = self.theme;
        let columns = usize::from(columns);
        execute!(
            output,
            MoveTo(0, 0),
            SetBackgroundColor(theme.bg_dark1),
            SetForegroundColor(theme.dark3),
            Clear(ClearType::CurrentLine)
        )?;
        let mut used = 0;
        for (index, tab) in self.session.tabs.iter().enumerate() {
            let name = tab
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| tab.path.to_string_lossy());
            let label = format!(" {}:{} ", index + 1, name);
            let available = columns.saturating_sub(used);
            if available == 0 {
                break;
            }
            let label = truncate_right(&label, available);
            if label.is_empty() {
                break;
            }
            if index == self.session.active_tab {
                execute!(
                    output,
                    SetBackgroundColor(theme.blue),
                    SetForegroundColor(theme.bg_dark),
                    SetAttribute(Attribute::Bold),
                    Print(&label),
                    SetAttribute(Attribute::NormalIntensity)
                )?;
            } else {
                execute!(
                    output,
                    SetBackgroundColor(theme.bg),
                    SetForegroundColor(theme.dark3),
                    Print(&label)
                )?;
            }
            used += Line::raw(label.as_str()).width();
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
        self.last_render_view = None;
        self.pending.clear();
        self.pending_vertical_scroll = 0;
        self.smooth_scroll_remaining = 0;
    }

    fn continuous_view(&self) -> bool {
        self.viewer.continuous_scroll && self.link_picker.is_none() && self.search_picker.is_none()
    }

    fn viewport(&self) -> io::Result<Viewport> {
        #[cfg(test)]
        if let Some(viewport) = self.canvas_viewport {
            return Ok(viewport);
        }
        Viewport::detect(u16::from(self.session.tabs.len() > 1))
    }

    fn render_key(&self, viewport: Viewport) -> RenderKey {
        let selected_link_ordinal = self
            .link_picker
            .as_ref()
            .and_then(|state| state.selection_key)
            .and_then(|(page, ordinal)| (page == self.tab().page).then_some(ordinal));
        self.tab()
            .render_key(viewport, self.link_mode, selected_link_ordinal)
    }

    fn begin_inverse_search(
        &mut self,
        mouse: MouseEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let Some((frame, placement, image_top)) = self.page_at_mouse(mouse, viewport) else {
            return Ok(());
        };
        let Some(pixel) = source_pixel_at_cell(placement, image_top, &frame, mouse) else {
            return Ok(());
        };
        self.begin_inverse_at(frame.key, pixel, viewport, output)
    }
    fn begin_inverse_at(
        &mut self,
        key: RenderKey,
        pixel: (u32, u32),
        viewport: Viewport,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let frame = self
            .visible_page
            .as_ref()
            .filter(|page| page.frame.key == key)
            .or_else(|| self.visible_pages.iter().find(|page| page.frame.key == key))
            .map(|page| Arc::clone(&page.frame))
            .or_else(|| self.tab().cache.get(&key).cloned());
        let Some(frame) = frame else {
            self.draw_status(output, viewport, "label target is no longer visible")?;
            return Ok(());
        };
        let request_id = self.navigation.next_request_id;
        self.navigation.next_request_id = request_id.wrapping_add(1);
        self.navigation.inverse = Some(PendingInverse {
            revision: frame.revision,
            operation: crate::process::Operation::default(),
            document_id: key.document_id,
            page: key.page,
            request_id,
        });
        self.worker
            .page_point(frame.revision, request_id, pixel.0, pixel.1, key);
        self.draw_status(output, viewport, "inverse search: resolving location...")?;
        Ok(())
    }

    fn receive_page_point(
        &mut self,
        click: SynctexClick,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(pending) = self.navigation.inverse.take() else {
            return Ok(());
        };
        if pending.document_id != click.document_id
            || pending.page != click.page
            || pending.request_id != click.request_id
        {
            self.navigation.inverse = Some(pending);
            return Ok(());
        }
        let result = (|| -> io::Result<()> {
            pending.operation.check()?;
            if pending.revision != click.revision {
                return Err(io::Error::other("stale hit-test revision"));
            }
            let resolved = click.result.map_err(io::Error::other)?;
            let index = self
                .tab_index(click.document_id)
                .ok_or_else(|| io::Error::other("clicked document closed"))?;
            self.navigation.worker.submit(InverseTask {
                request_id: pending.request_id,
                path: self.session.tabs[index].path.clone(),
                revision: pending.revision,
                page: pending.page,
                click: resolved,
                word_precision: self.viewer.word_precision,
                radius: self.viewer.source_context_lines as u32,
                editor: self.editor.clone(),
                inverse_search: self
                    .navigation
                    .source_maps
                    .get(&self.session.tabs[index].path)
                    .cloned(),
                best_guess: self
                    .navigation
                    .best_guess
                    .contains(&self.session.tabs[index].path),
                operation: pending.operation.clone(),
            })
        })();
        match result {
            Ok(()) => self.navigation.inverse = Some(pending),
            Err(error) => self.draw_status(
                output,
                self.viewport()?,
                &format!("inverse search: {error}"),
            )?,
        }
        Ok(())
    }

    fn poll_inverse_search(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        while let Ok(reply) = self.navigation.worker.replies.try_recv() {
            if !self
                .navigation
                .inverse
                .as_ref()
                .is_some_and(|pending| pending.request_id == reply.request_id)
            {
                continue;
            }
            self.navigation.inverse.take();
            let viewport = self.viewport()?;
            match reply.result {
                Err(error) => {
                    self.draw_status(output, viewport, &format!("inverse search: {error}"))?
                }
                Ok(result) => {
                    let target = result.location;
                    let clipboard_only = matches!(self.editor, crate::editor::Editor::None);
                    if clipboard_only {
                        write_clipboard_osc52(
                            output,
                            &format!("{}:{}:{}", target.file, target.line, target.byte_column),
                        )?;
                    }
                    let shown = target.file.rsplit('/').next().unwrap_or(&target.file);
                    let mut warning = result.warning;
                    if let Some(warning) = &mut warning {
                        for assumption in [
                            "assuming saved source is unchanged since compilation",
                            "best-guess navigation: assuming saved source is unchanged since compilation",
                        ] {
                            if let Some(start) = warning.find(assumption) {
                                let end = start + assumption.len();
                                if (start == 0 || warning[..start].ends_with("; "))
                                    && (end == warning.len() || warning[end..].starts_with("; "))
                                {
                                    let range = if end < warning.len() {
                                        start..end + 2
                                    } else {
                                        start.saturating_sub(2)..end
                                    };
                                    warning.replace_range(range, "");
                                }
                            }
                        }
                    }
                    let detail = warning
                        .as_deref()
                        .filter(|warning| !warning.is_empty())
                        .unwrap_or(match (target.precise, clipboard_only) {
                            (true, true) => "word, copied",
                            (false, true) => "line only, copied",
                            (true, false) => "word",
                            (false, false) => "line only",
                        });
                    self.draw_status(
                        output,
                        viewport,
                        &format!(
                            "inverse search: {shown}:{}:{} ({detail})",
                            target.line,
                            target.byte_column + 1
                        ),
                    )?;
                }
            }
        }
        if let Some(error) = self
            .navigation
            .inverse
            .as_ref()
            .and_then(|p| p.operation.check().err())
        {
            self.navigation.inverse.take();
            self.draw_status(
                output,
                self.viewport()?,
                &format!("inverse search: {error}"),
            )?;
        }
        Ok(())
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

fn source_pixel_at_cell(
    placement: ImagePlacement,
    image_top: u16,
    frame: &Frame,
    mouse: MouseEvent,
) -> Option<(u32, u32)> {
    let cell = placement.source_cell(
        mouse.column,
        mouse.row.checked_sub(image_top)?,
        frame.width,
        frame.height,
    )?;
    Some((cell.x + cell.width / 2, cell.y + cell.height / 2))
}

fn link_at_cell(
    links: &[PageLink],
    placement: ImagePlacement,
    image_width: u32,
    image_height: u32,
    viewport_top: u16,
    column: u16,
    row: u16,
) -> Option<LinkTarget> {
    let cell = placement.source_cell(
        column,
        row.checked_sub(viewport_top)?,
        image_width,
        image_height,
    )?;
    let (x0, x1, y0, y1) = (cell.x, cell.x + cell.width, cell.y, cell.y + cell.height);
    let cell_center_x = u64::from(x0) + u64::from(x1);
    let cell_center_y = u64::from(y0) + u64::from(y1);

    links
        .iter()
        .filter(|link| {
            link.rect.left < x1
                && link.rect.right > x0
                && link.rect.top < y1
                && link.rect.bottom > y0
        })
        .min_by_key(|link| {
            let link_center_x = u64::from(link.rect.left) + u64::from(link.rect.right);
            let link_center_y = u64::from(link.rect.top) + u64::from(link.rect.bottom);
            cell_center_x
                .abs_diff(link_center_x)
                .saturating_pow(2)
                .saturating_add(cell_center_y.abs_diff(link_center_y).saturating_pow(2))
        })
        .map(|link| link.target.clone())
}

/// Keep a visible horizontal target stationary; otherwise reveal it with the least movement.
fn horizontal_scroll_to_reveal(
    current: u32,
    page_width: u32,
    viewport_width: u32,
    target_left: f32,
    target_right: f32,
) -> u32 {
    let left = (target_left.min(target_right).floor() as u32).min(page_width.saturating_sub(1));
    let right = (target_left.max(target_right).ceil() as u32)
        .max(left.saturating_add(1))
        .min(page_width);
    let right_aligned = right.saturating_sub(viewport_width);
    let min_scroll = left.min(right_aligned);
    let max_scroll = left.max(right_aligned);
    let page_max = page_width.saturating_sub(viewport_width);
    // Continuous view can share an offset larger than this page can crop.
    // Judge visibility using this page's actual crop, without moving its wider neighbor.
    if (min_scroll..=max_scroll).contains(&current.min(page_max)) {
        current
    } else {
        current.clamp(min_scroll, max_scroll).min(page_max)
    }
}

fn badge_glyph_size(text_height: u32) -> u32 {
    // Keep short labels readable at small on-page word sizes.
    text_height.saturating_sub(2).max(8)
}

fn same_render_view(a: RenderKey, b: RenderKey) -> bool {
    a.document_id == b.document_id
        && a.width == b.width
        && a.height == b.height
        && a.zoom == b.zoom
        && a.fit == b.fit
        && a.invert == b.invert
        && a.dark_mode_style == b.dark_mode_style
}

struct ProjectedMatch {
    source: crate::pdf::PixelRect,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn project_match_rect(
    rect: &crate::pdf::PixelRect,
    placement: ImagePlacement,
    top: u16,
    frame_size: (u32, u32),
    viewport: Viewport,
    cell_size: (u32, u32),
) -> Option<ProjectedMatch> {
    let (frame_width, frame_height) = frame_size;
    let (cell_width, cell_height) = cell_size;
    let crop = placement.crop.unwrap_or(crate::kitty::Crop {
        x: 0,
        y: 0,
        width: frame_width,
        height: frame_height,
    });
    if crop.width == 0 || crop.height == 0 {
        return None;
    }
    let crop_right = crop.x.saturating_add(crop.width).min(frame_width);
    let crop_bottom = crop.y.saturating_add(crop.height).min(frame_height);
    let x0 = rect.left.max(crop.x);
    let x1 = rect.right.min(crop_right);
    let y0 = rect.top.max(crop.y);
    let y1 = rect.bottom.min(crop_bottom);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let native = placement.native_cell.is_some();
    let source_width = x1 - x0;
    let source_height = y1 - y0;
    let x = u32::from(placement.left) * cell_width
        + if native {
            x0 - crop.x
        } else {
            (x0 - crop.x) * u32::from(placement.columns) * cell_width / crop.width
        };
    let page_y = u32::from(top.saturating_sub(viewport.top)) * cell_height;
    let y = if native {
        page_y + placement.offset_y + (y0 - crop.y)
    } else {
        page_y + (y0 - crop.y) * u32::from(placement.rows) * cell_height / crop.height
    };
    let width = if native {
        source_width
    } else {
        source_width * u32::from(placement.columns) * cell_width / crop.width
    };
    let height = if native {
        source_height
    } else {
        source_height * u32::from(placement.rows) * cell_height / crop.height
    };
    if width == 0
        || height == 0
        || x >= u32::from(viewport.pixel_width)
        || y >= u32::from(viewport.pixel_height)
    {
        return None;
    }
    Some(ProjectedMatch {
        source: crate::pdf::PixelRect {
            left: x0,
            right: x1,
            top: y0,
            bottom: y1,
        },
        x,
        y,
        width,
        height,
    })
}

fn visible_match_points(
    rects: &[crate::pdf::PixelRect],
    placement: ImagePlacement,
    top: u16,
    frame_width: u32,
    frame_height: u32,
    viewport: Viewport,
    cell_size: (u32, u32),
) -> Option<((u32, u32), (u32, u32))> {
    let mut hit = None;
    let mut anchor = None;
    for rect in rects {
        let Some(projected) = project_match_rect(
            rect,
            placement,
            top,
            (frame_width, frame_height),
            viewport,
            cell_size,
        ) else {
            continue;
        };
        let ProjectedMatch {
            source,
            x,
            y,
            width,
            height,
        } = projected;
        let (x0, y0) = (source.left, source.top);
        let source_width = source.right - source.left;
        let source_height = source.bottom - source.top;
        let visible_right = x.saturating_add(width).min(u32::from(viewport.pixel_width));
        let visible_bottom = y
            .saturating_add(height)
            .min(u32::from(viewport.pixel_height));
        let screen_x = x + (visible_right - x) / 2;
        let screen_y = y + (visible_bottom - y) / 2;
        let point = (
            x0 + (u64::from(screen_x - x) * u64::from(source_width) / u64::from(width)) as u32,
            y0 + (u64::from(screen_y - y) * u64::from(source_height) / u64::from(height)) as u32,
        );
        hit.get_or_insert(point);
        anchor = Some(point);
    }
    hit.zip(anchor)
}

/// Clip all styled status fragments to one row without emitting text controls.
struct StatusText<'a, T>(T, &'a Cell<usize>);

impl<T: fmt::Display> fmt::Display for StatusText<'_, T> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;
        write!(
            StatusWriter {
                output,
                columns: self.1
            },
            "{}",
            self.0
        )
    }
}

struct StatusWriter<'a, 'b> {
    output: &'a mut fmt::Formatter<'b>,
    columns: &'a Cell<usize>,
}

impl fmt::Write for StatusWriter<'_, '_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for part in value.split_inclusive(char::is_control) {
            let span = Span::raw(part);
            for grapheme in span.styled_graphemes(Style::default()) {
                let width = Span::raw(grapheme.symbol).width();
                if self.columns.get() == 0 || width > self.columns.get() {
                    self.columns.set(0);
                    return Ok(());
                }
                self.output.write_str(grapheme.symbol)?;
                self.columns.set(self.columns.get() - width);
            }
            if part.ends_with(char::is_control) && self.columns.get() > 0 {
                self.output.write_str(" ")?;
                self.columns.set(self.columns.get() - 1);
            }
        }
        Ok(())
    }
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

/// Steps the zoom percentage by one increment, snapping to the `ZOOM_STEP`
/// grid and clamping to the supported range.
fn stepped_zoom(current: u16, zoom_in: bool) -> u16 {
    let next = if zoom_in {
        current.saturating_add(ZOOM_STEP)
    } else {
        current.saturating_sub(ZOOM_STEP)
    };
    next.clamp(ZOOM_MIN, ZOOM_MAX)
}

fn scale_zoom(value: u32, old_zoom: u16, new_zoom: u16) -> u32 {
    ((u64::from(value) * u64::from(new_zoom) + u64::from(old_zoom) / 2) / u64::from(old_zoom))
        .min(u64::from(u32::MAX)) as u32
}

fn scale_pixels(value: u32, old_size: u32, new_size: u32) -> u32 {
    let old_size = u64::from(old_size.max(1));
    ((u64::from(value) * u64::from(new_size) + old_size / 2) / old_size).min(u64::from(u32::MAX))
        as u32
}

fn content_axis_scroll(start: f32, end: f32, size: u32, visible: u16) -> u32 {
    (((start + end) * 0.5 * size as f32 - f32::from(visible) * 0.5)
        .round()
        .max(0.0) as u32)
        .min(size.saturating_sub(u32::from(visible)))
}

fn content_scroll(frame: &Frame, viewport: Viewport) -> (i64, u32) {
    let [left, top, right, bottom] = frame.content_bounds;
    (
        i64::from(content_axis_scroll(
            left,
            right,
            frame.width,
            viewport.pixel_width,
        )),
        content_axis_scroll(top, bottom, frame.height, viewport.pixel_height),
    )
}

// Keep content-relative pan signed until placement: a narrower first visible
// page must not clamp the horizontal anchor of a wider following page.
fn content_horizontal_scroll(scroll_x: i64, from: &Frame, to: &Frame, viewport: Viewport) -> i64 {
    if from.key.fit == FitMode::Content {
        scroll_x - content_scroll(from, viewport).0 + content_scroll(to, viewport).0
    } else {
        scroll_x
    }
}

fn fitted_page_size(
    viewport: Viewport,
    width: f32,
    height: f32,
    content: [f32; 4],
    fit: FitMode,
    zoom: u16,
) -> (u32, u32) {
    let target_width = (u32::from(viewport.pixel_width) * u32::from(zoom) / 100).max(1);
    let target_height = (u32::from(viewport.pixel_height) * u32::from(zoom) / 100).max(1);
    // Match PDFium's f32 scaling and final rounding, using the original PDF
    // dimensions: reconstructing aspect ratio from a raster amplifies rounding.
    fit.raster_size(width, height, content, target_width, target_height)
}

/// Keep the PDF point under the viewport center fixed, even if it belongs to
/// the next visible page. Page gaps stay one terminal row rather than scaling.
/// Geometry includes an optional content-centered horizontal origin. Rebase
/// between these origins when the center page and first visible page differ.
fn centered_scaled_view(
    viewport: Viewport,
    mut page: u32,
    page_count: u32,
    (scroll_x, scroll_y): (i64, u32),
    scaled_size: impl Fn(u32, u32, u32) -> Option<(u32, u32, Option<u32>)>,
    continuous: bool,
    page_size: impl Fn(u32) -> Option<(u32, u32, Option<u32>)>,
) -> Result<(u32, i64, u32), u32> {
    let width = u32::from(viewport.pixel_width);
    let height = u32::from(viewport.pixel_height);
    let half_width = i64::from(width / 2);
    let half_height = i64::from(height / 2);
    let (cell_width, gap) = viewport.cell_size();
    let mut center_y = i64::from(scroll_y) + half_height;
    let mut geometry = page_size(page).ok_or(page)?;
    let initial_origin = geometry.2.unwrap_or(0);
    if continuous {
        while page + 1 < page_count {
            let span = i64::from(geometry.1) + i64::from(gap);
            if center_y < span {
                break;
            }
            center_y -= span;
            page += 1;
            geometry = page_size(page).ok_or(page)?;
        }
    }
    let (page_width, page_height, origin) = geometry;
    let scroll_x =
        (scroll_x - i64::from(initial_origin) + i64::from(origin.unwrap_or(0))).max(0) as u32;
    let (new_width, new_height, new_origin) =
        scaled_size(page, page_width, page_height).ok_or(page)?;
    let center_x = if page_width < width {
        // Narrow images are centered by whole terminal cells; leftover pixels
        // stay on the right, not symmetrically around the image midpoint.
        let left = u32::from(viewport.place(page_width, page_height, 0, 0).left) * cell_width;
        (width / 2).saturating_sub(left).min(page_width)
    } else {
        scroll_x
            .min(page_width.saturating_sub(width))
            .saturating_add(width / 2)
            .min(page_width)
    };
    let x = (i64::from(scale_pixels(center_x, page_width, new_width)) - half_width)
        .clamp(0, i64::from(new_width.saturating_sub(width))) as u32;
    if center_y >= i64::from(page_height) && (!continuous || page + 1 == page_count) {
        center_y = i64::from(page_height / 2);
    }
    let mut y = i64::from(scale_pixels(
        center_y.min(i64::from(page_height)) as u32,
        page_height,
        new_height,
    )) - half_height;
    if continuous {
        while y < 0 && page > 0 {
            page -= 1;
            let (previous_width, previous_height, _) = page_size(page).ok_or(page)?;
            y += i64::from(
                scaled_size(page, previous_width, previous_height)
                    .ok_or(page)?
                    .1,
            ) + i64::from(gap);
        }
        while page + 1 < page_count {
            let (current_width, current_height, _) = page_size(page).ok_or(page)?;
            let span = i64::from(
                scaled_size(page, current_width, current_height)
                    .ok_or(page)?
                    .1,
            ) + i64::from(gap);
            if y < span {
                break;
            }
            y -= span;
            page += 1;
        }
    }
    let (top_page_width, top_page_height, _) = page_size(page).ok_or(page)?;
    let (_, scaled_height, top_origin) =
        scaled_size(page, top_page_width, top_page_height).ok_or(page)?;
    let x = if let (Some(new_origin), Some(top_origin)) = (new_origin, top_origin) {
        i64::from(x) - i64::from(new_origin) + i64::from(top_origin)
    } else {
        i64::from(x)
    };
    let max_y = if continuous && page + 1 < page_count {
        scaled_height.saturating_add(gap - 1)
    } else {
        scaled_height.saturating_sub(height)
    };
    Ok((page, x, y.clamp(0, i64::from(max_y)) as u32))
}

fn numbered_tab_index(key: KeyEvent) -> Option<usize> {
    let digit = match key.code {
        KeyCode::Char(digit) if ('1'..='9').contains(&digit) => digit,
        _ => return None,
    };
    key.modifiers
        .contains(KeyModifiers::ALT)
        .then(|| usize::from(digit as u8 - b'1'))
}

#[path = "picker.rs"]
mod picker;
use picker::*;

#[cfg(test)]
mod tests {
    #[test]
    fn truncation_ignores_embedded_line_breaks_like_line_width() {
        assert_eq!(super::truncate_right("ab\ncdef", 5), "ab\ncd…");
        assert_eq!(super::truncate_left("ab\ncdef", 5), "…\ncdef");
        assert_eq!(super::truncate_right("ab\r\ncdef", 5), "ab\r\ncd…");
        assert_eq!(super::truncate_right("ab\ncd", 4), "ab\ncd");
    }

    #[test]
    fn horizontal_navigation_only_scrolls_to_reveal_hidden_targets() {
        let scroll = |current, left, right| {
            super::horizontal_scroll_to_reveal(current, 2000, 800, left, right)
        };
        assert_eq!(scroll(300, 400.0, 500.0), 300);
        assert_eq!(scroll(300, 300.0, 1100.0), 300);
        assert_eq!(scroll(300, 250.0, 400.0), 250);
        assert_eq!(scroll(300, 1000.0, 1150.0), 350);
        assert_eq!(scroll(300, 1100.0, 1100.0), 301);
        assert_eq!(scroll(300, 400.0, 400.0), 300);
        assert_eq!(scroll(300, 100.0, 1400.0), 300);
        assert_eq!(scroll(0, 100.0, 1400.0), 100);
        assert_eq!(scroll(900, 100.0, 1400.0), 600);
        assert_eq!(scroll(300, 2000.0, 2000.0), 1200);
        // Shared continuous scroll can exceed this narrow page's own crop.
        // Its x=50 target is visible at the actual per-page offset 20.
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 120, 100, 50.0, 51.0),
            100,
        );
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 120, 100, 0.0, 10.0),
            0,
        );
        assert_eq!(
            super::horizontal_scroll_to_reveal(100, 60, 100, 30.0, 35.0),
            100,
        );
    }

    use super::{
        BrowserState, FILE_STABLE_FOR, FileFingerprint, FileWatcher, LinkIndexProgress,
        LinkPickerDocument, LinkPickerFocus, LinkPickerGeometry, LinkPickerImage, LinkPickerState,
        ListEdge, PerformanceSnapshot, PositionedImage, SearchPickerState, SearchState,
        ZOOM_DEFAULT, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, apply_picker_navigation, centered_scaled_view,
        clear_image_canvas, clear_picker, clear_picker_filter, cycled_tab_index, draw_link_picker,
        draw_picker, draw_search_picker, draw_theme_picker, filter_document_links, filter_outline,
        filter_theme_indices, link_at_cell, link_picker_focus_for_key, link_picker_label,
        link_picker_link_at_position, link_picker_list_area, link_picker_panes,
        link_picker_visible_height, next_link_picker_layout, numbered_tab_index,
        outline_start_index, picker_color, picker_navigation_index, picker_rect,
        render_timing_status, restore_link_picker_split, search_target_page,
        show_link_picker_split, stale_status_row, stepped_zoom, synchronized_output,
        update_link_number_selection, write_clipboard_osc52,
    };
    use crate::config::LinkPickerLayout;
    use crate::kitty::Placement;
    use crate::pdf::{
        DocumentLink, LinkTarget, OutlineItem, PageLink, PageLinkRect, SearchPageMatch,
    };
    use crate::terminal::{ImagePlacement, Viewport};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::cell::Cell;
    use std::fs;
    use std::io::{self, Write};
    use std::time::Instant;

    fn continuous_frame(
        key: crate::pdf::RenderKey,
        revision: crate::synctex::DocumentRevision,
        highlighted: bool,
    ) -> std::sync::Arc<crate::pdf::Frame> {
        std::sync::Arc::new(crate::pdf::Frame {
            key,
            revision,
            width: 80,
            height: 240,
            page_width_pt: 80.0,
            page_height_pt: 240.0,
            content_bounds: [0.0, 0.0, 1.0, 1.0],
            compressed_rgba: crate::kitty::compress_rgba(&[255; 80 * 240 * 3]).unwrap(),
            render_elapsed: std::time::Duration::ZERO,
            dark_mode_elapsed: None,
            highlight_elapsed: None,
            compression_elapsed: std::time::Duration::ZERO,
            generation: 0,
            links: Vec::new(),
            flash: highlighted.then_some(crate::pdf::ForwardHighlight {
                rect: crate::pdf::SearchRect {
                    bottom: 10.0,
                    left: 10.0,
                    top: 20.0,
                    right: 20.0,
                },
                pixel_bounds: Some((10, 20, 220, 230)),
                word_precise: true,
                error: None,
            }),
        })
    }

    fn continuous_app() -> (super::App, Viewport, tempfile::NamedTempFile) {
        let _native = crate::pdf::pdfium_test_lock();
        let file = tempfile::NamedTempFile::new().unwrap();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        // These draw tests supply completed frames and an already-pending
        // replacement. A stopped worker keeps them independent of PDFium.
        let worker = crate::pdf::RenderWorker::spawn(
            super::INITIAL_DOCUMENT_ID,
            file.path().to_owned(),
            Some(file.path().join("missing-pdfium")),
        );
        assert!(worker.wait_until_ready().is_err());
        let config = toml::from_str("").unwrap();
        let defaults = super::AppDefaults::from_config(&config, None);
        let mut app = super::App::new(
            worker,
            2,
            0,
            file.path().to_owned(),
            FileWatcher::new(file.path()).unwrap(),
            (Vec::new(), revision.pdf),
            defaults,
        );
        let viewport = Viewport {
            columns: 8,
            rows: 20,
            pixel_width: 80,
            pixel_height: 200,
            top: 1,
            status_row: 21,
        };
        app.tab_mut().scroll_y = 100;
        for page in 0..2 {
            let key = app.page_key(page, viewport);
            app.tab_mut()
                .cache
                .insert(key, continuous_frame(key, revision, page == 1));
        }
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        // Match flash expiry: evict the highlighted page while its clean
        // replacement is pending, leaving its terminal image visible.
        let target = app.page_key(1, viewport);
        app.tab_mut().cache.retain(|key, _| key.page != 1);
        app.pending.insert(target, false);
        (app, viewport, file)
    }

    fn continuous_gap_app() -> (super::App, Viewport, tempfile::NamedTempFile) {
        let (mut app, viewport, file) = continuous_app();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        let key = app.page_key(1, viewport);
        let mut neighbor = continuous_frame(key, revision, false);
        let frame = std::sync::Arc::get_mut(&mut neighbor).unwrap();
        frame.width = 160;
        frame.height = 120;
        frame.compressed_rgba =
            crate::kitty::compress_rgba(&[220, 40, 80].repeat(160 * 120)).unwrap();
        app.tab_mut().cache.insert(key, neighbor);
        app.tab_mut().scroll_y = 245;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        (app, viewport, file)
    }

    #[test]
    fn continuous_cell_inverse_uses_visible_neighbor_intersection() {
        let (mut app, viewport, _file) = continuous_gap_app();
        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::ALT,
        };
        let (frame, placement, top) = app.page_at_mouse(mouse, viewport).unwrap();
        assert_eq!(frame.key.page, 1);
        assert_eq!(
            super::source_pixel_at_cell(placement, top, &frame, mouse),
            Some((5, 2))
        );
        let tiny = super::ImagePlacement {
            left: 2,
            columns: 1,
            rows: 1,
            crop: Some(crate::kitty::Crop {
                x: 159,
                y: 119,
                width: 1,
                height: 1,
            }),
            native_cell: Some((10, 10)),
            offset_y: 9,
            ..placement
        };
        let tiny_mouse = crossterm::event::MouseEvent { column: 2, ..mouse };
        assert_eq!(
            super::source_pixel_at_cell(tiny, mouse.row, &frame, tiny_mouse),
            Some((159, 119))
        );
        let outside = crossterm::event::MouseEvent { column: 3, ..mouse };
        assert_eq!(
            super::source_pixel_at_cell(tiny, mouse.row, &frame, outside),
            None
        );
        let padding = crossterm::event::MouseEvent { row: 14, ..mouse };
        assert!(app.page_at_mouse(padding, viewport).is_none());
        app.begin_inverse_search(padding, &mut Vec::new()).unwrap();
        assert!(app.navigation.inverse.is_none());
        app.begin_inverse_search(mouse, &mut Vec::new()).unwrap();
        assert_eq!(app.navigation.inverse.as_ref().unwrap().page, 1);
    }

    #[test]
    fn continuous_gap_screenshot_uses_submitted_neighbor_pixels() {
        let (app, viewport, _file) = continuous_gap_app();
        let pages = app.screenshot_pages(viewport).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gap.png");
        crate::screenshot::save(&path, viewport, &pages, None, [1, 2, 3]).unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (80, 200));
        let at = |x: usize, y: usize| &pixels[(y * 80 + x) * 4..(y * 80 + x + 1) * 4];
        assert_eq!(at(0, 4), [1, 2, 3, 255]);
        assert_eq!(at(79, 5), [220, 40, 80, 255]);
        assert_eq!(at(0, 124), [220, 40, 80, 255]);
        assert_eq!(at(0, 125), [1, 2, 3, 255]);
    }

    #[test]
    fn continuous_gap_picker_maps_submitted_neighbor_and_preserves_gap_view() {
        let (mut app, viewport, _file) = continuous_gap_app();
        let image = app.picker_image(viewport).unwrap();
        assert_eq!((image.source_width, image.source_height), (80, 120));
        assert_eq!(image.original.placement.offset_y, 5);
        assert_eq!(
            (
                image.original.placement.columns,
                image.original.placement.rows
            ),
            (0, 0)
        );
        app.retain_primary_image(&mut Vec::new()).unwrap();
        app.link_picker = Some(LinkPickerState::new(1));
        app.link_picker_geometry.layout = LinkPickerLayout::Vertical;
        let (preview, _) =
            link_picker_panes(super::link_picker_area(viewport), app.link_picker_geometry);
        let positioned =
            super::position_link_picker_image(image, preview, LinkPickerLayout::Vertical);
        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: positioned.left + positioned.placement.columns - 1,
            row: positioned.top + positioned.placement.rows - 1,
            modifiers: KeyModifiers::NONE,
        };
        let (frame, placement, top) = app.page_at_mouse(mouse, viewport).unwrap();
        let hit = placement
            .source_cell(mouse.column, mouse.row - top, frame.width, frame.height)
            .unwrap();
        assert_eq!(frame.key.page, 1);
        assert_eq!((hit.x + hit.width, hit.y + hit.height), (80, 120));
        app.link_picker_geometry.layout = LinkPickerLayout::Floating;
        let mouse = crossterm::event::MouseEvent {
            column: 0,
            row: viewport.top,
            ..mouse
        };
        let (frame, placement, top) = app.page_at_mouse(mouse, viewport).unwrap();
        let hit = placement
            .source_cell(mouse.column, mouse.row - top, frame.width, frame.height)
            .unwrap();
        assert_eq!((hit.y, hit.height), (0, 5));
        assert_eq!((app.tab().page, app.tab().scroll_y), (0, 245));
    }

    #[test]
    fn retained_picker_inverse_survives_render_cache_eviction() {
        let (mut app, viewport, _file) = continuous_gap_app();
        let visible = app.current_visible_page(viewport).unwrap();
        let key = visible.frame.key;
        let revision = visible.frame.revision;
        app.retain_primary_image(&mut Vec::new()).unwrap();
        app.link_picker = Some(LinkPickerState::new(1));
        app.tab_mut().cache.clear();
        let mut output = Vec::new();
        app.begin_inverse_at(key, (80, 60), viewport, &mut output)
            .unwrap();
        let inverse = app.navigation.inverse.as_ref().unwrap();
        assert_eq!(inverse.page, 1);
        assert_eq!(inverse.revision, revision);

        app.navigation.inverse = None;
        app.visible_page = None;
        output.clear();
        app.begin_inverse_at(key, (80, 60), viewport, &mut output)
            .unwrap();
        assert!(app.navigation.inverse.is_none());
    }

    #[test]
    fn screenshot_rejects_missing_or_stale_visible_neighbors() {
        let (mut app, viewport, file) = continuous_app();
        let target = app.page_key(1, viewport);
        app.missing_visible_page = Some(target);
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
        app.missing_visible_page = None;
        fs::write(file.path(), b"changed PDF revision").unwrap();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        app.visible_pages[1].frame = continuous_frame(target, revision, false);
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
    }

    #[test]
    fn screenshot_rejects_new_anchor_until_its_view_is_submitted() {
        let (mut app, viewport, _file) = continuous_gap_app();
        app.tab_mut().page = 1;
        app.tab_mut().scroll_y = 0;
        assert!(
            app.screenshot_pages(viewport)
                .err()
                .unwrap()
                .to_string()
                .contains("still rendering")
        );
        let frame = app.tab().cache[&app.page_key(1, viewport)].clone();
        app.draw_continuous(&frame, viewport, &mut Vec::new())
            .unwrap();
        let pages = app.screenshot_pages(viewport).unwrap();
        assert_eq!(pages[0].placement.offset_y, 0);
        assert_eq!(pages[0].frame.key.page, 1);
    }

    #[test]
    fn expired_flash_never_renders_a_removed_page_or_old_revision() {
        for shrink in [true, false] {
            let (mut app, _viewport, file) = continuous_app();
            let revision = app.tab().revision;
            app.pending.clear();
            if shrink {
                app.tab_mut().page_count = 1;
            } else {
                fs::write(file.path(), b"changed PDF revision").unwrap();
                app.tab_mut().revision = crate::synctex::PdfRevision::read(file.path()).unwrap();
            }
            app.navigation.flash = Some(super::PendingFlash {
                document_id: app.tab().document_id,
                revision,
                page: 1,
                positioning_pending: false,
                expires_at: Some(Instant::now()),
            });
            app.poll_flash_expiry().unwrap();
            assert!(app.navigation.flash.is_none());
            assert!(
                app.pending.is_empty(),
                "expired flash scheduled an invalid frame"
            );
            assert!(app.tab().cache.keys().all(|key| key.page != 1));
        }
    }

    #[test]
    fn rendered_destination_without_vertical_coordinate_preserves_scroll() {
        let (mut app, viewport, _file) = continuous_app();
        app.viewer.continuous_scroll = false;
        app.tab_mut().scroll_y = 20;
        app.tab_mut().pending_destination = Some(super::LinkDestination {
            content_pan_x: None,
            page: 0,
            // A 90-degree XYZ null 300 null destination specifies only rendered x.
            top_ratio: None,
            left_ratio: Some(0.75),
        });
        let frame = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_frame_unsynchronized(&frame, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_y, 20);
    }

    #[test]
    fn routine_forward_assumption_preserves_coarse_word_status() {
        let assumption = "assuming saved source is unchanged since compilation";
        for (precise, warning, label) in [
            (true, assumption, "forward search:"),
            (
                false,
                assumption,
                "forward search (SyncTeX region; no unique source word):",
            ),
            (
                false,
                "assuming saved source is unchanged since compilation; source was edited",
                "warning:",
            ),
        ] {
            let (mut app, mut viewport, file) = continuous_app();
            app.viewer.continuous_scroll = false;
            viewport.columns = 180;
            viewport.pixel_width = 1800;
            let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
            let key = app.page_key(0, viewport);
            let mut frame = continuous_frame(key, revision, true);
            std::sync::Arc::get_mut(&mut frame)
                .unwrap()
                .flash
                .as_mut()
                .unwrap()
                .word_precise = precise;
            let (reply, _client) = std::os::unix::net::UnixStream::pair().unwrap();
            app.navigation.forward = Some(super::PendingForward {
                request: crate::synctex::ForwardRequest {
                    pdf: file.path().to_owned(),
                    revision: revision.pdf,
                    page: 1,
                    h: 10.0,
                    v: 20.0,
                    width: 10.0,
                    height: 10.0,
                    word: None,
                    inverse_search: None,
                    best_guess: false,
                    warning: Some(warning.into()),
                },
                reply: crate::ipc::ForwardReply::new(reply),
                deadline: Instant::now() + crate::ipc::FORWARD_TIMEOUT,
                stage: super::ForwardStage::AwaitingFrame,
            });
            app.navigation.flash = Some(super::PendingFlash {
                document_id: key.document_id,
                revision: revision.pdf,
                page: 0,
                positioning_pending: false,
                expires_at: None,
            });
            app.draw_frame(&frame, viewport, &mut Vec::new()).unwrap();
            let status = String::from_utf8_lossy(&app.status_line);
            assert!(status.contains(&format!("{label} {warning}")), "{status}");
        }
    }

    #[test]
    fn continuous_redraw_clamps_retained_offsets_to_smaller_page() {
        let (mut app, mut viewport, _file) = continuous_app();
        viewport.pixel_width = 60;
        app.tab_mut().page_count = 1;
        app.tab_mut().scroll_x = 37;
        app.tab_mut().scroll_y = 123;
        let revision = app.tab().watcher.accepted;
        let key = app.page_key(0, viewport);
        let frame = continuous_frame(key, revision, false);
        app.tab_mut().cache.insert(key, frame.clone());
        app.draw_continuous(&frame, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (20, 40));
    }

    #[test]
    fn continuous_zoom_retains_x_offset_for_wider_page_below_narrow_top_page() {
        let (mut app, mut viewport, _file) = continuous_app();
        viewport.pixel_width = 60;
        let revision = app.tab().watcher.accepted;
        let narrow_key = app.page_key(0, viewport);
        let wide_key = app.page_key(1, viewport);
        let mut narrow = continuous_frame(narrow_key, revision, false);
        let first = std::sync::Arc::get_mut(&mut narrow).unwrap();
        first.width = 40;
        first.compressed_rgba = crate::kitty::compress_rgba(&[255; 40 * 240 * 3]).unwrap();
        let wide = continuous_frame(wide_key, revision, false);
        app.tab_mut().cache.clear();
        app.tab_mut().cache.insert(narrow_key, narrow.clone());
        app.tab_mut().cache.insert(wide_key, wide);
        app.tab_mut().scroll_x = 20;
        app.tab_mut().scroll_y = 220;
        app.draw_continuous(&narrow, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 20);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 0);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 20);
    }

    #[test]
    fn continuous_redraw_preserves_offset_for_wider_visible_target() {
        let (mut app, viewport, _file) = continuous_app();
        let target = app.page_key(1, viewport);
        let mut wide = continuous_frame(target, app.tab().watcher.accepted, true);
        std::sync::Arc::get_mut(&mut wide).unwrap().width = 160;
        app.tab_mut().cache.insert(target, wide);
        app.tab_mut().scroll_x = 45;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 45);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 0);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 45);
        app.tab_mut().cache.remove(&target);
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 45);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 45);
        app.tab_mut().scroll_y = 0;
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, 0);
    }

    #[test]
    fn reload_preserves_view_and_clamps_removed_pages() {
        let (mut app, _, _file) = continuous_app();
        let (mut other, _, _other_file) = continuous_app();
        let mut other_tab = other.session.tabs.pop().unwrap();
        other_tab.document_id += 1;
        app.session.tabs.push(other_tab);
        app.session.active_tab = 1;
        for pages in [2, 1] {
            let tab = &mut app.session.tabs[0];
            tab.page = 1;
            tab.scroll_x = 37;
            tab.scroll_y = 123;
            tab.zoom = 150;
            let document_id = tab.document_id;
            let revision = tab.revision;
            let fingerprint = tab.watcher.accepted;
            app.session.pending_open = Some(super::session::PendingOpen::Reload {
                document_id,
                fingerprint,
            });
            app.navigation.flash = Some(super::PendingFlash {
                document_id,
                revision,
                page: 1,
                positioning_pending: false,
                expires_at: Some(Instant::now()),
            });
            app.finish_open(document_id, pages, Vec::new(), fingerprint, &mut Vec::new())
                .unwrap();
            let tab = &app.session.tabs[0];
            assert_eq!(tab.page, pages - 1);
            assert_eq!((tab.scroll_x, tab.scroll_y, tab.zoom), (37, 123, 150));
            assert!(tab.cache.is_empty());
            assert!(app.navigation.flash.is_none());
        }
    }

    #[test]
    fn continuous_redraw_retains_expired_flash_until_replacement_upload() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let target = app.page_key(1, viewport);
        let highlighted = app.visible_pages[1].frame.clone();
        let old_id = app.visible_pages[1].image_id;
        let delete = format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\");

        app.generation += 1;
        app.tab_mut().scroll_y = 115;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(app.missing_visible_page, Some(target));
        assert!(app.pending.contains_key(&target));
        assert!(!app.tab().cache.contains_key(&target));
        assert_eq!(app.visible_pages.len(), 2);
        let retained = &app.visible_pages[1];
        assert!(std::sync::Arc::ptr_eq(&retained.frame, &highlighted));
        assert_eq!(retained.image_id, old_id);
        assert_eq!(retained.top, 14);
        assert_eq!(retained.placement.offset_y, 5);
        assert_eq!(retained.placement.crop.unwrap().height, 65);
        assert!(output.contains(&format!("\x1b_Ga=p,i={old_id},")));
        assert!(!output.contains("\x1b_Ga=T,"));
        assert!(!output.contains(&delete));

        let clean = continuous_frame(target, highlighted.revision, false);
        app.tab_mut().cache.insert(target, clean.clone());
        app.pending.remove(&target);
        let new_id = app.next_image_id;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        let upload = output
            .find(&format!("\x1b_Ga=T,f=24,s=80,v=240,i={new_id},"))
            .unwrap();
        let upload_end = upload + output[upload..].find("\x1b\\").unwrap() + 2;
        assert!(upload_end <= output.find(&delete).unwrap());
        assert_eq!(app.missing_visible_page, None);
        assert!(std::sync::Arc::ptr_eq(&app.visible_pages[1].frame, &clean));
        assert_eq!(app.visible_pages[1].image_id, new_id);
        assert!(app.visible_pages[1].frame.flash.is_none());
    }

    #[test]
    fn continuous_redraw_removes_offscreen_cache_miss() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let old_id = app.visible_pages[1].image_id;
        app.tab_mut().scroll_y = 0;
        let mut output = Vec::new();
        app.draw_continuous(&primary, viewport, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(app.visible_pages.len(), 1);
        assert_eq!(app.visible_pages[0].frame.key.page, 0);
        assert_eq!(app.missing_visible_page, None);
        assert!(output.contains(&format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\")));
        assert!(!output.contains(&format!("\x1b_Ga=p,i={old_id},")));
    }

    #[test]
    fn continuous_redraw_rejects_incompatible_visible_frames() {
        let (mut app, viewport, _file) = continuous_app();
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        let target = app.page_key(1, viewport);
        let revision = app.visible_pages[1].frame.revision;
        let other_file = tempfile::NamedTempFile::new().unwrap();
        let other_revision = crate::synctex::DocumentRevision::read(other_file.path()).unwrap();
        let mut incompatible = Vec::new();
        let changes: [fn(&mut crate::pdf::RenderKey); 13] = [
            |key: &mut crate::pdf::RenderKey| key.document_id += 1,
            |key: &mut crate::pdf::RenderKey| key.page += 1,
            |key: &mut crate::pdf::RenderKey| key.width += 1,
            |key: &mut crate::pdf::RenderKey| key.height += 1,
            |key: &mut crate::pdf::RenderKey| key.zoom += 1,
            |key: &mut crate::pdf::RenderKey| key.fit = crate::pdf::FitMode::Height,
            |key: &mut crate::pdf::RenderKey| key.invert = !key.invert,
            |key: &mut crate::pdf::RenderKey| key.dark_mode_style.background[0] ^= 1,
            |key: &mut crate::pdf::RenderKey| key.search_request_id += 1,
            |key: &mut crate::pdf::RenderKey| key.search_highlight[0] ^= 1,
            |key: &mut crate::pdf::RenderKey| key.link_mode = !key.link_mode,
            |key: &mut crate::pdf::RenderKey| key.link_highlight[0] ^= 1,
            |key: &mut crate::pdf::RenderKey| key.selected_link_ordinal = Some(1),
        ];
        for change in changes {
            let mut key = target;
            change(&mut key);
            incompatible.push((key, revision));
        }
        incompatible.push((target, other_revision));
        for (key, incompatible_revision) in incompatible {
            app.tab_mut()
                .cache
                .insert(target, continuous_frame(target, revision, true));
            app.draw_continuous(&primary, viewport, &mut Vec::new())
                .unwrap();
            app.tab_mut().cache.remove(&target);
            let old_id = app.visible_pages[1].image_id;
            app.visible_pages[1].frame = continuous_frame(key, incompatible_revision, true);
            let mut output = Vec::new();
            app.draw_continuous(&primary, viewport, &mut output)
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(app.visible_pages.len(), 1, "{key:?}");
            assert_eq!(app.missing_visible_page, Some(target));
            assert!(output.contains(&format!("\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\")));
            assert!(!output.contains(&format!("\x1b_Ga=p,i={old_id},")));
        }
    }

    #[test]
    fn synchronized_output_closes_successful_and_failed_updates() {
        let mut successful = Vec::new();
        let result: io::Result<()> = synchronized_output(&mut successful, |output| {
            output.write_all(b"complete frame")
        });
        result.expect("successful synchronized update");
        assert_eq!(successful, b"\x1b[?2026hcomplete frame\x1b[?2026l");

        let mut failed = Vec::new();
        let result: io::Result<()> = synchronized_output(&mut failed, |output| {
            output.write_all(b"partial frame")?;
            Err(io::Error::other("render failed"))
        });
        assert_eq!(
            result.expect_err("failed synchronized update").kind(),
            io::ErrorKind::Other
        );
        assert_eq!(failed, b"\x1b[?2026hpartial frame\x1b[?2026l");
    }

    #[test]
    fn synchronized_output_batches_flushes_and_reports_submission_errors() {
        #[derive(Default)]
        struct Sink {
            bytes: Vec<u8>,
            writes: usize,
            flushes: usize,
            fail_flush: bool,
        }

        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                if self.fail_flush {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed terminal"))
                } else {
                    Ok(())
                }
            }
        }

        for fail_flush in [false, true] {
            let mut output = io::BufWriter::new(Sink {
                fail_flush,
                ..Sink::default()
            });
            let result: io::Result<()> = synchronized_output(&mut output, |frame| {
                frame.write_all(b"first")?;
                frame.flush()?;
                frame.write_all(b"second")?;
                frame.flush()
            });
            if fail_flush {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            } else {
                result.unwrap();
            }
            let sink = output.get_ref();
            assert_eq!(sink.bytes, b"\x1b[?2026hfirstsecond\x1b[?2026l");
            assert_eq!(sink.writes, 1);
            assert_eq!(sink.flushes, 1);
        }
    }

    #[test]
    fn render_timings_are_compact_by_default_and_expand_on_demand() {
        assert_eq!(
            render_timing_status(15, Some(12), Some(0), 27, 17, false),
            "render 71ms"
        );
        assert_eq!(
            render_timing_status(15, Some(12), Some(0), 27, 17, true),
            "render 15ms  dark 12ms  highlight 0ms  compress 27ms  transfer 17ms"
        );

        let snapshot = PerformanceSnapshot {
            render_ms: 15,
            dark_mode_ms: Some(12),
            highlight_ms: Some(0),
            compression_ms: 27,
            transfer_ms: 17,
            link_count: 5,
        };
        assert_eq!(snapshot.status(false, true), "render 71ms  5 page links");
    }

    #[test]
    fn path_navigation_prefers_the_active_duplicate_then_any_matching_tab() {
        let (mut app, _, _file) = continuous_app();
        let (mut other, _, other_file) = continuous_app();
        let path = app.tab().path.clone();
        let mut duplicate = other.session.tabs.pop().unwrap();
        duplicate.document_id += 1;
        duplicate.path = path.clone();
        app.session.tabs.push(duplicate);

        app.session.active_tab = 1;
        assert_eq!(app.tab_index_for_path(&path), Some(1));
        app.session.active_tab = 0;
        assert_eq!(app.tab_index_for_path(&path), Some(0));

        app.session.tabs[0].path = other_file.path().to_owned();
        assert_eq!(app.tab_index_for_path(&path), Some(1));
    }

    #[test]
    fn loading_tab_does_not_hit_test_outgoing_document() {
        let (mut app, viewport, _file) = continuous_app();
        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 0,
            row: viewport.top,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            app.page_at_mouse(mouse, viewport)
                .unwrap()
                .0
                .key
                .document_id,
            app.tab().document_id
        );
        let (mut other, _, _other_file) = continuous_app();
        let mut tab = other.session.tabs.pop().unwrap();
        tab.document_id += 1;
        tab.cache.clear();
        app.session.tabs.push(tab);
        app.session.active_tab = 1;
        assert!(app.page_at_mouse(mouse, viewport).is_none());
    }

    #[test]
    fn tab_switch_replaces_outgoing_images_in_one_frame() {
        for continuous in [false, true] {
            let (mut app, viewport, _file) = continuous_app();
            app.viewer.continuous_scroll = continuous;
            let outgoing: Vec<_> = app.visible_pages.iter().map(|page| page.image_id).collect();
            let (mut other, _, _other_file) = continuous_app();
            let mut tab = other.session.tabs.pop().unwrap();
            tab.document_id += 1;
            tab.page_count = 1;
            tab.scroll_y = 0;
            tab.cache.clear();
            let key = tab.render_key(viewport, false, None);
            let revision = crate::synctex::DocumentRevision::read(&tab.path).unwrap();
            tab.cache
                .insert(key, continuous_frame(key, revision, false));
            app.session.tabs.push(tab);

            let mut output = Vec::new();
            app.select_tab(1, &mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(!output.contains("\x1b_Ga=d,d=A,"));
            let start = output.find("\x1b[?2026h").unwrap();
            let end = output.find("\x1b[?2026l").unwrap();
            for image_id in outgoing {
                let deletion = output
                    .find(&format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\"))
                    .expect("outgoing image must be released");
                assert!(start < deletion && deletion < end);
            }
            assert_eq!(app.submitted_key, Some(key));
            assert_eq!(app.visible_page.as_ref().unwrap().frame.key, key);
        }
    }

    #[test]
    fn tab_switching_wraps_in_both_directions() {
        assert_eq!(cycled_tab_index(0, 3, 1), 1);
        assert_eq!(cycled_tab_index(2, 3, 1), 0);
        assert_eq!(cycled_tab_index(2, 3, -1), 1);
        assert_eq!(cycled_tab_index(0, 3, -1), 2);
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
            Some(0)
        );
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT)),
            Some(8)
        );
        assert_eq!(
            numbered_tab_index(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn search_navigation_wraps_between_matching_pages() {
        let matches = [
            SearchPageMatch {
                page: 2,
                occurrences: 1,
                context: String::new(),
            },
            SearchPageMatch {
                page: 5,
                occurrences: 2,
                context: String::new(),
            },
            SearchPageMatch {
                page: 9,
                occurrences: 1,
                context: String::new(),
            },
        ];

        assert_eq!(search_target_page(&matches, 2, true), Some(5));
        assert_eq!(search_target_page(&matches, 9, true), Some(2));
        assert_eq!(search_target_page(&matches, 5, false), Some(2));
        assert_eq!(search_target_page(&matches, 2, false), Some(9));
    }

    #[test]
    fn search_status_reports_progress_and_results() {
        let mut search = SearchState {
            query: "needle".into(),
            request_id: 1,
            matches: Vec::new(),
            total_occurrences: 0,
            scanned: 8,
            total_pages: 20,
            searching: true,
            initial_navigation_pending: false,
        };
        assert_eq!(
            search.status_label(0).as_deref(),
            Some("  search 8/20  /needle")
        );

        search.searching = false;
        search.matches = vec![SearchPageMatch {
            page: 4,
            occurrences: 3,
            context: String::new(),
        }];
        search.total_occurrences = 3;
        assert_eq!(search.highlight_request_id(4), 1);
        assert_eq!(search.highlight_request_id(5), 0);
        assert_eq!(
            search.status_label(4).as_deref(),
            Some("  search 1/1 · 3 hits  /needle")
        );
    }

    #[test]
    fn search_picker_groups_contextual_results_by_section_and_page() {
        let search = SearchState {
            query: "teakettle".into(),
            request_id: 1,
            matches: vec![
                SearchPageMatch {
                    page: 0,
                    occurrences: 2,
                    context: "…copper teakettle simmering near gravel…".into(),
                },
                SearchPageMatch {
                    page: 2,
                    occurrences: 1,
                    context: "…a teakettle barrow parked in damp turnips…".into(),
                },
            ],
            total_occurrences: 3,
            scanned: 3,
            total_pages: 3,
            searching: false,
            initial_navigation_pending: false,
        };
        let outline = vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 0,
                depth: 0,
            },
            OutlineItem {
                title: "Methods".into(),
                page: 2,
                depth: 0,
            },
        ];
        let mut state = SearchPickerState::new(0);
        state.sync(0, &search.matches);
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Vertical);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_search_picker(
                    frame,
                    area,
                    &search,
                    &outline,
                    &state,
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw search picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Search teakettle"));
        assert!(rendered.contains("3 hits on 2 pages"));
        assert!(rendered.contains("Section  Introduction"));
        assert!(rendered.contains("Page 1 · 2 hits · current"));
        assert!(rendered.contains("copper teakettle simmering"));
        assert!(rendered.contains("Section  Methods"));
        assert!(rendered.contains("Page 3 · 1 hit"));
    }

    #[test]
    fn status_text_flattens_errors_without_terminal_controls() {
        let columns = Cell::new(80);
        assert_eq!(
            format!(
                "{}",
                super::StatusText("reload failed: bad\r\nfile\t\u{1b}[2J", &columns)
            ),
            "reload failed: bad  file  [2J"
        );
    }

    #[test]
    fn status_text_shares_the_row_budget_across_fragments() {
        let columns = Cell::new(6);
        let text = format!(
            "{}{}{}{}",
            super::StatusText(1, &columns),
            super::StatusText("/1 ", &columns),
            super::StatusText("漢字", &columns),
            super::StatusText(" help", &columns),
        );
        assert_eq!(text, "1/1 漢");
        let columns = Cell::new(0);
        assert_eq!(
            format!("{}", super::StatusText("reload failed", &columns)),
            ""
        );
    }

    #[test]
    fn status_text_preserves_combining_and_emoji_graphemes() {
        let columns = Cell::new(4);
        assert_eq!(
            format!("{}", super::StatusText("e\u{301}👩‍🔬xy", &columns)),
            "e\u{301}👩‍🔬x"
        );
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
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("document.pdf");
        fs::write(&pdf, "unchanged PDF").unwrap();
        let initial = FileFingerprint::read(&pdf).unwrap();
        fs::write(pdf.with_extension("synctex"), "new companion").unwrap();
        let changed = FileFingerprint::read(&pdf).unwrap();
        assert_ne!(initial, changed);
        let started = Instant::now();
        let mut watcher = FileWatcher {
            accepted: initial,
            candidate: None,
            failed: None,
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
    fn mouse_cell_intersection_selects_tiny_link_targets() {
        let target = LinkTarget::Internal {
            page: 7,
            top_ratio: Some(0.5),
            left_ratio: None,
        };
        let links = [PageLink {
            rect: PageLinkRect {
                left: 52,
                top: 22,
                right: 55,
                bottom: 25,
            },
            label: "[7]".into(),
            target: target.clone(),
        }];
        let placement = ImagePlacement {
            left: 10,
            columns: 20,
            rows: 10,
            crop: None,
            scroll_x: 0,
            scroll_y: 0,
            native_cell: None,
            offset_y: 0,
        };

        assert_eq!(
            link_at_cell(&links, placement, 200, 100, 1, 15, 3),
            Some(target)
        );
        assert_eq!(link_at_cell(&links, placement, 200, 100, 1, 14, 3), None);
        assert_eq!(link_at_cell(&links, placement, 200, 100, 1, 15, 0), None);
    }

    #[test]
    fn link_picker_supports_multi_digit_number_selection() {
        let mut input = String::new();

        assert_eq!(update_link_number_selection(&mut input, '1', 20), Some(0));
        assert_eq!(update_link_number_selection(&mut input, '0', 20), Some(9));
        assert_eq!(input, "10");
        assert_eq!(update_link_number_selection(&mut input, '9', 20), Some(8));
        assert_eq!(input, "9");
    }

    #[test]
    fn link_picker_supports_page_and_end_navigation_keys() {
        let control = KeyModifiers::CONTROL;
        assert_eq!(
            picker_navigation_index(
                3,
                30,
                KeyEvent::new(KeyCode::Char('f'), control),
                10,
                true,
                ListEdge::Wrap
            ),
            Some(13)
        );
        assert_eq!(
            picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('b'), control),
                10,
                true,
                ListEdge::Wrap
            ),
            Some(3)
        );
        assert_eq!(
            picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                10,
                true,
                ListEdge::Wrap
            ),
            Some(0)
        );
        assert_eq!(
            picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
                10,
                true,
                ListEdge::Wrap
            ),
            Some(29)
        );
        assert_eq!(
            picker_navigation_index(
                13,
                30,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                10,
                true,
                ListEdge::Wrap
            ),
            None
        );
    }

    #[test]
    fn split_link_picker_focuses_document_and_links_without_affecting_floating_layout() {
        let plain = KeyModifiers::NONE;
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Vertical,
                KeyEvent::new(KeyCode::Char('h'), plain),
            ),
            Some(LinkPickerFocus::Document)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Document,
                LinkPickerLayout::Horizontal,
                KeyEvent::new(KeyCode::Char('l'), plain),
            ),
            Some(LinkPickerFocus::Links)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Vertical,
                KeyEvent::new(KeyCode::Tab, plain),
            ),
            Some(LinkPickerFocus::Document)
        );
        assert_eq!(
            link_picker_focus_for_key(
                LinkPickerFocus::Links,
                LinkPickerLayout::Floating,
                KeyEvent::new(KeyCode::Char('h'), plain),
            ),
            None
        );
    }

    #[test]
    fn link_picker_filter_matches_labels_pages_and_urls() {
        let links = vec![
            DocumentLink {
                source_page: 1,
                source_top_ratio: 0.1,
                ordinal: 0,
                label: "(Zabbo et al., 2020)".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 10,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "project page".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Uri("https://example.invalid/paper".into()),
            },
        ];
        assert_eq!(filter_document_links(&links, "zabbo"), vec![0]);
        assert_eq!(filter_document_links(&links, "source page 3"), vec![1]);
        assert_eq!(filter_document_links(&links, "example paper"), vec![1]);
        assert!(filter_document_links(&links, "missing").is_empty());
    }

    #[test]
    fn link_picker_rows_cache_reuses_navigation_and_invalidates_changed_inputs() {
        use std::sync::Arc;
        let mut links = vec![DocumentLink {
            source_page: 1,
            source_top_ratio: 0.1,
            ordinal: 0,
            label: "zog".into(),
            source_context: None,
            reference_context: None,
            target: LinkTarget::Uri("https://example.invalid/zog".into()),
        }];
        let outline = vec![OutlineItem {
            title: "vex".into(),
            page: 0,
            depth: 0,
        }];
        let mut state = LinkPickerState::new(0);
        let first = state.rows(LinkPickerDocument::new(&links, &outline));
        state.select(0, &links);
        state.page = 1;
        state.filtering = true;
        assert!(Arc::ptr_eq(
            &first,
            &state.rows(LinkPickerDocument::new(&links, &outline))
        ));

        state.filter = "missing".into();
        let filtered = state.rows(LinkPickerDocument::new(&links, &outline));
        assert!(filtered.filtered.is_empty());
        assert!(!Arc::ptr_eq(&first, &filtered));
        state.filter.clear();
        let unfiltered = state.rows(LinkPickerDocument::new(&links, &outline));
        links.push(DocumentLink {
            source_page: 2,
            label: "vex".into(),
            ..links[0].clone()
        });
        let appended = state.rows(LinkPickerDocument::new(&links, &outline));
        assert_eq!(appended.filtered, vec![0, 1]);
        assert!(!Arc::ptr_eq(&unfiltered, &appended));
        assert_eq!(
            appended.display,
            super::link_picker_display_rows(&links, &[0, 1], &outline)
        );

        let replacement_outline = vec![OutlineItem {
            title: "zib".into(),
            page: 2,
            depth: 0,
        }];
        let mut document = LinkPickerDocument::new(&links, &replacement_outline);
        let replaced_outline = state.rows(document);
        assert!(!Arc::ptr_eq(&appended, &replaced_outline));
        assert_ne!(appended.display, replaced_outline.display);
        document.document_id = 1;
        let replaced_document = state.rows(document);
        assert!(!Arc::ptr_eq(&replaced_outline, &replaced_document));
        document.revision = 1;
        let replaced_revision = state.rows(document);
        assert!(!Arc::ptr_eq(&replaced_document, &replaced_revision));
        assert!(Arc::ptr_eq(&replaced_revision, &state.rows(document)));

        state.reset_for_document(0);
        assert!(!Arc::ptr_eq(&replaced_revision, &state.rows(document)));
    }

    #[test]
    fn link_picker_pointer_rows_resolve_only_link_entries() {
        let links = vec![
            DocumentLink {
                source_page: 1,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "first".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 4,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.4,
                ordinal: 0,
                label: "second".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 5,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
        ];
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let list = link_picker_list_area(area, geometry);
        let state = LinkPickerState::new(0);
        let document = LinkPickerDocument::new(&links, &[]);

        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y),
            None
        );
        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y + 1),
            Some(0)
        );
        assert_eq!(
            link_picker_link_at_position(area, document, &state, geometry, list.x, list.y + 3),
            Some(1)
        );
    }

    #[test]
    fn persistent_link_picker_tracks_the_current_page_in_the_document_index() {
        let links = vec![
            DocumentLink {
                source_page: 0,
                source_top_ratio: 0.1,
                ordinal: 0,
                label: "first".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 4,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 4,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "current".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 6,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 6,
                source_top_ratio: 0.3,
                ordinal: 0,
                label: "later".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
        ];
        let mut state = LinkPickerState::new(4);

        state.sync(4, &links, false);
        assert_eq!(state.selected, 1);

        state.sync(5, &links, false);
        assert_eq!(state.selected, 2);
        assert_eq!(state.selection_key, Some((6, 0)));
    }

    #[test]
    fn link_picker_shows_link_text_and_destinations() {
        let links = vec![
            DocumentLink {
                source_page: 2,
                source_top_ratio: 0.2,
                ordinal: 0,
                label: "[12]".into(),
                source_context: Some(
                    "Bramble crates nudge the lopsided kettle [12] in damp gravel. Additional synthetic context continues onto a second line for validation."
                        .into(),
                ),
                reference_context: Some("[12] Example et al. A synthetic reference title.".into()),
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
                    left_ratio: None,
                },
            },
            DocumentLink {
                source_page: 3,
                source_top_ratio: 0.3,
                ordinal: 0,
                label: "project page".into(),
                source_context: None,
                reference_context: None,
                target: LinkTarget::Uri("https://example.invalid/paper".into()),
            },
        ];
        let outline = vec![
            OutlineItem {
                title: "Introduction".into(),
                page: 2,
                depth: 0,
            },
            OutlineItem {
                title: "Project links".into(),
                page: 3,
                depth: 0,
            },
        ];
        let mut state = LinkPickerState::new(2);
        state.select(1, &links);
        state.number_input = "2".into();
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw link picker");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(40, 15)].bg, ratatui::style::Color::Reset);
        assert_eq!(
            buffer[(pane.x + pane.width / 2, pane.y + pane.height / 2)].bg,
            picker_color(crate::theme::TOKYO_NIGHT_MOON.bg_dark)
        );
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Links 2"));
        assert!(rendered.contains("auto/vertical"));
        assert!(rendered.contains("Section  Introduction"));
        assert!(rendered.contains("Section  Project links"));
        assert!(rendered.contains("Page 3 · current"));
        assert!(rendered.contains("citation [12]"));
        assert!(!rendered.contains("PDF page 8"));
        assert!(rendered.contains("project page"));
        assert!(!rendered.contains("Selected"));
        assert!(rendered.contains("Page 4 → https://example.invalid/paper"));
        assert!(!rendered.contains("copy URL"));
        assert!(!rendered.contains("2/2"));

        state.focus = LinkPickerFocus::Document;
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw document-focused split");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("PDF focused  · l: links"));

        state.focus = LinkPickerFocus::Links;

        state.select(0, &links);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw citation context");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Context  Bramble crates nudge"));
        assert!(rendered.contains("second line for validation."));
        assert!(rendered.contains("Reference  [12] Example et al."));

        state.filter = "project".into();
        state.filtering = true;
        state.select(1, &links);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw filtered links");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Links 1/2"));
        assert!(rendered.contains("/ project"));
        assert!(!rendered.contains("citation [12]"));
        assert!(!rendered.contains("1/1"));

        state.filter.clear();
        state.filtering = false;
        state.select(0, &links);

        let compact_area = Rect::new(0, 0, 80, 24);
        let compact_geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Horizontal);
        let (_, compact_pane) = link_picker_panes(compact_area, compact_geometry);
        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    compact_area,
                    LinkPickerDocument::new(&links, &outline),
                    &state,
                    LinkIndexProgress {
                        scanned: 12,
                        total_pages: 12,
                        indexing: false,
                    },
                    compact_geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw compact horizontal citation context");
        let buffer = terminal.backend().buffer();
        let rendered: String = (compact_pane.y..compact_pane.y + compact_pane.height)
            .flat_map(|y| {
                (compact_pane.x..compact_pane.x + compact_pane.width)
                    .map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("Context  Bramble crates nudge"));
        assert!(rendered.contains("Reference  [12] Example et al."));
    }

    #[test]
    fn persistent_link_picker_can_stay_open_on_a_page_without_links() {
        let area = Rect::new(0, 0, 160, 30);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let state = LinkPickerState::new(4);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&[], &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 28,
                        total_pages: 28,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw empty link picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Links 0"));
        assert!(rendered.contains("No annotated links in this document"));
    }

    #[test]
    fn floating_link_picker_is_centered_opaque_and_bordered() {
        let links = vec![DocumentLink {
            source_page: 0,
            source_top_ratio: 0.1,
            ordinal: 0,
            label: "project page".into(),
            source_context: None,
            reference_context: None,
            target: LinkTarget::Uri("https://example.invalid/paper".into()),
        }];
        let state = LinkPickerState::new(0);
        let area = Rect::new(0, 0, 100, 40);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Floating);
        let (_, popup) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 1,
                        total_pages: 1,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw floating link picker");
        let buffer = terminal.backend().buffer();

        assert_eq!(popup, Rect::new(12, 5, 75, 30));
        assert_eq!(buffer[(0, 0)].bg, ratatui::style::Color::Reset);
        assert_eq!(buffer[(popup.x, popup.y)].symbol(), "┌");
        assert_eq!(
            buffer[(popup.x + popup.width / 2, popup.y + popup.height / 2)].bg,
            picker_color(crate::theme::TOKYO_NIGHT_MOON.bg_dark)
        );
        let rendered: String = (popup.y..popup.y + popup.height)
            .flat_map(|y| {
                (popup.x..popup.x + popup.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();
        assert!(rendered.contains("j/k C-b/f g/G"));
        assert!(rendered.contains("esc"));
    }

    #[test]
    fn split_link_sidebar_has_no_navigation_footer() {
        let links = vec![DocumentLink {
            source_page: 0,
            source_top_ratio: 0.1,
            ordinal: 0,
            label: "project page".into(),
            source_context: None,
            reference_context: None,
            target: LinkTarget::Uri("https://example.invalid/paper".into()),
        }];
        let state = LinkPickerState::new(0);
        let area = Rect::new(0, 0, 80, 24);
        let geometry = LinkPickerGeometry::new(50, LinkPickerLayout::Auto);
        let (_, pane) = link_picker_panes(area, geometry);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| {
                draw_link_picker(
                    frame,
                    area,
                    LinkPickerDocument::new(&links, &[]),
                    &state,
                    LinkIndexProgress {
                        scanned: 1,
                        total_pages: 1,
                        indexing: false,
                    },
                    geometry,
                    crate::theme::TOKYO_NIGHT_MOON,
                )
            })
            .expect("draw compact link sidebar");
        let buffer = terminal.backend().buffer();
        let rendered: String = (pane.y..pane.y + pane.height)
            .flat_map(|y| {
                (pane.x..pane.x + pane.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(!rendered.contains("j/k"));
        assert!(!rendered.contains("↵"));
        assert!(!rendered.contains("s/a"));
        assert!(!rendered.contains("esc"));
        assert!(!rendered.contains("1/1"));
    }

    #[test]
    fn link_picker_reserves_room_for_details_when_space_allows() {
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            20
        );
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 20, 8),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            6
        );
    }

    #[test]
    fn link_picker_normalizes_split_numeric_citations() {
        assert_eq!(link_picker_label("[23]"), "citation [23]");
        assert_eq!(link_picker_label("8,"), "citation [8]");
        assert_eq!(link_picker_label("34]"), "citation [34]");
        assert_eq!(link_picker_label("project page"), "project page");
    }

    #[test]
    fn link_picker_cycles_explicit_layouts_from_the_resolved_layout() {
        let wide = Rect::new(0, 0, 120, 30);
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Auto),
            LinkPickerLayout::Horizontal
        );
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Horizontal),
            LinkPickerLayout::Floating
        );
        assert_eq!(
            next_link_picker_layout(wide, LinkPickerLayout::Floating),
            LinkPickerLayout::Vertical
        );
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
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            10,
            false,
        ));
        assert_eq!(browser.selected, 0);
    }

    #[test]
    fn picker_supports_page_and_end_navigation_keys() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for index in 0..8 {
            fs::write(directory.path().join(format!("{index}.pdf")), b"synthetic").expect("PDF");
        }
        let mut browser = BrowserState::new(directory.path().to_path_buf());

        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            3,
            false,
        ));
        assert_eq!(browser.selected, 3);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            3,
            false,
        ));
        assert_eq!(browser.selected, 0);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
            3,
            false,
        ));
        assert_eq!(browser.selected, browser.filtered_indices.len() - 1);
        assert!(apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            3,
            false,
        ));
        assert_eq!(browser.selected, 0);

        browser.filter = "one".into();
        browser.rebuild_filter();
        assert!(!apply_picker_navigation(
            &mut browser,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            3,
            true,
        ));
    }

    #[test]
    fn picker_escape_clears_the_filter_before_closing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pdf"), b"synthetic").expect("PDF");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "one".into();
        browser.rebuild_filter();

        assert!(clear_picker_filter(&mut browser));
        assert!(browser.filter.is_empty());
        assert!(!clear_picker_filter(&mut browser));
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
            .draw(|frame| draw_picker(frame, &browser, false, crate::theme::TOKYO_NIGHT_MOON))
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
    fn picker_uses_layered_theme_and_selection_marker() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("one.pdf"), b"synthetic").expect("PDF");
        let browser = BrowserState::new(directory.path().to_path_buf());
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let theme = crate::theme::TOKYO_NIGHT_MOON;
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_picker(frame, &browser, false, theme))
            .expect("draw picker");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 0)].bg, picker_color(theme.bg_dark1));
        assert_eq!(buffer[(popup.x, popup.y)].fg, picker_color(theme.blue7));
        assert_eq!(
            buffer[(popup.x + 1, popup.y + 1)].bg,
            picker_color(theme.bg_dark)
        );
        assert_eq!(buffer[(popup.x + 1, popup.y + 2)].symbol(), "▌");
        assert_eq!(
            buffer[(popup.x + 1, popup.y + 2)].bg,
            picker_color(theme.bg_highlight)
        );
    }

    #[test]
    fn theme_picker_previews_the_selected_palette() {
        let mut alternate = crate::theme::TOKYO_NIGHT_MOON;
        alternate.bg_dark1 = crossterm::style::Color::Rgb {
            r: 0x10,
            g: 0x20,
            b: 0x30,
        };
        let themes = vec![
            (
                "tokyo-night-moon".to_string(),
                crate::theme::TOKYO_NIGHT_MOON,
            ),
            ("synthetic-theme".to_string(), alternate),
        ];
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_theme_picker(frame, &themes, &[0, 1], 1, 0, "", false))
            .expect("draw theme picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (popup.y..popup.y + popup.height)
            .flat_map(|y| {
                (popup.x..popup.x + popup.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert_eq!(buffer[(0, 0)].bg, picker_color(alternate.bg_dark1));
        assert!(rendered.contains("tokyo-night-moon"));
        assert!(rendered.contains("synthetic-theme"));
        assert!(rendered.contains("Themes"));
        assert_eq!(filter_theme_indices(&themes, "synthetic"), vec![1]);
    }

    #[test]
    fn stepped_zoom_snaps_and_clamps_within_range() {
        assert_eq!(stepped_zoom(ZOOM_DEFAULT, true), ZOOM_DEFAULT + ZOOM_STEP);
        assert_eq!(stepped_zoom(ZOOM_DEFAULT, false), ZOOM_MIN);
        assert_eq!(stepped_zoom(ZOOM_MIN, false), ZOOM_MIN);
        assert_eq!(stepped_zoom(ZOOM_MAX, true), ZOOM_MAX);
        assert_eq!(stepped_zoom(ZOOM_MAX - ZOOM_STEP, true), ZOOM_MAX);
    }

    fn zoom_size(old: u16, new: u16) -> impl Fn(u32, u32, u32) -> Option<(u32, u32, Option<u32>)> {
        move |_, width, height| {
            Some((
                super::scale_zoom(width, old, new),
                super::scale_zoom(height, old, new),
                None,
            ))
        }
    }

    #[test]
    fn zoom_keeps_viewport_center_when_page_first_fits_then_overflows() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                zoom_size(100, 300),
                false,
                |_| Some((60, 40, None))
            ),
            Ok((0, 40, 20))
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (30, 50), zoom_size(100, 150), false, |_| {
                Some((200, 160, None))
            }),
            Ok((0, 70, 95))
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (70, 95), zoom_size(150, 100), false, |_| {
                Some((300, 240, None))
            }),
            Ok((0, 30, 50))
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (200, 160),
                zoom_size(150, 100),
                false,
                |_| Some((300, 240, None))
            ),
            Ok((0, 100, 80))
        );
    }

    #[test]
    fn zoom_uses_actual_terminal_cell_placement_when_page_first_fits() {
        let viewport = Viewport {
            columns: 80,
            rows: 29,
            pixel_width: 960,
            pixel_height: 580,
            top: 0,
            status_row: 29,
        };
        // 870px occupies 73 columns, offset by three 12px cells: its midpoint
        // is at x=471, nine pixels left of the viewport center.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                zoom_size(100, 125),
                false,
                |_| Some((870, 580, None))
            ),
            Ok((0, 75, 73))
        );
    }

    #[test]
    fn zoom_keeps_center_on_next_page_and_can_reveal_previous_page() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(viewport, 0, 2, (0, 90), zoom_size(100, 200), true, |page| {
                Some(if page == 0 {
                    (200, 100, None)
                } else {
                    (100, 120, None)
                })
            }),
            Ok((1, 50, 0))
        );
        assert_eq!(
            centered_scaled_view(viewport, 1, 2, (50, 0), zoom_size(200, 100), true, |page| {
                Some(if page == 0 {
                    (400, 200, None)
                } else {
                    (200, 240, None)
                })
            }),
            Ok((0, 0, 90))
        );
    }

    #[test]
    fn zoom_anchors_the_narrow_center_page_at_its_actual_crop() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        // Page one begins within the viewport and permits shared scroll_x=100,
        // but the centered page zero is only 120px wide and crops at x=20.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (100, 0),
                zoom_size(100, 200),
                true,
                |page| {
                    Some(if page == 0 {
                        (120, 60, None)
                    } else {
                        (300, 180, None)
                    })
                }
            ),
            Ok((0, 90, 40)),
        );
    }

    #[test]
    fn zoom_identifies_uncached_predecessor_and_keeps_short_page_center() {
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        assert_eq!(
            centered_scaled_view(viewport, 1, 2, (0, 0), zoom_size(200, 100), true, |page| {
                (page == 1).then_some((200, 160, None))
            }),
            Err(0)
        );
        assert_eq!(
            centered_scaled_view(viewport, 0, 1, (0, 0), zoom_size(100, 125), true, |_| Some(
                (100, 70, None)
            )),
            Ok((0, 13, 8))
        );
    }

    #[test]
    fn content_zoom_keeps_the_center_across_differently_indented_pages() {
        let viewport = Viewport {
            columns: 8,
            rows: 10,
            pixel_width: 80,
            pixel_height: 200,
            top: 0,
            status_row: 10,
        };
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (480, 650),
                |page, _, _| Some((800, 800, Some(if page == 0 { 610 } else { 110 }))),
                true,
                |page| Some((640, 640, Some(if page == 0 { 480 } else { 80 }))),
            ),
            Ok((1, 110, 13))
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (110, 13),
                |page, _, _| Some((640, 640, Some(if page == 0 { 480 } else { 80 }))),
                true,
                |page| Some((800, 800, Some(if page == 0 { 610 } else { 110 }))),
            ),
            Ok((0, 480, 650))
        );
    }

    #[test]
    fn content_zoom_preserves_pan_with_a_narrow_preceding_page() {
        let viewport = Viewport {
            columns: 8,
            rows: 10,
            pixel_width: 80,
            pixel_height: 200,
            top: 0,
            status_row: 10,
        };
        let position = centered_scaled_view(
            viewport,
            1,
            2,
            (320, 0),
            |page, _, _| {
                Some(if page == 0 {
                    (80, 80, Some(0))
                } else {
                    (640, 640, Some(280))
                })
            },
            true,
            |page| {
                Some(if page == 0 {
                    (100, 100, Some(0))
                } else {
                    (800, 800, Some(360))
                })
            },
        )
        .unwrap();
        assert_eq!(position, (0, -32, 80));
        let (mut app, _, _file) = continuous_app();
        app.tab_mut().fit = crate::pdf::FitMode::Content;
        app.tab_mut().center_content = false;
        let revision = app.tab().cache.values().next().unwrap().revision;
        for (page, size, bounds) in [
            (0, 80, [0.0, 0.0, 1.0, 1.0]),
            (1, 640, [0.4, 0.4, 0.6, 0.6]),
        ] {
            let key = app.page_key(page, viewport);
            let mut frame = continuous_frame(key, revision, false);
            let data = std::sync::Arc::get_mut(&mut frame).unwrap();
            data.width = size;
            data.height = size;
            data.content_bounds = bounds;
            app.tab_mut().cache.insert(key, frame);
        }
        app.tab_mut().page = position.0;
        app.tab_mut().scroll_x = position.1;
        app.tab_mut().scroll_y = position.2;
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.tab().scroll_x, -32);
        assert_eq!(app.visible_pages[0].frame.key.page, 1);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 248);
        app.pending_vertical_scroll = 20;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!((app.tab().page, app.tab().scroll_x), (1, 248));
        app.pending_vertical_scroll = -20;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!((app.tab().page, app.tab().scroll_x), (0, -32));
    }

    #[test]
    fn content_fit_centers_asymmetric_margins_only_once() {
        let (mut app, viewport, _file) = continuous_app();
        app.tab_mut().fit = crate::pdf::FitMode::Content;
        let key = app.render_key(viewport);
        let mut frame = continuous_frame(
            key,
            app.tab().cache.values().next().unwrap().revision,
            false,
        );
        let data = std::sync::Arc::get_mut(&mut frame).unwrap();
        data.width = 400;
        data.height = 800;
        data.content_bounds = [0.6, 0.5, 0.9, 0.75];
        app.tab_mut().center_content = true;
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (260, 400));
        app.tab_mut().scroll_x = 200;
        app.tab_mut().scroll_y = 350;
        app.reset_render_state();
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (200, 350));

        let page_config = toml::from_str("fit_mode = \"page\"").unwrap();
        let content_config = toml::from_str("fit_mode = \"content\"").unwrap();
        app.apply_config(&page_config);
        app.apply_config(&content_config);
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (260, 400));
        app.tab_mut().scroll_x = 200;
        app.tab_mut().scroll_y = 350;
        app.apply_config(&content_config);
        app.position_content_frame(&frame, viewport);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (200, 350));
    }

    #[test]
    fn forward_history_restores_pending_content_centering() {
        let (mut app, viewport, _file) = continuous_app();
        app.tab_mut().fit = crate::pdf::FitMode::Content;
        app.tab_mut().center_content = false;
        let origin = super::ViewPosition {
            page: 0,
            scroll_x: 37,
            scroll_y: 91,
            center_content: false,
        };
        let destination = super::ViewPosition {
            page: 1,
            center_content: true,
            ..origin
        };
        app.tab_mut().link_history.jump(origin, destination);
        app.tab_mut().link_history.backward(destination).unwrap();
        let key = app.page_key(1, viewport);
        let frame = continuous_frame(
            key,
            app.tab().cache.values().next().unwrap().revision,
            false,
        );
        app.tab_mut().cache.insert(key, frame);
        app.follow_link_forward(&mut Vec::new()).unwrap();
        assert_eq!(app.tab().page, 1);
        assert_eq!((app.tab().scroll_x, app.tab().scroll_y), (0, 20));
    }

    #[test]
    fn content_follow_link_preserves_omitted_axes() {
        let (mut app, viewport, _file) = continuous_app();
        app.canvas_viewport = Some(viewport);
        app.tab_mut().page_count = 12;
        app.tab_mut().fit = crate::pdf::FitMode::Content;
        app.tab_mut().center_content = false;
        let revision = app.tab().cache.values().next().unwrap().revision;
        for page in [0, 1, 2, 3, 4] {
            let key = app.page_key(page, viewport);
            let mut frame = continuous_frame(key, revision, false);
            let data = std::sync::Arc::get_mut(&mut frame).unwrap();
            data.width = 400;
            data.height = 800;
            data.content_bounds = [0.6, 0.5, 0.9, 0.75];
            app.tab_mut().cache.insert(key, frame);
        }

        // /FitH top (left_ratio == None): preserve content-relative pan and apply top_ratio.
        app.tab_mut().scroll_x = 120;
        app.tab_mut().scroll_y = 310;
        app.follow_link(
            crate::pdf::LinkTarget::Internal {
                page: 1,
                top_ratio: Some(0.25),
                left_ratio: None,
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (1, 120, 200)
        );

        // Cross-page (None, None) destination onto differently indented page 2:
        // rebase content-relative horizontal pan (-140px from 260 -> -80px from 60) and preserve scroll_y.
        let key2 = app.page_key(2, viewport);
        std::sync::Arc::get_mut(app.tab_mut().cache.get_mut(&key2).unwrap())
            .unwrap()
            .content_bounds = [0.1, 0.5, 0.4, 0.75];
        app.follow_link(
            crate::pdf::LinkTarget::Internal {
                page: 2,
                top_ratio: None,
                left_ratio: None,
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (2, -80, 200)
        );

        // /FitV left (top_ratio == None): start from target page's content origin and reveal left_ratio,
        // while preserving scroll_y.
        app.follow_link(
            crate::pdf::LinkTarget::Internal {
                page: 3,
                top_ratio: None,
                left_ratio: Some(0.65),
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (3, 260, 200)
        );

        // Full (Some(left), Some(top)) destination: center content horizontally first, then reveal.
        app.follow_link(
            crate::pdf::LinkTarget::Internal {
                page: 4,
                top_ratio: Some(0.5),
                left_ratio: Some(0.1),
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (4, 40, 400)
        );

        // Uncached distant cross-page /FitH link (page 4 -> page 10): receive_frame evicts page 4
        // from tab.cache before resolving the destination, yet content-relative pan (-220px from 260)
        // must still rebase onto page 10's content center (60 + (-220) = -160).
        assert!(matches!(
            app.follow_link(
                crate::pdf::LinkTarget::Internal {
                    page: 10,
                    top_ratio: Some(0.25),
                    left_ratio: None,
                },
                &mut Vec::new(),
            ),
            Err(super::AppError::Renderer(_))
        ));
        let key10 = app.page_key(10, viewport);
        let mut frame10 = std::sync::Arc::try_unwrap(continuous_frame(key10, revision, false))
            .expect("unique synthetic frame");
        frame10.width = 400;
        frame10.height = 800;
        frame10.content_bounds = [0.1, 0.5, 0.4, 0.75];
        frame10.generation = app.generation;
        app.receive_frame(frame10, &mut Vec::new()).unwrap();
        assert!(!app.tab().cache.contains_key(&app.page_key(4, viewport)));
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (10, -160, 200)
        );

        // Chaining through an unrendered intermediate page (10 -> uncached 6 -> cached 10)
        // must preserve the signed content-relative pan instead of resetting to content center.
        assert!(matches!(
            app.follow_link(
                crate::pdf::LinkTarget::Internal {
                    page: 6,
                    top_ratio: Some(0.5),
                    left_ratio: None,
                },
                &mut Vec::new(),
            ),
            Err(super::AppError::Renderer(_))
        ));
        app.follow_link(
            crate::pdf::LinkTarget::Internal {
                page: 10,
                top_ratio: Some(0.75),
                left_ratio: None,
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (10, -160, 600)
        );
    }

    #[test]
    fn content_scrolling_keeps_differently_indented_pages_visible() {
        let (mut app, viewport, _file) = continuous_app();
        app.tab_mut().fit = crate::pdf::FitMode::Content;
        app.tab_mut().center_content = false;
        let revision = app.tab().cache.values().next().unwrap().revision;
        for (page, bounds) in [(0, [0.7, 0.4, 0.9, 0.6]), (1, [0.1, 0.4, 0.3, 0.6])] {
            let key = app.page_key(page, viewport);
            let mut frame = continuous_frame(key, revision, false);
            let data = std::sync::Arc::get_mut(&mut frame).unwrap();
            data.width = 400;
            data.height = 400;
            data.content_bounds = bounds;
            app.tab_mut().cache.insert(key, frame);
        }
        let primary = app.tab().cache[&app.page_key(0, viewport)].clone();
        app.tab_mut().scroll_x = super::content_scroll(&primary, viewport).0 + 10;
        app.tab_mut().scroll_y = 350;
        app.draw_continuous(&primary, viewport, &mut Vec::new())
            .unwrap();
        assert_eq!(app.visible_pages.len(), 2);
        assert_eq!(app.visible_pages[0].placement.scroll_x, 290);
        assert_eq!(app.visible_pages[1].placement.scroll_x, 50);
        assert_eq!(app.tab().scroll_x, 290);

        app.pending_vertical_scroll = 70;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (1, 50, 10)
        );
        app.pending_vertical_scroll = -70;
        assert!(app.apply_vertical_scroll(viewport).unwrap());
        assert_eq!(
            (app.tab().page, app.tab().scroll_x, app.tab().scroll_y),
            (0, 290, 350)
        );
    }

    #[test]
    fn fit_cycle_preserves_zoomed_portrait_and_landscape_centers() {
        use crate::pdf::FitMode;
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        for (size, scroll, fit, expected) in [
            ((80, 160), (0, 40), FitMode::Width, (0, 50, 160)),
            ((200, 400), (50, 160), FitMode::Height, (0, 0, 40)),
            ((80, 160), (0, 40), FitMode::Page, (0, 0, 40)),
            ((200, 80), (50, 0), FitMode::Width, (0, 50, 0)),
            ((200, 80), (50, 0), FitMode::Height, (0, 150, 40)),
            ((400, 160), (150, 40), FitMode::Page, (0, 50, 0)),
        ] {
            assert_eq!(
                centered_scaled_view(
                    viewport,
                    0,
                    1,
                    scroll,
                    |_, width, height| {
                        let (width, height) = super::fitted_page_size(
                            viewport,
                            width as f32,
                            height as f32,
                            [0.0, 0.0, 1.0, 1.0],
                            fit,
                            200,
                        );
                        Some((width, height, None))
                    },
                    false,
                    |_| Some((size.0, size.1, None)),
                ),
                Ok(expected),
                "{fit:?}, {size:?}"
            );
        }
    }

    #[test]
    fn fit_cycle_keeps_center_on_next_page_with_different_aspect_ratio() {
        use crate::pdf::FitMode;
        let viewport = Viewport {
            columns: 10,
            rows: 8,
            pixel_width: 100,
            pixel_height: 80,
            top: 0,
            status_row: 8,
        };
        // The center is 25px down page one, not on the first visible landscape page.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                2,
                (0, 35),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Width,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| Some(if page == 0 {
                    (100, 40, None)
                } else {
                    (40, 80, None)
                }),
            ),
            Ok((1, 0, 23))
        );
        // Shrinking that portrait page must fetch its predecessor before moving the view.
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (0, 23),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Height,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| (page == 1).then_some((100, 200, None)),
            ),
            Err(0)
        );
        assert_eq!(
            centered_scaled_view(
                viewport,
                1,
                2,
                (0, 23),
                |_, width, height| Some(super::fitted_page_size(
                    viewport,
                    width as f32,
                    height as f32,
                    [0.0, 0.0, 1.0, 1.0],
                    FitMode::Height,
                    100
                ))
                .map(|(width, height)| (width, height, None)),
                true,
                |page| Some(if page == 0 {
                    (100, 40, None)
                } else {
                    (100, 200, None)
                }),
            ),
            Ok((0, 0, 75))
        );
    }

    #[test]
    fn fit_cycle_preserves_center_when_old_raster_rounds_the_aspect_ratio() {
        let viewport = Viewport {
            columns: 50,
            rows: 40,
            pixel_width: 500,
            pixel_height: 800,
            top: 0,
            status_row: 40,
        };
        // Fit-page rounds a 210x10000pt page to 17x800px. Recovering its
        // aspect ratio from those pixels would shift the center by 140px.
        assert_eq!(
            centered_scaled_view(
                viewport,
                0,
                1,
                (0, 0),
                |_, _, _| Some(super::fitted_page_size(
                    viewport,
                    210.0,
                    10000.0,
                    [0.0, 0.0, 1.0, 1.0],
                    crate::pdf::FitMode::Width,
                    100,
                ))
                .map(|(width, height)| (width, height, None)),
                false,
                |_| Some((17, 800, None)),
            ),
            Ok((0, 0, 11505))
        );
    }

    #[test]
    fn picker_disambiguates_recent_files_with_long_parent_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let current = directory.path().join("working");
        fs::create_dir(&current).unwrap();
        let mut recents = Vec::new();
        for name in ["alpha", "beta"] {
            let parent = directory.path().join("long-prefix-".repeat(8)).join(name);
            fs::create_dir_all(&parent).unwrap();
            let file = parent.join("same.pdf");
            fs::write(&file, b"synthetic").unwrap();
            recents.push(file);
        }
        let mut browser = BrowserState::new(current);
        browser.set_recents(recents);
        let area = Rect::new(0, 0, 80, 30);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        for filter in ["", "same"] {
            browser.filter = filter.into();
            browser.rebuild_filter();
            terminal
                .draw(|frame| {
                    draw_picker(
                        frame,
                        &browser,
                        !filter.is_empty(),
                        crate::theme::TOKYO_NIGHT_MOON,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..area.height)
                .map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            for parent in ["alpha", "beta"] {
                assert!(
                    rows.iter()
                        .any(|row| row.contains("same.pdf") && row.contains(parent)),
                    "identical filenames must retain their distinct parent: {rows:?}"
                );
            }
        }
    }

    #[test]
    fn closing_picker_clears_its_terminal_buffer() {
        let mut output = Vec::new();

        clear_picker(&mut output, crate::theme::TOKYO_NIGHT_MOON).expect("clear picker");

        assert!(output.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(output.windows(6).any(|window| window == b"\x1b[?25l"));
    }

    #[test]
    fn image_canvas_uses_terminal_background_without_a_scaled_graphics_layer() {
        let mut output = Vec::new();
        let viewport = Viewport {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
            top: 0,
            status_row: 24,
        };

        clear_image_canvas(&mut output, viewport).expect("prepare canvas");

        assert!(!output.windows(3).any(|window| window == b"\x1b_"));
        assert!(output.windows(4).any(|window| window == b"\x1b[0m"));
        assert_eq!(
            output
                .windows(4)
                .filter(|window| *window == b"\x1b[2K")
                .count(),
            usize::from(viewport.rows)
        );
    }

    #[test]
    fn link_picker_uses_grimoire_auto_split() {
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 50, 30), Rect::new(50, 1, 50, 30))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 80, 50),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 80, 25), Rect::new(0, 26, 80, 25))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(30, LinkPickerLayout::Auto),
            ),
            (Rect::new(0, 1, 70, 30), Rect::new(70, 1, 30, 30))
        );
    }

    #[test]
    fn link_picker_supports_forced_and_floating_layouts() {
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 80, 50),
                LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            ),
            (Rect::new(0, 1, 40, 50), Rect::new(40, 1, 40, 50))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Horizontal),
            ),
            (Rect::new(0, 1, 100, 15), Rect::new(0, 16, 100, 15))
        );
        assert_eq!(
            link_picker_panes(
                Rect::new(0, 1, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Floating),
            ),
            (Rect::new(0, 1, 100, 30), Rect::new(12, 5, 75, 22))
        );
    }

    #[test]
    fn link_picker_repositions_the_retained_page_without_retransmitting_it() {
        let mut output = Vec::new();
        let area = Rect::new(0, 0, 100, 30);
        let theme = crate::theme::TOKYO_NIGHT_MOON;
        let image = LinkPickerImage {
            image_id: 12,
            source_width: 600,
            source_height: 800,
            crop: None,
            cell_width: 10,
            cell_height: 20,
            original: PositionedImage {
                left: 20,
                top: 0,
                placement: Placement {
                    image_id: 12,
                    columns: 60,
                    rows: 30,
                    offset_y: 0,
                    z_index: super::PAGE_IMAGE_Z_INDEX,
                    crop: None,
                },
            },
        };

        show_link_picker_split(
            &mut output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            theme,
        )
        .expect("show link picker split");
        restore_link_picker_split(
            &mut output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Vertical),
            theme,
        )
        .expect("restore link picker split");

        let output = String::from_utf8(output).expect("terminal output");
        assert!(output.contains("a=p,i=12,p=1,c=45,r=30"));
        assert!(output.contains("a=p,i=12,p=1,c=60,r=30"));
        assert_eq!(output.matches("a=p,i=12").count(), 2);
        assert!(!output.contains("a=T"));
        assert!(!output.contains("\x1b[2J"));

        let mut floating_output = Vec::new();
        show_link_picker_split(
            &mut floating_output,
            area,
            image,
            LinkPickerGeometry::new(50, LinkPickerLayout::Floating),
            theme,
        )
        .expect("show floating link picker");
        let floating_output = String::from_utf8(floating_output).expect("terminal output");
        assert!(!floating_output.contains("a=p"));
        assert!(!floating_output.contains("a=T"));
    }
    #[test]
    fn tiny_visible_text_keeps_flash_labels_readable() {
        assert_eq!(super::badge_glyph_size(3), 8);
        assert_eq!(super::badge_glyph_size(10), 8);
        assert_eq!(super::badge_glyph_size(18), 16);
    }

    #[test]
    fn jump_label_overlay_is_cropped_and_screenshots_render_it_on_demand() {
        let (mut app, viewport, _file) = continuous_app();
        let key = app.visible_pages[0].frame.key;
        app.flash_font = Some(crate::screenshot::FlashFont::load("monospace").unwrap());
        app.label_matches.push(super::LabeledMatch {
            visible: crate::pdf::VisibleMatch {
                key,
                char_index: 0,
                rects: vec![crate::pdf::PixelRect {
                    left: 20,
                    right: 35,
                    top: 150,
                    bottom: 158,
                }],
                hit: (25, 154),
                anchor: (25, 154),
                next_char: None,
            },
            label: "q".into(),
        });
        let overlay = app.label_overlay_pixels(viewport).unwrap().unwrap();
        assert_eq!((overlay.col, overlay.row), (2, 5));
        assert!(
            overlay.rgba.len()
                < usize::from(viewport.pixel_width) * usize::from(viewport.pixel_height) * 4
        );
        let mut output = Vec::new();
        app.draw_label_overlay(&mut output).unwrap();
        assert!(app.label_overlay_id.is_some());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(&format!(
            "\x1b[{};{}H",
            viewport.top + overlay.row + 1,
            overlay.col + 1
        )));
        assert!(output.contains(&format!(
            "s={},v={}",
            overlay.pixel_width, overlay.pixel_height
        )));
        assert!(output.contains(&format!(",c={},r={}", overlay.columns, overlay.rows)));

        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.png");
        app.save_screenshot(&active).unwrap();
        app.clear_label_overlay(&mut Vec::new()).unwrap();
        let inactive = directory.path().join("inactive.png");
        app.save_screenshot(&inactive).unwrap();
        let pixels = |path| {
            let mut reader =
                png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                    .read_info()
                    .unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            reader.next_frame(&mut pixels).unwrap();
            pixels
        };
        let active = pixels(active);
        let inactive = pixels(inactive);
        let width = usize::from(viewport.pixel_width);
        let x0 = usize::from(overlay.col) * 10;
        let y0 = usize::from(overlay.row) * 10;
        assert_ne!(active, inactive);
        for y in 0..usize::from(viewport.pixel_height) {
            for x in 0..width {
                let offset = (y * width + x) * 4;
                let mut expected: [u8; 4] = inactive[offset..offset + 4].try_into().unwrap();
                if (x0..x0 + usize::from(overlay.pixel_width)).contains(&x)
                    && (y0..y0 + usize::from(overlay.pixel_height)).contains(&y)
                {
                    let src = ((y - y0) * usize::from(overlay.pixel_width) + x - x0) * 4;
                    let source = &overlay.rgba[src..src + 4];
                    let alpha = u32::from(source[3]);
                    for channel in 0..3 {
                        expected[channel] = ((u32::from(source[channel]) * alpha
                            + u32::from(expected[channel]) * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
                assert_eq!(&active[offset..offset + 4], &expected);
            }
        }
        app.label_overlay_id = Some(99);
        app.label_matches.clear();
        app.flash_font = None;
        app.save_screenshot(&directory.path().join("empty.png"))
            .unwrap();
    }

    #[test]
    fn match_projection_clamps_frame_bounds_for_native_and_scaled_placements() {
        let viewport = Viewport {
            columns: 10,
            rows: 10,
            pixel_width: 100,
            pixel_height: 100,
            top: 2,
            status_row: 12,
        };
        let mut placement = ImagePlacement {
            left: 1,
            columns: 4,
            rows: 4,
            crop: Some(crate::kitty::Crop {
                x: 10,
                y: 20,
                width: 40,
                height: 40,
            }),
            scroll_x: 10,
            scroll_y: 20,
            native_cell: Some((10, 10)),
            offset_y: 3,
        };
        let rect = crate::pdf::PixelRect {
            left: 20,
            top: 30,
            right: 60,
            bottom: 70,
        };
        for native in [true, false] {
            placement.native_cell = native.then_some((10, 10));
            placement.columns = if native { 4 } else { 8 };
            let projected =
                super::project_match_rect(&rect, placement, 3, (35, 45), viewport, (10, 10))
                    .unwrap();
            assert_eq!((projected.source.right, projected.source.bottom), (35, 45));
            assert_eq!(
                (projected.x, projected.y, projected.width, projected.height),
                if native {
                    (20, 23, 15, 15)
                } else {
                    (30, 20, 30, 15)
                }
            );
            assert_eq!(
                super::visible_match_points(&[rect], placement, 3, 35, 45, viewport, (10, 10)),
                Some(((27, 37), (27, 37)))
            );
        }
        assert!(
            super::project_match_rect(&rect, placement, 3, (10, 20), viewport, (10, 10)).is_none()
        );
    }

    #[test]
    fn visible_match_targets_only_displayed_glyphs() {
        use crate::pdf::PixelRect;

        let viewport = Viewport {
            columns: 10,
            rows: 2,
            pixel_width: 100,
            pixel_height: 20,
            top: 0,
            status_row: 2,
        };
        let placement = ImagePlacement {
            left: 0,
            columns: 10,
            rows: 2,
            crop: Some(crate::kitty::Crop {
                x: 0,
                y: 10,
                width: 120,
                height: 20,
            }),
            scroll_x: 0,
            scroll_y: 10,
            native_cell: Some((10, 10)),
            offset_y: 0,
        };
        let rects = [
            PixelRect {
                left: 10,
                top: 0,
                right: 20,
                bottom: 10,
            },
            PixelRect {
                left: 90,
                top: 20,
                right: 110,
                bottom: 30,
            },
        ];
        assert_eq!(
            super::visible_match_points(&rects, placement, 0, 120, 40, viewport, (10, 10)),
            Some(((95, 25), (95, 25)))
        );
    }

    #[test]
    fn link_picker_resets_preview_and_reindexes_without_link_mode() {
        let (mut app, _viewport, _file) = continuous_app();
        app.link_mode = false;
        let mut picker = LinkPickerState::new(0);
        picker.filter = "keep".into();
        picker.number_input = "12".into();
        picker.selection_key = Some((0, 3));
        picker.awaiting_current_page = false;
        picker.pending_preview = Some(super::PendingLinkPreview {
            selection_key: (0, 3),
            ready_at: Instant::now(),
        });
        picker.preview_origin = Some(super::ViewPosition {
            page: 0,
            scroll_x: 10,
            scroll_y: 20,
            center_content: false,
        });
        app.link_picker = Some(picker);
        app.search_picker = Some(SearchPickerState::new(0));
        app.tab_mut().page = 1;

        app.reset_active_tab_pickers();
        app.ensure_link_index();

        assert!(app.search_picker.is_none());
        let picker = app.link_picker.as_ref().unwrap();
        assert_eq!(picker.page, 1);
        assert_eq!(picker.filter, "keep");
        assert!(picker.number_input.is_empty());
        assert!(picker.selection_key.is_none());
        assert!(picker.awaiting_current_page);
        assert!(picker.pending_preview.is_none());
        assert!(picker.preview_origin.is_none());
        assert!(app.tab().link_index.started());
    }

    #[test]
    fn apply_config_updates_viewer_and_tab_defaults() {
        let (mut app, _viewport, _file) = continuous_app();
        let config: crate::config::Config = toml::from_str(
            "fit_mode = \"width\"\ndark_mode = true\npersistent_link_picker = true\n\
             [viewer]\nscroll_step_percent = 8\nsmooth_scroll = true\n",
        )
        .expect("config");
        app.apply_config(&config);
        assert_eq!(app.viewer.scroll_step_percent, 8);
        assert!(app.viewer.smooth_scroll);
        assert!(app.persistent_link_picker);
        assert_eq!(app.default_fit, crate::pdf::FitMode::Width);
        assert!(app.default_invert);
        assert_eq!(app.tab().fit, crate::pdf::FitMode::Width);
        assert!(app.tab().invert);
    }

    #[test]
    fn classic_vim_zathura_motions_navigate_pages_and_history() {
        let (mut app, viewport, file) = continuous_app();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        app.tab_mut().page_count = 15;
        app.pending.clear();
        for page in 0..15 {
            let key = app.page_key(page, viewport);
            app.tab_mut()
                .cache
                .insert(key, continuous_frame(key, revision, false));
        }
        let mut output = Vec::new();

        // J / K step pages directly even in continuous_scroll mode
        app.handle_key(
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 1);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 0);

        // [count]G jumps to page
        for key in [
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        ] {
            app.handle_key(key, &mut output).unwrap();
        }
        assert_eq!(app.tab().page, 11);
        assert!(app.goto_input.is_none());

        // [count]gg jumps to page without the second 'g' hitting normal mode
        for key in [
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        ] {
            app.handle_key(key, &mut output).unwrap();
        }
        assert_eq!(app.tab().page, 13);
        assert!(app.goto_input.is_none());

        // [count]<Enter> jumps to page
        for key in [
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            app.handle_key(key, &mut output).unwrap();
        }
        assert_eq!(app.tab().page, 2);

        // Jump list (b / Ctrl-o / Ctrl-i) across links, SyncTeX forward search, and manual scroll
        app.tab_mut().scroll_y = 0;
        app.follow_link(
            LinkTarget::Internal {
                page: 7,
                top_ratio: Some(0.0),
                left_ratio: None,
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 7);
        app.follow_link(
            LinkTarget::Internal {
                page: 11,
                top_ratio: Some(0.0),
                left_ratio: None,
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 11);

        let _ = app.apply_forward_request(
            &crate::synctex::ForwardRequest {
                pdf: file.path().to_owned(),
                revision: revision.pdf,
                page: 5,
                h: 10.0,
                v: 20.0,
                width: 10.0,
                height: 10.0,
                word: None,
                inverse_search: None,
                best_guess: false,
                warning: None,
            },
            0,
            &mut output,
        );
        let forward_key = app.page_key(4, viewport);
        app.tab_mut()
            .cache
            .insert(forward_key, continuous_frame(forward_key, revision, false));
        assert_eq!(app.tab().page, 4);

        // b / Ctrl-o walk backward: 4 -> 11 -> 7 -> 2
        app.handle_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 11);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 7);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 2);

        // B / Ctrl-i walk forward: 2 -> 7 -> 11 -> 4
        for (key, expected) in [
            (KeyEvent::new(KeyCode::Char('B'), KeyModifiers::NONE), 7),
            (KeyEvent::new(KeyCode::Char('b'), KeyModifiers::SHIFT), 11),
            (KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL), 4),
        ] {
            app.handle_key(key, &mut output).unwrap();
            assert_eq!(app.tab().page, expected);
        }

        // Manual scroll from tip (4 -> 5), then Ctrl-o returns to 4 first, and Ctrl-i returns to 5
        app.handle_key(
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 5);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 4);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL),
            &mut output,
        )
        .unwrap();
        assert_eq!(app.tab().page, 5);
        app.tab_mut().link_history.clamp_pages(10);
        assert!(
            app.tab()
                .link_history
                .entries
                .iter()
                .all(|entry| entry.page < 10)
        );

        // d / u and Ctrl-d / Ctrl-u / Ctrl-f / Ctrl-b page viewport
        app.viewer.continuous_scroll = false;
        app.tab_mut().scroll_y = 0;
        for (down, up) in [
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            ),
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            ),
            (
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            ),
        ] {
            let before = app.tab().page;
            app.handle_key(down, &mut output).unwrap();
            assert_eq!((app.tab().page, app.tab().scroll_y), (before, 40));
            app.handle_key(up, &mut output).unwrap();
            assert_eq!((app.tab().page, app.tab().scroll_y), (before, 0));
        }

        // Picker Ctrl-d / Ctrl-u paging
        assert_eq!(
            picker_navigation_index(
                2,
                20,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                5,
                true,
                ListEdge::Wrap
            ),
            Some(7)
        );
        assert_eq!(
            picker_navigation_index(
                7,
                20,
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
                5,
                true,
                ListEdge::Wrap
            ),
            Some(2)
        );
    }

    #[test]
    fn fuzz_config_combinations_and_live_viewer_transitions() {
        let (mut app, viewport, file) = continuous_app();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        app.tab_mut().page_count = 6;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let fit_modes = ["page", "width", "height"];
        let dark_modes = ["dark_mode = false", "dark_mode = true", "invert = true"];
        let themes = [
            "",
            "theme = \"\"",
            "theme = \"Tokyo Night\"",
            "theme = \"Catppuccin Latte\"",
            "theme = \"/nonexistent/theme.toml\"",
        ];
        let catalogs = [
            "",
            "theme_catalog = \"\"",
            "theme_catalog = \"/nonexistent/catalog.toml\"",
        ];
        let split_percents = [
            "",
            "link_picker_split_percent = 0",
            "link_picker_split_percent = 20",
            "link_picker_split_percent = 50",
            "link_picker_split_percent = 80",
            "link_picker_split_percent = 100",
        ];
        let layouts = ["auto", "vertical", "horizontal", "floating"];
        let synctex_flags = ["", "synctex_enabled = false", "synctex_enabled = true"];
        let editors = [
            "[editor]\ntransport = \"none\"",
            "[editor]\ntransport = \"socket\"\npath = \"editor.sock\"",
            "[editor]\ntransport = \"command\"\nargv = [\"nvim\", \"+{line}\", \"{file}\"]",
        ];
        let forward_sockets = [
            "",
            "forward_socket = \"\"",
            "forward_socket = \"forward.sock\"",
        ];
        let prefetch_values = [0u64, 1, 3, u32::MAX as u64];
        let step_percents = [1u64, 5, 100];
        let page_percents = [1u64, 85, 100];
        let ease_divisors = [1u64, 4, 100];
        let frame_ms_values = [1u64, 16, 1000];
        let flash_durations = [1u64, 1000, 60000];
        let context_lines = [0u64, 4, 100];
        let flash_fonts = ["monospace", "", "nonexistent-font"];

        let mut output = Vec::new();
        for i in 0..96usize {
            let fit = fit_modes[i % fit_modes.len()];
            let dark = dark_modes[(i / 3) % dark_modes.len()];
            let theme = themes[(i / 5) % themes.len()];
            let catalog = catalogs[(i / 7) % catalogs.len()];
            let persistent = i % 2 == 1;
            let split = split_percents[(i / 2) % split_percents.len()];
            let layout = layouts[(i / 4) % layouts.len()];
            let synctex = synctex_flags[(i / 6) % synctex_flags.len()];
            let editor = editors[(i / 8) % editors.len()];
            let forward_socket = forward_sockets[(i / 9) % forward_sockets.len()];
            let continuous = (i / 2) % 2 == 0;
            let smooth = (i / 3) % 2 == 1;
            let prefetch = prefetch_values[(i / 4) % prefetch_values.len()];
            let step_pct = step_percents[(i / 5) % step_percents.len()];
            let page_pct = page_percents[(i / 6) % page_percents.len()];
            let ease_div = ease_divisors[(i / 7) % ease_divisors.len()];
            let frame_ms = frame_ms_values[(i / 8) % frame_ms_values.len()];
            let set_title = (i / 5) % 2 == 0;
            let center_forward = (i / 6) % 2 == 0;
            let flash_ms = flash_durations[(i / 9) % flash_durations.len()];
            let font = flash_fonts[(i / 10) % flash_fonts.len()];
            let word_precision = (i / 7) % 2 == 0;
            let ctx_lines = context_lines[(i / 11) % context_lines.len()];
            let nvim_bool = i % 2 == 0;

            let toml_text = format!(
                "fit_mode = \"{fit}\"\n\
                 {dark}\n\
                 {theme}\n\
                 {catalog}\n\
                 persistent_link_picker = {persistent}\n\
                 {split}\n\
                 link_picker_layout = \"{layout}\"\n\
                 {synctex}\n\
                 {forward_socket}\n\
                 {editor}\n\
                 [viewer]\n\
                 continuous_scroll = {continuous}\n\
                 prefetch_pages = {prefetch}\n\
                 smooth_scroll = {smooth}\n\
                 scroll_frame_ms = {frame_ms}\n\
                 scroll_ease_divisor = {ease_div}\n\
                 scroll_step_percent = {step_pct}\n\
                 page_scroll_percent = {page_pct}\n\
                 set_window_title = {set_title}\n\
                 center_forward_search = {center_forward}\n\
                 flash_duration_ms = {flash_ms}\n\
                 flash_label_font = \"{font}\"\n\
                 flash_label_foreground = \"#000000\"\n\
                 flash_label_background = \"#c0caf5\"\n\
                 word_precision = {word_precision}\n\
                 source_context_lines = {ctx_lines}\n\
                 [nvim]\n\
                 compile = {nvim_bool}\n\
                 focus_on_inverse = {nvim_bool}\n\
                 focus_on_forward = {nvim_bool}\n\
                 attach_only = {nvim_bool}\n\
                 executable = \"nvim\"\n"
            );
            std::fs::write(&config_path, &toml_text).unwrap();
            let config = crate::config::Config::load_path(&config_path).unwrap();

            let mut link_state = LinkPickerState::new(app.tab().page);
            link_state.focus = LinkPickerFocus::Document;
            app.link_picker = if i % 3 == 0 { Some(link_state) } else { None };
            let mut search_state = SearchPickerState::new(app.tab().page);
            search_state.focus = LinkPickerFocus::Document;
            app.search_picker = if i % 5 == 0 && app.link_picker.is_none() {
                Some(search_state)
            } else {
                None
            };

            app.apply_config(&config);
            if app.link_picker_geometry.layout == LinkPickerLayout::Floating {
                assert!(
                    app.link_picker
                        .as_ref()
                        .is_none_or(|state| state.focus == LinkPickerFocus::Links)
                );
                assert!(
                    app.search_picker
                        .as_ref()
                        .is_none_or(|state| state.focus == LinkPickerFocus::Links)
                );
            }
            if !app.viewer.smooth_scroll {
                assert_eq!(app.smooth_scroll_remaining, 0);
            }
            if !app.viewer.set_window_title {
                assert!(app.title_document.is_none());
            }

            app.link_picker = None;
            app.search_picker = None;
            app.canvas_viewport = Some(viewport);
            app.reset_render_state();
            for page in 0..6 {
                let key = app.page_key(page, viewport);
                app.tab_mut()
                    .cache
                    .insert(key, continuous_frame(key, revision, false));
            }
            app.request_current(&mut output).unwrap();
            let key = app.render_key(viewport);
            app.receive_frame(
                std::sync::Arc::into_inner(continuous_frame(key, revision, false)).unwrap(),
                &mut output,
            )
            .unwrap();

            // Exercise small & large vertical/horizontal scrolling + smooth scroll tick
            for page in 0..6 {
                let key = app.page_key(page, viewport);
                app.tab_mut()
                    .cache
                    .insert(key, continuous_frame(key, revision, false));
            }
            app.move_view(super::Axis::Vertical, true, false, true, &mut output)
                .unwrap();
            app.move_view(super::Axis::Vertical, true, true, true, &mut output)
                .unwrap();
            app.move_view(super::Axis::Horizontal, true, false, false, &mut output)
                .unwrap();
            app.smooth_scroll_tick = Instant::now() - std::time::Duration::from_secs(2);
            app.poll_smooth_scroll(&mut output).unwrap();
            app.move_view(super::Axis::Vertical, false, false, true, &mut output)
                .unwrap();
        }
    }

    #[test]
    fn same_page_jump_and_clamped_history_preserve_return_positions() {
        use super::{JumpList, ViewPosition};

        let origin = ViewPosition {
            page: 0,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        };
        let destination = ViewPosition {
            page: 0,
            scroll_x: 0,
            scroll_y: 120,
            center_content: false,
        };
        let mut history = JumpList::default();
        history.jump(origin, origin);
        history.update_tip(destination);
        assert_eq!(history.backward(destination), Some(origin));
        assert_eq!(history.forward(), Some(destination));

        // Clamping pages that collapse duplicate entries before the cursor must keep the cursor on
        // the same active entry rather than shifting it forward.
        let mut clamped = JumpList::default();
        let p5 = ViewPosition {
            page: 5,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        };
        let p4 = ViewPosition {
            page: 4,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        };
        let p1 = ViewPosition {
            page: 1,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        };
        let p0 = ViewPosition {
            page: 0,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        };
        clamped.push(p5);
        clamped.push(p4);
        clamped.push(p1);
        clamped.push(p0);
        assert_eq!(clamped.backward(p0), Some(p1));
        clamped.clamp_pages(3);
        assert_eq!(
            clamped.backward(p1),
            Some(ViewPosition {
                page: 2,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            })
        );

        let mut multi_run = JumpList::default();
        for page in [0, 4, 5, 6, 3] {
            multi_run.push(ViewPosition {
                page,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            });
        }
        assert_eq!(
            multi_run.backward(ViewPosition {
                page: 3,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            }),
            Some(ViewPosition {
                page: 6,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            })
        );
        multi_run.clamp_pages(5);
        assert_eq!(
            multi_run.forward(),
            Some(ViewPosition {
                page: 3,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            })
        );
        assert_eq!(
            multi_run.backward(ViewPosition {
                page: 3,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            }),
            Some(ViewPosition {
                page: 4,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            })
        );
        assert_eq!(
            multi_run.backward(ViewPosition {
                page: 4,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            }),
            Some(ViewPosition {
                page: 0,
                scroll_x: 0,
                scroll_y: 0,
                center_content: false,
            })
        );
    }

    #[test]
    fn config_reload_retains_session_prefix_on_editor_socket() {
        use super::AppDefaults;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "forward_socket = \"forward.sock\"\n[editor]\ntransport = \"none\"\n",
        )
        .unwrap();
        let mut initial = crate::config::Config::load_path(&config_path).unwrap();
        initial.select_session(Some("work")).unwrap();

        let (mut app, _viewport, _file) = continuous_app();
        let defaults = AppDefaults::from_config(&initial, None);
        app.editor = defaults.editor;
        app.forward_socket = defaults.forward_socket;
        app.session_name = defaults.session;

        std::fs::write(
            &config_path,
            "forward_socket = \"forward.sock\"\n[editor]\ntransport = \"socket\"\npath = \"editor.sock\"\n",
        )
        .unwrap();
        let reloaded = crate::config::Config::load_path(&config_path).unwrap();
        app.apply_config(&reloaded);

        match &app.editor {
            crate::editor::Editor::Socket { path } => {
                assert!(
                    path.ends_with("/work-editor.sock"),
                    "expected session-prefixed editor socket, got {path}"
                );
            }
            other => panic!("expected socket editor after reload, got {other:?}"),
        }
    }

    #[test]
    fn modified_char_shortcuts_are_ignored_and_forward_quit_stops_viewer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _viewport, _file) = continuous_app();
        let initial_scroll_y = app.tab().scroll_y;
        let mut output = Vec::new();
        for key in [
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT),
        ] {
            assert!(!app.handle_key(key, &mut output).unwrap());
        }
        app.link_picker = Some(LinkPickerState::new(0));
        app.link_picker.as_mut().unwrap().focus = LinkPickerFocus::Document;
        for key in [
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        ] {
            assert!(!app.handle_key(key, &mut output).unwrap());
        }
        assert!(app.link_picker.is_some());
        assert_eq!(app.tab().scroll_y, initial_scroll_y);
        assert_eq!(app.smooth_scroll_remaining, 0);

        let socket_path = dir.path().join("private/quit.sock");
        app.forward_socket = Some(socket_path.to_str().unwrap().to_owned());
        app.poll_forward_socket(&mut output).unwrap();
        let mut client = std::os::unix::net::UnixStream::connect(&socket_path).unwrap();
        client.write_all(br#"{"type":"quit"}"#).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            app.poll_forward_socket(&mut output),
            Err(super::AppError::Quit)
        ));
        let mut reply = String::new();
        std::io::Read::read_to_string(&mut client, &mut reply).unwrap();
        assert!(reply.contains(r#""ok":true"#), "{reply}");
    }

    #[test]
    fn late_search_results_do_not_clobber_navigation_after_picker_closes() {
        let (mut app, viewport, file) = continuous_app();
        let revision = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        app.pending.clear();
        for page in 0..2 {
            let key = app.page_key(page, viewport);
            app.tab_mut()
                .cache
                .insert(key, continuous_frame(key, revision, false));
        }
        let mut output = Vec::new();
        app.start_search("sample".into(), &mut output).unwrap();
        let request_id = app.tab().search.request_id;
        assert!(app.tab().search.initial_navigation_pending);
        app.close_search_picker(&mut output).unwrap();
        assert!(!app.tab().search.initial_navigation_pending);
        app.set_page(1, &mut output).unwrap();
        app.tab_mut().scroll_x = 12;
        app.tab_mut().scroll_y = 34;
        let mut highlighted_page_0 = app.page_key(0, viewport);
        highlighted_page_0.search_request_id = request_id;
        app.tab_mut().cache.insert(
            highlighted_page_0,
            continuous_frame(highlighted_page_0, revision, false),
        );
        let document_id = app.tab().document_id;
        app.receive_search_results(
            document_id,
            request_id,
            vec![SearchPageMatch {
                page: 0,
                occurrences: 1,
                context: "sample".into(),
            }],
            1,
            &mut output,
        )
        .unwrap();
        assert_eq!((app.tab().page, app.tab().scroll_y), (1, 34));
    }

    #[test]
    fn watcher_failed_fingerprint_jump_alphabet_and_path_prefix_boundaries_hold() {
        use std::path::Path;
        use std::time::Duration;
        // B4: failed fingerprint is not retried until disk metadata changes again.
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut watcher = super::session::FileWatcher::new(file.path()).unwrap();
        std::fs::write(file.path(), b"corrupt-zibble").unwrap();
        let fp = crate::synctex::DocumentRevision::read(file.path()).unwrap();
        let start = Instant::now();
        let _ = watcher.observe(fp, start);
        let broken_fp = watcher.observe(fp, start + Duration::from_millis(500));
        assert_eq!(broken_fp, Some(fp));
        watcher.mark_failed(fp);
        assert!(
            watcher
                .observe(fp, start + Duration::from_secs(2))
                .is_none()
        );

        // B5: 48 distinct continuation classes across >= 2 matches leaves 1 symbol; must not spin.
        let (app, viewport, _) = continuous_app();
        let key = app.page_key(0, viewport);
        let classes = "asdfghjklqwertyuiopzxcvbnm!@#$%^&*()[]{};:,.?/\\|";
        let matches: Vec<crate::pdf::VisibleMatch> = classes
            .chars()
            .take(classes.chars().count() - 1)
            .map(|c| crate::pdf::VisibleMatch {
                key,
                char_index: 0,
                rects: Vec::new(),
                hit: (0, 0),
                anchor: (0, 0),
                next_char: Some(c),
            })
            .collect();
        assert!(super::App::assign_jump_labels(&matches).is_none());

        // B14 + L8: sibling home prefix and root HOME="/" are never stripped; "../" ignores parent name.
        assert_eq!(
            super::picker::shorten_path_with_home(
                "/synth/quuxle/doc.pdf",
                Some(Path::new("/synth/quux"))
            ),
            "/synth/quuxle/doc.pdf"
        );
        assert_eq!(
            super::picker::shorten_path_with_home("/synth/quux.pdf", Some(Path::new("/"))),
            "/synth/quux.pdf"
        );
        let root = tempfile::tempdir().unwrap();
        let parent_dir = root.path().join("zibbleparent");
        let child_dir = parent_dir.join("nested");
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::write(child_dir.join("quux.pdf"), b"%PDF-1.4\n").unwrap();
        let mut browser = crate::browser::BrowserState::with_recents(child_dir, Vec::new());
        browser.filter = "zibbleparent".into();
        browser.rebuild_filter();
        assert!(
            browser
                .filtered_indices
                .iter()
                .all(|&i| browser.entries[i].name != "../")
        );

        // L6: empty visible-match reply clears label_loading so input_wait returns to idle cadence.
        let (mut label_app, label_vp, label_file) = continuous_app();
        label_app.pending.clear();
        label_app.canvas_viewport = Some(label_vp);
        let label_rev = crate::synctex::DocumentRevision::read(label_file.path()).unwrap();
        label_app.label_query = Some("zqxj".into());
        label_app.label_loading = true;
        label_app.label_request_id = 2;
        assert_eq!(label_app.input_wait(), std::time::Duration::from_millis(10));
        label_app
            .receive_visible_matches(
                super::INITIAL_DOCUMENT_ID,
                1,
                label_rev,
                Vec::new(),
                &mut Vec::new(),
            )
            .unwrap();
        assert!(!label_app.label_loading);
        assert!(label_app.input_wait() >= std::time::Duration::from_millis(50));

        // H1: older prefetched desired_key bumping generation clears stale pending keys and re-requests them.
        {
            let _native = crate::pdf::pdfium_test_lock();
            let pdf_path = root.path().join("two.pdf");
            let objs = [
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            ];
            let mut pdf = b"%PDF-1.7\n".to_vec();
            let mut offs = Vec::new();
            for (idx, obj) in objs.iter().enumerate() {
                offs.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n{obj}\nendobj\n", idx + 1).as_bytes());
            }
            let xref = pdf.len();
            pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
            for off in offs {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            std::fs::write(&pdf_path, pdf).unwrap();
            let worker =
                crate::pdf::RenderWorker::spawn(super::INITIAL_DOCUMENT_ID, pdf_path.clone(), None);
            let (pages, outline, rev) = worker.wait_until_ready().unwrap();
            let cfg: crate::config::Config =
                toml::from_str("[viewer]\ncontinuous_scroll = true\nprefetch_pages = 0\n").unwrap();
            let mut live = super::App::new(
                worker,
                pages,
                0,
                pdf_path,
                FileWatcher::from_revision(rev),
                (outline, rev.pdf),
                super::AppDefaults::from_config(&cfg, None),
            );
            live.canvas_viewport = Some(label_vp);
            live.view_generation = 1;
            live.generation = 2;
            live.worker.begin_generation(2);
            let key0 = live.page_key(0, label_vp);
            let key1 = live.page_key(1, label_vp);
            live.desired_key = Some(key0);
            live.pending.insert(key0, false);
            live.pending.insert(key1, false);
            live.pending_scale = Some((key1, super::FitMode::Width, 100));
            let mut stale_prefetch =
                std::sync::Arc::into_inner(continuous_frame(key0, rev, false)).unwrap();
            stale_prefetch.generation = 1;
            live.receive_frame(stale_prefetch, &mut Vec::new()).unwrap();
            assert_eq!(live.generation, 3);
            assert!(live.pending.contains_key(&key1));
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            let mut got_key1 = false;
            while Instant::now() < deadline && !got_key1 {
                if let Ok(crate::pdf::WorkerMessage::Frame(f)) = live.worker.try_recv() {
                    if f.key == key1 && f.generation == 3 {
                        got_key1 = true;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            assert!(got_key1);

            // B12: Opening/switching tabs rolls back uncommitted search_picker preview on the outgoing tab.
            live.session.tabs[0].page = 1;
            let mut sp = super::SearchPickerState::new(1);
            sp.preview_origin = Some(super::ViewPosition::top(0));
            live.search_picker = Some(sp);
            live.restore_active_picker_preview();
            assert_eq!(live.session.tabs[0].page, 0);
            assert!(
                live.search_picker
                    .as_ref()
                    .unwrap()
                    .preview_origin
                    .is_none()
            );
        }

        // B11: Kitty lowercase + SHIFT normalizes in picker navigation and filter editing.
        let shift_g = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('g'),
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(
            super::picker::picker_navigation_index(
                0,
                5,
                shift_g,
                3,
                true,
                super::picker::ListEdge::Clamp,
            ),
            Some(4)
        );
        let mut filter_buf = String::from("q");
        assert!(super::picker::edit_picker_filter(&mut filter_buf, shift_g));
        assert_eq!(filter_buf, "qG");

        // P1 + P5: Out-of-window prefetches are pruned from pending; duplicate frames reuse Arc<Frame>;
        // active render errors clear pending_vertical_scroll and smooth_scroll_remaining.
        let (mut multi, multi_vp, _) = continuous_app();
        multi.canvas_viewport = Some(multi_vp);
        multi.pending.clear();
        multi.viewer.prefetch_pages = 1;
        let far_key = multi.page_key(9, multi_vp);
        multi.pending.insert(far_key, true);
        multi.sync_worker_view_window(0);
        assert!(!multi.pending.contains_key(&far_key));
        assert!(multi.input_wait() >= std::time::Duration::from_millis(50));

        let k0 = multi.page_key(0, multi_vp);
        multi.view_generation = 1;
        multi.generation = 1;
        let f_a = std::sync::Arc::into_inner(continuous_frame(k0, label_rev, false)).unwrap();
        let f_b = std::sync::Arc::into_inner(continuous_frame(k0, label_rev, false)).unwrap();
        multi.receive_frame(f_a, &mut Vec::new()).unwrap();
        let ptr_a = std::sync::Arc::clone(multi.tab().cache.get(&k0).unwrap());
        multi.receive_frame(f_b, &mut Vec::new()).unwrap();
        let ptr_b = std::sync::Arc::clone(multi.tab().cache.get(&k0).unwrap());
        assert!(std::sync::Arc::ptr_eq(&ptr_a, &ptr_b));

        multi.pending_vertical_scroll = 12;
        multi.smooth_scroll_remaining = 8;
        multi
            .receive_render_error(k0, multi.view_generation, "zibble", &mut Vec::new())
            .unwrap();
        assert_eq!(multi.pending_vertical_scroll, 0);
        assert_eq!(multi.smooth_scroll_remaining, 0);
    }
}

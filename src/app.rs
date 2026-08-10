use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
use ratatui::widgets::{Block, Borders, Clear as RatatuiClear, Paragraph};
use thiserror::Error;

use crate::browser::BrowserState;
use crate::config::{Config, LinkPickerLayout};
use crate::kitty::{self, Placement};
use crate::pdf::{
    DarkModeStyle, DocumentId, DocumentLink, FitMode, Frame, LinkTarget, OutlineItem, PageLink,
    RenderKey, RenderRequest, RenderWorker, SearchPageMatch, WorkerMessage,
};
use crate::terminal::{ImagePlacement, TerminalGuard, Viewport};
use crate::theme::Palette;

const FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILE_STABLE_FOR: Duration = Duration::from_millis(150);
const RELOAD_RETRY_DELAY: Duration = Duration::from_millis(500);
const INITIAL_DOCUMENT_ID: DocumentId = 1;
const PAGE_BACKGROUND_Z_INDEX: i32 = i32::MIN / 2 - 3;
const PAGE_IMAGE_Z_INDEX: i32 = i32::MIN / 2 - 2;
const PAGE_BACKGROUND_IMAGE_ID: u32 = u32::MAX - 2;

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
    let defaults = AppDefaults::from(config);
    let theme = defaults.theme;
    let _terminal = TerminalGuard::enter(&mut output, theme)?;
    let path = match path {
        Some(path) => path.canonicalize()?,
        None => match pick_pdf(std::env::current_dir()?, &mut output, theme)? {
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
        defaults,
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
                WorkerMessage::SearchProgress {
                    document_id,
                    request_id,
                    scanned,
                    total,
                } => app.receive_search_progress(
                    document_id,
                    request_id,
                    scanned,
                    total,
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

        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && app.handle_key(key, &mut output)? =>
                {
                    break;
                }
                Event::Resize(_, _) => app.request_current(&mut output)?,
                Event::Mouse(mouse) => app.handle_mouse(mouse, &mut output)?,
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
    performance_snapshot: Option<PerformanceSnapshot>,
    default_fit: FitMode,
    default_invert: bool,
    goto_input: Option<String>,
    search_input: Option<String>,
    next_search_request_id: u64,
    next_link_request_id: u64,
    link_mode: bool,
    link_picker: Option<LinkPickerState>,
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
    show_performance: bool,
    theme: Palette,
    themes: Vec<(String, Palette)>,
    theme_index: usize,
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
    persistent_link_picker: bool,
    link_picker_geometry: LinkPickerGeometry,
}

impl From<&Config> for AppDefaults {
    fn from(config: &Config) -> Self {
        let themes = crate::theme::available_themes(config.theme_catalog(), config.theme());
        let configured_theme = crate::theme::load_or_default(config.theme());
        let theme_index = themes
            .iter()
            .position(|(_, theme)| *theme == configured_theme)
            .unwrap_or(0);
        let theme = themes
            .get(theme_index)
            .map_or(configured_theme, |(_, theme)| *theme);
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
            theme,
            themes,
            theme_index,
        }
    }
}

struct Tab {
    document_id: DocumentId,
    path: PathBuf,
    watcher: FileWatcher,
    page_count: u32,
    page: u32,
    fit: FitMode,
    invert: bool,
    dark_mode_style: DarkModeStyle,
    search_highlight: [u8; 3],
    link_highlight: [u8; 3],
    scroll_x: u32,
    scroll_y: u32,
    outline: Arc<Vec<OutlineItem>>,
    cache: HashMap<RenderKey, Arc<Frame>>,
    search: SearchState,
    link_history: Vec<ViewPosition>,
    pending_destination: Option<LinkDestination>,
    link_index: LinkIndexState,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewPosition {
    page: u32,
    scroll_x: u32,
    scroll_y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LinkDestination {
    page: u32,
    top_ratio: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkPickerState {
    page: u32,
    selected: usize,
    number_input: String,
    selection_key: Option<(u32, u32)>,
    awaiting_current_page: bool,
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

struct LinkIndexUpdate {
    request_id: u64,
    links: Vec<DocumentLink>,
    scanned: u32,
    total: u32,
    complete: bool,
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
            selection_key: None,
            awaiting_current_page: true,
        }
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

#[derive(Default)]
struct SearchState {
    query: String,
    request_id: u64,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
    scanned: u32,
    total_pages: u32,
    searching: bool,
}

impl SearchState {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

impl Tab {
    fn render_key(&self, viewport: Viewport, link_mode: bool) -> RenderKey {
        RenderKey {
            document_id: self.document_id,
            page: self.page,
            width: viewport.pixel_width,
            height: viewport.pixel_height,
            fit: self.fit,
            invert: self.invert,
            dark_mode_style: self.dark_mode_style,
            search_request_id: self.search.highlight_request_id(self.page),
            search_highlight: self.search_highlight,
            link_mode,
            link_highlight: self.link_highlight,
        }
    }
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
        defaults: AppDefaults,
    ) -> Self {
        let default_fit = defaults.fit;
        let default_invert = defaults.invert;
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
                dark_mode_style: defaults.dark_mode_style,
                search_highlight: defaults.search_highlight,
                link_highlight: defaults.link_highlight,
                scroll_x: 0,
                scroll_y: 0,
                outline: Arc::new(outline),
                cache: HashMap::new(),
                search: SearchState::default(),
                link_history: Vec::new(),
                pending_destination: None,
                link_index: LinkIndexState::new(page_count),
            }],
            active_tab: 0,
            next_document_id: INITIAL_DOCUMENT_ID + 1,
            generation: 0,
            desired_key: None,
            pending: HashSet::new(),
            visible_image_id: None,
            next_image_id: 1,
            last_status_row: None,
            performance_snapshot: None,
            default_fit,
            default_invert,
            goto_input: None,
            search_input: None,
            next_search_request_id: 1,
            next_link_request_id: 1,
            link_mode: false,
            link_picker: None,
            persistent_link_picker: defaults.persistent_link_picker,
            link_picker_geometry: defaults.link_picker_geometry,
            show_performance: false,
            theme: defaults.theme,
            themes: defaults.themes,
            theme_index: defaults.theme_index,
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
                tab.search = SearchState::default();
                tab.link_history.clear();
                tab.pending_destination = None;
                tab.link_index = LinkIndexState::new(pages);
                if index == self.active_tab {
                    self.reset_render_state();
                    self.ensure_link_index();
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
                    dark_mode_style: DarkModeStyle::new(
                        self.theme.document.background,
                        self.theme.document.foreground,
                    ),
                    search_highlight: terminal_color_rgb(self.theme.yellow),
                    link_highlight: terminal_color_rgb(self.theme.cyan),
                    scroll_x: 0,
                    scroll_y: 0,
                    outline: Arc::new(outline),
                    cache: HashMap::new(),
                    search: SearchState::default(),
                    link_history: Vec::new(),
                    pending_destination: None,
                    link_index: LinkIndexState::new(pages),
                });
                self.active_tab = self.tabs.len() - 1;
                self.reset_render_state();
                self.ensure_link_index();
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
        match pick_pdf(directory, output, self.theme)? {
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
        let selection = pick_outline(&outline, self.tab().page, output, self.theme)?;
        if let Some(page) = selection {
            let page = page.min(self.tab().page_count - 1);
            self.tab_mut().page = page;
            self.tab_mut().scroll_x = 0;
            self.tab_mut().scroll_y = 0;
        }
        self.reset_render_state();
        self.request_current(output)
    }

    fn open_theme_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        let selection = pick_theme(&self.themes, self.theme_index, output)?;
        if let Some(index) = selection {
            self.apply_theme(index);
        }
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)
    }

    fn open_help(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        self.clear_viewer(output)?;
        show_help(output, self.theme)?;
        self.clear_viewer(output)?;
        self.reset_render_state();
        self.request_current(output)
    }

    fn apply_theme(&mut self, index: usize) {
        let Some((_, theme)) = self.themes.get(index) else {
            return;
        };
        let theme = *theme;
        self.theme_index = index;
        self.theme = theme;
        let style = DarkModeStyle::new(theme.document.background, theme.document.foreground);
        let search_highlight = terminal_color_rgb(theme.yellow);
        let link_highlight = terminal_color_rgb(theme.cyan);
        for tab in &mut self.tabs {
            tab.dark_mode_style = style;
            tab.search_highlight = search_highlight;
            tab.link_highlight = link_highlight;
            tab.cache.clear();
        }
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
        let theme = self.theme;
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

    fn begin_search(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
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

    fn draw_search(&self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        let theme = self.theme;
        let input = self.search_input.as_deref().unwrap_or_default();
        execute!(
            output,
            MoveTo(0, viewport.status_row),
            SetBackgroundColor(theme.bg_dark),
            Clear(ClearType::CurrentLine),
            Print(" "),
            SetForegroundColor(theme.yellow),
            Print("search: "),
            SetForegroundColor(theme.fg),
            Print(input),
            SetForegroundColor(theme.comment),
            Print("  (enter to search, esc to cancel)"),
            SetBackgroundColor(theme.bg),
            SetForegroundColor(theme.fg)
        )?;
        output.flush()
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
        };
        self.worker.search(document_id, request_id, query);
        self.request_current(output)
    }

    fn receive_search_progress(
        &mut self,
        document_id: DocumentId,
        request_id: u64,
        scanned: u32,
        total: u32,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(index) = self.tab_index(document_id) else {
            return Ok(());
        };
        if self.tabs[index].search.request_id != request_id {
            return Ok(());
        }
        self.tabs[index].search.scanned = scanned;
        self.tabs[index].search.total_pages = total;
        if index == self.active_tab {
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
        if self.tabs[index].search.request_id != request_id {
            return Ok(());
        }
        let current_page = self.tabs[index].page;
        let target_page = matches
            .iter()
            .find(|result| result.page >= current_page)
            .or_else(|| matches.first())
            .map(|result| result.page);
        let search = &mut self.tabs[index].search;
        search.matches = matches;
        search.total_occurrences = total_occurrences;
        search.scanned = search.total_pages;
        search.searching = false;

        if index != self.active_tab {
            return Ok(());
        }
        if let Some(page) = target_page {
            self.tabs[index].page = page;
            self.tabs[index].scroll_x = 0;
            self.tabs[index].scroll_y = 0;
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
        let link_index = &mut self.tabs[index].link_index;
        if link_index.request_id != update.request_id {
            return Ok(());
        }
        link_index.links.extend(update.links);
        link_index.scanned = update.scanned;
        link_index.total_pages = update.total;
        link_index.indexing = !update.complete;

        if index == self.active_tab && self.link_picker.is_some() {
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
        self.tab_mut().page = page;
        self.tab_mut().scroll_x = 0;
        self.tab_mut().scroll_y = 0;
        self.request_current(output)
    }

    fn clear_search(&mut self, output: &mut impl Write) -> Result<bool, AppError> {
        if self.tab().search.query.is_empty() {
            return Ok(false);
        }
        let document_id = self.tab().document_id;
        let request_id = self.tab().search.request_id;
        self.worker.cancel_search(document_id, request_id);
        self.tab_mut().search = SearchState::default();
        self.request_current(output)?;
        Ok(true)
    }

    fn toggle_link_mode(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.pending_open.is_some() {
            return Ok(());
        }
        self.set_link_mode(!self.link_mode, output)
    }

    fn set_link_mode(&mut self, enabled: bool, output: &mut impl Write) -> Result<(), AppError> {
        if enabled == self.link_mode {
            return Ok(());
        }
        if enabled {
            execute!(output, EnableMouseCapture)?;
        } else {
            execute!(output, DisableMouseCapture)?;
        }
        self.link_mode = enabled;
        self.ensure_link_index();
        self.request_current(output)
    }

    fn ensure_link_index(&mut self) {
        if !self.link_mode || self.tab().link_index.started() {
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

    fn handle_mouse(&mut self, mouse: MouseEvent, output: &mut impl Write) -> Result<(), AppError> {
        if !self.link_mode
            || mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || self.pending_open.is_some()
        {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let key = self.tab().render_key(viewport, self.link_mode);
        let Some(frame) = self.tab().cache.get(&key).cloned() else {
            self.draw_status(output, viewport, "links are still rendering")?;
            return Ok(());
        };
        let placement = viewport.place(
            frame.width,
            frame.height,
            self.tab().scroll_x,
            self.tab().scroll_y,
        );
        let (placement, image_top) = if self.link_picker.is_some() {
            let Some(image_id) = self.visible_image_id else {
                return Ok(());
            };
            let (preview, _) =
                link_picker_panes(link_picker_area(viewport), self.link_picker_geometry);
            let positioned = position_link_picker_image(
                LinkPickerImage::new(image_id, &frame, placement, viewport),
                preview,
                self.link_picker_geometry.layout,
            );
            (
                ImagePlacement {
                    left: positioned.left,
                    columns: positioned.placement.columns,
                    rows: positioned.placement.rows,
                    crop: positioned.placement.crop,
                    scroll_x: placement.scroll_x,
                    scroll_y: placement.scroll_y,
                },
                positioned.top,
            )
        } else {
            (placement, viewport.top)
        };
        let Some(target) = link_at_cell(
            &frame.links,
            placement,
            frame.width,
            frame.height,
            image_top,
            mouse.column,
            mouse.row,
        ) else {
            self.draw_status(output, viewport, "no link here")?;
            return Ok(());
        };
        if self.link_picker.is_some() && !self.persistent_link_picker {
            self.close_link_picker(output)?;
        }
        self.follow_link(target, output)
    }

    fn follow_link(&mut self, target: LinkTarget, output: &mut impl Write) -> Result<(), AppError> {
        match target {
            LinkTarget::Internal { page, top_ratio } => {
                let current = ViewPosition {
                    page: self.tab().page,
                    scroll_x: self.tab().scroll_x,
                    scroll_y: self.tab().scroll_y,
                };
                let page = page.min(self.tab().page_count - 1);
                let tab = self.tab_mut();
                if tab.link_history.len() == 100 {
                    tab.link_history.remove(0);
                }
                tab.link_history.push(current);
                tab.page = page;
                tab.scroll_x = 0;
                tab.scroll_y = 0;
                tab.pending_destination = Some(LinkDestination { page, top_ratio });
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

    fn follow_link_back(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(previous) = self.tab_mut().link_history.pop() else {
            self.draw_status(output, self.viewport()?, "no link history")?;
            return Ok(());
        };
        let tab = self.tab_mut();
        tab.page = previous.page.min(tab.page_count - 1);
        tab.scroll_x = previous.scroll_x;
        tab.scroll_y = previous.scroll_y;
        tab.pending_destination = None;
        self.request_current(output)
    }

    fn open_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if !self.link_mode || self.pending_open.is_some() {
            return Ok(());
        }
        self.ensure_link_index();
        let viewport = self.viewport()?;
        let key = self.tab().render_key(viewport, self.link_mode);
        let Some(frame) = self.tab().cache.get(&key).cloned() else {
            self.draw_status(output, viewport, "links are still rendering")?;
            return Ok(());
        };
        let Some(image_id) = self.visible_image_id else {
            self.draw_status(output, viewport, "page is still rendering")?;
            return Ok(());
        };
        let tab = self.tab();
        let placement = viewport.place(frame.width, frame.height, tab.scroll_x, tab.scroll_y);
        let image = LinkPickerImage::new(image_id, &frame, placement, viewport);

        self.link_picker = Some(LinkPickerState::new(self.tab().page));
        show_link_picker_split(
            output,
            link_picker_area(viewport),
            image,
            self.link_picker_geometry,
            self.theme,
        )?;
        self.redraw_link_picker(output)
    }

    fn redraw_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(_) = self.link_picker else {
            return Ok(());
        };
        let viewport = self.viewport()?;
        let page = self.tab().page;
        let links = self.tab().link_index.links.clone();
        let progress = LinkIndexProgress::from(&self.tab().link_index);
        let state = self.link_picker.as_mut().expect("link picker state");
        state.sync(page, &links, progress.indexing);
        let state = state.clone();
        draw_link_picker_terminal(
            output,
            link_picker_area(viewport),
            &links,
            &state,
            progress,
            self.link_picker_geometry,
            self.theme,
        )?;
        Ok(())
    }

    fn close_link_picker(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.link_picker.take().is_none() {
            return Ok(());
        }
        let viewport = self.viewport()?;
        let key = self.tab().render_key(viewport, self.link_mode);
        let frame = self.tab().cache.get(&key).cloned();
        let image_id = self.visible_image_id;
        if let (Some(frame), Some(image_id)) = (frame, image_id) {
            let tab = self.tab();
            let placement = viewport.place(frame.width, frame.height, tab.scroll_x, tab.scroll_y);
            let image = LinkPickerImage::new(image_id, &frame, placement, viewport);
            restore_link_picker_split(
                output,
                link_picker_area(viewport),
                image,
                self.link_picker_geometry,
                self.theme,
            )?;
        } else {
            self.reset_render_state();
            self.request_current(output)?;
        }
        Ok(())
    }

    fn handle_link_picker_key(
        &mut self,
        key: KeyEvent,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        match key.code {
            KeyCode::Esc => return self.close_link_picker(output),
            KeyCode::Char('L') => {
                self.close_link_picker(output)?;
                return self.set_link_mode(false, output);
            }
            KeyCode::Char('b') => {
                self.follow_link_back(output)?;
                return Ok(());
            }
            _ => {}
        }

        let viewport = self.viewport()?;
        let links = self.tab().link_index.links.clone();
        let link_count = links.len();
        let indexing = self.tab().link_index.indexing;
        let page = self.tab().page;
        let visible_height =
            link_picker_visible_height(link_picker_area(viewport), self.link_picker_geometry);
        let state = self.link_picker.as_mut().expect("link picker state");
        state.sync(page, &links, indexing);

        let mut redraw = true;
        let mut target = None;
        match key.code {
            KeyCode::Enter => {
                target = links.get(state.selected).map(|link| link.target.clone());
                state.number_input.clear();
                redraw = false;
            }
            KeyCode::Down | KeyCode::Char('j') if link_count > 0 => {
                let selected = if state.selected + 1 == link_count {
                    0
                } else {
                    state.selected + 1
                };
                state.select(selected, &links);
                state.number_input.clear();
            }
            KeyCode::Up | KeyCode::Char('k') if link_count > 0 => {
                let selected = if state.selected == 0 {
                    link_count - 1
                } else {
                    state.selected - 1
                };
                state.select(selected, &links);
                state.number_input.clear();
            }
            KeyCode::Home if link_count > 0 => {
                state.select(0, &links);
                state.number_input.clear();
            }
            KeyCode::End if link_count > 0 => {
                state.select(link_count - 1, &links);
                state.number_input.clear();
            }
            KeyCode::PageDown if link_count > 0 => {
                state.select(
                    (state.selected + visible_height).min(link_count - 1),
                    &links,
                );
                state.number_input.clear();
            }
            KeyCode::PageUp if link_count > 0 => {
                state.select(state.selected.saturating_sub(visible_height), &links);
                state.number_input.clear();
            }
            KeyCode::Backspace => {
                state.number_input.pop();
                if let Some(index) = link_number_index(&state.number_input, link_count) {
                    state.select(index, &links);
                }
            }
            KeyCode::Char(digit) if digit.is_ascii_digit() => {
                if let Some(index) =
                    update_link_number_selection(&mut state.number_input, digit, link_count)
                {
                    state.select(index, &links);
                }
            }
            _ => redraw = false,
        }

        if let Some(target) = target {
            if !self.persistent_link_picker {
                self.close_link_picker(output)?;
            }
            self.follow_link(target, output)?;
        } else if redraw {
            self.redraw_link_picker(output)?;
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, output: &mut impl Write) -> Result<bool, AppError> {
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
        match key.code {
            KeyCode::Char('?') => self.open_help(output)?,
            KeyCode::Char('q') => return self.close_current(output),
            KeyCode::Char(':') => self.begin_goto(output)?,
            KeyCode::Char('/') => self.begin_search(output)?,
            KeyCode::Char('n') => self.navigate_search(true, output)?,
            KeyCode::Char('N') => self.navigate_search(false, output)?,
            KeyCode::Char('L') => self.toggle_link_mode(output)?,
            KeyCode::Char('b') => self.follow_link_back(output)?,
            KeyCode::Enter if self.link_mode => self.open_link_picker(output)?,
            KeyCode::Esc if self.link_mode => self.set_link_mode(false, output)?,
            KeyCode::Esc if self.clear_search(output)? => {}
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
            KeyCode::Char('p') => self.toggle_performance(output)?,
            KeyCode::Char('t') => self.open_outline(output)?,
            KeyCode::Char('T') => self.open_theme_picker(output)?,
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
        let key = self.tab().render_key(viewport, self.link_mode);
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
        let key = self.tab().render_key(viewport, self.link_mode);
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

    fn toggle_performance(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        self.show_performance = !self.show_performance;
        let Some(snapshot) = self.performance_snapshot else {
            return Ok(());
        };
        let state = snapshot.status(self.show_performance, self.link_mode);
        self.draw_status(output, self.viewport()?, &state)?;
        Ok(())
    }

    fn request_current(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let viewport = self.prepare_viewport(output)?;
        self.draw_tab_bar(output)?;
        let key = self.tab().render_key(viewport, self.link_mode);
        self.desired_key = Some(key);
        self.generation = self.generation.wrapping_add(1);
        self.worker.begin_generation(self.generation);
        self.pending.clear();
        self.performance_snapshot = None;

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
                && cached.dark_mode_style == key.dark_mode_style
                && cached.page.abs_diff(current_page) <= 1
        });

        if self.desired_key == Some(key) {
            let viewport = self.viewport()?;
            let current_key = self.tab().render_key(viewport, self.link_mode);
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
        if let Some(destination) = self.tab_mut().pending_destination.take()
            && destination.page == frame.key.page
        {
            let target_y = destination
                .top_ratio
                .map(|ratio| (ratio * frame.height as f32).round() as u32)
                .unwrap_or(0);
            self.tab_mut().scroll_x = 0;
            self.tab_mut().scroll_y = target_y;
        }
        let tab = self.tab();
        let placement = viewport.place(frame.width, frame.height, tab.scroll_x, tab.scroll_y);
        self.tab_mut().scroll_x = placement.scroll_x;
        self.tab_mut().scroll_y = placement.scroll_y;
        let image_id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        let original = PositionedImage {
            left: placement.left,
            top: viewport.top,
            placement: Placement {
                image_id,
                columns: placement.columns,
                rows: placement.rows,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: placement.crop,
            },
        };
        let positioned = if self.link_picker.is_some() {
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
        kitty::transmit_compressed_rgba(
            output,
            &frame.compressed_rgba,
            frame.width,
            frame.height,
            positioned.placement,
        )?;
        let transfer_elapsed = transfer_started.elapsed();
        if let Some(previous) = self.visible_image_id.replace(image_id) {
            kitty::delete_image(output, previous)?;
        }
        let snapshot = PerformanceSnapshot {
            render_ms: frame.render_elapsed.as_millis(),
            dark_mode_ms: frame.dark_mode_elapsed.map(|elapsed| elapsed.as_millis()),
            highlight_ms: frame.highlight_elapsed.map(|elapsed| elapsed.as_millis()),
            compression_ms: frame.compression_elapsed.as_millis(),
            transfer_ms: transfer_elapsed.as_millis(),
            link_count: frame.links.len(),
        };
        self.performance_snapshot = Some(snapshot);
        let state = snapshot.status(self.show_performance, self.link_mode);
        let links = self.tab().link_index.links.clone();
        let progress = LinkIndexProgress::from(&self.tab().link_index);
        if let Some(link_picker) = &mut self.link_picker {
            link_picker.sync(frame.key.page, &links, progress.indexing);
            let link_picker = link_picker.clone();
            draw_link_picker_terminal(
                output,
                link_picker_area(viewport),
                &links,
                &link_picker,
                progress,
                self.link_picker_geometry,
                self.theme,
            )?;
        }
        self.draw_status(output, viewport, &state)?;
        Ok(())
    }

    fn prepare_image_canvas(&self, output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
        execute!(output, ResetColor)?;
        for row in viewport.top..viewport.top.saturating_add(viewport.rows) {
            execute!(output, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }

        let background = kitty::compress_rgba(&rgba_pixel(self.theme.bg, u8::MAX))?;
        execute!(output, MoveTo(0, viewport.top))?;
        kitty::transmit_compressed_rgba(
            output,
            &background,
            1,
            1,
            Placement {
                image_id: PAGE_BACKGROUND_IMAGE_ID,
                columns: viewport.columns,
                rows: viewport.rows,
                z_index: PAGE_BACKGROUND_Z_INDEX,
                crop: None,
            },
        )
    }

    fn draw_status(
        &self,
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
        if !mode.is_empty() {
            mode.push_str("  ");
        }
        let search_status = tab.search.status_label(tab.page);
        let link_status = self.link_mode.then_some("  click/enter: open  esc: close");
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
            SetForegroundColor(theme.magenta),
            Print(search_status.as_deref().unwrap_or_default()),
            SetForegroundColor(theme.cyan),
            Print(link_status.unwrap_or_default()),
            SetForegroundColor(theme.comment),
            Print("  ?: help"),
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
            let neighbor = RenderKey {
                page,
                search_request_id: self.tab().search.highlight_request_id(page),
                ..key
            };
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
        self.ensure_link_index();
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
        let theme = self.theme;
        kitty::delete_all(output)?;
        self.visible_image_id = None;
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

    fn draw_tab_bar(&self, output: &mut impl Write) -> io::Result<()> {
        if self.tabs.len() < 2 {
            return Ok(());
        }
        let theme = self.theme;
        let columns = usize::from(crossterm::terminal::size()?.0);
        execute!(
            output,
            MoveTo(0, 0),
            SetBackgroundColor(theme.bg_dark1),
            SetForegroundColor(theme.dark3),
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

fn link_at_cell(
    links: &[PageLink],
    placement: ImagePlacement,
    image_width: u32,
    image_height: u32,
    viewport_top: u16,
    column: u16,
    row: u16,
) -> Option<LinkTarget> {
    let local_column = column.checked_sub(placement.left)?;
    let local_row = row.checked_sub(viewport_top)?;
    if local_column >= placement.columns || local_row >= placement.rows {
        return None;
    }
    let (source_x, source_y, visible_width, visible_height) = placement
        .crop
        .map_or((0, 0, image_width, image_height), |crop| {
            (crop.x, crop.y, crop.width, crop.height)
        });
    let x0 = source_x + scaled_cell_boundary(local_column, placement.columns, visible_width);
    let x1 = source_x
        + scaled_cell_boundary(
            local_column.saturating_add(1),
            placement.columns,
            visible_width,
        );
    let y0 = source_y + scaled_cell_boundary(local_row, placement.rows, visible_height);
    let y1 = source_y
        + scaled_cell_boundary(local_row.saturating_add(1), placement.rows, visible_height);
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

fn scaled_cell_boundary(cell: u16, cells: u16, pixels: u32) -> u32 {
    ((u64::from(cell) * u64::from(pixels)) / u64::from(cells.max(1))) as u32
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

fn pick_pdf(
    directory: PathBuf,
    output: &mut impl Write,
    theme: Palette,
) -> Result<Option<PathBuf>, AppError> {
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
            terminal.draw(|frame| draw_picker(frame, &browser, theme))?;
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
    clear_picker(output, theme)?;
    Ok(selection)
}

fn clear_picker(output: &mut impl Write, theme: Palette) -> io::Result<()> {
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

fn show_help(output: &mut impl Write, theme: Palette) -> Result<(), AppError> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;

    loop {
        if redraw {
            terminal.autoresize()?;
            terminal.draw(|frame| draw_help_menu(frame, theme))?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => redraw = true,
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && matches!(
                        key.code,
                        KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc
                    ) =>
            {
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn pick_theme(
    themes: &[(String, Palette)],
    current: usize,
    output: &mut impl Write,
) -> Result<Option<usize>, AppError> {
    let mut selected = current.min(themes.len().saturating_sub(1));
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    let mut redraw = true;
    let mut visible_height = 1;

    let selection = loop {
        if redraw {
            terminal.autoresize()?;
            let area = terminal.size()?;
            visible_height = usize::from(picker_rect(area.into()).height.saturating_sub(3).max(1));
            terminal.draw(|frame| draw_theme_picker(frame, themes, selected))?;
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
        let last = themes.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Enter => break Some(selected),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = if selected == last { 0 } else { selected + 1 };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                selected = if selected == 0 { last } else { selected - 1 };
            }
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = last,
            KeyCode::PageDown => selected = (selected + visible_height).min(last),
            KeyCode::PageUp => selected = selected.saturating_sub(visible_height),
            _ => continue,
        }
        redraw = true;
    };
    drop(terminal);
    Ok(selection)
}

fn draw_link_picker_terminal(
    output: &mut impl Write,
    area: Rect,
    links: &[DocumentLink],
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::new(backend)?;
    terminal
        .draw(|frame| draw_link_picker(frame, area, links, state, progress, geometry, theme))?;
    drop(terminal);
    execute!(output, Hide)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PositionedImage {
    left: u16,
    top: u16,
    placement: Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkPickerImage {
    image_id: u32,
    source_width: u32,
    source_height: u32,
    crop: Option<kitty::Crop>,
    cell_width: u32,
    cell_height: u32,
    original: PositionedImage,
}

impl LinkPickerImage {
    fn new(image_id: u32, frame: &Frame, placement: ImagePlacement, viewport: Viewport) -> Self {
        let (source_width, source_height) = placement
            .crop
            .map(|crop| (crop.width, crop.height))
            .unwrap_or((frame.width, frame.height));
        Self {
            image_id,
            source_width,
            source_height,
            crop: placement.crop,
            cell_width: (u32::from(viewport.pixel_width) / u32::from(viewport.columns)).max(1),
            cell_height: (u32::from(viewport.pixel_height) / u32::from(viewport.rows)).max(1),
            original: PositionedImage {
                left: placement.left,
                top: viewport.top,
                placement: Placement {
                    image_id,
                    columns: placement.columns,
                    rows: placement.rows,
                    z_index: PAGE_IMAGE_Z_INDEX,
                    crop: placement.crop,
                },
            },
        }
    }

    fn fit(self, pane: Rect) -> PositionedImage {
        let max_width = u32::from(pane.width).saturating_mul(self.cell_width);
        let max_height = u32::from(pane.height).saturating_mul(self.cell_height);
        let scale = (max_width as f64 / f64::from(self.source_width))
            .min(max_height as f64 / f64::from(self.source_height))
            .min(1.0);
        let pixel_width = (f64::from(self.source_width) * scale).round().max(1.0) as u32;
        let pixel_height = (f64::from(self.source_height) * scale).round().max(1.0) as u32;
        let columns = pixel_width
            .div_ceil(self.cell_width)
            .min(u32::from(pane.width))
            .max(1) as u16;
        let rows = pixel_height
            .div_ceil(self.cell_height)
            .min(u32::from(pane.height))
            .max(1) as u16;
        PositionedImage {
            left: pane.x + pane.width.saturating_sub(columns) / 2,
            top: pane.y + pane.height.saturating_sub(rows) / 2,
            placement: Placement {
                image_id: self.image_id,
                columns,
                rows,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: self.crop,
            },
        }
    }
}

fn position_link_picker_image(
    image: LinkPickerImage,
    preview: Rect,
    layout: LinkPickerLayout,
) -> PositionedImage {
    if layout == LinkPickerLayout::Floating {
        image.original
    } else {
        image.fit(preview)
    }
}

fn link_picker_area(viewport: Viewport) -> Rect {
    Rect::new(0, viewport.top, viewport.columns, viewport.rows)
}

fn resolved_link_picker_layout(area: Rect, layout: LinkPickerLayout) -> LinkPickerLayout {
    match layout {
        LinkPickerLayout::Auto if u32::from(area.width) >= u32::from(area.height) * 2 => {
            LinkPickerLayout::Vertical
        }
        LinkPickerLayout::Auto => LinkPickerLayout::Horizontal,
        layout => layout,
    }
}

fn link_picker_panes(area: Rect, geometry: LinkPickerGeometry) -> (Rect, Rect) {
    let picker_percent = geometry.split_percent.clamp(20, 80);
    let document_percent = 100 - picker_percent;
    let panes = match resolved_link_picker_layout(area, geometry.layout) {
        LinkPickerLayout::Vertical => Layout::horizontal([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Horizontal => Layout::vertical([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Floating => return (area, picker_rect(area)),
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    (panes[0], panes[1])
}

fn place_positioned_image(output: &mut impl Write, image: PositionedImage) -> io::Result<()> {
    execute!(output, MoveTo(image.left, image.top))?;
    kitty::place_image(output, image.placement)
}

fn show_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let (preview, pane) = link_picker_panes(area, geometry);
    execute!(
        output,
        SetBackgroundColor(theme.bg_dark),
        SetForegroundColor(theme.fg)
    )?;
    for row in pane.y..pane.y.saturating_add(pane.height) {
        execute!(
            output,
            MoveTo(pane.x, row),
            Print(" ".repeat(usize::from(pane.width)))
        )?;
    }
    if geometry.layout != LinkPickerLayout::Floating {
        place_positioned_image(output, image.fit(preview))?;
    }
    execute!(output, Hide)?;
    output.flush()
}

fn clear_link_picker_pane(
    output: &mut impl Write,
    area: Rect,
    geometry: LinkPickerGeometry,
) -> io::Result<()> {
    let (_, pane) = link_picker_panes(area, geometry);
    execute!(output, ResetColor)?;
    for row in pane.y..pane.y.saturating_add(pane.height) {
        execute!(
            output,
            MoveTo(pane.x, row),
            Print(" ".repeat(usize::from(pane.width)))
        )?;
    }
    output.flush()
}

fn restore_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    clear_link_picker_pane(output, area, geometry)?;
    place_positioned_image(output, image.original)?;
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Hide
    )?;
    output.flush()
}

fn rgba_pixel(color: crossterm::style::Color, alpha: u8) -> [u8; 4] {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => [r, g, b, alpha],
        _ => [0, 0, 0, alpha],
    }
}

fn link_number_index(input: &str, link_count: usize) -> Option<usize> {
    input
        .parse::<usize>()
        .ok()
        .filter(|number| (1..=link_count).contains(number))
        .map(|number| number - 1)
}

fn update_link_number_selection(
    input: &mut String,
    digit: char,
    link_count: usize,
) -> Option<usize> {
    let mut candidate = input.clone();
    candidate.push(digit);
    if let Some(index) = link_number_index(&candidate, link_count) {
        *input = candidate;
        return Some(index);
    }

    input.clear();
    input.push(digit);
    if let Some(index) = link_number_index(input, link_count) {
        Some(index)
    } else {
        input.clear();
        None
    }
}

fn pick_outline(
    items: &[OutlineItem],
    current_page: u32,
    output: &mut impl Write,
    theme: Palette,
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
                draw_outline(
                    frame,
                    items,
                    &filtered,
                    selected,
                    scroll_offset,
                    &filter,
                    theme,
                )
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
    clear_picker(output, theme)?;
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
    theme: Palette,
) {
    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Outline ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
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
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(filter, Style::default().fg(colors.text).bg(colors.chrome)),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter_line).style(Style::default().bg(colors.chrome)),
        rows[0],
    );

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
                Style::default().fg(colors.text).bg(colors.selection)
            } else {
                Style::default().fg(colors.text).bg(colors.surface)
            };
            let indent = "  ".repeat(usize::from(item.depth).min(6) + 1);
            let page_label = format!(" {} ", item.page + 1);
            let mut line = Line::from(vec![
                Span::styled(if is_selected { "▌" } else { " " }, style.fg(colors.accent)),
                Span::styled(indent, style),
                Span::styled(item.title.clone(), style.add_modifier(Modifier::BOLD)),
            ]);
            let used = line.width();
            let page_width = page_label.chars().count();
            if used + page_width < width {
                let page_style = if is_selected {
                    style
                } else {
                    Style::default().fg(colors.muted).bg(colors.surface)
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
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[1],
    );

    frame.render_widget(
        Paragraph::new(picker_hint_line(
            &[("enter", "jump"), ("esc", "close")],
            None,
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
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

fn draw_picker(frame: &mut RatatuiFrame, browser: &BrowserState, theme: Palette) {
    let colors = PickerTheme::from(theme);
    let entries: Vec<_> = browser.filtered_entries().collect();
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let directory = shorten_path(&browser.current_dir.display().to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(format!(" {directory} "))
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
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
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(
                browser.filter.as_str(),
                Style::default().fg(colors.text).bg(colors.chrome),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter).style(Style::default().bg(colors.chrome)),
        rows[0],
    );

    let visible_height = usize::from(rows[1].height);
    let recent_heading_index = browser.recent_heading_index();
    let mut lines = Vec::with_capacity(visible_height);
    for (index, entry) in entries.iter().enumerate().skip(browser.scroll_offset) {
        if Some(index) == recent_heading_index && lines.len() + 1 < visible_height {
            lines.push(picker_recent_heading_line(
                usize::from(rows[1].width),
                colors,
            ));
        }
        if lines.len() >= visible_height {
            break;
        }
        lines.push(picker_entry_line(
            entry,
            browser,
            index == browser.selected,
            usize::from(rows[1].width),
            colors,
        ));
        if lines.len() >= visible_height {
            break;
        }
    }
    if lines.is_empty() {
        let message = if browser.filter.is_empty() {
            "   No PDF files found"
        } else {
            "   No matches"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[1],
    );

    let status = if browser.recursive_loading() {
        Some((
            format!("scanning • {} shown", entries.len()),
            colors.loading,
        ))
    } else {
        let position = if entries.is_empty() {
            0
        } else {
            browser.selected + 1
        };
        Some((format!("{position}/{}", entries.len()), colors.muted))
    };
    frame.render_widget(
        Paragraph::new(picker_hint_line(
            &[("enter", "open"), ("esc", "close")],
            status,
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

fn draw_link_picker(
    frame: &mut RatatuiFrame,
    area: Rect,
    links: &[DocumentLink],
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) {
    let selected = state.selected;
    let page = state.page;
    let mut colors = PickerTheme::from(theme);
    colors.surface = picker_color(theme.bg_dark);
    colors.chrome = picker_color(theme.bg_dark1);
    let (_, pane) = link_picker_panes(area, geometry);
    frame.render_widget(RatatuiClear, pane);
    let pane_borders = match resolved_link_picker_layout(area, geometry.layout) {
        LinkPickerLayout::Vertical => Borders::LEFT,
        LinkPickerLayout::Horizontal => Borders::TOP,
        LinkPickerLayout::Floating => Borders::ALL,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let pane_block = Block::default()
        .borders(pane_borders)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border));
    let inner = pane_block.inner(pane);
    frame.render_widget(pane_block, pane);
    let detail_height = link_picker_detail_height(inner.height);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(detail_height),
        Constraint::Length(1),
    ])
    .split(inner);

    let mut header = vec![
        Span::styled(
            " Links ",
            Style::default()
                .fg(colors.accent)
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            links.len().to_string(),
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
    ];
    if progress.indexing {
        header.push(Span::styled(
            format!("  indexing {}/{}", progress.scanned, progress.total_pages),
            Style::default().fg(colors.loading).bg(colors.chrome),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(header))
            .style(Style::default().bg(colors.chrome))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(colors.border)),
            ),
        rows[0],
    );

    if links.is_empty() {
        let message = if progress.indexing {
            format!(
                "Indexing document links · {}/{} pages",
                progress.scanned, progress.total_pages
            )
        } else {
            "No annotated links in this document".to_string()
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(colors.muted).bg(colors.surface)),
            rows[1],
        );
    } else {
        let display_rows = link_picker_display_rows(links);
        let visible_height = usize::from(rows[1].height).max(1);
        let (start, end) = link_picker_row_window(&display_rows, selected, visible_height);
        let width = usize::from(rows[1].width);
        let number_width = links.len().to_string().len().max(1);
        let lines: Vec<_> = display_rows[start..end]
            .iter()
            .map(|display_row| match *display_row {
                LinkPickerDisplayRow::Page(source_page) => {
                    link_picker_page_heading_line(source_page, source_page == page, width, colors)
                }
                LinkPickerDisplayRow::Link(index) => link_picker_entry_line(
                    index,
                    &links[index],
                    selected,
                    number_width,
                    width,
                    colors,
                ),
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(colors.surface)),
            rows[1],
        );
    }

    if detail_height > 0
        && let Some(link) = links.get(selected)
    {
        draw_link_picker_detail(frame, rows[2], link, selected, colors);
    }

    let status = if links.is_empty() {
        "no links".to_string()
    } else {
        format!("{}/{}", selected + 1, links.len())
    };
    let action = match links.get(selected).map(|link| &link.target) {
        Some(LinkTarget::Uri(_)) => "copy URL",
        _ => "jump",
    };
    let compact_bindings = [("j/k", ""), ("#", ""), ("↵", ""), ("esc", "")];
    let full_bindings = [
        ("j/k", "move"),
        ("#", "select"),
        ("enter", action),
        ("esc", "close"),
    ];
    let bindings = if rows[3].width < 48 {
        compact_bindings.as_slice()
    } else {
        full_bindings.as_slice()
    };
    frame.render_widget(
        Paragraph::new(picker_hint_line(
            bindings,
            Some((status, colors.muted)),
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        rows[3],
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkPickerDisplayRow {
    Page(u32),
    Link(usize),
}

fn link_picker_display_rows(links: &[DocumentLink]) -> Vec<LinkPickerDisplayRow> {
    let mut rows = Vec::with_capacity(links.len());
    let mut previous_page = None;
    for (index, link) in links.iter().enumerate() {
        if previous_page != Some(link.source_page) {
            rows.push(LinkPickerDisplayRow::Page(link.source_page));
            previous_page = Some(link.source_page);
        }
        rows.push(LinkPickerDisplayRow::Link(index));
    }
    rows
}

fn link_picker_row_window(
    rows: &[LinkPickerDisplayRow],
    selected: usize,
    visible_height: usize,
) -> (usize, usize) {
    let selected_row = rows
        .iter()
        .position(|row| *row == LinkPickerDisplayRow::Link(selected))
        .unwrap_or(0);
    let height = visible_height.max(1).min(rows.len());
    let max_start = rows.len().saturating_sub(height);
    let start = selected_row.saturating_sub(height / 2).min(max_start);
    (start, start + height)
}

fn link_picker_page_heading_line(
    page: u32,
    current: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let label = if current {
        format!(" Page {} · current ", page + 1)
    } else {
        format!(" Page {} ", page + 1)
    };
    let used = 2 + Line::raw(label.as_str()).width();
    let mut spans = vec![
        Span::styled("  ", Style::default().bg(colors.surface)),
        Span::styled(
            label,
            Style::default()
                .fg(if current { colors.recent } else { colors.muted })
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if used < width {
        spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    Line::from(spans)
}

fn link_picker_entry_line(
    index: usize,
    link: &DocumentLink,
    selected: usize,
    number_width: usize,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let is_selected = index == selected;
    let background = if is_selected {
        colors.selection
    } else {
        colors.surface
    };
    let style = Style::default().fg(colors.text).bg(background);
    let number = format!("{:>number_width$}  ", index + 1);
    let prefix_width = 2 + Line::raw(number.as_str()).width();
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(prefix_width),
    );
    let mut line = Line::from(vec![
        Span::styled(
            if is_selected { "▌ " } else { "  " },
            style.fg(colors.accent),
        ),
        Span::styled(number, style.fg(colors.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            label,
            if is_selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        ),
    ]);
    let used = line.width();
    if used < width {
        line.spans
            .push(Span::styled(" ".repeat(width - used), style));
    }
    line
}

fn draw_link_picker_detail(
    frame: &mut RatatuiFrame,
    area: Rect,
    link: &DocumentLink,
    selected: usize,
    colors: PickerTheme,
) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(colors.border))
        .style(Style::default().bg(colors.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = usize::from(inner.width);
    let number = format!("{}  ", selected + 1);
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(Line::raw(number.as_str()).width()),
    );
    let target = match &link.target {
        LinkTarget::Internal { page, .. } => format!("PDF page {}", page + 1),
        LinkTarget::Uri(uri) => uri.clone(),
    };
    let source = format!("Page {}", link.source_page + 1);
    let fixed_width = Line::raw(source.as_str()).width() + 3;
    let target = truncate_right(&target, width.saturating_sub(fixed_width));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                number,
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label,
                Style::default()
                    .fg(colors.text)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                source,
                Style::default().fg(colors.recent).bg(colors.surface),
            ),
            Span::styled(" → ", Style::default().fg(colors.muted).bg(colors.surface)),
            Span::styled(
                target,
                Style::default().fg(colors.text_dim).bg(colors.surface),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        inner,
    );
}

fn link_picker_label(label: &str) -> String {
    let citation = label.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '[' | ']' | ',' | ';')
    });
    if !citation.is_empty() && citation.chars().all(|character| character.is_ascii_digit()) {
        format!("citation [{citation}]")
    } else {
        label.to_string()
    }
}

fn draw_help_menu(frame: &mut RatatuiFrame, theme: Palette) {
    const NAVIGATION: &[(&str, &str)] = &[
        ("j/k · ↑/↓", "move vertically"),
        ("h/l · ←/→", "move horizontally"),
        ("Space · PgDn", "page viewport forward"),
        ("Backspace · PgUp", "page viewport backward"),
        ("g / G", "first / last page"),
        (":", "go to page"),
        ("/", "search document"),
        ("n / N", "next / prev match"),
        ("Enter (links)", "document links"),
        ("Tab / Shift-Tab", "switch tabs"),
    ];
    const VIEWER: &[(&str, &str)] = &[
        ("m", "cycle fit mode"),
        ("i", "toggle dark mode"),
        ("p", "toggle performance timings"),
        ("t", "table of contents"),
        ("T", "choose theme"),
        ("y", "copy page text"),
        ("L", "toggle clickable links"),
        ("b", "return from followed link"),
        ("f", "open PDF in new tab"),
        ("q", "close tab / exit"),
        ("Esc", "clear search / exit"),
        ("?", "open help"),
    ];

    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Help ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);

    frame.render_widget(
        Paragraph::new(help_lines("Navigation", NAVIGATION, 18, colors))
            .style(Style::default().bg(colors.surface)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(help_lines("Viewer", VIEWER, 5, colors))
            .style(Style::default().bg(colors.surface)),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new(picker_hint_line(&[("? / esc / q", "close")], None, colors))
            .style(Style::default().bg(colors.chrome)),
        rows[1],
    );
}

fn help_lines(
    title: &'static str,
    bindings: &[(&str, &str)],
    key_width: usize,
    colors: PickerTheme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(bindings.len() + 2);
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(colors.directory)
            .bg(colors.surface)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(bindings.iter().map(|(key, action)| {
        Line::from(vec![
            Span::styled(
                format!(" {key:<key_width$}"),
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.selection)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {action}"),
                Style::default().fg(colors.text).bg(colors.surface),
            ),
        ])
    }));
    lines
}

fn draw_theme_picker(frame: &mut RatatuiFrame, themes: &[(String, Palette)], selected: usize) {
    let theme = themes
        .get(selected)
        .map_or(crate::theme::TOKYO_NIGHT_MOON, |(_, theme)| *theme);
    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = picker_rect(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        area,
    );
    frame.render_widget(RatatuiClear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(" Themes ")
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let visible_height = usize::from(rows[0].height).max(1);
    let first_visible = selected.saturating_add(1).saturating_sub(visible_height);
    let width = usize::from(rows[0].width);
    let lines: Vec<_> = themes
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_height)
        .map(|(index, (name, _))| {
            let is_selected = index == selected;
            let background = if is_selected {
                colors.selection
            } else {
                colors.surface
            };
            let mut style = Style::default().fg(colors.text).bg(background);
            if is_selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            let mut line = Line::from(vec![
                Span::styled(
                    if is_selected { "▌ " } else { "  " },
                    Style::default().fg(colors.accent).bg(background),
                ),
                Span::styled(name.clone(), style),
            ]);
            let used = line.width();
            if used < width {
                line.spans.push(Span::styled(
                    " ".repeat(width - used),
                    Style::default().bg(background),
                ));
            }
            line
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        rows[0],
    );

    let status = Some((format!("{}/{}", selected + 1, themes.len()), colors.muted));
    frame.render_widget(
        Paragraph::new(picker_hint_line(
            &[("j/k", "select"), ("enter", "apply"), ("esc", "cancel")],
            status,
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        rows[1],
    );
}

fn picker_recent_heading_line(width: usize, colors: PickerTheme) -> Line<'static> {
    let label = " Most Recent ";
    let mut spans = vec![
        Span::styled("  ", Style::default().bg(colors.surface)),
        Span::styled(
            label,
            Style::default()
                .fg(colors.recent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let used = 2 + label.chars().count();
    if used < width {
        spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    Line::from(spans)
}

fn truncate_left(value: &str, max_width: usize) -> String {
    if Line::raw(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let suffix_width = max_width.saturating_sub(1);
    let mut suffix = Vec::new();
    let mut used = 0;
    for character in value.chars().rev() {
        let width = Line::raw(character.to_string()).width();
        if used + width > suffix_width {
            break;
        }
        suffix.push(character);
        used += width;
    }
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

fn truncate_right(value: &str, max_width: usize) -> String {
    if Line::raw(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let prefix_width = max_width.saturating_sub(1);
    let mut prefix = String::new();
    let mut used = 0;
    for character in value.chars() {
        let width = Line::raw(character.to_string()).width();
        if used + width > prefix_width {
            break;
        }
        prefix.push(character);
        used += width;
    }
    prefix.push('…');
    prefix
}

fn picker_entry_line(
    entry: &crate::browser::BrowserEntry,
    browser: &BrowserState,
    selected: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let background = if selected {
        colors.selection
    } else {
        colors.surface
    };
    let marker_style = Style::default().fg(colors.accent).bg(background);
    let icon = if entry.name == "../" {
        "↑ "
    } else if entry.is_dir {
        "› "
    } else {
        "  "
    };
    let icon_color = if entry.name == "../" {
        colors.text_dim
    } else if entry.is_dir {
        colors.directory
    } else if entry.is_recent {
        colors.recent
    } else {
        colors.text
    };
    let mut spans = vec![
        Span::styled(if selected { "▌ " } else { "  " }, marker_style),
        Span::styled(icon, Style::default().fg(icon_color).bg(background)),
    ];

    let matches = browser.match_indices(&entry.name);
    let basename_start = if entry.is_dir {
        0
    } else {
        entry
            .name
            .char_indices()
            .rev()
            .find(|(_, character)| *character == '/')
            .map_or(0, |(index, _)| entry.name[..=index].chars().count())
    };
    for (index, character) in entry.name.chars().enumerate() {
        let foreground = if matches.binary_search(&index).is_ok() {
            colors.matched
        } else if index < basename_start || entry.name == "../" {
            colors.text_dim
        } else if entry.is_recent {
            colors.recent
        } else if entry.is_dir {
            colors.directory
        } else {
            colors.text
        };
        let mut style = Style::default().fg(foreground).bg(background);
        if matches.binary_search(&index).is_ok() || (selected && index >= basename_start) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(character.to_string(), style));
    }

    let mut line = Line::from(spans);
    let mut used = line.width();
    if entry.is_recent
        && browser.filter.is_empty()
        && let Some(parent) = entry.path.parent()
    {
        let parent = shorten_path(&parent.to_string_lossy());
        let available = width.saturating_sub(used + 2);
        if available >= 3 {
            let parent = truncate_left(&parent, available);
            let parent_width = Line::raw(parent.as_str()).width();
            let gap = width.saturating_sub(used + parent_width);
            line.spans.push(Span::styled(
                " ".repeat(gap),
                Style::default().bg(background),
            ));
            line.spans.push(Span::styled(
                parent,
                Style::default().fg(colors.text_dim).bg(background),
            ));
            used = width;
        }
    }
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(background),
        ));
    }
    line
}

fn picker_hint_line(
    bindings: &[(&str, &str)],
    status: Option<(String, RatatuiColor)>,
    colors: PickerTheme,
) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default().bg(colors.chrome))];
    for (key, action) in bindings {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(colors.accent)
                .bg(colors.selection)
                .add_modifier(Modifier::BOLD),
        ));
        if !action.is_empty() {
            spans.push(Span::styled(
                format!(" {action}  "),
                Style::default().fg(colors.muted).bg(colors.chrome),
            ));
        }
    }
    if let Some((status, color)) = status {
        spans.push(Span::styled(
            status,
            Style::default().fg(color).bg(colors.chrome),
        ));
    }
    Line::from(spans)
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

fn link_picker_detail_height(height: u16) -> u16 {
    if height >= 10 { 3 } else { 0 }
}

fn link_picker_visible_height(area: Rect, geometry: LinkPickerGeometry) -> usize {
    let (_, pane) = link_picker_panes(area, geometry);
    let border_height = match resolved_link_picker_layout(area, geometry.layout) {
        LinkPickerLayout::Vertical => 0,
        LinkPickerLayout::Horizontal => 1,
        LinkPickerLayout::Floating => 2,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let content_height = pane.height.saturating_sub(border_height);
    usize::from(
        content_height
            .saturating_sub(3 + link_picker_detail_height(content_height))
            .max(1),
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

#[derive(Clone, Copy)]
struct PickerTheme {
    backdrop: RatatuiColor,
    surface: RatatuiColor,
    chrome: RatatuiColor,
    selection: RatatuiColor,
    border: RatatuiColor,
    accent: RatatuiColor,
    directory: RatatuiColor,
    recent: RatatuiColor,
    matched: RatatuiColor,
    loading: RatatuiColor,
    text: RatatuiColor,
    text_dim: RatatuiColor,
    muted: RatatuiColor,
}

impl From<Palette> for PickerTheme {
    fn from(theme: Palette) -> Self {
        Self {
            backdrop: picker_color(theme.bg_dark1),
            surface: picker_color(theme.bg),
            chrome: picker_color(theme.bg_dark),
            selection: picker_color(theme.bg_highlight),
            border: picker_color(theme.blue7),
            accent: picker_color(theme.blue),
            directory: picker_color(theme.blue1),
            recent: picker_color(theme.yellow),
            matched: picker_color(theme.magenta),
            loading: picker_color(theme.cyan),
            text: picker_color(theme.fg),
            text_dim: picker_color(theme.fg_dark),
            muted: picker_color(theme.comment),
        }
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
        BrowserState, FILE_STABLE_FOR, FileFingerprint, FileWatcher, LinkIndexProgress,
        LinkPickerGeometry, LinkPickerImage, LinkPickerState, PerformanceSnapshot, PositionedImage,
        SearchState, apply_picker_navigation, clear_picker, cycled_tab_index, draw_help_menu,
        draw_link_picker, draw_picker, draw_theme_picker, filter_outline, link_at_cell,
        link_picker_label, link_picker_panes, link_picker_visible_height, outline_start_index,
        picker_color, picker_rect, render_timing_status, restore_link_picker_split,
        search_target_page, shorten_path, show_link_picker_split, stale_status_row,
        update_link_number_selection, write_clipboard_osc52,
    };
    use crate::config::LinkPickerLayout;
    use crate::kitty::Placement;
    use crate::pdf::{
        DocumentLink, LinkTarget, OutlineItem, PageLink, PageLinkRect, SearchPageMatch,
    };
    use crate::terminal::ImagePlacement;
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
    fn tab_switching_wraps_in_both_directions() {
        assert_eq!(cycled_tab_index(0, 3, 1), 1);
        assert_eq!(cycled_tab_index(2, 3, 1), 0);
        assert_eq!(cycled_tab_index(2, 3, -1), 1);
        assert_eq!(cycled_tab_index(0, 3, -1), 2);
    }

    #[test]
    fn search_navigation_wraps_between_matching_pages() {
        let matches = [
            SearchPageMatch {
                page: 2,
                occurrences: 1,
            },
            SearchPageMatch {
                page: 5,
                occurrences: 2,
            },
            SearchPageMatch {
                page: 9,
                occurrences: 1,
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
        };
        assert_eq!(
            search.status_label(0).as_deref(),
            Some("  search 8/20  /needle")
        );

        search.searching = false;
        search.matches = vec![SearchPageMatch {
            page: 4,
            occurrences: 3,
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
    fn mouse_cell_intersection_selects_tiny_link_targets() {
        let target = LinkTarget::Internal {
            page: 7,
            top_ratio: Some(0.5),
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
    fn persistent_link_picker_tracks_the_current_page_in_the_document_index() {
        let links = vec![
            DocumentLink {
                source_page: 0,
                ordinal: 0,
                label: "first".into(),
                target: LinkTarget::Internal {
                    page: 4,
                    top_ratio: None,
                },
            },
            DocumentLink {
                source_page: 4,
                ordinal: 0,
                label: "current".into(),
                target: LinkTarget::Internal {
                    page: 6,
                    top_ratio: None,
                },
            },
            DocumentLink {
                source_page: 6,
                ordinal: 0,
                label: "later".into(),
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
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
                ordinal: 0,
                label: "[12]".into(),
                target: LinkTarget::Internal {
                    page: 7,
                    top_ratio: None,
                },
            },
            DocumentLink {
                source_page: 3,
                ordinal: 0,
                label: "project page".into(),
                target: LinkTarget::Uri("https://example.invalid/paper".into()),
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
                    &links,
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
        assert!(rendered.contains("Page 3 · current"));
        assert!(rendered.contains("citation [12]"));
        assert!(!rendered.contains("PDF page 8"));
        assert!(rendered.contains("project page"));
        assert!(!rendered.contains("Selected"));
        assert!(rendered.contains("Page 4 → https://example.invalid/paper"));
        assert!(rendered.contains("copy URL"));
        assert!(rendered.contains("2/2"));
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
                    &[],
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
            ordinal: 0,
            label: "project page".into(),
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
                    &links,
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
    }

    #[test]
    fn document_link_sidebar_uses_compact_controls_when_narrow() {
        let links = vec![DocumentLink {
            source_page: 0,
            ordinal: 0,
            label: "project page".into(),
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
                    &links,
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

        assert!(rendered.contains("j/k"));
        assert!(rendered.contains("↵"));
        assert!(rendered.contains("esc"));
        assert!(rendered.contains("1/1"));
    }

    #[test]
    fn link_picker_reserves_room_for_details_when_space_allows() {
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 100, 30),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            24
        );
        assert_eq!(
            link_picker_visible_height(
                Rect::new(0, 0, 20, 8),
                LinkPickerGeometry::new(50, LinkPickerLayout::Auto),
            ),
            5
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
            .draw(|frame| draw_picker(frame, &browser, crate::theme::TOKYO_NIGHT_MOON))
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
            .draw(|frame| draw_picker(frame, &browser, theme))
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
            .draw(|frame| draw_theme_picker(frame, &themes, 1))
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
    }

    #[test]
    fn help_menu_lists_viewer_keybindings() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = picker_rect(area);
        let theme = crate::theme::TOKYO_NIGHT_MOON;
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_help_menu(frame, theme))
            .expect("draw help menu");
        let buffer = terminal.backend().buffer();
        let rendered: String = (popup.y..popup.y + popup.height)
            .flat_map(|y| {
                (popup.x..popup.x + popup.width).map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert_eq!(buffer[(0, 0)].bg, picker_color(theme.bg_dark1));
        assert!(rendered.contains("Navigation"));
        assert!(rendered.contains("Viewer"));
        assert!(rendered.contains("search document"));
        assert!(rendered.contains("next / prev match"));
        assert!(rendered.contains("document links"));
        assert!(rendered.contains("toggle performance timings"));
        assert!(rendered.contains("choose theme"));
        assert!(rendered.contains("open PDF in new tab"));
        assert!(rendered.contains("clear search / exit"));
        assert!(rendered.contains("? / esc / q"));
    }

    #[test]
    fn picker_labels_recent_files_with_parent_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recent = directory.path().join("recent.pdf");
        let parent = shorten_path(&directory.path().display().to_string());
        fs::write(&recent, b"synthetic").expect("PDF");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent]);
        let area = Rect::new(0, 0, 80, 30);
        let popup = picker_rect(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        terminal
            .draw(|frame| draw_picker(frame, &browser, crate::theme::TOKYO_NIGHT_MOON))
            .expect("draw picker");
        let buffer = terminal.backend().buffer();
        let rendered: String = (popup.y + 1..popup.y + popup.height - 1)
            .flat_map(|y| {
                (popup.x + 1..popup.x + popup.width - 1)
                    .map(move |x| buffer[(x, y)].symbol().to_string())
            })
            .collect();

        assert!(rendered.contains("Most Recent"));
        assert!(rendered.contains(&parent));
    }

    #[test]
    fn closing_picker_clears_its_terminal_buffer() {
        let mut output = Vec::new();

        clear_picker(&mut output, crate::theme::TOKYO_NIGHT_MOON).expect("clear picker");

        assert!(output.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(output.windows(6).any(|window| window == b"\x1b[?25l"));
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
}

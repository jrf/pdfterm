use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased, unbounded};
use pdfium_render::prelude::{
    PdfAction, PdfBookmark, PdfDestination, PdfDestinationViewSettings, PdfDocument, PdfLink,
    PdfMatrix, PdfPage, PdfPageObject, PdfPageObjectCommon, PdfPageObjectsCommon, PdfPoints,
    PdfRect, PdfRenderConfig, Pdfium,
};

const LOW_CHROMA_THRESHOLD: u8 = 10;
const MAX_DARK_MODE_WORKERS: usize = 8;
const MAX_FORM_DEPTH: u8 = 32;
const IMAGE_MASK_SAMPLES: usize = 4;
const PARALLEL_DARK_MODE_PIXELS: usize = 250_000;
const SEARCH_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const SEARCH_HIGHLIGHT_ALPHA: u16 = 88;
const LINK_HIGHLIGHT_ALPHA: u16 = 40;
const LINK_BORDER_ALPHA: u16 = 210;

pub type DocumentId = u64;

/// One entry in a document's outline (table of contents).
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub page: u32,
    pub depth: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchPageMatch {
    pub page: u32,
    pub occurrences: u32,
}

/// How a page is scaled to the terminal viewport.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FitMode {
    /// Fit the whole page within the viewport (no scrolling).
    #[default]
    Page,
    /// Match the page width to the viewport; scroll vertically when taller.
    Width,
    /// Match the page height to the viewport; scroll horizontally when wider.
    Height,
}

impl FitMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Page => Self::Width,
            Self::Width => Self::Height,
            Self::Height => Self::Page,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Page => "fit-page",
            Self::Width => "fit-width",
            Self::Height => "fit-height",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DarkModeStyle {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
}

impl DarkModeStyle {
    pub const fn new(background: [u8; 3], foreground: [u8; 3]) -> Self {
        Self {
            background,
            foreground,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderKey {
    pub document_id: DocumentId,
    pub page: u32,
    pub width: u16,
    pub height: u16,
    pub fit: FitMode,
    pub invert: bool,
    pub dark_mode_style: DarkModeStyle,
    pub search_request_id: u64,
    pub search_highlight: [u8; 3],
    pub link_mode: bool,
    pub link_highlight: [u8; 3],
}

#[derive(Debug)]
pub struct RenderRequest {
    pub key: RenderKey,
    pub generation: u64,
}

#[derive(Debug)]
pub struct Frame {
    pub key: RenderKey,
    pub width: u32,
    pub height: u32,
    pub compressed_rgba: Vec<u8>,
    pub render_elapsed: Duration,
    pub dark_mode_elapsed: Option<Duration>,
    pub highlight_elapsed: Option<Duration>,
    pub compression_elapsed: Duration,
    pub generation: u64,
    pub links: Vec<PageLink>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Internal { page: u32, top_ratio: Option<f32> },
    Uri(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageLinkRect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageLink {
    pub rect: PageLinkRect,
    pub label: String,
    pub target: LinkTarget,
}

#[derive(Debug)]
pub enum WorkerMessage {
    Ready {
        pages: u32,
        outline: Vec<OutlineItem>,
    },
    Opened {
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
    },
    OpenError {
        document_id: DocumentId,
        error: String,
    },
    Text {
        document_id: DocumentId,
        page: u32,
        content: String,
    },
    SearchProgress {
        document_id: DocumentId,
        request_id: u64,
        scanned: u32,
        total: u32,
    },
    SearchResults {
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    },
    Frame(Frame),
    Error(String),
}

enum WorkerCommand {
    Open {
        document_id: DocumentId,
        path: PathBuf,
    },
    Close(DocumentId),
    ExtractText {
        document_id: DocumentId,
        page: u32,
    },
    Search {
        document_id: DocumentId,
        request_id: u64,
        query: String,
    },
    CancelSearch {
        document_id: DocumentId,
        request_id: u64,
    },
}

enum WorkerTask {
    Open {
        document_id: DocumentId,
        path: PathBuf,
    },
    Close(DocumentId),
    ExtractText {
        document_id: DocumentId,
        page: u32,
    },
    StartSearch {
        document_id: DocumentId,
        request_id: u64,
        query: String,
    },
    CancelSearch {
        document_id: DocumentId,
        request_id: u64,
    },
    SearchPage(SearchJob),
    Render(RenderRequest),
}

struct SearchJob {
    document_id: DocumentId,
    request_id: u64,
    needle: String,
    next_page: u32,
    total_pages: u32,
    matches: Vec<SearchPageMatch>,
    total_occurrences: u32,
    highlights: HashMap<u32, Vec<SearchRect>>,
    last_progress: Instant,
}

struct SearchHighlights {
    request_id: u64,
    pages: HashMap<u32, Vec<SearchRect>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SearchRect {
    bottom: f32,
    left: f32,
    top: f32,
    right: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

struct CachedPageText {
    raw: String,
    normalized: String,
    source_index_by_byte: Vec<usize>,
}

struct WorkerChannels {
    priority_rx: Receiver<RenderRequest>,
    prefetch_rx: Receiver<RenderRequest>,
    command_rx: Receiver<WorkerCommand>,
    message_tx: Sender<WorkerMessage>,
}

pub struct RenderWorker {
    priority_tx: Sender<RenderRequest>,
    prefetch_tx: Sender<RenderRequest>,
    command_tx: Sender<WorkerCommand>,
    message_rx: Receiver<WorkerMessage>,
    latest_generation: Arc<AtomicU64>,
}

impl RenderWorker {
    pub fn spawn(document_id: DocumentId, path: PathBuf, pdfium_library: Option<PathBuf>) -> Self {
        let (priority_tx, priority_rx) = unbounded();
        let (prefetch_tx, prefetch_rx) = unbounded();
        let (command_tx, command_rx) = unbounded();
        let (message_tx, message_rx) = unbounded();
        let latest_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&latest_generation);

        thread::spawn(move || {
            run_worker(
                path,
                document_id,
                pdfium_library.as_deref(),
                WorkerChannels {
                    priority_rx,
                    prefetch_rx,
                    command_rx,
                    message_tx,
                },
                worker_generation,
            );
        });

        Self {
            priority_tx,
            prefetch_tx,
            command_tx,
            message_rx,
            latest_generation,
        }
    }

    pub fn wait_until_ready(&self) -> Result<(u32, Vec<OutlineItem>), String> {
        match self.message_rx.recv() {
            Ok(WorkerMessage::Ready { pages, outline }) => Ok((pages, outline)),
            Ok(WorkerMessage::Error(error)) => Err(error),
            Ok(
                WorkerMessage::Opened { .. }
                | WorkerMessage::OpenError { .. }
                | WorkerMessage::Text { .. }
                | WorkerMessage::SearchProgress { .. }
                | WorkerMessage::SearchResults { .. }
                | WorkerMessage::Frame(_),
            ) => Err("renderer sent a frame before initialization".into()),
            Err(_) => Err("renderer stopped during initialization".into()),
        }
    }

    pub fn render(&self, request: RenderRequest) -> Result<(), String> {
        self.priority_tx
            .send(request)
            .map_err(|_| "renderer stopped".into())
    }

    pub fn prefetch(&self, request: RenderRequest) {
        let _ = self.prefetch_tx.send(request);
    }

    pub fn begin_generation(&self, generation: u64) {
        self.latest_generation.store(generation, Ordering::Release);
    }

    pub fn open(&self, document_id: DocumentId, path: PathBuf) -> Result<(), String> {
        self.command_tx
            .send(WorkerCommand::Open { document_id, path })
            .map_err(|_| "renderer stopped".into())
    }

    pub fn close(&self, document_id: DocumentId) {
        let _ = self.command_tx.send(WorkerCommand::Close(document_id));
    }

    pub fn extract_text(&self, document_id: DocumentId, page: u32) {
        let _ = self
            .command_tx
            .send(WorkerCommand::ExtractText { document_id, page });
    }

    pub fn search(&self, document_id: DocumentId, request_id: u64, query: String) {
        let _ = self.command_tx.send(WorkerCommand::Search {
            document_id,
            request_id,
            query,
        });
    }

    pub fn cancel_search(&self, document_id: DocumentId, request_id: u64) {
        let _ = self.command_tx.send(WorkerCommand::CancelSearch {
            document_id,
            request_id,
        });
    }

    pub fn try_recv(&self) -> Result<WorkerMessage, TryRecvError> {
        self.message_rx.try_recv()
    }
}

fn run_worker(
    path: PathBuf,
    initial_document_id: DocumentId,
    pdfium_library: Option<&Path>,
    channels: WorkerChannels,
    latest_generation: Arc<AtomicU64>,
) {
    let WorkerChannels {
        priority_rx,
        prefetch_rx,
        command_rx,
        message_tx,
    } = channels;
    let result = (|| -> Result<(), String> {
        let pdfium = load_pdfium(pdfium_library)?;
        let document = pdfium
            .load_pdf_from_file(&path, None)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let pages = u32::try_from(document.pages().len())
            .map_err(|_| "PDFium returned a negative page count".to_string())?;
        if pages == 0 {
            return Err(format!("{} has no pages", path.display()));
        }
        let outline = extract_outline(&document);
        message_tx
            .send(WorkerMessage::Ready { pages, outline })
            .map_err(|_| "viewer stopped".to_string())?;
        let mut documents = HashMap::from([(initial_document_id, document)]);
        let mut text_cache: HashMap<DocumentId, Vec<Option<CachedPageText>>> =
            HashMap::from([(initial_document_id, empty_text_cache(pages))]);
        let mut search_jobs = VecDeque::new();
        let mut search_highlights: HashMap<DocumentId, SearchHighlights> = HashMap::new();

        loop {
            let task = match command_rx.try_recv() {
                Ok(command) => command.into(),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => {
                    match priority_rx.try_recv() {
                        Ok(request) => WorkerTask::Render(request),
                        Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {
                            if let Some(job) = search_jobs.pop_front() {
                                WorkerTask::SearchPage(job)
                            } else {
                                match prefetch_rx.try_recv() {
                                    Ok(request) => WorkerTask::Render(request),
                                    Err(TryRecvError::Disconnected | TryRecvError::Empty) => {
                                        select_biased! {
                                            recv(command_rx) -> command => match command {
                                                Ok(command) => command.into(),
                                                Err(_) => break,
                                            },
                                            recv(priority_rx) -> request => match request {
                                                Ok(request) => WorkerTask::Render(request),
                                                Err(_) => break,
                                            },
                                            recv(prefetch_rx) -> request => match request {
                                                Ok(request) => WorkerTask::Render(request),
                                                Err(_) => break,
                                            },
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };

            let request = match task {
                WorkerTask::Open {
                    document_id,
                    path: new_path,
                } => {
                    match pdfium.load_pdf_from_file(&new_path, None) {
                        Ok(replacement) => {
                            let pages = u32::try_from(replacement.pages().len()).map_err(|_| {
                                "PDFium returned a negative page count while opening a document"
                                    .to_string()
                            })?;
                            if pages == 0 {
                                message_tx
                                    .send(WorkerMessage::OpenError {
                                        document_id,
                                        error: format!("{} has no pages", new_path.display()),
                                    })
                                    .map_err(|_| "viewer stopped".to_string())?;
                            } else {
                                let outline = extract_outline(&replacement);
                                documents.insert(document_id, replacement);
                                text_cache.insert(document_id, empty_text_cache(pages));
                                search_jobs.retain(|job| job.document_id != document_id);
                                search_highlights.remove(&document_id);
                                message_tx
                                    .send(WorkerMessage::Opened {
                                        document_id,
                                        pages,
                                        outline,
                                    })
                                    .map_err(|_| "viewer stopped".to_string())?;
                            }
                        }
                        Err(error) => {
                            message_tx
                                .send(WorkerMessage::OpenError {
                                    document_id,
                                    error: format!(
                                        "could not open {}: {error}",
                                        new_path.display()
                                    ),
                                })
                                .map_err(|_| "viewer stopped".to_string())?;
                        }
                    }
                    continue;
                }
                WorkerTask::Close(document_id) => {
                    documents.remove(&document_id);
                    text_cache.remove(&document_id);
                    search_jobs.retain(|job| job.document_id != document_id);
                    search_highlights.remove(&document_id);
                    continue;
                }
                WorkerTask::ExtractText { document_id, page } => {
                    if let (Some(document), Some(cache)) = (
                        documents.get(&document_id),
                        text_cache.get_mut(&document_id),
                    ) {
                        let content = cached_page_text(document, page, cache)
                            .map_or_else(String::new, |text| text.raw.clone());
                        message_tx
                            .send(WorkerMessage::Text {
                                document_id,
                                page,
                                content,
                            })
                            .map_err(|_| "viewer stopped".to_string())?;
                    }
                    continue;
                }
                WorkerTask::StartSearch {
                    document_id,
                    request_id,
                    query,
                } => {
                    search_jobs.retain(|job| job.document_id != document_id);
                    search_highlights.remove(&document_id);
                    let needle = normalize_search_text(&query);
                    if !needle.is_empty()
                        && let Some(cache) = text_cache.get(&document_id)
                    {
                        let total_pages = u32::try_from(cache.len()).unwrap_or(u32::MAX);
                        search_jobs.push_front(SearchJob {
                            document_id,
                            request_id,
                            needle,
                            next_page: 0,
                            total_pages,
                            matches: Vec::new(),
                            total_occurrences: 0,
                            highlights: HashMap::new(),
                            last_progress: Instant::now(),
                        });
                        message_tx
                            .send(WorkerMessage::SearchProgress {
                                document_id,
                                request_id,
                                scanned: 0,
                                total: total_pages,
                            })
                            .map_err(|_| "viewer stopped".to_string())?;
                    }
                    continue;
                }
                WorkerTask::CancelSearch {
                    document_id,
                    request_id,
                } => {
                    search_jobs.retain(|job| {
                        job.document_id != document_id || job.request_id != request_id
                    });
                    if search_highlights
                        .get(&document_id)
                        .is_some_and(|highlights| highlights.request_id == request_id)
                    {
                        search_highlights.remove(&document_id);
                    }
                    continue;
                }
                WorkerTask::SearchPage(mut job) => {
                    let Some(document) = documents.get(&job.document_id) else {
                        continue;
                    };
                    let Some(cache) = text_cache.get_mut(&job.document_id) else {
                        continue;
                    };
                    let (occurrences, rectangles) =
                        search_page(document, job.next_page, cache, &job.needle);
                    if occurrences > 0 {
                        job.matches.push(SearchPageMatch {
                            page: job.next_page,
                            occurrences,
                        });
                        job.total_occurrences = job.total_occurrences.saturating_add(occurrences);
                        job.highlights.insert(job.next_page, rectangles);
                    }
                    job.next_page = job.next_page.saturating_add(1);

                    if job.next_page >= job.total_pages {
                        search_highlights.insert(
                            job.document_id,
                            SearchHighlights {
                                request_id: job.request_id,
                                pages: job.highlights,
                            },
                        );
                        message_tx
                            .send(WorkerMessage::SearchResults {
                                document_id: job.document_id,
                                request_id: job.request_id,
                                matches: job.matches,
                                total_occurrences: job.total_occurrences,
                            })
                            .map_err(|_| "viewer stopped".to_string())?;
                    } else {
                        if job.last_progress.elapsed() >= SEARCH_PROGRESS_INTERVAL {
                            message_tx
                                .send(WorkerMessage::SearchProgress {
                                    document_id: job.document_id,
                                    request_id: job.request_id,
                                    scanned: job.next_page,
                                    total: job.total_pages,
                                })
                                .map_err(|_| "viewer stopped".to_string())?;
                            job.last_progress = Instant::now();
                        }
                        search_jobs.push_back(job);
                    }
                    continue;
                }
                WorkerTask::Render(request) => request,
            };

            if request.generation != latest_generation.load(Ordering::Acquire) {
                continue;
            }

            let page_index = i32::try_from(request.key.page).map_err(|_| {
                format!("page {} exceeds PDFium's index range", request.key.page + 1)
            })?;
            let Some(document) = documents.get(&request.key.document_id) else {
                continue;
            };
            let page = document.pages().get(page_index).map_err(|error| {
                format!("could not load page {}: {error}", request.key.page + 1)
            })?;
            let target_width = i32::from(request.key.width);
            let target_height = i32::from(request.key.height);
            let base_config = PdfRenderConfig::new()
                .set_reverse_byte_order(true)
                .use_lcd_text_rendering(true)
                .force_half_tone(false)
                .use_print_quality(false);
            let config = match request.key.fit {
                FitMode::Page => {
                    base_config.scale_page_to_display_size(target_width, target_height)
                }
                FitMode::Width => base_config.set_target_width(target_width),
                FitMode::Height => base_config.set_target_height(target_height),
            };
            let render_started = Instant::now();
            let bitmap = page.render_with_config(&config).map_err(|error| {
                format!("could not render page {}: {error}", request.key.page + 1)
            })?;
            let render_elapsed = render_started.elapsed();

            let width = bitmap.width() as u32;
            let height = bitmap.height() as u32;
            let mut raw_rgba = bitmap.as_raw_bytes();
            let dark_mode_elapsed = request.key.invert.then(|| {
                let started = Instant::now();
                apply_dark_mode(
                    &page,
                    &config,
                    width,
                    height,
                    &mut raw_rgba,
                    request.key.dark_mode_style,
                );
                started.elapsed()
            });
            let search_rectangles = (request.key.search_request_id != 0)
                .then(|| {
                    search_highlights
                        .get(&request.key.document_id)
                        .filter(|highlights| highlights.request_id == request.key.search_request_id)
                        .and_then(|highlights| highlights.pages.get(&request.key.page))
                })
                .flatten();
            let links = if request.key.link_mode {
                extract_page_links(document, &page, &config, width, height)
            } else {
                Vec::new()
            };
            let highlight_elapsed = (search_rectangles.is_some() || !links.is_empty()).then(|| {
                let started = Instant::now();
                if let Some(rectangles) = search_rectangles {
                    apply_search_highlights(
                        &page,
                        &config,
                        width,
                        height,
                        &mut raw_rgba,
                        rectangles,
                        request.key.search_highlight,
                    );
                }
                if !links.is_empty() {
                    apply_link_highlights(
                        &mut raw_rgba,
                        width,
                        height,
                        &links,
                        request.key.link_highlight,
                    );
                }
                started.elapsed()
            });
            let compression_started = Instant::now();
            let compressed_rgba = crate::kitty::compress_rgba(&raw_rgba).map_err(|error| {
                format!("could not compress page {}: {error}", request.key.page + 1)
            })?;
            let compression_elapsed = compression_started.elapsed();

            if request.generation != latest_generation.load(Ordering::Acquire) {
                continue;
            }

            message_tx
                .send(WorkerMessage::Frame(Frame {
                    key: request.key,
                    width,
                    height,
                    compressed_rgba,
                    render_elapsed,
                    dark_mode_elapsed,
                    highlight_elapsed,
                    compression_elapsed,
                    generation: request.generation,
                    links,
                }))
                .map_err(|_| "viewer stopped".to_string())?;
        }

        Ok(())
    })();

    if let Err(error) = result {
        let _ = message_tx.send(WorkerMessage::Error(error));
    }
}

/// Reads a document's bookmark tree into a flat, depth-tagged outline in
/// prefix (reading) order. Bookmarks without a resolvable destination page are
/// skipped, but their children are still visited.
fn extract_outline(document: &PdfDocument) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    if let Some(root) = document.bookmarks().root() {
        collect_bookmarks(root, 0, &mut items);
    }
    items
}

fn collect_bookmarks(mut bookmark: PdfBookmark, depth: u16, items: &mut Vec<OutlineItem>) {
    loop {
        if let Some(page) = bookmark
            .destination()
            .and_then(|destination| destination.page_index().ok())
            .and_then(|index| u32::try_from(index).ok())
        {
            let title = bookmark.title().unwrap_or_default();
            let title = if title.trim().is_empty() {
                "(untitled)".to_string()
            } else {
                title
            };
            items.push(OutlineItem { title, page, depth });
        }
        if let Some(child) = bookmark.first_child() {
            collect_bookmarks(child, depth.saturating_add(1), items);
        }
        match bookmark.next_sibling() {
            Some(sibling) => bookmark = sibling,
            None => break,
        }
    }
}

/// Applies Polaris's dark-mode lightness transform while leaving embedded
/// images unchanged.
fn apply_dark_mode(
    page: &PdfPage<'_>,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    rgba: &mut [u8],
    style: DarkModeStyle,
) {
    let mask = image_mask(page, config, width, height);
    darken_rgba(rgba, mask.as_deref(), style);
}

/// Builds a pixel mask for image page objects so photos and figures retain
/// their original colors.
fn image_mask(
    page: &PdfPage<'_>,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    let mut mask = None;

    for object in page.objects().iter() {
        mask_images_in_object(
            &object,
            PdfMatrix::IDENTITY,
            0,
            page,
            config,
            width,
            height,
            &mut mask,
        );
    }

    mask
}

#[allow(clippy::too_many_arguments)]
fn mask_images_in_object(
    object: &PdfPageObject<'_>,
    parent_transform: PdfMatrix,
    depth: u8,
    page: &PdfPage<'_>,
    config: &PdfRenderConfig,
    width: usize,
    height: usize,
    mask: &mut Option<Vec<u8>>,
) {
    if object.as_image_object().is_some() {
        let Ok(bounds) = object.bounds() else {
            return;
        };
        let corners = [
            (bounds.x1(), bounds.y1()),
            (bounds.x2(), bounds.y2()),
            (bounds.x3(), bounds.y3()),
            (bounds.x4(), bounds.y4()),
        ]
        .map(|(x, y)| parent_transform.apply_to_points(x, y));
        let pixels = corners.map(|(x, y)| page.points_to_pixels(x, y, config));
        let [Ok(p1), Ok(p2), Ok(p3), Ok(p4)] = pixels else {
            return;
        };
        let mask = mask.get_or_insert_with(|| vec![0; width * height]);
        mask_quadrilateral(
            mask,
            width,
            height,
            [(p1.0, p1.1), (p2.0, p2.1), (p3.0, p3.1), (p4.0, p4.1)],
        );
        return;
    }

    if depth >= MAX_FORM_DEPTH {
        return;
    }
    let Some(form) = object.as_x_object_form_object() else {
        return;
    };
    let Ok(form_transform) = form.matrix() else {
        return;
    };
    let child_transform = form_transform.multiply(parent_transform);
    for child in form.iter() {
        mask_images_in_object(
            &child,
            child_transform,
            depth + 1,
            page,
            config,
            width,
            height,
            mask,
        );
    }
}

fn mask_quadrilateral(mask: &mut [u8], width: usize, height: usize, points: [(i32, i32); 4]) {
    let left = points
        .iter()
        .map(|point| point.0)
        .min()
        .unwrap()
        .clamp(0, width as i32) as usize;
    let right = points
        .iter()
        .map(|point| point.0)
        .max()
        .unwrap()
        .clamp(0, width as i32) as usize;
    let top = points
        .iter()
        .map(|point| point.1)
        .min()
        .unwrap()
        .clamp(0, height as i32) as usize;
    let bottom = points
        .iter()
        .map(|point| point.1)
        .max()
        .unwrap()
        .clamp(0, height as i32) as usize;
    if left >= right || top >= bottom {
        return;
    }

    let points = points.map(|(x, y)| (x as f32, y as f32));
    let local_width = right - left;
    let mut differences = vec![0_i16; local_width + 1];
    for y in top..bottom {
        differences.fill(0);
        let mut boundaries = [(0, 0_u16); IMAGE_MASK_SAMPLES * 2];
        let mut boundary_count = 0;
        for sample in 0..IMAGE_MASK_SAMPLES {
            let sample_y = y as f32 + (sample as f32 + 0.5) / IMAGE_MASK_SAMPLES as f32;
            let Some((start, end)) = polygon_span(&points, sample_y) else {
                continue;
            };
            add_span_coverage(
                &mut differences,
                &mut boundaries,
                &mut boundary_count,
                start.clamp(left as f32, right as f32) - left as f32,
                end.clamp(left as f32, right as f32) - left as f32,
            );
        }

        boundaries[..boundary_count].sort_unstable_by_key(|boundary| boundary.0);
        let row = &mut mask[y * width + left..y * width + right];
        let mut running = 0_i16;
        let mut boundary = 0;
        for (x, masked) in row.iter_mut().enumerate() {
            running += differences[x];
            let mut coverage = running as u16;
            while boundary < boundary_count && boundaries[boundary].0 == x {
                coverage += boundaries[boundary].1;
                boundary += 1;
            }
            let coverage =
                ((coverage + IMAGE_MASK_SAMPLES as u16 / 2) / IMAGE_MASK_SAMPLES as u16) as u8;
            *masked = (*masked).max(coverage);
        }
    }
}

fn polygon_span(points: &[(f32, f32); 4], y: f32) -> Option<(f32, f32)> {
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    let mut intersections = 0;
    for index in 0..points.len() {
        let (x1, y1) = points[index];
        let (x2, y2) = points[(index + 1) % points.len()];
        if !((y1 <= y && y < y2) || (y2 <= y && y < y1)) {
            continue;
        }
        let x = x1 + (y - y1) * (x2 - x1) / (y2 - y1);
        left = left.min(x);
        right = right.max(x);
        intersections += 1;
    }
    (intersections >= 2 && left < right).then_some((left, right))
}

fn add_span_coverage(
    differences: &mut [i16],
    boundaries: &mut [(usize, u16)],
    boundary_count: &mut usize,
    start: f32,
    end: f32,
) {
    if start >= end {
        return;
    }
    let first = start.floor().max(0.0) as usize;
    let last = end.ceil().min((differences.len() - 1) as f32) as usize;
    if first >= last {
        return;
    }
    if last == first + 1 {
        boundaries[*boundary_count] = (first, ((end - start) * 255.0).round() as u16);
        *boundary_count += 1;
        return;
    }

    boundaries[*boundary_count] = (first, ((first as f32 + 1.0 - start) * 255.0).round() as u16);
    *boundary_count += 1;

    let full_end = end.floor() as usize;
    if first + 1 < full_end {
        differences[first + 1] += 255;
        differences[full_end] -= 255;
    }
    if full_end < last {
        boundaries[*boundary_count] = (full_end, ((end - full_end as f32) * 255.0).round() as u16);
        *boundary_count += 1;
    }
}

#[derive(Clone, Copy)]
struct DarkModeTransform {
    style: DarkModeStyle,
    background_lightness: f32,
    lightness_range: f32,
}

impl DarkModeTransform {
    fn new(style: DarkModeStyle) -> Self {
        let background_lightness = rgb_lightness(style.background);
        Self {
            style,
            background_lightness,
            lightness_range: rgb_lightness(style.foreground) - background_lightness,
        }
    }
}

fn rgb_lightness(color: [u8; 3]) -> f32 {
    let max = color[0].max(color[1]).max(color[2]);
    let min = color[0].min(color[1]).min(color[2]);
    (f32::from(max) + f32::from(min)) / (255.0 * 2.0)
}

fn darken_rgba(rgba: &mut [u8], mask: Option<&[u8]>, style: DarkModeStyle) {
    let pixel_count = rgba.len() / 4;
    debug_assert_eq!(rgba.len(), pixel_count * 4);
    debug_assert!(mask.is_none_or(|mask| mask.len() == pixel_count));
    let transform = DarkModeTransform::new(style);

    let worker_count = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_DARK_MODE_WORKERS)
        .min(pixel_count);
    if pixel_count < PARALLEL_DARK_MODE_PIXELS || worker_count == 1 {
        darken_rgba_chunk(rgba, mask, transform);
        return;
    }

    let pixels_per_chunk = pixel_count.div_ceil(worker_count);
    let bytes_per_chunk = pixels_per_chunk * 4;
    thread::scope(|scope| match mask {
        Some(mask) => {
            for (rgba, mask) in rgba
                .chunks_mut(bytes_per_chunk)
                .zip(mask.chunks(pixels_per_chunk))
            {
                scope.spawn(move || darken_rgba_chunk(rgba, Some(mask), transform));
            }
        }
        None => {
            for rgba in rgba.chunks_mut(bytes_per_chunk) {
                scope.spawn(move || darken_rgba_chunk(rgba, None, transform));
            }
        }
    });
}

fn darken_rgba_chunk(rgba: &mut [u8], mask: Option<&[u8]>, transform: DarkModeTransform) {
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let mask_value = mask.map_or(0, |mask| mask[index]);
        if mask_value == 255 {
            continue;
        }

        let [red, green, blue, _alpha] = pixel else {
            unreachable!();
        };
        let original = [*red, *green, *blue];
        let transformed = dark_mode_pixel(original, transform);
        if mask_value == 0 {
            [*red, *green, *blue] = transformed;
        } else {
            let mask_ratio = f32::from(mask_value) / 255.0;
            *red = blend_channel(transformed[0], original[0], mask_ratio);
            *green = blend_channel(transformed[1], original[1], mask_ratio);
            *blue = blend_channel(transformed[2], original[2], mask_ratio);
        }
    }
}

fn dark_mode_pixel([red, green, blue]: [u8; 3], transform: DarkModeTransform) -> [u8; 3] {
    let max_channel = red.max(green).max(blue);
    let min_channel = red.min(green).min(blue);
    if max_channel - min_channel < LOW_CHROMA_THRESHOLD {
        let sum = usize::from(red) + usize::from(green) + usize::from(blue);
        let amount = dark_mode_curve_lut()[sum];
        return std::array::from_fn(|channel| {
            lerp_channel(
                transform.style.background[channel],
                transform.style.foreground[channel],
                amount,
            )
        });
    }

    let red_f = f32::from(red) / 255.0;
    let green_f = f32::from(green) / 255.0;
    let blue_f = f32::from(blue) / 255.0;
    let max_f = f32::from(max_channel) / 255.0;
    let min_f = f32::from(min_channel) / 255.0;
    let lightness = (max_f + min_f) * 0.5;
    let new_lightness =
        transform.background_lightness + (1.0 - lightness.powf(1.2)) * transform.lightness_range;
    let chroma = max_f - min_f;

    let hue = if max_channel == red {
        (green_f - blue_f) / chroma + if green_f < blue_f { 6.0 } else { 0.0 }
    } else if max_channel == green {
        (blue_f - red_f) / chroma + 2.0
    } else {
        (red_f - green_f) / chroma + 4.0
    } / 6.0;
    let saturation = if lightness > 0.5 {
        chroma / (2.0 - max_f - min_f)
    } else {
        chroma / (max_f + min_f)
    };
    let q = if new_lightness < 0.5 {
        new_lightness * (1.0 + saturation)
    } else {
        new_lightness + saturation - new_lightness * saturation
    };
    let p = 2.0 * new_lightness - q;

    [
        unit_to_u8(hue_to_rgb(p, q, hue + 1.0 / 3.0)),
        unit_to_u8(hue_to_rgb(p, q, hue)),
        unit_to_u8(hue_to_rgb(p, q, hue - 1.0 / 3.0)),
    ]
}

fn dark_mode_curve_lut() -> &'static [u8; 766] {
    static LUT: OnceLock<[u8; 766]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|sum| {
            let average = sum as f32 / (255.0 * 3.0);
            unit_to_u8(1.0 - average.powf(1.2))
        })
    })
}

fn lerp_channel(start: u8, end: u8, amount: u8) -> u8 {
    let amount = u32::from(amount);
    ((u32::from(start) * (255 - amount) + u32::from(end) * amount + 127) / 255) as u8
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

fn unit_to_u8(value: f32) -> u8 {
    (value * 255.0).clamp(0.0, 255.0) as u8
}

fn blend_channel(transformed: u8, original: u8, mask_ratio: f32) -> u8 {
    (f32::from(transformed) * (1.0 - mask_ratio) + f32::from(original) * mask_ratio)
        .round()
        .clamp(0.0, 255.0) as u8
}

impl From<WorkerCommand> for WorkerTask {
    fn from(command: WorkerCommand) -> Self {
        match command {
            WorkerCommand::Open { document_id, path } => Self::Open { document_id, path },
            WorkerCommand::Close(document_id) => Self::Close(document_id),
            WorkerCommand::ExtractText { document_id, page } => {
                Self::ExtractText { document_id, page }
            }
            WorkerCommand::Search {
                document_id,
                request_id,
                query,
            } => Self::StartSearch {
                document_id,
                request_id,
                query,
            },
            WorkerCommand::CancelSearch {
                document_id,
                request_id,
            } => Self::CancelSearch {
                document_id,
                request_id,
            },
        }
    }
}

fn empty_text_cache(pages: u32) -> Vec<Option<CachedPageText>> {
    (0..pages).map(|_| None).collect()
}

fn cached_page_text<'a>(
    document: &PdfDocument,
    page: u32,
    cache: &'a mut [Option<CachedPageText>],
) -> Option<&'a CachedPageText> {
    let index = usize::try_from(page).ok()?;
    let slot = cache.get_mut(index)?;
    if slot.is_none() {
        let page_index = i32::try_from(page).ok()?;
        let page = document.pages().get(page_index).ok()?;
        let text = page.text().ok()?;
        let raw = text.all();
        let (normalized, source_index_by_byte) =
            normalize_search_characters(text.chars().iter().filter_map(|character| {
                character
                    .unicode_char()
                    .map(|value| (character.index(), value))
            }));
        *slot = Some(CachedPageText {
            raw,
            normalized,
            source_index_by_byte,
        });
    }
    slot.as_ref()
}

fn normalize_search_text(value: &str) -> String {
    normalize_search_characters(value.chars().enumerate()).0
}

fn count_search_matches(haystack: &str, needle: &str) -> u32 {
    u32::try_from(haystack.match_indices(needle).count()).unwrap_or(u32::MAX)
}

fn normalize_search_characters(
    characters: impl IntoIterator<Item = (usize, char)>,
) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut source_index_by_byte = Vec::new();
    let mut pending_whitespace = None;
    for (source_index, character) in characters {
        if character.is_whitespace() {
            pending_whitespace.get_or_insert(source_index);
            continue;
        }
        if let Some(whitespace_source) = pending_whitespace.take()
            && !normalized.is_empty()
        {
            push_normalized_character(
                &mut normalized,
                &mut source_index_by_byte,
                whitespace_source,
                ' ',
            );
        }
        for character in character.to_lowercase() {
            push_normalized_character(
                &mut normalized,
                &mut source_index_by_byte,
                source_index,
                character,
            );
        }
    }
    (normalized, source_index_by_byte)
}

fn push_normalized_character(
    normalized: &mut String,
    source_index_by_byte: &mut Vec<usize>,
    source_index: usize,
    character: char,
) {
    normalized.push(character);
    source_index_by_byte.extend(std::iter::repeat_n(source_index, character.len_utf8()));
}

fn search_page(
    document: &PdfDocument,
    page: u32,
    cache: &mut [Option<CachedPageText>],
    needle: &str,
) -> (u32, Vec<SearchRect>) {
    let Some(cached) = cached_page_text(document, page, cache) else {
        return (0, Vec::new());
    };
    let source_ranges: Vec<_> = cached
        .normalized
        .match_indices(needle)
        .filter_map(|(start, _)| {
            let end = start.checked_add(needle.len())?;
            let first = *cached.source_index_by_byte.get(start)?;
            let last = *cached.source_index_by_byte.get(end.checked_sub(1)?)?;
            Some((first, last.saturating_sub(first).saturating_add(1)))
        })
        .collect();
    let occurrences = count_search_matches(&cached.normalized, needle);
    if source_ranges.is_empty() {
        return (0, Vec::new());
    }

    let Ok(page_index) = i32::try_from(page) else {
        return (occurrences, Vec::new());
    };
    let Ok(page) = document.pages().get(page_index) else {
        return (occurrences, Vec::new());
    };
    let Ok(text) = page.text() else {
        return (occurrences, Vec::new());
    };
    let mut rectangles = Vec::new();
    for (start, count) in source_ranges {
        let segments = text.segments_subset(start, count);
        rectangles.extend(segments.iter().map(|segment| {
            let bounds = segment.bounds();
            SearchRect {
                bottom: bounds.bottom().value,
                left: bounds.left().value,
                top: bounds.top().value,
                right: bounds.right().value,
            }
        }));
    }
    (occurrences, rectangles)
}

fn apply_search_highlights(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    rgba: &mut [u8],
    rectangles: &[SearchRect],
    color: [u8; 3],
) {
    for rectangle in rectangles {
        let corners = [
            (rectangle.left, rectangle.top),
            (rectangle.right, rectangle.top),
            (rectangle.left, rectangle.bottom),
            (rectangle.right, rectangle.bottom),
        ];
        let pixels: Vec<_> = corners
            .into_iter()
            .filter_map(|(x, y)| {
                page.points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), config)
                    .ok()
            })
            .collect();
        if pixels.len() != corners.len() {
            continue;
        }
        let Some(min_x) = pixels.iter().map(|(x, _)| *x).min() else {
            continue;
        };
        let Some(max_x) = pixels.iter().map(|(x, _)| *x).max() else {
            continue;
        };
        let Some(min_y) = pixels.iter().map(|(_, y)| *y).min() else {
            continue;
        };
        let Some(max_y) = pixels.iter().map(|(_, y)| *y).max() else {
            continue;
        };
        let left = min_x.saturating_sub(1).clamp(0, width as i32) as u32;
        let right = max_x.saturating_add(2).clamp(0, width as i32) as u32;
        let top = min_y.saturating_sub(1).clamp(0, height as i32) as u32;
        let bottom = max_y.saturating_add(2).clamp(0, height as i32) as u32;
        blend_highlight_rectangle(
            rgba,
            width,
            height,
            PixelRect {
                left,
                top,
                right,
                bottom,
            },
            color,
        );
    }
}

fn extract_page_links(
    document: &PdfDocument,
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
) -> Vec<PageLink> {
    let page_text = page.text().ok();
    page.links()
        .iter()
        .filter_map(|link| {
            let target = resolve_link_target(document, &link)?;
            let bounds = link.rect().ok()?;
            let rect = page_rect_to_pixels(page, config, width, height, bounds)?;
            let label = page_text
                .as_ref()
                .map(|text| link_text_label(&text.inside_rect(bounds)))
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| link_target_label(&target));
            Some(PageLink {
                rect: PageLinkRect {
                    left: rect.left,
                    top: rect.top,
                    right: rect.right,
                    bottom: rect.bottom,
                },
                label,
                target,
            })
        })
        .collect()
}

fn link_text_label(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let mut label: String = characters.by_ref().take(80).collect();
    if characters.next().is_some() {
        label.push('…');
    }
    label
}

fn link_target_label(target: &LinkTarget) -> String {
    match target {
        LinkTarget::Internal { page, .. } => format!("page {}", page + 1),
        LinkTarget::Uri(uri) => uri.clone(),
    }
}

fn resolve_link_target(document: &PdfDocument, link: &PdfLink<'_>) -> Option<LinkTarget> {
    if let Some(destination) = link.destination() {
        return resolve_internal_destination(document, destination);
    }
    match link.action()? {
        PdfAction::LocalDestination(action) => {
            resolve_internal_destination(document, action.destination().ok()?)
        }
        PdfAction::Uri(action) => action.uri().ok().map(LinkTarget::Uri),
        _ => None,
    }
}

fn resolve_internal_destination(
    document: &PdfDocument,
    destination: PdfDestination<'_>,
) -> Option<LinkTarget> {
    let page_index = destination.page_index().ok()?;
    let page = u32::try_from(page_index).ok()?;
    let target_y = match destination.view_settings().ok() {
        Some(PdfDestinationViewSettings::SpecificCoordinatesAndZoom(_, y, _)) => y,
        Some(PdfDestinationViewSettings::FitPageHorizontallyToWindow(y))
        | Some(PdfDestinationViewSettings::FitBoundsHorizontallyToWindow(y)) => y,
        Some(PdfDestinationViewSettings::FitPageToRectangle(rect)) => Some(rect.top()),
        _ => None,
    };
    let top_ratio = target_y.and_then(|y| {
        let target_page = document.pages().get(page_index).ok()?;
        let page_height = target_page.height().value;
        (page_height > 0.0).then(|| ((page_height - y.value) / page_height).clamp(0.0, 1.0))
    });
    Some(LinkTarget::Internal { page, top_ratio })
}

fn page_rect_to_pixels(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    bounds: PdfRect,
) -> Option<PixelRect> {
    let corners = [
        (bounds.left().value, bounds.top().value),
        (bounds.right().value, bounds.top().value),
        (bounds.left().value, bounds.bottom().value),
        (bounds.right().value, bounds.bottom().value),
    ];
    let pixels: Vec<_> = corners
        .into_iter()
        .filter_map(|(x, y)| {
            page.points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), config)
                .ok()
        })
        .collect();
    if pixels.len() != corners.len() {
        return None;
    }
    let min_x = pixels.iter().map(|(x, _)| *x).min()?;
    let max_x = pixels.iter().map(|(x, _)| *x).max()?;
    let min_y = pixels.iter().map(|(_, y)| *y).min()?;
    let max_y = pixels.iter().map(|(_, y)| *y).max()?;
    Some(PixelRect {
        left: min_x.saturating_sub(1).clamp(0, width as i32) as u32,
        right: max_x.saturating_add(2).clamp(0, width as i32) as u32,
        top: min_y.saturating_sub(1).clamp(0, height as i32) as u32,
        bottom: max_y.saturating_add(2).clamp(0, height as i32) as u32,
    })
}

fn apply_link_highlights(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    links: &[PageLink],
    color: [u8; 3],
) {
    for link in links {
        let rect = PixelRect {
            left: link.rect.left,
            top: link.rect.top,
            right: link.rect.right,
            bottom: link.rect.bottom,
        };
        blend_rectangle(rgba, width, height, rect, color, LINK_HIGHLIGHT_ALPHA);
        let border = 2;
        for edge in [
            PixelRect {
                bottom: rect.top.saturating_add(border),
                ..rect
            },
            PixelRect {
                top: rect.bottom.saturating_sub(border),
                ..rect
            },
            PixelRect {
                right: rect.left.saturating_add(border),
                ..rect
            },
            PixelRect {
                left: rect.right.saturating_sub(border),
                ..rect
            },
        ] {
            blend_rectangle(rgba, width, height, edge, color, LINK_BORDER_ALPHA);
        }
    }
}

fn blend_highlight_rectangle(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rectangle: PixelRect,
    color: [u8; 3],
) {
    blend_rectangle(
        rgba,
        width,
        height,
        rectangle,
        color,
        SEARCH_HIGHLIGHT_ALPHA,
    );
}

fn blend_rectangle(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rectangle: PixelRect,
    color: [u8; 3],
    alpha: u16,
) {
    let left = rectangle.left.min(width);
    let right = rectangle.right.min(width);
    let top = rectangle.top.min(height);
    let bottom = rectangle.bottom.min(height);
    if left >= right || top >= bottom {
        return;
    }
    let Ok(stride) = usize::try_from(width).map(|width| width.saturating_mul(4)) else {
        return;
    };
    for y in top..bottom {
        let Ok(row_start) = usize::try_from(y).map(|y| y.saturating_mul(stride)) else {
            continue;
        };
        for x in left..right {
            let Ok(offset) =
                usize::try_from(x).map(|x| row_start.saturating_add(x.saturating_mul(4)))
            else {
                continue;
            };
            let Some(pixel) = rgba.get_mut(offset..offset.saturating_add(4)) else {
                continue;
            };
            for (channel, highlight) in pixel[..3].iter_mut().zip(color) {
                let original = u16::from(*channel);
                *channel = ((original * (255 - alpha) + u16::from(highlight) * alpha) / 255) as u8;
            }
        }
    }
}

fn load_pdfium(library: Option<&Path>) -> Result<Pdfium, String> {
    let bindings = if let Some(path) = library {
        Pdfium::bind_to_library(path)
            .map_err(|error| format!("could not load Pdfium from {}: {error}", path.display()))?
    } else {
        let executable_library = std::env::current_exe().ok().and_then(|path| {
            path.parent()
                .map(Pdfium::pdfium_platform_library_name_at_path)
        });
        match executable_library
            .as_ref()
            .and_then(|path| Pdfium::bind_to_library(path).ok())
        {
            Some(bindings) => bindings,
            None => {
                let embedded = crate::embedded_pdfium::materialize().map_err(|error| {
                    format!("could not extract embedded PDFium to the user cache: {error}")
                })?;
                Pdfium::bind_to_library(&embedded).map_err(|embedded_error| {
                    format!(
                        "could not load embedded PDFium from {}: {embedded_error}",
                        embedded.display()
                    )
                })?
            }
        }
    };

    Ok(Pdfium::new(bindings))
}

#[cfg(test)]
mod tests {
    use super::{
        DarkModeStyle, DarkModeTransform, FitMode, PixelRect, blend_highlight_rectangle,
        count_search_matches, dark_mode_pixel, darken_rgba, mask_quadrilateral,
        normalize_search_text,
    };

    const NEUTRAL_DARK_MODE: DarkModeStyle = DarkModeStyle::new([30, 30, 30], [209, 209, 209]);

    #[test]
    fn pdfium_image_mask_and_text_cache_work() {
        use super::{
            LinkTarget, apply_link_highlights, apply_search_highlights, cached_page_text,
            empty_text_cache, extract_page_links, image_mask, load_pdfium, search_page,
        };
        use pdfium_render::prelude::{
            PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints, PdfRenderConfig,
        };

        let pdfium = load_pdfium(None).unwrap();
        let source = pdfium
            .load_pdf_from_byte_vec(synthetic_image_pdf(), None)
            .unwrap();
        let source_page = source.pages().get(0).unwrap();
        let mut document = pdfium.create_new_pdf().unwrap();
        let mut form = source_page
            .objects()
            .copy_into_x_object_form_object(&mut document)
            .unwrap();
        form.as_x_object_form_object_mut()
            .unwrap()
            .scale(0.5, 0.5)
            .unwrap();
        document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::from_points(
                source_page.width(),
                source_page.height(),
            ))
            .unwrap()
            .objects_mut()
            .add_object(form)
            .unwrap();
        let page = document.pages().get(0).unwrap();
        let config = PdfRenderConfig::new().set_target_width(400);
        let bitmap = page.render_with_config(&config).unwrap();
        let mask = image_mask(
            &page,
            &config,
            bitmap.width() as u32,
            bitmap.height() as u32,
        )
        .unwrap();
        assert_eq!(mask.iter().filter(|&&value| value == 255).count(), 10_000);

        let mut text_document = pdfium.create_new_pdf().expect("create document");
        let mut text_page = text_document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .expect("create page");
        let font = text_document.fonts_mut().courier();
        text_page
            .objects_mut()
            .create_text_object(
                PdfPoints::new(20.0),
                PdfPoints::new(20.0),
                "Synthetic Needle synthetic needle",
                font,
                PdfPoints::new(12.0),
            )
            .expect("create text object");
        drop(text_page);

        let mut cache = empty_text_cache(1);
        let text = cached_page_text(&text_document, 0, &mut cache).expect("cached page text");
        assert_eq!(
            count_search_matches(&text.normalized, "synthetic needle"),
            2
        );
        let (occurrences, rectangles) =
            search_page(&text_document, 0, &mut cache, "synthetic needle");
        assert_eq!(occurrences, 2);
        assert!(!rectangles.is_empty());

        let page = text_document.pages().get(0).expect("text page");
        let config = PdfRenderConfig::new()
            .set_reverse_byte_order(true)
            .set_target_width(400);
        let bitmap = page.render_with_config(&config).expect("render text page");
        let mut highlighted = bitmap.as_raw_bytes();
        let original = highlighted.clone();
        apply_search_highlights(
            &page,
            &config,
            bitmap.width() as u32,
            bitmap.height() as u32,
            &mut highlighted,
            &rectangles,
            [0xff, 0xc7, 0x77],
        );
        assert_ne!(highlighted, original);

        let link_document = pdfium
            .load_pdf_from_byte_vec(synthetic_link_pdf(), None)
            .expect("load synthetic linked PDF");
        let link_page = link_document.pages().get(0).expect("first page");
        let link_config = PdfRenderConfig::new()
            .set_reverse_byte_order(true)
            .set_target_width(400);
        let link_bitmap = link_page
            .render_with_config(&link_config)
            .expect("render linked page");
        let link_width = link_bitmap.width() as u32;
        let link_height = link_bitmap.height() as u32;
        let links = extract_page_links(
            &link_document,
            &link_page,
            &link_config,
            link_width,
            link_height,
        );

        assert_eq!(links.len(), 2);
        assert!(links.iter().any(|link| link.label == "page 2"));
        assert!(
            links
                .iter()
                .any(|link| link.label == "https://example.invalid/paper")
        );
        assert!(links.iter().any(|link| matches!(
            link.target,
            LinkTarget::Internal {
                page: 1,
                top_ratio: Some(ratio)
            } if (ratio - 0.25).abs() < f32::EPSILON
        )));
        assert!(links.iter().any(|link| {
            matches!(&link.target, LinkTarget::Uri(uri) if uri == "https://example.invalid/paper")
        }));

        let mut link_highlighted = link_bitmap.as_raw_bytes();
        let link_original = link_highlighted.clone();
        apply_link_highlights(
            &mut link_highlighted,
            link_width,
            link_height,
            &links,
            [0x86, 0xe1, 0xfc],
        );
        assert_ne!(link_highlighted, link_original);
    }

    fn synthetic_image_pdf() -> Vec<u8> {
        let page_stream = "q 200 0 0 200 100 100 cm /Im0 Do Q";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{page_stream}\nendstream",
                page_stream.len()
            ),
            "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length 3 >>\nstream\nRGB\nendstream".to_string(),
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn synthetic_link_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] /Annots [7 0 R 8 0 R] /Contents 5 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] /Contents 6 0 R >>".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
            "<< /Type /Annot /Subtype /Link /Rect [10 10 80 30] /Border [0 0 0] /Dest [4 0 R /XYZ null 300 null] >>".to_string(),
            "<< /Type /Annot /Subtype /Link /Rect [100 10 180 30] /Border [0 0 0] /A << /S /URI /URI (https://example.invalid/paper) >> >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn dark_mode_bounds_grayscale_and_keeps_alpha() {
        let mut pixels = [0, 0, 0, 128, 255, 255, 255, 64];
        darken_rgba(&mut pixels, None, NEUTRAL_DARK_MODE);
        assert_eq!(pixels, [209, 209, 209, 128, 30, 30, 30, 64]);
    }

    #[test]
    fn dark_mode_uses_theme_document_colors() {
        let style = DarkModeStyle::new([30, 32, 48], [200, 211, 245]);
        let mut pixels = [0, 0, 0, 255, 255, 255, 255, 255];
        darken_rgba(&mut pixels, None, style);
        assert_eq!(pixels, [200, 211, 245, 255, 30, 32, 48, 255]);
    }

    #[test]
    fn dark_mode_preserves_hue() {
        let [red, green, blue] =
            dark_mode_pixel([20, 100, 200], DarkModeTransform::new(NEUTRAL_DARK_MODE));
        assert!(blue > green);
        assert!(green > red);
    }

    #[test]
    fn dark_mode_mask_preserves_images() {
        let mut pixels = [255, 255, 255, 255, 20, 100, 200, 128];
        darken_rgba(&mut pixels, Some(&[0, 255]), NEUTRAL_DARK_MODE);
        assert_eq!(&pixels[..4], &[30, 30, 30, 255]);
        assert_eq!(&pixels[4..], &[20, 100, 200, 128]);
    }

    #[test]
    fn image_mask_tracks_rotated_bounds_with_soft_edges() {
        let mut mask = vec![0; 9 * 9];
        mask_quadrilateral(&mut mask, 9, 9, [(4, 1), (7, 4), (4, 7), (1, 4)]);

        assert_eq!(mask[4 * 9 + 4], 255);
        assert_eq!(mask[9 + 1], 0);
        assert!((1..255).contains(&mask[9 + 3]));
    }

    #[test]
    fn dark_mode_handles_full_hd_page() {
        let mut pixels = vec![255; 1920 * 1080 * 4];
        darken_rgba(&mut pixels, None, NEUTRAL_DARK_MODE);
        assert_eq!(&pixels[..4], &[30, 30, 30, 255]);
        assert_eq!(&pixels[pixels.len() - 4..], &[30, 30, 30, 255]);
    }

    #[test]
    fn fit_mode_cycles_page_width_height() {
        assert_eq!(FitMode::Page.cycle(), FitMode::Width);
        assert_eq!(FitMode::Width.cycle(), FitMode::Height);
        assert_eq!(FitMode::Height.cycle(), FitMode::Page);
    }

    #[test]
    fn search_normalizes_case_and_whitespace() {
        let text = normalize_search_text("  Alpha\n\tBETA  alpha beta ");

        assert_eq!(text, "alpha beta alpha beta");
        assert_eq!(count_search_matches(&text, "alpha beta"), 2);
    }

    #[test]
    fn link_labels_normalize_whitespace_and_truncate() {
        let label = super::link_text_label("  [12]\n nearby   citation  ");
        assert_eq!(label, "[12] nearby citation");

        let long = super::link_text_label(&"x".repeat(100));
        assert_eq!(long.chars().count(), 81);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn search_highlight_blends_rgb_and_preserves_alpha() {
        let mut pixels = [0, 0, 0, 123, 255, 255, 255, 45];

        blend_highlight_rectangle(
            &mut pixels,
            2,
            1,
            PixelRect {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            },
            [255, 199, 119],
        );

        assert_eq!(pixels, [88, 68, 41, 123, 255, 255, 255, 45]);
    }
}

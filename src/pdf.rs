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
    PdfMatrix, PdfPage, PdfPageObject, PdfPageObjectCommon, PdfPageObjectsCommon,
    PdfPageTextRenderMode, PdfPoints, PdfRect, PdfRenderConfig, Pdfium, PdfiumError,
};

use crate::synctex::{DocumentRevision, ForwardWord, PdfRevision, words};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

const LOW_CHROMA_THRESHOLD: u8 = 10;
const MAX_DARK_MODE_WORKERS: usize = 8;
const MAX_FORM_DEPTH: u8 = 32;
const IMAGE_MASK_SAMPLES: usize = 4;
const PARALLEL_DARK_MODE_PIXELS: usize = 250_000;
const SEARCH_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const LINK_INDEX_PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const SEARCH_HIGHLIGHT_ALPHA: u16 = 88;
const LINK_HIGHLIGHT_ALPHA: u16 = 40;
const LINK_BORDER_ALPHA: u16 = 210;
const SELECTED_LINK_HIGHLIGHT_ALPHA: u16 = 72;
const SELECTED_LINK_BORDER_ALPHA: u16 = 255;
const DARK_LINK_BORDER_ALPHA: u16 = 96;
const DARK_LINK_BLUE_DOMINANCE: u8 = 28;
const MINIMUM_DARK_LINK_CONTRAST: f32 = 4.5;

pub type DocumentId = u64;

/// One entry in a document's outline (table of contents).
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub page: u32,
    pub depth: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPageMatch {
    pub page: u32,
    pub occurrences: u32,
    pub context: String,
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

// Share geometry between rendering and inverse search. Do not use PDFium's
// scale_page_to_display_size(): it rotates landscape pages 90 degrees.
fn build_fit_config(
    base_config: PdfRenderConfig,
    fit: FitMode,
    target_width: i32,
    target_height: i32,
) -> PdfRenderConfig {
    match fit {
        FitMode::Page => base_config
            .set_target_width(target_width)
            .set_maximum_width(target_width)
            .set_maximum_height(target_height),
        FitMode::Width => base_config.set_target_width(target_width),
        FitMode::Height => base_config.set_target_height(target_height),
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
    /// Zoom level as a percentage of the fitted size (100 = fit exactly).
    pub zoom: u16,
    pub fit: FitMode,
    pub invert: bool,
    pub dark_mode_style: DarkModeStyle,
    pub search_request_id: u64,
    pub search_highlight: [u8; 3],
    pub link_mode: bool,
    pub link_highlight: [u8; 3],
    pub selected_link_ordinal: Option<u32>,
}

#[derive(Debug)]
pub struct RenderRequest {
    pub key: RenderKey,
    pub generation: u64,
}

#[derive(Debug)]
pub struct Frame {
    pub key: RenderKey,
    pub revision: DocumentRevision,
    pub width: u32,
    pub height: u32,
    pub page_width_pt: f32,
    pub page_height_pt: f32,
    pub compressed_rgba: Vec<u8>,
    pub render_elapsed: Duration,
    pub dark_mode_elapsed: Option<Duration>,
    pub highlight_elapsed: Option<Duration>,
    pub compression_elapsed: Duration,
    pub generation: u64,
    pub links: Vec<PageLink>,
    pub flash: Option<ForwardHighlight>,
}

#[derive(Clone, Debug)]
pub struct ForwardHighlight {
    pub rect: SearchRect,
    /// Rendered frame bounds after PDFium's crop and rotation transforms,
    /// ordered left, right, top, bottom.
    pub pixel_bounds: Option<(i32, i32, i32, i32)>,
    pub word_precise: bool,
    /// Refinement errors fail this forward request, not the renderer.
    pub error: Option<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Internal {
        page: u32,
        top_ratio: Option<f32>,
        left_ratio: Option<f32>,
    },
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

#[derive(Clone, Debug, PartialEq)]
pub struct DocumentLink {
    pub source_page: u32,
    pub source_top_ratio: f32,
    pub ordinal: u32,
    pub label: String,
    pub source_context: Option<String>,
    pub reference_context: Option<String>,
    pub target: LinkTarget,
}

#[derive(Debug)]
pub struct ResolvedClick {
    /// Original compiler page coordinates, independent of the displayed crop/rotation.
    pub synctex: crate::synctex::InversePoint,
    /// Preserve the existing compiler-service wire convention for Typst.
    pub typst: crate::synctex::InversePoint,
    pub text: Result<Option<(String, usize)>, String>,
}

/// SyncTeX uses the compiler's original paper height and an absolute x coordinate,
/// not the rendered page's height or CropBox origin. A nonzero MediaBox origin
/// does not move pdfTeX's output origin: its paper height is the unrotated extent.
/// PDFium page/device transforms handle the displayed crop and rotation separately.
#[derive(Clone, Copy)]
struct SourcePageCoordinates {
    height: f32,
}

impl SourcePageCoordinates {
    fn for_page(page: &PdfPage) -> Result<Self, String> {
        let media = page
            .boundaries()
            .media()
            .map_err(|error| format!("could not read original PDF paper bounds: {error}"))?
            .bounds;
        if ![
            media.left().value,
            media.bottom().value,
            media.right().value,
            media.top().value,
            media.width().value,
            media.height().value,
        ]
        .into_iter()
        .all(f32::is_finite)
            || media.width().value <= 0.0
            || media.height().value <= 0.0
        {
            return Err("PDFium returned invalid original paper bounds".into());
        }
        Ok(Self {
            height: media.height().value,
        })
    }

    // This involution is shared by forward boxes, inverse clicks, and batch input.
    fn flip_y(self, y: f32) -> Result<f32, String> {
        let flipped = self.height - y;
        if !y.is_finite() || !flipped.is_finite() {
            return Err("original page coordinate is not finite".into());
        }
        Ok(flipped)
    }

    fn pdf_point(self, x: f32, y_from_top: f32) -> Result<(f32, f32), String> {
        if !x.is_finite() {
            return Err("original page coordinate is not finite".into());
        }
        Ok((x, self.flip_y(y_from_top)?))
    }

    fn inverse_point(
        self,
        page: u32,
        pdf_x: f32,
        pdf_y: f32,
    ) -> Result<crate::synctex::InversePoint, String> {
        let (x, y_from_top) = self.pdf_point(pdf_x, pdf_y)?;
        Ok(crate::synctex::InversePoint {
            page,
            x,
            y_from_top,
            page_height_pt: self.height,
        })
    }

    fn pdf_rect(self, rect: SearchRect) -> Result<SearchRect, String> {
        let (left, top) = self.pdf_point(rect.left, rect.top)?;
        let (right, bottom) = self.pdf_point(rect.right, rect.bottom)?;
        Ok(SearchRect {
            left,
            right,
            top,
            bottom,
        })
    }
}

#[derive(Debug)]
pub enum WorkerMessage {
    Ready {
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: PdfRevision,
    },
    Opened {
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: PdfRevision,
    },
    PagePoint {
        document_id: DocumentId,
        page: u32,
        request_id: u64,
        revision: DocumentRevision,
        result: Result<ResolvedClick, String>,
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
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    },
    SearchResults {
        document_id: DocumentId,
        request_id: u64,
        matches: Vec<SearchPageMatch>,
        total_occurrences: u32,
    },
    LinkIndexProgress {
        document_id: DocumentId,
        request_id: u64,
        links: Vec<DocumentLink>,
        scanned: u32,
        total: u32,
        complete: bool,
    },
    VisibleMatches {
        document_id: DocumentId,
        request_id: u64,
        revision: DocumentRevision,
        matches: Vec<VisibleMatch>,
    },
    VisibleMatchesError {
        document_id: DocumentId,
        request_id: u64,
        revision: DocumentRevision,
        error: String,
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
    VisibleMatches {
        document_id: DocumentId,
        request_id: u64,
        revision: DocumentRevision,
        query: String,
        keys: Vec<(RenderKey, u32, u32)>,
        generation: u64,
    },
    CancelSearch {
        document_id: DocumentId,
        request_id: u64,
    },
    PagePoint {
        document_id: DocumentId,
        page: u32,
        request_id: u64,
        x: u32,
        y: u32,
        key: RenderKey,
        revision: DocumentRevision,
    },
    Flash {
        document_id: DocumentId,
        page: u32,
        rect: SearchRect,
        word: Option<ForwardWord>,
    },
    ClearFlash {
        document_id: DocumentId,
    },
    IndexLinks {
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
    PagePoint {
        document_id: DocumentId,
        page: u32,
        request_id: u64,
        x: u32,
        y: u32,
        key: RenderKey,
        revision: DocumentRevision,
    },
    StartSearch {
        document_id: DocumentId,
        request_id: u64,
        query: String,
    },
    VisibleMatches {
        document_id: DocumentId,
        request_id: u64,
        revision: DocumentRevision,
        query: String,
        keys: Vec<(RenderKey, u32, u32)>,
        generation: u64,
    },
    CancelSearch {
        document_id: DocumentId,
        request_id: u64,
    },
    StartLinkIndex {
        document_id: DocumentId,
        request_id: u64,
    },
    Flash {
        document_id: DocumentId,
        page: u32,
        rect: SearchRect,
        word: Option<ForwardWord>,
    },
    ClearFlash {
        document_id: DocumentId,
    },
    IndexLinkPage(LinkIndexJob),
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

struct LinkIndexJob {
    document_id: DocumentId,
    request_id: u64,
    next_page: u32,
    total_pages: u32,
    pending_links: Vec<DocumentLink>,
    last_progress: Instant,
}

struct SearchHighlights {
    request_id: u64,
    pages: HashMap<u32, Vec<SearchRect>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchRect {
    pub bottom: f32,
    pub left: f32,
    pub top: f32,
    pub right: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleMatch {
    pub key: RenderKey,
    pub char_index: usize,
    pub rects: Vec<PixelRect>,
    pub hit: (u32, u32),
    pub anchor: (u32, u32),
    pub next_char: Option<char>,
}
#[derive(Clone, Debug)]
struct CachedPageText {
    raw: String,
    normalized: String,
    source_index_by_byte: Vec<usize>,
    visible: Option<(String, Vec<(usize, usize)>)>,
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
    visible_generation: Arc<AtomicU64>,
}

impl RenderWorker {
    pub fn spawn(document_id: DocumentId, path: PathBuf, pdfium_library: Option<PathBuf>) -> Self {
        let (priority_tx, priority_rx) = unbounded();
        let (prefetch_tx, prefetch_rx) = unbounded();
        let (command_tx, command_rx) = unbounded();
        let (message_tx, message_rx) = unbounded();
        let latest_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&latest_generation);
        let visible_generation = Arc::new(AtomicU64::new(0));
        let worker_visible_generation = Arc::clone(&visible_generation);

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
                worker_visible_generation,
            );
        });

        Self {
            priority_tx,
            prefetch_tx,
            command_tx,
            message_rx,
            latest_generation,
            visible_generation,
        }
    }

    pub fn wait_until_ready(&self) -> Result<(u32, Vec<OutlineItem>, PdfRevision), String> {
        match self.message_rx.recv() {
            Ok(WorkerMessage::Ready {
                pages,
                outline,
                revision,
            }) => Ok((pages, outline, revision)),
            Ok(WorkerMessage::Error(error)) => Err(error),
            Ok(
                WorkerMessage::Opened { .. }
                | WorkerMessage::OpenError { .. }
                | WorkerMessage::LinkIndexProgress { .. }
                | WorkerMessage::Text { .. }
                | WorkerMessage::SearchProgress { .. }
                | WorkerMessage::SearchResults { .. }
                | WorkerMessage::VisibleMatchesError { .. }
                | WorkerMessage::VisibleMatches { .. }
                | WorkerMessage::PagePoint { .. }
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
        self.visible_generation.fetch_add(1, Ordering::AcqRel);
        self.command_tx
            .send(WorkerCommand::Open { document_id, path })
            .map_err(|_| "renderer stopped".into())
    }

    pub fn close(&self, document_id: DocumentId) {
        self.visible_generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.command_tx.send(WorkerCommand::Close(document_id));
    }

    pub fn extract_text(&self, document_id: DocumentId, page: u32) {
        let _ = self
            .command_tx
            .send(WorkerCommand::ExtractText { document_id, page });
    }

    pub fn page_point(
        &self,
        revision: DocumentRevision,
        request_id: u64,
        x: u32,
        y: u32,
        key: RenderKey,
    ) {
        let _ = self.command_tx.send(WorkerCommand::PagePoint {
            document_id: key.document_id,
            page: key.page,
            revision,
            request_id,
            x,
            y,
            key,
        });
    }

    pub fn flash(
        &self,
        document_id: DocumentId,
        page: u32,
        rect: SearchRect,
        word: Option<ForwardWord>,
    ) {
        let _ = self.command_tx.send(WorkerCommand::Flash {
            document_id,
            page,
            rect,
            word,
        });
    }

    pub fn clear_flash(&self, document_id: DocumentId) {
        let _ = self
            .command_tx
            .send(WorkerCommand::ClearFlash { document_id });
    }

    pub fn search(&self, document_id: DocumentId, request_id: u64, query: String) {
        let _ = self.command_tx.send(WorkerCommand::Search {
            document_id,
            request_id,
            query,
        });
    }
    pub fn find_visible(
        &self,
        document_id: DocumentId,
        request_id: u64,
        revision: DocumentRevision,
        query: String,
        keys: Vec<(RenderKey, u32, u32)>,
    ) -> Result<(), String> {
        let generation = self
            .visible_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.command_tx
            .send(WorkerCommand::VisibleMatches {
                document_id,
                request_id,
                revision,
                query,
                keys,
                generation,
            })
            .map_err(|_| "renderer stopped".into())
    }
    pub fn cancel_visible(&self) {
        self.visible_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn cancel_search(&self, document_id: DocumentId, request_id: u64) {
        let _ = self.command_tx.send(WorkerCommand::CancelSearch {
            document_id,
            request_id,
        });
    }

    pub fn index_links(&self, document_id: DocumentId, request_id: u64) {
        let _ = self.command_tx.send(WorkerCommand::IndexLinks {
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
    visible_generation: Arc<AtomicU64>,
) {
    let WorkerChannels {
        priority_rx,
        prefetch_rx,
        command_rx,
        message_tx,
    } = channels;
    let result = (|| -> Result<(), String> {
        let pdfium = load_pdfium(pdfium_library)?;
        let revision = DocumentRevision::read(&path).map_err(|error| error.to_string())?;
        let document = pdfium
            .load_pdf_from_file(&path, None)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let pages = u32::try_from(document.pages().len())
            .map_err(|_| "PDFium returned a negative page count".to_string())?;
        if pages == 0 {
            return Err(format!("{} has no pages", path.display()));
        }
        let outline = extract_outline(&document);
        revision.check(&path).map_err(|error| error.to_string())?;
        message_tx
            .send(WorkerMessage::Ready {
                pages,
                outline,
                revision: revision.pdf,
            })
            .map_err(|_| "viewer stopped".to_string())?;
        let mut documents = HashMap::from([(initial_document_id, document)]);
        let mut revisions = HashMap::from([(initial_document_id, revision)]);
        let mut text_cache: HashMap<DocumentId, Vec<Option<CachedPageText>>> =
            HashMap::from([(initial_document_id, empty_text_cache(pages))]);
        let mut search_jobs = VecDeque::new();
        let mut link_index_jobs = VecDeque::new();
        let mut search_highlights: HashMap<DocumentId, SearchHighlights> = HashMap::new();
        let mut flash_highlights: HashMap<DocumentId, (u32, ForwardHighlight)> = HashMap::new();

        loop {
            let task = match command_rx.try_recv() {
                Ok(command) => command.into(),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => {
                    match priority_rx.try_recv() {
                        Ok(request) => WorkerTask::Render(request),
                        Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {
                            if let Some(job) = link_index_jobs.pop_front() {
                                WorkerTask::IndexLinkPage(job)
                            } else if let Some(job) = search_jobs.pop_front() {
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
                    let opened = (|| {
                        let revision =
                            DocumentRevision::read(&new_path).map_err(|error| error.to_string())?;
                        let document = pdfium
                            .load_pdf_from_file(&new_path, None)
                            .map_err(|error| error.to_string())?;
                        revision
                            .check(&new_path)
                            .map_err(|error| error.to_string())?;
                        Ok::<_, String>((document, revision))
                    })();
                    match opened {
                        Ok((replacement, revision)) => {
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
                                revisions.insert(document_id, revision);
                                text_cache.insert(document_id, empty_text_cache(pages));
                                search_jobs.retain(|job| job.document_id != document_id);
                                link_index_jobs.retain(|job| job.document_id != document_id);
                                search_highlights.remove(&document_id);
                                flash_highlights.remove(&document_id);
                                message_tx
                                    .send(WorkerMessage::Opened {
                                        document_id,
                                        pages,
                                        outline,
                                        revision: revision.pdf,
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
                    revisions.remove(&document_id);
                    text_cache.remove(&document_id);
                    search_jobs.retain(|job| job.document_id != document_id);
                    link_index_jobs.retain(|job| job.document_id != document_id);
                    search_highlights.remove(&document_id);
                    flash_highlights.remove(&document_id);
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
                WorkerTask::VisibleMatches {
                    document_id,
                    request_id,
                    revision,
                    query,
                    keys,
                    generation,
                } => {
                    if generation != visible_generation.load(Ordering::Acquire) {
                        continue;
                    }
                    if revisions.get(&document_id) != Some(&revision) {
                        if !documents.contains_key(&document_id) {
                            message_tx
                                .send(WorkerMessage::VisibleMatchesError {
                                    document_id,
                                    request_id,
                                    revision,
                                    error: "visible-match document is closed".into(),
                                })
                                .map_err(|_| "viewer stopped".to_string())?;
                        }
                        continue;
                    }
                    let result = (|| -> Result<Option<Vec<VisibleMatch>>, String> {
                        let document = documents
                            .get(&document_id)
                            .ok_or("visible-match document is closed")?;
                        let cache = text_cache
                            .get_mut(&document_id)
                            .ok_or("visible-match document text cache is unavailable")?;
                        find_visible_matches(
                            document,
                            document_id,
                            &query,
                            &keys,
                            cache,
                            generation,
                            &visible_generation,
                        )
                    })();
                    if generation != visible_generation.load(Ordering::Acquire) {
                        continue;
                    }
                    match result {
                        Ok(Some(matches)) => message_tx
                            .send(WorkerMessage::VisibleMatches {
                                document_id,
                                request_id,
                                revision,
                                matches,
                            })
                            .map_err(|_| "viewer stopped".to_string())?,
                        Ok(None) => {}
                        Err(error) => message_tx
                            .send(WorkerMessage::VisibleMatchesError {
                                document_id,
                                request_id,
                                revision,
                                error,
                            })
                            .map_err(|_| "viewer stopped".to_string())?,
                    }
                    continue;
                }
                WorkerTask::PagePoint {
                    document_id,
                    page,
                    request_id,
                    x,
                    y,
                    key,
                    revision,
                } => {
                    let result =
                        (|| -> Result<ResolvedClick, String> {
                            if revisions.get(&document_id) != Some(&revision) {
                                return Err("hit-test document revision is no longer loaded".into());
                            }
                            let document = documents
                                .get(&document_id)
                                .ok_or("hit-test document is closed")?;
                            let page_index = i32::try_from(page).map_err(|e| e.to_string())?;
                            let rendered = document
                                .pages()
                                .get(page_index)
                                .map_err(|e| e.to_string())?;
                            let zoom = i32::from(key.zoom.max(1));
                            let target_width = (i32::from(key.width) * zoom / 100).max(1);
                            let target_height = (i32::from(key.height) * zoom / 100).max(1);
                            let base_config = PdfRenderConfig::new()
                                .set_reverse_byte_order(true)
                                .use_lcd_text_rendering(true)
                                .force_half_tone(false)
                                .use_print_quality(false);
                            let config =
                                build_fit_config(base_config, key.fit, target_width, target_height);
                            let (pdf_x, pdf_y) = rendered
                                .pixels_to_points(x as i32, y as i32, &config)
                                .map_err(|e| e.to_string())?;
                            Ok(ResolvedClick {
                                synctex: SourcePageCoordinates::for_page(&rendered)?
                                    .inverse_point(page + 1, pdf_x.value, pdf_y.value)?,
                                typst: SourcePageCoordinates {
                                    height: rendered.height().value,
                                }
                                .inverse_point(
                                    page + 1,
                                    pdf_x.value,
                                    pdf_y.value,
                                )?,
                                text: clicked_text(&rendered, pdf_x.value, pdf_y.value),
                            })
                        })();
                    message_tx
                        .send(WorkerMessage::PagePoint {
                            document_id,
                            page,
                            request_id,
                            revision,
                            result,
                        })
                        .map_err(|_| "viewer stopped".to_string())?;
                    continue;
                }
                WorkerTask::Flash {
                    document_id,
                    page,
                    rect,
                    word,
                } => {
                    // Forward-search flash state: the app re-issues the render
                    // (and the clear) around this store, so nothing else to do.
                    // Translate original source coordinates before PDF-space text
                    // refinement; rendering applies the crop/rotation afterwards.
                    let Some(document) = documents.get(&document_id) else {
                        continue;
                    };
                    let target = document
                        .pages()
                        .get(page as i32)
                        .map_err(|e| e.to_string())?;
                    let translated = SourcePageCoordinates::for_page(&target)
                        .and_then(|coordinates| coordinates.pdf_rect(rect));
                    let result = (|| -> Result<(SearchRect, bool), String> {
                        let coarse = translated?;
                        let Some(word) = word else {
                            return Ok((coarse, false));
                        };
                        let cache = text_cache
                            .get_mut(&document_id)
                            .ok_or("missing PDF text cache")?;
                        let cached = cached_page_text(document, page, cache)
                            .ok_or("could not extract PDF text for word highlighting")?;
                        let refined = forward_word_rect(&target, cached, &word, coarse)?;
                        Ok((refined.unwrap_or(coarse), refined.is_some()))
                    })();
                    let (rect, word_precise, error) = match result {
                        Ok((rect, precise)) => (rect, precise, None),
                        Err(error) => (rect, false, Some(Arc::<str>::from(error))),
                    };
                    flash_highlights.insert(
                        document_id,
                        (
                            page,
                            ForwardHighlight {
                                rect,
                                pixel_bounds: None,
                                word_precise,
                                error,
                            },
                        ),
                    );
                    continue;
                }
                WorkerTask::ClearFlash { document_id } => {
                    flash_highlights.remove(&document_id);
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
                                matches: Vec::new(),
                                total_occurrences: 0,
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
                WorkerTask::StartLinkIndex {
                    document_id,
                    request_id,
                } => {
                    link_index_jobs.retain(|job| job.document_id != document_id);
                    let Some(document) = documents.get(&document_id) else {
                        continue;
                    };
                    let total_pages = u32::try_from(document.pages().len()).unwrap_or(u32::MAX);
                    message_tx
                        .send(WorkerMessage::LinkIndexProgress {
                            document_id,
                            request_id,
                            links: Vec::new(),
                            scanned: 0,
                            total: total_pages,
                            complete: total_pages == 0,
                        })
                        .map_err(|_| "viewer stopped".to_string())?;
                    if total_pages > 0 {
                        link_index_jobs.push_front(LinkIndexJob {
                            document_id,
                            request_id,
                            next_page: 0,
                            total_pages,
                            pending_links: Vec::new(),
                            last_progress: Instant::now(),
                        });
                    }
                    continue;
                }
                WorkerTask::IndexLinkPage(mut job) => {
                    let Some(document) = documents.get(&job.document_id) else {
                        continue;
                    };
                    let Some(cache) = text_cache.get_mut(&job.document_id) else {
                        continue;
                    };
                    if let Ok(page_index) = i32::try_from(job.next_page)
                        && let Ok(page) = document.pages().get(page_index)
                    {
                        job.pending_links.extend(extract_document_links(
                            document,
                            &page,
                            job.next_page,
                            cache,
                        ));
                    }
                    job.next_page = job.next_page.saturating_add(1);
                    let complete = job.next_page >= job.total_pages;
                    if complete || job.last_progress.elapsed() >= LINK_INDEX_PROGRESS_INTERVAL {
                        message_tx
                            .send(WorkerMessage::LinkIndexProgress {
                                document_id: job.document_id,
                                request_id: job.request_id,
                                links: std::mem::take(&mut job.pending_links),
                                scanned: job.next_page,
                                total: job.total_pages,
                                complete,
                            })
                            .map_err(|_| "viewer stopped".to_string())?;
                        job.last_progress = Instant::now();
                    }
                    if !complete {
                        link_index_jobs.push_back(job);
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
                    let (occurrences, rectangles, context) =
                        search_page(document, job.next_page, cache, &job.needle);
                    if occurrences > 0 {
                        job.matches.push(SearchPageMatch {
                            page: job.next_page,
                            occurrences,
                            context,
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
                                    matches: job.matches.clone(),
                                    total_occurrences: job.total_occurrences,
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
            let zoom = i32::from(request.key.zoom.max(1));
            let target_width = i32::from(request.key.width) * zoom / 100;
            let target_height = i32::from(request.key.height) * zoom / 100;
            let target_width = target_width.max(1);
            let target_height = target_height.max(1);
            let base_config = PdfRenderConfig::new()
                .set_reverse_byte_order(true)
                .use_lcd_text_rendering(true)
                .force_half_tone(false)
                .use_print_quality(false);
            let config =
                build_fit_config(base_config, request.key.fit, target_width, target_height);
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
            let links = extract_page_links(document, &page, &config, width, height);
            let selected_link_rectangles =
                selected_page_link_rectangles(&links, request.key.selected_link_ordinal);
            let dark_mode_link_rectangles = if request.key.invert {
                if request.key.link_mode {
                    links.iter().map(|link| link.rect).collect()
                } else {
                    extract_page_link_rectangles(&page, &config, width, height)
                }
            } else {
                Vec::new()
            };
            let flash = flash_highlights
                .get(&request.key.document_id)
                .filter(|(page, _)| *page == request.key.page)
                .map(|(_, highlight)| {
                    let mut highlight = highlight.clone();
                    if highlight.error.is_none() {
                        highlight.pixel_bounds =
                            page_rect_pixel_bounds(&page, &config, highlight.rect);
                        if highlight.pixel_bounds.is_none() {
                            highlight.error =
                                Some("could not convert forward highlight to frame pixels".into());
                        }
                    }
                    highlight
                });
            let flash_rectangle = flash
                .as_ref()
                .filter(|highlight| highlight.error.is_none())
                .map(|highlight| &highlight.rect);
            let highlight_elapsed = (search_rectangles.is_some()
                || (request.key.link_mode && !links.is_empty())
                || !selected_link_rectangles.is_empty()
                || !dark_mode_link_rectangles.is_empty()
                || flash_rectangle.is_some())
            .then(|| {
                let started = Instant::now();
                if !dark_mode_link_rectangles.is_empty() {
                    apply_dark_mode_link_contrast(
                        &mut raw_rgba,
                        width,
                        height,
                        &dark_mode_link_rectangles,
                        request.key.dark_mode_style,
                        request.key.link_highlight,
                    );
                }
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
                if request.key.link_mode && !links.is_empty() {
                    apply_link_highlights(
                        &mut raw_rgba,
                        width,
                        height,
                        &links,
                        request.key.link_highlight,
                        request.key.invert,
                    );
                }
                if !selected_link_rectangles.is_empty() {
                    apply_selected_link_highlights(
                        &mut raw_rgba,
                        width,
                        height,
                        &selected_link_rectangles,
                        request.key.link_highlight,
                    );
                }
                if let Some(rect) = flash_rectangle {
                    apply_search_highlights(
                        &page,
                        &config,
                        width,
                        height,
                        &mut raw_rgba,
                        std::slice::from_ref(rect),
                        [255, 0, 0],
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
                    revision: revisions[&request.key.document_id],
                    width,
                    height,
                    page_width_pt: page.width().value,
                    page_height_pt: page.height().value,
                    compressed_rgba,
                    render_elapsed,
                    dark_mode_elapsed,
                    highlight_elapsed,
                    compression_elapsed,
                    generation: request.generation,
                    links,
                    flash,
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
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let mask_value = mask.map_or(0, |mask| mask[index]);
        if mask_value == 255 {
            continue;
        }

        let [red, green, blue, _alpha] = pixel;
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
            WorkerCommand::VisibleMatches {
                document_id,
                request_id,
                revision,
                query,
                keys,
                generation,
            } => Self::VisibleMatches {
                document_id,
                request_id,
                revision,
                query,
                keys,
                generation,
            },
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
            WorkerCommand::PagePoint {
                document_id,
                page,
                request_id,
                x,
                y,
                key,
                revision,
            } => Self::PagePoint {
                document_id,
                page,
                request_id,
                x,
                y,
                key,
                revision,
            },
            WorkerCommand::IndexLinks {
                document_id,
                request_id,
            } => Self::StartLinkIndex {
                document_id,
                request_id,
            },
            WorkerCommand::Flash {
                document_id,
                page,
                rect,
                word,
            } => Self::Flash {
                document_id,
                page,
                rect,
                word,
            },
            WorkerCommand::ClearFlash { document_id } => Self::ClearFlash { document_id },
        }
    }
}

/// Keep a small text neighborhood around the clicked glyph, not the entire page.
fn clicked_text(page: &PdfPage, x: f32, y: f32) -> Result<Option<(String, usize)>, String> {
    let text = page.text().map_err(|error| error.to_string())?;
    let chars = text.chars();
    let x = PdfPoints::new(x);
    let y = PdfPoints::new(y);
    let Some(mut character) =
        chars.get_char_near_point(x, PdfPoints::new(6.0), y, PdfPoints::new(6.0))
    else {
        return Ok(None);
    };
    // PDFium can choose a large mathematical glyph over a smaller word glyph
    // when their bounds overlap. Prefer the smallest alphabetic glyph actually
    // under the pointer, while retaining PDFium's nearest-hit fallback.
    if !char::from_u32(character.unicode_value()).is_some_and(char::is_alphanumeric)
        && let Ok(bounds) = character.tight_bounds()
        && bounds.contains(x, y)
    {
        let mut area = bounds.width().value * bounds.height().value;
        for candidate in chars.iter() {
            if !char::from_u32(candidate.unicode_value()).is_some_and(char::is_alphabetic) {
                continue;
            }
            if let Ok(bounds) = candidate.tight_bounds()
                && bounds.contains(x, y)
            {
                let candidate_area = bounds.width().value * bounds.height().value;
                if candidate_area < area {
                    character = candidate;
                    area = candidate_area;
                }
            }
        }
    }
    unicode_context(chars.len(), character.index(), |index| {
        chars
            .get(index)
            .map(|character| character.unicode_value())
            .map_err(|error| error.to_string())
    })
    .map(Some)
}

/// Resolve newline-delimited clicks in original, unrotated SyncTeX page points
/// (absolute x and y down from the compiler paper top) without starting the viewer.
/// PDFium and the document stay open; each point owns its resolver deadline.
pub fn synctex_edit_batch(
    pdf: &Path,
    library: Option<&Path>,
    settings: &crate::config::ViewerSettings,
    input: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Point {
        page: u32,
        x: f32,
        y: f32,
    }

    let pdfium = load_pdfium(library)?;
    let document = pdfium
        .load_pdf_from_file(pdf, None)
        .map_err(|error| format!("could not open {}: {error}", pdf.display()))?;
    let mut cached_page: Option<(u32, PdfPage<'_>)> = None;
    for (line_number, line) in input.lines().enumerate() {
        let input_line = line.as_ref().ok().cloned();
        let result = (|| -> Result<serde_json::Value, String> {
            let line = line.map_err(|error| error.to_string())?;
            let point: Point = serde_json::from_str(&line).map_err(|error| error.to_string())?;
            if point.page == 0 || !point.x.is_finite() || !point.y.is_finite() {
                return Err("page must be positive and coordinates finite".into());
            }
            if cached_page.as_ref().map(|(page, _)| *page) != Some(point.page) {
                let page_index =
                    i32::try_from(point.page - 1).map_err(|error| error.to_string())?;
                let page = document
                    .pages()
                    .get(page_index)
                    .map_err(|error| format!("could not load page {}: {error}", point.page))?;
                cached_page = Some((point.page, page));
            }
            let page = &cached_page.as_ref().expect("current page is cached").1;
            let coordinates = SourcePageCoordinates::for_page(page)?;
            let (pdf_x, pdf_y) = coordinates.pdf_point(point.x, point.y)?;
            let (left, bottom, right, top) =
                effective_page_bounds(page).ok_or("could not determine visible PDF page bounds")?;
            if pdf_x < left || pdf_x > right || pdf_y < bottom || pdf_y > top {
                return Err("point is outside the visible PDF page".into());
            }
            let (context, offset) = match clicked_text(page, pdf_x, pdf_y) {
                Ok(Some(context)) => context,
                Ok(None) => return Err("PDFium found no text near point".into()),
                Err(error) => return Err(format!("PDFium text hit-test failed: {error}")),
            };
            let resolution = crate::synctex::resolve_inverse(
                pdf,
                coordinates.inverse_point(point.page, pdf_x, pdf_y)?,
                settings.word_precision.then_some((&context, offset)),
                settings.source_context_lines as u32,
                &crate::process::Operation::new(std::time::Duration::from_secs(30)),
            )
            .map_err(|error| error.to_string())?;
            let pdf_word = words(&context).into_iter().find_map(|(start, word)| {
                (start <= offset && offset < start + word.len()).then(|| word.to_owned())
            });
            Ok(serde_json::json!({
                "ok": true,
                "page": point.page,
                "x": point.x,
                "y": point.y,
                "location": resolution.location,
                "warning": resolution.warning,
                "error": null,
                "pdf_word": pdf_word,
            }))
        })();
        let json = match result {
            Ok(value) => value,
            Err(error) => serde_json::json!({
                "ok": false,
                "line": line_number + 1,
                "input": input_line,
                "location": null,
                "warning": null,
                "error": error,
            }),
        };
        serde_json::to_writer(&mut output, &json).map_err(|error| error.to_string())?;
        output.write_all(b"\n").map_err(|error| error.to_string())?;
        if line_number % 32 == 31 {
            output.flush().map_err(|error| error.to_string())?;
        }
    }
    output.flush().map_err(|error| error.to_string())
}

/// PDFium may expose either Unicode scalars or separate UTF-16 surrogate units.
/// Both halves of a pair must select the same UTF-8 glyph, including at the
/// neighborhood boundary. Invalid units are errors, never adjacent-glyph jumps.
fn unicode_context(
    len: usize,
    clicked: usize,
    mut value: impl FnMut(usize) -> Result<u32, String>,
) -> Result<(String, usize), String> {
    if clicked >= len {
        return Err("clicked character is outside PDF text".into());
    }
    let high = |value| (0xd800..=0xdbff).contains(&value);
    let low = |value| (0xdc00..=0xdfff).contains(&value);
    let mut start = clicked.saturating_sub(96);
    let mut end = len.min(clicked.saturating_add(97));
    if start > 0 && low(value(start)?) && high(value(start - 1)?) {
        start -= 1;
    }
    if end < len && high(value(end - 1)?) && low(value(end)?) {
        end += 1;
    }
    let mut context = String::with_capacity(end - start);
    let mut offset = 0;
    let mut index = start;
    while index < end {
        let unit = value(index)?;
        let next = if high(unit) && index + 1 < end {
            Some(value(index + 1)?)
        } else {
            None
        };
        let (scalar, width) = match next {
            Some(next) if low(next) => (0x10000 + ((unit - 0xd800) << 10) + next - 0xdc00, 2),
            _ => (unit, 1),
        };
        let character = char::from_u32(scalar)
            .ok_or_else(|| format!("invalid PDF Unicode value {unit:#x} at character {index}"))?;
        if (index..index + width).contains(&clicked) {
            offset = context.len();
        }
        context.push(character);
        index += width;
    }
    Ok((context, offset))
}

fn overlapping_match_ranges_cancellable(
    haystack: &str,
    needle: &str,
    mut is_current: impl FnMut() -> bool,
) -> Option<Vec<(usize, usize)>> {
    if needle.is_empty() {
        return Some(Vec::new());
    }
    let mut ranges = Vec::new();
    let mut offset = 0;
    while let Some(relative) = haystack.get(offset..)?.find(needle) {
        if !is_current() {
            return None;
        }
        let start = offset + relative;
        let first_char = haystack[start..].chars().next()?;
        ranges.push((start, start + needle.len()));
        offset = start + first_char.len_utf8();
    }
    Some(ranges)
}

fn find_visible_matches(
    document: &PdfDocument,
    document_id: DocumentId,
    query: &str,
    keys: &[(RenderKey, u32, u32)],
    cache: &mut [Option<CachedPageText>],
    generation: u64,
    latest_generation: &AtomicU64,
) -> Result<Option<Vec<VisibleMatch>>, String> {
    let needle = normalize_search_text(query);
    if needle.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut matches = Vec::new();
    for &(key, frame_width, frame_height) in keys {
        if generation != latest_generation.load(Ordering::Acquire) {
            return Ok(None);
        }
        if key.document_id != document_id {
            return Err(format!(
                "render key belongs to document {}, expected {document_id}",
                key.document_id
            ));
        }
        if frame_width == 0 || frame_height == 0 {
            return Err(format!(
                "visible frame for page {} has zero dimensions",
                u64::from(key.page) + 1
            ));
        }
        let page_index = i32::try_from(key.page).map_err(|_| {
            format!(
                "page {} exceeds PDFium's index range",
                u64::from(key.page) + 1
            )
        })?;
        let page = document.pages().get(page_index).map_err(|error| {
            format!(
                "could not load visible page {}: {error}",
                u64::from(key.page) + 1
            )
        })?;
        let zoom = i32::from(key.zoom.max(1));
        let target_width = (i32::from(key.width) * zoom / 100).max(1);
        let target_height = (i32::from(key.height) * zoom / 100).max(1);
        let config = build_fit_config(PdfRenderConfig::new(), key.fit, target_width, target_height);
        let Some(cached) =
            cached_visible_page_text(document, key.page, cache, generation, latest_generation)
        else {
            if generation != latest_generation.load(Ordering::Acquire) {
                return Ok(None);
            }
            return Err(format!(
                "could not extract visible text from page {}",
                u64::from(key.page) + 1
            ));
        };
        let Some((normalized, source_index_by_byte)) = cached.visible.as_ref() else {
            continue;
        };
        let Some(occurrences) = overlapping_match_ranges_cancellable(normalized, &needle, || {
            generation == latest_generation.load(Ordering::Acquire)
        }) else {
            return Ok(None);
        };
        if occurrences.is_empty() {
            continue;
        }
        let text = page.text().map_err(|error| error.to_string())?;
        let chars = text.chars();
        for (start, end) in occurrences {
            if generation != latest_generation.load(Ordering::Acquire) {
                return Ok(None);
            }
            let Some(source_ranges) = source_index_by_byte.get(start..end) else {
                continue;
            };
            let Some(&(first, _)) = source_ranges.first() else {
                continue;
            };
            let mut rects = Vec::new();
            let mut visible = true;
            let mut previous = None;
            for &(glyph_start, glyph_end) in source_ranges {
                if generation != latest_generation.load(Ordering::Acquire) {
                    return Ok(None);
                }
                let glyph_range = (glyph_start, glyph_end);
                if previous == Some(glyph_range) {
                    continue;
                }
                previous = Some(glyph_range);
                if chars
                    .get(glyph_start)
                    .ok()
                    .and_then(|character| char::from_u32(character.unicode_value()))
                    .is_some_and(char::is_whitespace)
                {
                    continue;
                }
                let previous_rect_count = rects.len();
                for index in glyph_start..glyph_end {
                    if generation != latest_generation.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let Ok(character) = chars.get(index) else {
                        continue;
                    };
                    if char::from_u32(character.unicode_value()).is_some_and(char::is_whitespace) {
                        continue;
                    }
                    let Ok(render_mode) = character.render_mode() else {
                        continue;
                    };
                    if !text_mode_has_visible_paint(
                        render_mode,
                        || character.fill_color().ok().map(|color| color.alpha()),
                        || character.stroke_color().ok().map(|color| color.alpha()),
                    ) {
                        continue;
                    }
                    let Ok(bounds) = character.tight_bounds() else {
                        continue;
                    };
                    let Some(rect) =
                        page_rect_to_pixels(&page, &config, frame_width, frame_height, bounds)
                    else {
                        continue;
                    };
                    if rect.right > rect.left && rect.bottom > rect.top {
                        rects.push(rect);
                    }
                }
                if rects.len() == previous_rect_count && glyph_start != glyph_end {
                    visible = false;
                    break;
                }
            }
            if !visible {
                continue;
            }
            let Some(first_rect) = rects.first() else {
                continue;
            };
            let last_range = *source_ranges
                .last()
                .expect("non-empty occurrence source range");
            let next_char = source_index_by_byte
                .get(end)
                .filter(|next_range| **next_range != last_range)
                .and_then(|&(start, stop)| {
                    source_range_char(start, stop, |index| {
                        chars
                            .get(index)
                            .ok()
                            .map(|character| character.unicode_value())
                    })
                })
                .filter(|character| character.is_alphanumeric() || *character == '_');
            let Some(last_rect) = rects.last() else {
                continue;
            };
            let center = |rect: &PixelRect| {
                (
                    rect.left + (rect.right - rect.left) / 2,
                    rect.top + (rect.bottom - rect.top) / 2,
                )
            };
            let hit = center(first_rect);
            let anchor = center(last_rect);
            matches.push(VisibleMatch {
                key,
                char_index: first,
                rects,
                hit,
                anchor,
                next_char,
            });
        }
    }
    Ok(Some(matches))
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
            visible: None,
        });
    }
    slot.as_ref()
}
fn cached_visible_page_text<'a>(
    document: &PdfDocument,
    page: u32,
    cache: &'a mut [Option<CachedPageText>],
    generation: u64,
    latest_generation: &AtomicU64,
) -> Option<&'a CachedPageText> {
    let index = usize::try_from(page).ok()?;
    cached_page_text(document, page, cache)?;
    let slot = cache.get_mut(index)?.as_mut()?;
    if slot.visible.is_none() {
        let page_index = i32::try_from(page).ok()?;
        let pdf_page = document.pages().get(page_index).ok()?;
        let (clip_left, clip_bottom, clip_right, clip_top) = effective_page_bounds(&pdf_page)?;
        let text = pdf_page.text().ok()?;
        let chars = text.chars();
        let mut visible_characters = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            if generation != latest_generation.load(Ordering::Acquire) {
                return None;
            }
            let Ok(character) = chars.get(index) else {
                index += 1;
                continue;
            };
            let first = character.unicode_value();
            let mut width = 1;
            let scalar = if (0xd800..=0xdbff).contains(&first) && index + 1 < chars.len() {
                let second = chars.get(index + 1).ok()?.unicode_value();
                if (0xdc00..=0xdfff).contains(&second) {
                    width = 2;
                    0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                } else {
                    first
                }
            } else {
                first
            };
            let end = index + width;
            let Some(value) = char::from_u32(scalar) else {
                index = end;
                continue;
            };
            if value.is_whitespace() {
                // PDFium also inserts whitespace without a text object. Keep those
                // separators, but not invisible or transparent painted spaces.
                let visible = character.render_mode().map_or(true, |mode| {
                    text_mode_has_visible_paint(
                        mode,
                        || character.fill_color().ok().map(|color| color.alpha()),
                        || character.stroke_color().ok().map(|color| color.alpha()),
                    )
                });
                if visible {
                    visible_characters.push((index, end, value));
                }
                index = end;
                continue;
            }
            let mut has_visible_glyph = false;
            for glyph_index in index..end {
                let Ok(glyph) = chars.get(glyph_index) else {
                    continue;
                };
                let Ok(render_mode) = glyph.render_mode() else {
                    continue;
                };
                if !text_mode_has_visible_paint(
                    render_mode,
                    || glyph.fill_color().ok().map(|color| color.alpha()),
                    || glyph.stroke_color().ok().map(|color| color.alpha()),
                ) {
                    continue;
                }
                let Ok(bounds) = glyph.tight_bounds() else {
                    continue;
                };
                if bounds.right().value > clip_left
                    && bounds.left().value < clip_right
                    && bounds.top().value > clip_bottom
                    && bounds.bottom().value < clip_top
                {
                    has_visible_glyph = true;
                    break;
                }
            }
            if has_visible_glyph {
                visible_characters.push((index, end, value));
            }
            index = end;
        }
        if generation != latest_generation.load(Ordering::Acquire) {
            return None;
        }
        slot.visible = Some(normalize_visible_search_characters(visible_characters));
    }
    Some(slot)
}

fn effective_page_bounds(page: &PdfPage) -> Option<(f32, f32, f32, f32)> {
    const WIDTH: i32 = 100_000;
    let page_width = page.width().value;
    let page_height = page.height().value;
    if !page_width.is_finite() || !page_height.is_finite() || page_width <= 0.0 {
        return None;
    }
    let scaled_height = (page_height * WIDTH as f32 / page_width).round();
    if !scaled_height.is_finite() || !(1.0..=i32::MAX as f32).contains(&scaled_height) {
        return None;
    }
    let height = scaled_height as i32;
    let config = PdfRenderConfig::new().set_target_width(WIDTH);
    let points: Vec<_> = [(0, 0), (WIDTH, 0), (0, height), (WIDTH, height)]
        .into_iter()
        .map(|(x, y)| page.pixels_to_points(x, y, &config).ok())
        .collect::<Option<_>>()?;
    let left = points
        .iter()
        .map(|(x, _)| x.value)
        .fold(f32::INFINITY, f32::min);
    let right = points
        .iter()
        .map(|(x, _)| x.value)
        .fold(f32::NEG_INFINITY, f32::max);
    let bottom = points
        .iter()
        .map(|(_, y)| y.value)
        .fold(f32::INFINITY, f32::min);
    let top = points
        .iter()
        .map(|(_, y)| y.value)
        .fold(f32::NEG_INFINITY, f32::max);
    (left.is_finite() && right.is_finite() && bottom.is_finite() && top.is_finite())
        .then_some((left, bottom, right, top))
}
fn text_mode_has_visible_paint(
    mode: PdfPageTextRenderMode,
    fill_alpha: impl FnOnce() -> Option<u8>,
    stroke_alpha: impl FnOnce() -> Option<u8>,
) -> bool {
    use PdfPageTextRenderMode::{
        FilledThenStroked, FilledThenStrokedClipping, FilledUnstroked, FilledUnstrokedClipping,
        StrokedUnfilled, StrokedUnfilledClipping,
    };
    match mode {
        FilledUnstroked | FilledUnstrokedClipping => fill_alpha().is_some_and(|alpha| alpha > 0),
        StrokedUnfilled | StrokedUnfilledClipping => stroke_alpha().is_some_and(|alpha| alpha > 0),
        FilledThenStroked | FilledThenStrokedClipping => {
            fill_alpha().is_some_and(|alpha| alpha > 0)
                || stroke_alpha().is_some_and(|alpha| alpha > 0)
        }
        PdfPageTextRenderMode::Unknown
        | PdfPageTextRenderMode::Invisible
        | PdfPageTextRenderMode::InvisibleClipping => false,
    }
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
fn normalize_visible_search_characters(
    characters: impl IntoIterator<Item = (usize, usize, char)>,
) -> (String, Vec<(usize, usize)>) {
    let mut normalized = String::new();
    let mut source_ranges = Vec::new();
    let mut pending_whitespace = None;
    for (source_start, source_end, character) in characters {
        if character.is_whitespace() {
            pending_whitespace.get_or_insert((source_start, source_end));
            continue;
        }
        if let Some(whitespace_range) = pending_whitespace.take()
            && !normalized.is_empty()
        {
            push_visible_normalized_character(
                &mut normalized,
                &mut source_ranges,
                whitespace_range,
                ' ',
            );
        }
        for character in character.to_lowercase() {
            push_visible_normalized_character(
                &mut normalized,
                &mut source_ranges,
                (source_start, source_end),
                character,
            );
        }
    }
    (normalized, source_ranges)
}

fn source_range_char(
    start: usize,
    end: usize,
    mut unit: impl FnMut(usize) -> Option<u32>,
) -> Option<char> {
    let first = unit(start)?;
    let scalar = match end.checked_sub(start)? {
        1 => first,
        2 if (0xd800..=0xdbff).contains(&first) => {
            let second = unit(start + 1)?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return None;
            }
            0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
        }
        _ => return None,
    };
    char::from_u32(scalar)
}

fn push_visible_normalized_character(
    normalized: &mut String,
    source_ranges: &mut Vec<(usize, usize)>,
    source_range: (usize, usize),
    character: char,
) {
    normalized.push(character);
    source_ranges.extend(std::iter::repeat_n(source_range, character.len_utf8()));
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

fn same_forward_word(a: &str, b: &str) -> bool {
    a.nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .eq(b
            .nfkd()
            .filter(|character| !is_combining_mark(*character))
            .flat_map(char::to_lowercase))
}

fn forward_neighbor_score(tokens: &[(usize, &str)], hint: &ForwardWord, index: usize) -> u32 {
    let mut score = 0;
    for distance in 1..=3isize {
        for direction in [-1, 1] {
            let delta = distance * direction;
            if let (Some(a), Some(b)) = (
                index.checked_add_signed(delta).and_then(|i| tokens.get(i)),
                hint.selected
                    .checked_add_signed(delta)
                    .and_then(|i| hint.words.get(i)),
            ) && same_forward_word(a.1, b)
            {
                score += 4 - distance as u32;
            }
        }
    }
    score
}

fn forward_context_score(text: &str, hint: &ForwardWord) -> Option<u32> {
    let selected = hint.words.get(hint.selected)?;
    let tokens = words(text);
    tokens
        .iter()
        .enumerate()
        .filter(|(_, (_, word))| same_forward_word(word, selected))
        .map(|(index, _)| forward_neighbor_score(&tokens, hint, index))
        .max()
}

/// Beamer can keep future overlay text in the PDF at coordinates well outside
/// the page. PDFium extracts those glyphs, so raw page text is not evidence
/// that the words are visible on the current overlay.
fn visible_forward_text(page: &PdfPage) -> Option<String> {
    let text = page.text().ok()?;
    let crop = page
        .boundaries()
        .crop()
        .map(|boundary| boundary.bounds)
        .unwrap_or_else(|_| page.page_size());
    let (normalized, _) =
        normalize_search_characters(text.chars().iter().filter_map(|character| {
            let value = character.unicode_char()?;
            if value.is_whitespace() {
                let visible = character.render_mode().map_or(true, |mode| {
                    text_mode_has_visible_paint(
                        mode,
                        || character.fill_color().ok().map(|color| color.alpha()),
                        || character.stroke_color().ok().map(|color| color.alpha()),
                    )
                });
                return visible.then_some((character.index(), value));
            }
            let bounds = character.tight_bounds().ok()?;
            let within_crop = bounds.right().value > crop.left().value
                && bounds.left().value < crop.right().value
                && bounds.top().value > crop.bottom().value
                && bounds.bottom().value < crop.top().value;
            let painted = character.render_mode().ok().is_some_and(|mode| {
                text_mode_has_visible_paint(
                    mode,
                    || character.fill_color().ok().map(|color| color.alpha()),
                    || character.stroke_color().ok().map(|color| color.alpha()),
                )
            });
            (within_crop && painted).then_some((character.index(), value))
        }));
    Some(normalized)
}

/// Prefer a later SyncTeX candidate only when visible PDF text matches the
/// selected source word together with nearby source words. A single word or
/// an unreadable page never overrides the first complete SyncTeX result.
pub(crate) fn first_visible_forward_page(
    pdf: &Path,
    candidates: &[u32],
    hint: &ForwardWord,
    library: Option<&Path>,
) -> Result<Option<u32>, String> {
    if candidates.len() < 2 || hint.words.len() < 2 {
        return Ok(None);
    }
    let pdfium = load_pdfium(library)?;
    let document = pdfium
        .load_pdf_from_file(pdf, None)
        .map_err(|error| format!("could not open {}: {error}", pdf.display()))?;
    let pages = u32::try_from(document.pages().len())
        .map_err(|_| "PDFium returned a negative page count".to_string())?;
    let score = |page: u32| -> Option<u32> {
        let index = page.checked_sub(1)?;
        if index >= pages {
            return None;
        }
        let index = i32::try_from(index).ok()?;
        let page = document.pages().get(index).ok()?;
        let text = visible_forward_text(&page)?;
        forward_context_score(&text, hint)
    };
    let first_score = score(candidates[0]);
    if first_score.is_some_and(|value| value > 0) {
        return Ok(None);
    }
    for &page in candidates.iter().take(64).skip(1) {
        let candidate_score = score(page);
        if candidate_score.is_some_and(|value| value > 0) {
            return Ok(Some(page));
        }
    }
    Ok(None)
}

/// Match complete PDF words inside the SyncTeX region. Point-only anchors also
/// allow a unique complete source-context match across text runs and wrapped
/// lines; weak context stays in the anchored run. Ties retain the coarse region.
fn forward_word_rect(
    page: &PdfPage,
    cached: &CachedPageText,
    hint: &ForwardWord,
    region: SearchRect,
) -> Result<Option<SearchRect>, String> {
    let tokens = words(&cached.normalized);
    let text = page.text().map_err(|e| e.to_string())?;
    let point_only = region.left == region.right && region.top == region.bottom;
    let complete_context_score = (point_only && hint.words.len() >= 3).then(|| {
        hint.words
            .iter()
            .enumerate()
            .map(|(index, _)| match index.abs_diff(hint.selected) {
                distance @ 1..=3 => 4 - distance as u32,
                _ => 0,
            })
            .sum::<u32>()
    });
    let mut best = None;
    let mut tied = false;
    for (index, &(start, word)) in tokens.iter().enumerate() {
        if !same_forward_word(word, &hint.words[hint.selected]) {
            continue;
        }
        let end = start + word.len();
        let first = cached.source_index_by_byte[start];
        let last = cached.source_index_by_byte[end - 1];
        // Reject omitted Unicode units and partial case-expanded glyphs.
        if last - first + 1 != word.chars().count()
            || start > 0 && cached.source_index_by_byte[start - 1] == first
            || cached.source_index_by_byte.get(end) == Some(&last)
        {
            continue;
        }
        let segments = text.segments_subset(first, last - first + 1);
        let mut rectangles = segments.iter();
        let Some(segment) = rectangles.next() else {
            continue;
        };
        if rectangles.next().is_some() {
            continue;
        }
        let bounds = segment.bounds();
        let x = (bounds.left().value + bounds.right().value) * 0.5;
        let y = (bounds.bottom().value + bounds.top().value) * 0.5;
        let score = forward_neighbor_score(&tokens, hint, index);
        if point_only {
            // Typst anchors a source span's first glyph, even if the selected
            // word follows a font switch or a soft wrap. Complete context is
            // evidence across those boundaries; without it, require the
            // actual PDF text run and baseline rather than an arbitrary radius.
            if complete_context_score != Some(score) {
                let character = text.chars().get(first).map_err(|e| e.to_string())?;
                let object = character.text_object().map_err(|e| e.to_string())?;
                let run = object.bounds().map_err(|e| e.to_string())?.to_rect();
                let origin = object.matrix().map_err(|e| e.to_string())?;
                let baseline = character.origin_y().map_err(|e| e.to_string())?.value;
                if region.left < run.left().value.min(origin.e()) - 2.0
                    || region.left > run.right().value.max(origin.e()) + 2.0
                    || region.top < bounds.bottom().value.min(baseline) - 2.0
                    || region.top > bounds.top().value.max(baseline) + 2.0
                {
                    continue;
                }
            }
        } else if x < region.left - 2.0
            || x > region.right + 2.0
            || y < region.top.min(region.bottom) - 2.0
            || y > region.top.max(region.bottom) + 2.0
        {
            continue;
        }
        let rect = SearchRect {
            left: bounds.left().value,
            right: bounds.right().value,
            top: bounds.top().value,
            bottom: bounds.bottom().value,
        };
        match best {
            Some((old, _)) if score < old => {}
            Some((old, _)) if score == old => tied = true,
            _ => {
                best = Some((score, rect));
                tied = false;
            }
        }
    }
    Ok(best.filter(|_| !tied).map(|(_, rect)| rect))
}

fn search_page(
    document: &PdfDocument,
    page: u32,
    cache: &mut [Option<CachedPageText>],
    needle: &str,
) -> (u32, Vec<SearchRect>, String) {
    let Some(cached) = cached_page_text(document, page, cache) else {
        return (0, Vec::new(), String::new());
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
        return (0, Vec::new(), String::new());
    }
    let context = search_match_context(cached, source_ranges[0]);

    let Ok(page_index) = i32::try_from(page) else {
        return (occurrences, Vec::new(), context);
    };
    let Ok(page) = document.pages().get(page_index) else {
        return (occurrences, Vec::new(), context);
    };
    let Ok(text) = page.text() else {
        return (occurrences, Vec::new(), context);
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
    (occurrences, rectangles, context)
}

fn search_match_context(cached: &CachedPageText, range: (usize, usize)) -> String {
    let (start, count) = range;
    let start_byte = cached
        .raw
        .char_indices()
        .nth(start)
        .map_or(cached.raw.len(), |(index, _)| index);
    let end_byte = cached
        .raw
        .char_indices()
        .nth(start.saturating_add(count))
        .map_or(cached.raw.len(), |(index, _)| index);
    context_window(&cached.raw, start_byte..end_byte, 70, 90)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
        let Some((min_x, max_x, min_y, max_y)) = page_rect_pixel_bounds(page, config, *rectangle)
        else {
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
    let mut links: Vec<_> = page
        .links()
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
        .collect();
    sort_page_links_reading_order(&mut links);
    links
}

fn extract_page_link_rectangles(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
) -> Vec<PageLinkRect> {
    page.links()
        .iter()
        .filter_map(|link| {
            let bounds = link.rect().ok()?;
            let rect = page_rect_to_pixels(page, config, width, height, bounds)?;
            Some(PageLinkRect {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            })
        })
        .collect()
}

fn extract_document_links(
    document: &PdfDocument,
    page: &PdfPage,
    source_page: u32,
    text_cache: &mut [Option<CachedPageText>],
) -> Vec<DocumentLink> {
    const INDEX_WIDTH: u32 = 1_000;
    let config = PdfRenderConfig::new().set_target_width(i32::try_from(INDEX_WIDTH).unwrap());
    let page_width = page.width().value.max(1.0);
    let index_height =
        ((INDEX_WIDTH as f32 * page.height().value / page_width).ceil() as u32).max(1);
    let source_text = cached_page_text(document, source_page, text_cache)
        .map(|text| normalize_context_text(&text.raw));
    let mut label_occurrences = HashMap::<String, usize>::new();
    coalesce_document_link_fragments(
        extract_page_links(document, page, &config, INDEX_WIDTH, index_height),
        source_text.as_deref(),
    )
    .into_iter()
    .enumerate()
    .map(|(ordinal, link)| {
        let occurrence = label_occurrences.entry(link.label.clone()).or_default();
        let source_context = source_text
            .as_deref()
            .and_then(|text| context_around_label(text, &link.label, *occurrence, 90, 110));
        *occurrence += 1;
        let reference_context = match &link.target {
            LinkTarget::Internal { page, .. } => citation_number(&link.label).and_then(|number| {
                cached_page_text(document, *page, text_cache)
                    .and_then(|text| reference_context(&normalize_context_text(&text.raw), number))
            }),
            LinkTarget::Uri(_) => None,
        }
        .filter(|reference| source_context.as_ref() != Some(reference));
        DocumentLink {
            source_page,
            source_top_ratio: (link.rect.top as f32 / index_height as f32).clamp(0.0, 1.0),
            ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
            label: link.label,
            source_context,
            reference_context,
            target: link.target,
        }
    })
    .collect()
}

fn coalesce_document_link_fragments(
    links: Vec<PageLink>,
    source_text: Option<&str>,
) -> Vec<PageLink> {
    page_link_group_indices(&links)
        .into_iter()
        .filter_map(|group| {
            let mut indices = group.into_iter();
            let mut combined = links.get(indices.next()?)?.clone();
            for index in indices {
                let fragment = &links[index];
                append_link_fragment(&mut combined.label, &fragment.label, source_text);
                combined.rect = fragment.rect;
            }
            Some(combined)
        })
        .collect()
}

fn selected_page_link_rectangles(
    links: &[PageLink],
    selected_ordinal: Option<u32>,
) -> Vec<PageLinkRect> {
    let Some(selected) = selected_ordinal.and_then(|ordinal| usize::try_from(ordinal).ok()) else {
        return Vec::new();
    };
    page_link_group_indices(links)
        .get(selected)
        .into_iter()
        .flatten()
        .map(|index| links[*index].rect)
        .collect()
}

fn page_link_group_indices(links: &[PageLink]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::with_capacity(links.len());
    for (index, link) in links.iter().enumerate() {
        if let Some(group) = groups.iter_mut().rev().find(|group| {
            let previous = &links[*group.last().expect("link group is not empty")];
            previous.target == link.target && link_fragments_are_adjacent(previous.rect, link.rect)
        }) {
            group.push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    groups
}

fn append_link_fragment(label: &mut String, fragment: &str, source_text: Option<&str>) {
    if label.is_empty() || fragment.is_empty() {
        label.push_str(fragment);
        return;
    }
    let spaced = format!("{label} {fragment}");
    let joined = format!("{label}{fragment}");
    if source_text.is_some_and(|text| text.contains(&spaced)) {
        *label = spaced;
    } else if source_text.is_some_and(|text| text.contains(&joined)) {
        *label = joined;
    } else {
        *label = spaced;
    }
}

fn link_fragments_are_adjacent(previous: PageLinkRect, next: PageLinkRect) -> bool {
    let previous_height = previous.bottom.saturating_sub(previous.top).max(1);
    let next_height = next.bottom.saturating_sub(next.top).max(1);
    let line_height = previous_height.max(next_height);
    let top_delta = previous.top.abs_diff(next.top);
    let same_line = top_delta <= line_height / 2 + 1
        && next.left.saturating_sub(previous.right) <= line_height.saturating_mul(2);
    let wrapped_line = next.top > previous.top
        && next.top.saturating_sub(previous.top) <= line_height.saturating_mul(2)
        && next.left < previous.left;
    same_line || wrapped_line
}

fn normalize_context_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn citation_number(label: &str) -> Option<&str> {
    let number = label.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '[' | ']' | ',' | ';')
    });
    (!number.is_empty() && number.chars().all(|character| character.is_ascii_digit()))
        .then_some(number)
}

fn context_around_label(
    text: &str,
    label: &str,
    occurrence: usize,
    before: usize,
    after: usize,
) -> Option<String> {
    let label = normalize_context_text(label);
    if label.is_empty() {
        return None;
    }
    let range = nth_match(text, &label, occurrence)?;
    Some(context_window(text, range, before, after))
}

fn reference_context(text: &str, number: &str) -> Option<String> {
    let candidates = [
        format!("[{number}]"),
        format!("{number}."),
        format!("{number})"),
    ];
    let range = candidates
        .iter()
        .find_map(|candidate| nth_match(text, candidate, 0))?;
    let mut context = context_window(text, range.clone(), 0, 220);
    if let Some(stripped) = context.strip_prefix('…') {
        context = stripped.to_string();
    }
    if let Some(next_reference) = next_bracketed_number(&context, range.end - range.start) {
        context.truncate(next_reference);
        context = context.trim_end().to_string();
        if range.end < text.len() {
            context.push('…');
        }
    }
    Some(context)
}

fn nth_match(text: &str, needle: &str, occurrence: usize) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    for current in 0..=occurrence {
        let start = text.get(offset..)?.find(needle)? + offset;
        let end = start + needle.len();
        if current == occurrence {
            return Some(start..end);
        }
        offset = end;
    }
    None
}

fn context_window(
    text: &str,
    range: std::ops::Range<usize>,
    before: usize,
    after: usize,
) -> String {
    let characters: Vec<_> = text.chars().collect();
    let start_character = text[..range.start].chars().count();
    let end_character = start_character + text[range].chars().count();
    let mut start = start_character.saturating_sub(before);
    while start > 0 && !characters[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = end_character.saturating_add(after).min(characters.len());
    while end < characters.len() && !characters[end].is_whitespace() {
        end += 1;
    }
    let mut context: String = characters[start..end].iter().collect();
    context = context.trim().to_string();
    if start > 0 {
        context.insert(0, '…');
    }
    if end < characters.len() {
        context.push('…');
    }
    context
}

fn next_bracketed_number(text: &str, start: usize) -> Option<usize> {
    text.get(start..)?;
    let mut offset = start;
    while let Some(relative) = text.get(offset..)?.find('[') {
        let candidate = offset + relative;
        let after = text.get(candidate + 1..)?;
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b']') {
            return Some(candidate);
        }
        offset = candidate + 1;
    }
    None
}

fn sort_page_links_reading_order(links: &mut [PageLink]) {
    links.sort_by_key(|link| {
        (
            link.rect.top,
            link.rect.left,
            link.rect.bottom,
            link.rect.right,
        )
    });
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
    let (target_x, target_y) = match destination.view_settings().ok() {
        Some(PdfDestinationViewSettings::SpecificCoordinatesAndZoom(x, y, _)) => (x, y),
        Some(PdfDestinationViewSettings::FitPageHorizontallyToWindow(y))
        | Some(PdfDestinationViewSettings::FitBoundsHorizontallyToWindow(y)) => (None, y),
        Some(PdfDestinationViewSettings::FitPageVerticallyToWindow(x))
        | Some(PdfDestinationViewSettings::FitBoundsVerticallyToWindow(x)) => (x, None),
        Some(PdfDestinationViewSettings::FitPageToRectangle(rect)) => {
            (Some(rect.left()), Some(rect.top()))
        }
        _ => (None, None),
    };
    let target_page = (target_x.is_some() || target_y.is_some())
        .then(|| document.pages().get(page_index).ok())
        .flatten();
    let (left_ratio, top_ratio) = target_page
        .as_ref()
        .and_then(|page| {
            // A fixed square maps both rendered axes directly to normalized positions,
            // including page rotation and the visible crop/media-box origin.
            const SIZE: i32 = 10_000;
            let config = PdfRenderConfig::new().set_fixed_size(SIZE, SIZE);
            let (left, top) = page_destination_position(page, &config, target_x, target_y)?;
            let ratio = |position: i32| (position as f32 / SIZE as f32).clamp(0.0, 1.0);
            Some((left.map(ratio), top.map(ratio)))
        })
        .unwrap_or((None, None));
    Some(LinkTarget::Internal {
        page,
        top_ratio,
        left_ratio,
    })
}

fn page_destination_position(
    page: &PdfPage,
    config: &PdfRenderConfig,
    x: Option<PdfPoints>,
    y: Option<PdfPoints>,
) -> Option<(Option<i32>, Option<i32>)> {
    let x_value = x.unwrap_or(PdfPoints::ZERO);
    let y_value = y.unwrap_or(PdfPoints::ZERO);
    let (horizontal, vertical) = page.points_to_pixels(x_value, y_value, config).ok()?;
    if x.is_some() && y.is_some() {
        return Some((Some(horizontal), Some(vertical)));
    }
    // Rotation determines the optional rendered axes exactly; comparing transformed
    // integer pixels would lose omitted coordinates on large or fractionally cropped pages.
    let axes_swapped = matches!(
        page.rotation().ok()?,
        pdfium_render::prelude::PdfPageRenderRotation::Degrees90
            | pdfium_render::prelude::PdfPageRenderRotation::Degrees270
    );
    let (has_left, has_top) = if axes_swapped {
        (y.is_some(), x.is_some())
    } else {
        (x.is_some(), y.is_some())
    };
    Some((has_left.then_some(horizontal), has_top.then_some(vertical)))
}

fn page_rect_pixel_bounds(
    page: &PdfPage,
    config: &PdfRenderConfig,
    bounds: SearchRect,
) -> Option<(i32, i32, i32, i32)> {
    if ![bounds.left, bounds.right, bounds.top, bounds.bottom]
        .into_iter()
        .all(f32::is_finite)
    {
        return None;
    }
    let mut left = i32::MAX;
    let mut right = i32::MIN;
    let mut top = i32::MAX;
    let mut bottom = i32::MIN;
    for (x, y) in [
        (bounds.left, bounds.top),
        (bounds.right, bounds.top),
        (bounds.left, bounds.bottom),
        (bounds.right, bounds.bottom),
    ] {
        let (x, y) = page
            .points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), config)
            .ok()?;
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
    }
    Some((left, right, top, bottom))
}

fn page_rect_to_pixels(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    bounds: PdfRect,
) -> Option<PixelRect> {
    let (left, right, top, bottom) = page_rect_pixel_bounds(
        page,
        config,
        SearchRect {
            left: bounds.left().value,
            right: bounds.right().value,
            top: bounds.top().value,
            bottom: bounds.bottom().value,
        },
    )?;
    Some(PixelRect {
        left: left.saturating_sub(1).clamp(0, width as i32) as u32,
        right: right.saturating_add(2).clamp(0, width as i32) as u32,
        top: top.saturating_sub(1).clamp(0, height as i32) as u32,
        bottom: bottom.saturating_add(2).clamp(0, height as i32) as u32,
    })
}

fn apply_link_highlights(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    links: &[PageLink],
    color: [u8; 3],
    dark_mode: bool,
) {
    for link in links {
        let rect = PixelRect {
            left: link.rect.left,
            top: link.rect.top,
            right: link.rect.right,
            bottom: link.rect.bottom,
        };
        if dark_mode {
            blend_rectangle(
                rgba,
                width,
                height,
                PixelRect {
                    top: rect.bottom.saturating_sub(1),
                    ..rect
                },
                color,
                DARK_LINK_BORDER_ALPHA,
            );
        } else {
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
}

fn apply_selected_link_highlights(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rectangles: &[PageLinkRect],
    color: [u8; 3],
) {
    for rectangle in rectangles {
        let rect = PixelRect {
            left: rectangle.left,
            top: rectangle.top,
            right: rectangle.right,
            bottom: rectangle.bottom,
        };
        blend_rectangle(
            rgba,
            width,
            height,
            rect,
            color,
            SELECTED_LINK_HIGHLIGHT_ALPHA,
        );
        let border = 3;
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
            blend_rectangle(rgba, width, height, edge, color, SELECTED_LINK_BORDER_ALPHA);
        }
    }
}

fn apply_dark_mode_link_contrast(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rectangles: &[PageLinkRect],
    style: DarkModeStyle,
    accent: [u8; 3],
) {
    let Ok(stride) = usize::try_from(width).map(|width| width.saturating_mul(4)) else {
        return;
    };
    for rectangle in rectangles {
        let left = rectangle.left.min(width);
        let right = rectangle.right.min(width);
        let top = rectangle.top.min(height);
        let bottom = rectangle.bottom.min(height);
        let Some(background) = dominant_rectangle_color(rgba, width, height, *rectangle) else {
            continue;
        };
        let Some(ink) = darkest_blue_link_ink(rgba, width, height, *rectangle, background) else {
            continue;
        };
        let target = if contrast_ratio(style.foreground, background) >= MINIMUM_DARK_LINK_CONTRAST {
            style.foreground
        } else {
            accent
        };
        let direction = [
            f32::from(ink[0]) - f32::from(background[0]),
            f32::from(ink[1]) - f32::from(background[1]),
            f32::from(ink[2]) - f32::from(background[2]),
        ];
        let denominator = direction
            .iter()
            .map(|channel| channel * channel)
            .sum::<f32>();
        if denominator < 1.0 {
            continue;
        }
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
                let original = [pixel[0], pixel[1], pixel[2]];
                if !is_blue_link_ink(original, background) {
                    continue;
                }
                let delta = [
                    f32::from(original[0]) - f32::from(background[0]),
                    f32::from(original[1]) - f32::from(background[1]),
                    f32::from(original[2]) - f32::from(background[2]),
                ];
                let coverage = delta
                    .iter()
                    .zip(direction)
                    .map(|(channel, direction)| channel * direction)
                    .sum::<f32>()
                    / denominator;
                let coverage = coverage.clamp(0.0, 1.0);
                if coverage < 0.02 {
                    continue;
                }
                let adjusted: [u8; 3] = std::array::from_fn(|channel| {
                    blend_channel(background[channel], target[channel], coverage)
                });
                pixel[..3].copy_from_slice(&adjusted);
            }
        }
    }
}

fn dominant_rectangle_color(
    rgba: &[u8],
    width: u32,
    height: u32,
    rectangle: PageLinkRect,
) -> Option<[u8; 3]> {
    let left = rectangle.left.min(width);
    let right = rectangle.right.min(width);
    let top = rectangle.top.min(height);
    let bottom = rectangle.bottom.min(height);
    let stride = usize::try_from(width).ok()?.checked_mul(4)?;
    let mut colors = HashMap::<u16, (u32, [u64; 3])>::new();
    for y in top..bottom {
        let row_start = usize::try_from(y).ok()?.checked_mul(stride)?;
        for x in left..right {
            let offset = row_start.checked_add(usize::try_from(x).ok()?.checked_mul(4)?)?;
            let pixel = rgba.get(offset..offset.checked_add(3)?)?;
            let key = (u16::from(pixel[0] >> 3) << 10)
                | (u16::from(pixel[1] >> 3) << 5)
                | u16::from(pixel[2] >> 3);
            let entry = colors.entry(key).or_insert((0, [0; 3]));
            entry.0 += 1;
            for (sum, channel) in entry.1.iter_mut().zip(pixel) {
                *sum += u64::from(*channel);
            }
        }
    }
    let (_, (count, sums)) = colors.into_iter().max_by_key(|(_, (count, _))| *count)?;
    Some(std::array::from_fn(|channel| {
        u8::try_from(sums[channel] / u64::from(count)).unwrap_or(u8::MAX)
    }))
}

fn darkest_blue_link_ink(
    rgba: &[u8],
    width: u32,
    height: u32,
    rectangle: PageLinkRect,
    background: [u8; 3],
) -> Option<[u8; 3]> {
    let left = rectangle.left.min(width);
    let right = rectangle.right.min(width);
    let top = rectangle.top.min(height);
    let bottom = rectangle.bottom.min(height);
    let stride = usize::try_from(width).ok()?.checked_mul(4)?;
    let mut farthest = None;
    let mut farthest_distance = 0_u32;
    for y in top..bottom {
        let row_start = usize::try_from(y).ok()?.checked_mul(stride)?;
        for x in left..right {
            let offset = row_start.checked_add(usize::try_from(x).ok()?.checked_mul(4)?)?;
            let pixel = rgba.get(offset..offset.checked_add(3)?)?;
            let pixel = [pixel[0], pixel[1], pixel[2]];
            if !is_blue_link_ink(pixel, background) {
                continue;
            }
            let distance = pixel
                .iter()
                .zip(background)
                .map(|(channel, background)| u32::from(channel.abs_diff(background)).pow(2))
                .sum();
            if distance > farthest_distance {
                farthest = Some(pixel);
                farthest_distance = distance;
            }
        }
    }
    farthest
}

fn is_blue_link_ink(pixel: [u8; 3], background: [u8; 3]) -> bool {
    pixel[2].saturating_sub(pixel[0].max(pixel[1])) >= DARK_LINK_BLUE_DOMINANCE
        && relative_luminance(pixel) > relative_luminance(background) + 0.005
        && pixel
            .iter()
            .zip(background)
            .any(|(channel, background)| channel.abs_diff(background) >= 8)
}

fn contrast_ratio(first: [u8; 3], second: [u8; 3]) -> f32 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    let (lighter, darker) = if first >= second {
        (first, second)
    } else {
        (second, first)
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: [u8; 3]) -> f32 {
    let linear = color.map(|channel| {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    });
    linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
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
    let path = match library {
        Some(path) => path.to_path_buf(),
        None => crate::embedded_pdfium::materialize().map_err(|error| {
            format!("could not extract embedded PDFium to the user cache: {error}")
        })?,
    };
    match Pdfium::bind_to_library(&path) {
        Ok(bindings) => Ok(Pdfium::new(bindings)),
        Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Ok(Pdfium::default()),
        Err(error) => Err(format!(
            "could not load PDFium from {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
pub(crate) fn pdfium_test_lock() -> std::sync::MutexGuard<'static, ()> {
    // Native fixtures share PDFium state; do not interleave independent fixture
    // transactions. Production confines PDFium to one render worker.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().expect("native PDFium fixture lock poisoned")
}

#[cfg(test)]
mod tests {
    fn close_test_worker(worker: &super::RenderWorker, revision: super::DocumentRevision) {
        worker.close(1);
        worker
            .find_visible(1, u64::MAX, revision, String::new(), Vec::new())
            .unwrap();
        match worker
            .message_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap()
        {
            super::WorkerMessage::VisibleMatchesError { request_id, .. } => {
                assert_eq!(request_id, u64::MAX);
            }
            message => panic!("expected closed native fixture, got {message:?}"),
        }
    }

    fn coordinate_frame(worker: &super::RenderWorker) -> super::Frame {
        match worker
            .message_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap()
        {
            super::WorkerMessage::Frame(frame) => frame,
            message => panic!("expected coordinate frame, got {message:?}"),
        }
    }

    fn coordinate_rgba(frame: &super::Frame) -> Vec<u8> {
        use std::io::Read;
        let mut rgba = Vec::new();
        flate2::read::ZlibDecoder::new(frame.compressed_rgba.as_slice())
            .read_to_end(&mut rgba)
            .unwrap();
        assert_eq!(rgba.len(), (frame.width * frame.height * 4) as usize);
        rgba
    }

    #[test]
    fn synctex_worker_points_and_highlights_follow_original_paper_geometry() {
        let _native = super::pdfium_test_lock();
        use crate::{
            editor::Editor,
            navigation::{InverseTask, NavigationWorker},
            process::Operation,
            synctex,
        };
        use pdfium_render::prelude::{PdfPoints, PdfRenderConfig};
        use std::{fmt::Write, process::Command, time::Duration};

        // pdfTeX's original paper is 400x600bp even on pages whose visible
        // CropBox is offset, /Rotate swaps display axes, or MediaBox starts
        // elsewhere. Changing original paper extents without updating SyncTeX
        // is not a coordinate convention and cannot repair an outdated sidecar.
        let cases = [
            (0, "", ""),
            (90, "", ""),
            (180, "", ""),
            (270, "", ""),
            (0, "/CropBox [20 30 380 570]", ""),
            (90, "/CropBox [20 30 380 570]", ""),
            (180, "/CropBox [20 30 380 570]", ""),
            (270, "/CropBox [20 30 380 570]", ""),
            (90, "/CropBox [40 60 390 590]", "/MediaBox [20 30 420 630]"),
            (270, "/CropBox [40 60 390 590]", "/MediaBox [20 30 420 630]"),
        ];
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("coordinates.tex");
        let pdf = directory.path().join("coordinates.pdf");
        let mut tex = String::from(
            "\\documentclass{article}\n\
             \\pdfcompresslevel=0\\pdfobjcompresslevel=0\n\
             \\usepackage[paperwidth=400bp,paperheight=600bp,margin=72bp]{geometry}\n\
             \\pagestyle{empty}\n\
             \\begin{document}\n",
        );
        let mut lines = Vec::new();
        for (index, (rotation, crop, media)) in cases.iter().enumerate() {
            if index != 0 {
                tex.push_str("\\newpage\n");
            }
            writeln!(tex, "\\pdfpageattr{{/Rotate {rotation} {crop} {media}}}").unwrap();
            lines.push(tex.lines().count() as u32 + 1);
            tex.push_str(
                "\\noindent ZenithAlpha confirms the original page origin.\\par\n\
                 \\vspace{110bp}\n\
                 \\noindent MeridianBeta identifies a different source line.\\par\n\
                 \\vspace{110bp}\n\
                 \\noindent NadirGamma identifies the bottom region.\\par\n",
            );
        }
        tex.push_str("\\end{document}\n");
        std::fs::write(&source, &tex).unwrap();
        let output = crate::process::output(
            Command::new("pdflatex")
                .current_dir(directory.path())
                .args([
                    "-interaction=nonstopmode",
                    "-halt-on-error",
                    "-synctex=1",
                    "coordinates.tex",
                ]),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for real coordinate regressions");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );

        let pdfium = super::load_pdfium(None).unwrap();
        let document = pdfium.load_pdf_from_file(&pdf, None).unwrap();
        let worker = super::RenderWorker::spawn(1, pdf.clone(), None);
        assert_eq!(worker.wait_until_ready().unwrap().0, cases.len() as u32);
        let navigation = NavigationWorker::new();
        let revision = synctex::DocumentRevision::read(&pdf).unwrap();
        worker.begin_generation(1);
        let mut batch_input = String::new();
        for (index, line) in lines.iter().copied().enumerate() {
            let page = document.pages().get(index as i32).unwrap();
            let text = page.text().unwrap();
            let first = text
                .chars()
                .iter()
                .position(|character| character.unicode_value() == u32::from('Z'))
                .unwrap();
            let glyph = text.chars().get(first + 3).unwrap().tight_bounds().unwrap();
            let pdf_x = (glyph.left().value + glyph.right().value) * 0.5;
            let pdf_y = (glyph.top().value + glyph.bottom().value) * 0.5;
            writeln!(
                batch_input,
                "{}",
                serde_json::json!({
                    "page": index + 1,
                    "x": pdf_x,
                    "y": 600.0 - pdf_y,
                })
            )
            .unwrap();
            let selected = text.segments_subset(first, "ZenithAlpha".len());
            assert_eq!(selected.len(), 1);
            let word = selected.iter().next().unwrap().bounds();
            let word_rect = super::SearchRect {
                left: word.left().value,
                right: word.right().value,
                top: word.top().value,
                bottom: word.bottom().value,
            };
            let column = tex
                .lines()
                .nth(line as usize - 1)
                .unwrap()
                .find("ZenithAlpha")
                .unwrap() as u32
                + 1;
            let request = synctex::resolve_forward(&pdf, &source, line, column).unwrap();
            assert_eq!(request.page, index as u32 + 1);
            for (fit, zoom) in [(super::FitMode::Page, 100), (super::FitMode::Width, 125)] {
                let key = super::RenderKey {
                    document_id: 1,
                    page: index as u32,
                    width: 600,
                    height: 800,
                    zoom,
                    fit,
                    invert: false,
                    dark_mode_style: super::DarkModeStyle::new([0; 3], [255; 3]),
                    search_request_id: 0,
                    search_highlight: [255, 255, 0],
                    link_mode: false,
                    link_highlight: [255, 255, 0],
                    selected_link_ordinal: None,
                };
                let config = super::build_fit_config(
                    PdfRenderConfig::new(),
                    fit,
                    i32::from(key.width) * i32::from(zoom) / 100,
                    i32::from(key.height) * i32::from(zoom) / 100,
                );
                worker.clear_flash(1);
                worker
                    .render(super::RenderRequest { key, generation: 1 })
                    .unwrap();
                let plain = coordinate_frame(&worker);
                let (pixel_x, pixel_y) = page
                    .points_to_pixels(PdfPoints::new(pdf_x), PdfPoints::new(pdf_y), &config)
                    .unwrap();
                worker.page_point(revision, 1, pixel_x as u32, pixel_y as u32, key);
                let super::WorkerMessage::PagePoint { result, .. } = worker
                    .message_rx
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                else {
                    panic!("missing coordinate inverse point");
                };
                let click = result.unwrap();
                assert!((click.synctex.x - pdf_x).abs() < 1.0, "{click:?}");
                assert!(
                    (click.synctex.y_from_top - (600.0 - pdf_y)).abs() < 1.0,
                    "{click:?}"
                );
                assert_eq!(click.synctex.page_height_pt, 600.0);
                // The compiler-service protocol retains its previous semantics.
                assert_eq!(click.typst.x, click.synctex.x);
                assert!(
                    (click.typst.y_from_top
                        - click.synctex.y_from_top
                        - (page.height().value - 600.0))
                        .abs()
                        < 0.01
                );
                assert_eq!(click.typst.page_height_pt, page.height().value);
                navigation
                    .submit(InverseTask {
                        request_id: 1,
                        path: pdf.clone(),
                        revision,
                        page: index as u32,
                        click,
                        word_precision: true,
                        radius: 4,
                        editor: Editor::None,
                        inverse_search: None,
                        operation: Operation::default(),
                    })
                    .unwrap();
                let inverse = navigation
                    .replies
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .result
                    .unwrap();
                assert_eq!(inverse.location.line, line);
                assert!(inverse.location.precise, "{:?}", inverse.location);
                assert_eq!(inverse.location.byte_column, (column - 1) as usize);

                // Compare actual flashed pixels with PDFium's painted text bounds,
                // not merely the forward/inverse conversion's own round trip.
                worker.flash(1, index as u32, request.rect(), request.word.clone());
                worker
                    .render(super::RenderRequest { key, generation: 1 })
                    .unwrap();
                let precise = coordinate_frame(&worker);
                let highlight = precise.flash.as_ref().unwrap();
                assert!(highlight.error.is_none(), "{:?}", highlight.error);
                assert!(highlight.word_precise);
                let expected = super::page_rect_pixel_bounds(&page, &config, word_rect).unwrap();
                assert_eq!(highlight.pixel_bounds, Some(expected));
                let before = coordinate_rgba(&plain);
                let after = coordinate_rgba(&precise);
                let mut changed = (u32::MAX, 0, u32::MAX, 0);
                for (pixel, (before, after)) in before
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(after.as_chunks::<4>().0)
                    .enumerate()
                {
                    if before != after {
                        let x = pixel as u32 % precise.width;
                        let y = pixel as u32 / precise.width;
                        changed.0 = changed.0.min(x);
                        changed.1 = changed.1.max(x + 1);
                        changed.2 = changed.2.min(y);
                        changed.3 = changed.3.max(y + 1);
                    }
                }
                assert_eq!(
                    changed,
                    (
                        expected.0.saturating_sub(1).clamp(0, precise.width as i32) as u32,
                        expected.1.saturating_add(2).clamp(0, precise.width as i32) as u32,
                        expected.2.saturating_sub(1).clamp(0, precise.height as i32) as u32,
                        expected.3.saturating_add(2).clamp(0, precise.height as i32) as u32,
                    )
                );

                worker.flash(1, index as u32, request.rect(), None);
                worker
                    .render(super::RenderRequest { key, generation: 1 })
                    .unwrap();
                let coarse = coordinate_frame(&worker);
                let highlight = coarse.flash.as_ref().unwrap();
                assert!(!highlight.word_precise);
                assert!(highlight.error.is_none());
                let expected = super::SearchRect {
                    left: request.h,
                    right: request.h + request.width,
                    top: 600.0 - (request.v - request.height),
                    bottom: 600.0 - request.v,
                };
                assert_eq!(
                    highlight.pixel_bounds,
                    super::page_rect_pixel_bounds(&page, &config, expected)
                );

                if index == 0 && zoom == 100 {
                    worker.flash(
                        1,
                        0,
                        super::SearchRect {
                            left: f32::NAN,
                            ..request.rect()
                        },
                        None,
                    );
                    worker
                        .render(super::RenderRequest { key, generation: 1 })
                        .unwrap();
                    let invalid = coordinate_frame(&worker);
                    let highlight = invalid.flash.as_ref().unwrap();
                    assert!(highlight.error.as_ref().unwrap().contains("not finite"));
                    assert!(highlight.pixel_bounds.is_none());
                    assert_eq!(coordinate_rgba(&invalid), before);
                }
            }
        }
        close_test_worker(&worker, revision);
        let mut batch_output = Vec::new();
        super::synctex_edit_batch(
            &pdf,
            None,
            &crate::config::ViewerSettings::default(),
            std::io::Cursor::new(batch_input),
            &mut batch_output,
        )
        .unwrap();
        let records = String::from_utf8(batch_output).unwrap();
        assert_eq!(records.lines().count(), cases.len());
        for (record, expected_line) in records.lines().zip(lines) {
            let record: serde_json::Value = serde_json::from_str(record).unwrap();
            assert_eq!(record["ok"], true, "{record}");
            assert_eq!(record["pdf_word"], "ZenithAlpha");
            assert_eq!(record["location"]["line"], expected_line);
            assert_eq!(record["location"]["precise"], true);
        }
    }

    #[test]
    fn visible_search_finds_overlapping_unicode_occurrences() {
        assert_eq!(
            super::overlapping_match_ranges_cancellable("banana", "ana", || true).unwrap(),
            [(1, 4), (3, 6)]
        );
        assert_eq!(
            super::overlapping_match_ranges_cancellable("ééé", "éé", || true).unwrap(),
            [(0, 4), (2, 6)]
        );
    }
    #[test]
    fn visible_source_ranges_decode_characters_without_reusing_the_next_glyph() {
        let units = [b'S' as u32, b'e' as u32, 0xd83d, 0xde00, b'!' as u32];
        let decode = |start, end| super::source_range_char(start, end, |i| units.get(i).copied());
        assert_eq!(decode(1, 2), Some('e'));
        assert_eq!(decode(2, 4), Some('😀'));
        assert_eq!(decode(1, 3), None);
    }

    #[test]
    fn effective_page_bounds_respect_crops_inheritance_and_rotation() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        let cases = [
            (
                synthetic_bounds_pdf("[100 200 500 600]", "[150 250 450 550]", 90, false),
                (150.0, 250.0, 450.0, 550.0),
            ),
            (
                synthetic_bounds_pdf("[100 200 500 600]", "[50 150 550 650]", 270, false),
                (100.0, 200.0, 500.0, 600.0),
            ),
            (
                synthetic_bounds_pdf("[100 200 500 600]", "[150 250 450 550]", 270, true),
                (150.0, 250.0, 450.0, 550.0),
            ),
        ];
        for (pdf, expected) in cases {
            let document = pdfium.load_pdf_from_byte_vec(pdf, None).unwrap();
            let page = document.pages().get(0).unwrap();
            let actual = super::effective_page_bounds(&page).unwrap();
            for (actual, expected) in [actual.0, actual.1, actual.2, actual.3]
                .into_iter()
                .zip([expected.0, expected.1, expected.2, expected.3])
            {
                assert!(
                    (actual - expected).abs() < 0.1,
                    "effective page bound {actual} differs from expected {expected}"
                );
            }
        }
    }

    #[test]
    fn invalid_visible_page_reports_an_error_without_stopping_the_renderer() {
        let _native = super::pdfium_test_lock();
        let pdf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            pdf.path(),
            synthetic_bounds_pdf("[0 0 200 100]", "[0 0 200 100]", 0, false),
        )
        .unwrap();
        let worker = super::RenderWorker::spawn(1, pdf.path().to_path_buf(), None);
        assert_eq!(worker.wait_until_ready().unwrap().0, 1);
        let revision = super::DocumentRevision::read(pdf.path()).unwrap();
        let key = super::RenderKey {
            document_id: 1,
            page: u32::MAX,
            width: 100,
            height: 100,
            zoom: 100,
            fit: super::FitMode::Page,
            invert: false,
            dark_mode_style: super::DarkModeStyle::new([0; 3], [255; 3]),
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [255, 255, 0],
            selected_link_ordinal: None,
        };
        worker
            .find_visible(1, 1, revision, "a".into(), vec![(key, 100, 100)])
            .unwrap();
        let reply = worker
            .message_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let super::WorkerMessage::VisibleMatchesError { error, .. } = reply else {
            panic!("invalid page did not return a scoped error");
        };
        assert!(error.contains("4294967296"), "{error}");
        worker
            .find_visible(
                1,
                2,
                revision,
                "".into(),
                vec![(super::RenderKey { page: 0, ..key }, 100, 100)],
            )
            .unwrap();
        let reply = worker
            .message_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            reply,
            super::WorkerMessage::VisibleMatches { request_id: 2, .. }
        ));
        close_test_worker(&worker, revision);
    }

    fn synthetic_bounds_pdf(
        media_box: &str,
        crop_box: &str,
        rotation: i32,
        inherited: bool,
    ) -> Vec<u8> {
        let boxes = format!("/MediaBox {media_box} /CropBox {crop_box}");
        let (parent_boxes, page_boxes) = if inherited {
            (boxes, String::new())
        } else {
            (String::new(), boxes)
        };
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {parent_boxes} >>"),
            format!(
                "<< /Type /Page /Parent 2 0 R /Rotate {rotation} {page_boxes} /Resources << >> /Contents 4 0 R >>"
            ),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
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
    fn forward_words_match_tex_accents_in_pdf_text() {
        assert!(super::same_forward_word("Čech", "Cech"));
        assert!(super::same_forward_word("Cafe\u{301}", "café"));
        assert!(!super::same_forward_word("Cech", "Delaunay"));
    }

    #[test]
    fn clicked_unicode_maps_both_surrogate_halves_to_one_glyph() {
        let values = [0xe9, 0xd835, 0xdefc, b'z' as u32, 0x1d465];
        for (clicked, offset) in [0, 2, 2, 6, 7].into_iter().enumerate() {
            let (text, actual) =
                super::unicode_context(values.len(), clicked, |i| Ok(values[i])).unwrap();
            assert_eq!(text, "é𝛼z𝑥");
            assert_eq!(actual, offset);
        }
        for values in [[0xd835, 0x61], [0xdc00, 0x61], [0x110000, 0x61]] {
            assert!(super::unicode_context(values.len(), 0, |i| Ok(values[i])).is_err());
        }
    }

    #[test]
    fn clicked_unicode_neighborhood_never_splits_surrogate_pairs() {
        let mut values = vec![b'x' as u32; 260];
        values[..2].copy_from_slice(&[0xd835, 0xdefc]);
        let (text, offset) = super::unicode_context(values.len(), 97, |i| Ok(values[i])).unwrap();
        assert!(text.starts_with('𝛼'));
        assert_eq!(&text[offset..offset + 1], "x");
        values[..2].fill(b'x' as u32);
        values[192..194].copy_from_slice(&[0xd835, 0xdefc]);
        let (text, offset) = super::unicode_context(values.len(), 96, |i| Ok(values[i])).unwrap();
        assert!(text.ends_with('𝛼'));
        assert_eq!(&text[offset..offset + 1], "x");
    }

    use super::{
        DarkModeStyle, DarkModeTransform, FitMode, LinkTarget, PageLink, PageLinkRect, PixelRect,
        apply_dark_mode_link_contrast, apply_link_highlights, apply_selected_link_highlights,
        blend_highlight_rectangle, context_around_label, contrast_ratio, count_search_matches,
        dark_mode_pixel, darken_rgba, dominant_rectangle_color, mask_quadrilateral,
        normalize_search_text, reference_context, selected_page_link_rectangles,
    };

    const NEUTRAL_DARK_MODE: DarkModeStyle = DarkModeStyle::new([30, 30, 30], [209, 209, 209]);

    #[test]
    fn link_context_includes_surrounding_text() {
        let text = "Earlier work uses sparse retrieval. A realistic limitation appears in dense retrieval [23] under distribution shift. Later work follows.";

        let context = context_around_label(text, "[23]", 0, 40, 20).expect("context");

        assert!(context.contains("limitation appears in dense retrieval [23]"));
        assert!(context.starts_with('…'));
        assert!(context.ends_with('…'));
    }

    #[test]
    fn link_context_expands_truncated_edges_to_complete_words() {
        let text = "alpha object representation omega tail";

        let context = context_around_label(text, "representation", 0, 4, 2).expect("context");

        assert_eq!(context, "…object representation omega…");
    }

    #[test]
    fn reference_context_stops_before_the_next_numbered_reference() {
        let text = "[22] Prior et al. Prior work. [23] Weller et al. On theoretical limitations of retrieval. [24] Later et al. Later work.";

        let context = reference_context(text, "23").expect("reference context");

        assert!(context.starts_with("[23] Weller et al."), "{context:?}");
        assert!(!context.contains("[24]"));
    }

    #[test]
    fn point_forward_words_cross_runs_only_with_unique_complete_context() {
        let _native = super::pdfium_test_lock();
        use pdfium_render::prelude::{PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints};

        let pdfium = super::load_pdfium(None).unwrap();
        let mut document = pdfium.create_new_pdf().unwrap();
        let mut page = document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .unwrap();
        let courier = document.fonts_mut().courier();
        let helvetica = document.fonts_mut().helvetica();
        for (x, y, text, font) in [
            (20.0, 200.0, "Before café ", courier),
            (106.4, 200.0, "anchor after", helvetica),
            (20.0, 160.0, "Long context begins", courier),
            (20.0, 140.0, "with chosen words here", helvetica),
            (20.0, 100.0, "repeat target suffix", courier),
            (20.0, 60.0, "repeat target suffix", helvetica),
        ] {
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(x),
                    PdfPoints::new(y),
                    text,
                    font,
                    PdfPoints::new(12.0),
                )
                .unwrap();
        }
        drop(page);
        let page = document.pages().get(0).unwrap();
        let mut cache = super::empty_text_cache(1);
        let cached = super::cached_page_text(&document, 0, &mut cache).unwrap();
        let anchor = super::SearchRect {
            left: 20.0,
            right: 20.0,
            top: 200.0,
            bottom: 200.0,
        };
        let mut hint = crate::synctex::ForwardWord {
            words: ["Before", "Cafe\u{301}", "anchor", "after"]
                .map(String::from)
                .into(),
            selected: 2,
        };
        let rect = super::forward_word_rect(&page, cached, &hint, anchor)
            .unwrap()
            .unwrap();
        assert!(rect.left > 100.0 && rect.right < 155.0, "{rect:?}");
        assert!(rect.bottom >= 198.0 && rect.top < 215.0, "{rect:?}");

        // Matching only part of the context must not escape the anchored run.
        hint.words[1] = "missing".into();
        assert!(
            super::forward_word_rect(&page, cached, &hint, anchor)
                .unwrap()
                .is_none()
        );
        hint.words = ["anchor", "after"].map(String::from).into();
        hint.selected = 0;
        assert!(
            super::forward_word_rect(&page, cached, &hint, anchor)
                .unwrap()
                .is_none()
        );

        hint.words = ["context", "begins", "with", "chosen", "words", "here"]
            .map(String::from)
            .into();
        hint.selected = 3;
        let wrapped_anchor = super::SearchRect {
            top: 160.0,
            bottom: 160.0,
            ..anchor
        };
        let wrapped = super::forward_word_rect(&page, cached, &hint, wrapped_anchor)
            .unwrap()
            .unwrap();
        assert!(wrapped.left > 35.0 && wrapped.right < 100.0, "{wrapped:?}");
        assert!(
            wrapped.bottom >= 138.0 && wrapped.top < 155.0,
            "{wrapped:?}"
        );
        assert!(
            super::forward_word_rect(
                &page,
                cached,
                &hint,
                super::SearchRect {
                    right: 300.0,
                    top: 175.0,
                    ..wrapped_anchor
                },
            )
            .unwrap()
            .is_none(),
            "nonzero SyncTeX regions must not use page-wide context"
        );

        hint.words = ["repeat", "target", "suffix"].map(String::from).into();
        hint.selected = 1;
        assert!(
            super::forward_word_rect(
                &page,
                cached,
                &hint,
                super::SearchRect {
                    top: 100.0,
                    bottom: 100.0,
                    ..anchor
                },
            )
            .unwrap()
            .is_none(),
            "even complete context is ambiguous when repeated"
        );
    }

    #[test]
    fn point_forward_words_use_anchored_pdf_text_runs() {
        let _native = super::pdfium_test_lock();
        use pdfium_render::prelude::{PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints};

        let pdfium = super::load_pdfium(None).unwrap();
        let mut document = pdfium.create_new_pdf().unwrap();
        let mut page = document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .unwrap();
        let font = document.fonts_mut().courier();
        for (x, y, text) in [
            (20.0, 100.0, "alpha needle beta gamma needle delta"),
            (350.0, 100.0, "other needle beta elsewhere"),
            (20.0, 60.0, "distant needle beta elsewhere"),
            (20.0, 160.0, "café déjà"),
        ] {
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(x),
                    PdfPoints::new(y),
                    text,
                    font,
                    PdfPoints::new(12.0),
                )
                .unwrap();
        }
        drop(page);
        let page = document.pages().get(0).unwrap();
        let mut cache = super::empty_text_cache(1);
        let cached = super::cached_page_text(&document, 0, &mut cache).unwrap();
        let anchor = super::SearchRect {
            left: 20.0,
            right: 20.0,
            top: 100.0,
            bottom: 100.0,
        };
        let mut hint = crate::synctex::ForwardWord {
            words: ["alpha", "needle", "beta"].map(String::from).into(),
            selected: 1,
        };
        let first = super::forward_word_rect(&page, cached, &hint, anchor)
            .unwrap()
            .unwrap();
        assert!(first.left > 60.0 && first.right < 115.0, "{first:?}");
        assert!(first.bottom >= 98.0 && first.top < 115.0, "{first:?}");

        // The span's start can precede the selected word by several words.
        // Context selects the second occurrence without reaching other runs.
        hint.words = ["gamma", "needle", "delta"].map(String::from).into();
        let second = super::forward_word_rect(&page, cached, &hint, anchor)
            .unwrap()
            .unwrap();
        assert!(second.left > 190.0 && second.right < 245.0, "{second:?}");
        assert!(second.bottom >= 98.0 && second.top < 115.0, "{second:?}");

        hint.selected = 0;
        for word in ["needle", "need", "elsewhere"] {
            hint.words = vec![word.into()];
            assert!(
                super::forward_word_rect(&page, cached, &hint, anchor)
                    .unwrap()
                    .is_none(),
                "ambiguous, partial, or unrelated-run word {word:?}"
            );
        }
        hint.words = vec!["alpha".into()];
        hint.selected = 0;
        for point in [
            super::SearchRect {
                left: 5.0,
                right: 5.0,
                ..anchor
            },
            super::SearchRect {
                top: 200.0,
                bottom: 200.0,
                ..anchor
            },
            // Only a true point gets run-based refinement. A zero-width
            // SyncTeX region still uses the original word-center filter.
            super::SearchRect {
                top: 110.0,
                ..anchor
            },
        ] {
            assert!(
                super::forward_word_rect(&page, cached, &hint, point)
                    .unwrap()
                    .is_none()
            );
        }

        assert!(cached.normalized.contains("café déjà"));
        hint.words = ["Cafe\u{301}", "déjà"].map(String::from).into();
        hint.selected = 0;
        let unicode = super::forward_word_rect(
            &page,
            cached,
            &hint,
            super::SearchRect {
                top: 160.0,
                bottom: 160.0,
                ..anchor
            },
        )
        .unwrap()
        .unwrap();
        assert!(unicode.left >= 19.0 && unicode.right < 55.0, "{unicode:?}");
        assert!(unicode.bottom > 155.0 && unicode.top < 175.0, "{unicode:?}");
    }

    #[test]
    fn pdfium_image_mask_and_text_cache_work() {
        let _native = super::pdfium_test_lock();
        use super::{
            LinkTarget, apply_link_highlights, apply_search_highlights, cached_page_text,
            empty_text_cache, extract_document_links, extract_page_links, image_mask, load_pdfium,
            search_page,
        };
        use pdfium_render::prelude::{
            PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints, PdfRenderConfig,
        };

        let pdfium = load_pdfium(None).unwrap();
        fit_page_keeps_landscape_pages_landscape(&pdfium);
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
        let (occurrences, rectangles, context) =
            search_page(&text_document, 0, &mut cache, "synthetic needle");
        assert_eq!(occurrences, 2);
        assert!(!rectangles.is_empty());
        assert!(context.contains("Synthetic Needle"));

        let page = text_document.pages().get(0).expect("text page");
        let cached = cached_page_text(&text_document, 0, &mut cache).unwrap();
        let region = super::SearchRect {
            left: 0.0,
            right: 400.0,
            top: 100.0,
            bottom: 0.0,
        };
        let mut hint = crate::synctex::ForwardWord {
            words: vec!["needle".into()],
            selected: 0,
        };
        assert!(
            super::forward_word_rect(&page, cached, &hint, region)
                .unwrap()
                .is_none()
        );
        hint.words = ["synthetic", "needle", "synthetic", "needle"]
            .map(String::from)
            .into();
        let mut previous = 0.0;
        for selected in 0..4 {
            hint.selected = selected;
            let rect = super::forward_word_rect(&page, cached, &hint, region)
                .unwrap()
                .unwrap();
            assert!(rect.left > previous);
            assert!(rect.right - rect.left < 80.0, "word, not the complete line");
            previous = rect.left;
        }
        hint.words = vec!["needle".into()];
        hint.selected = 0;
        let right_half = super::SearchRect {
            left: 200.0,
            ..region
        };
        let rect = super::forward_word_rect(&page, cached, &hint, right_half)
            .unwrap()
            .unwrap();
        assert!(rect.left > 200.0);
        hint.words[0] = "need".into();
        assert!(
            super::forward_word_rect(&page, cached, &hint, region)
                .unwrap()
                .is_none()
        );
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
            .load_pdf_from_byte_vec(synthetic_link_pdf("XYZ null 300 null"), None)
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
                top_ratio: Some(ratio),
                left_ratio: None,
            } if (ratio - 0.25).abs() < f32::EPSILON
        )));
        assert!(links.iter().any(|link| {
            matches!(&link.target, LinkTarget::Uri(uri) if uri == "https://example.invalid/paper")
        }));

        let page_count = u32::try_from(link_document.pages().len()).unwrap();
        let mut link_text_cache = empty_text_cache(page_count);
        let document_links =
            extract_document_links(&link_document, &link_page, 6, &mut link_text_cache);
        assert_eq!(document_links.len(), 2);
        assert!(document_links.iter().all(|link| link.source_page == 6));
        assert!(
            document_links
                .iter()
                .all(|link| (0.8..=1.0).contains(&link.source_top_ratio))
        );
        assert_eq!(document_links[0].ordinal, 0);
        assert_eq!(document_links[1].ordinal, 1);

        let mut link_highlighted = link_bitmap.as_raw_bytes();
        let link_original = link_highlighted.clone();
        apply_link_highlights(
            &mut link_highlighted,
            link_width,
            link_height,
            &links,
            [0x86, 0xe1, 0xfc],
            false,
        );
        assert_ne!(link_highlighted, link_original);
    }

    fn fit_page_keeps_landscape_pages_landscape(pdfium: &pdfium_render::prelude::Pdfium) {
        use super::{FitMode, build_fit_config};
        use pdfium_render::prelude::{PdfPagePaperSize, PdfPoints, PdfRenderConfig};

        let mut document = pdfium.create_new_pdf().expect("create document");
        document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::from_points(
                PdfPoints::new(400.0),
                PdfPoints::new(300.0),
            ))
            .expect("create landscape page");
        let page = document.pages().get(0).expect("first page");
        let config = build_fit_config(PdfRenderConfig::new(), FitMode::Page, 400, 400);
        let bitmap = page.render_with_config(&config).expect("render page");
        assert!(
            bitmap.width() > bitmap.height(),
            "fit-page rendered a landscape page as {}x{}; expected landscape (width > height)",
            bitmap.width(),
            bitmap.height()
        );
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

    #[test]
    fn internal_links_preserve_optional_rendered_destinations() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        for (geometry, x_position, y_position) in [
            ("", (Some(0.25), None), (None, Some(0.25))),
            ("/Rotate 90", (None, Some(0.25)), (Some(0.75), None)),
            ("/Rotate 180", (Some(0.75), None), (None, Some(0.75))),
            ("/Rotate 270", (None, Some(0.75)), (Some(0.25), None)),
        ] {
            let full_position = (x_position.0.or(y_position.0), x_position.1.or(y_position.1));
            for (view, (left_ratio, top_ratio)) in [
                ("XYZ 100 300 null", full_position),
                ("XYZ 100 null null", x_position),
                ("XYZ null 300 null", y_position),
                ("XYZ null null null", (None, None)),
                ("FitH 300", y_position),
                ("FitBH 300", y_position),
                ("FitV 100", x_position),
                ("FitBV 100", x_position),
                ("FitR 100 100 200 300", full_position),
            ] {
                let document = pdfium
                    .load_pdf_from_byte_vec(synthetic_link_pdf_geometry(view, geometry), None)
                    .unwrap();
                let page = document.pages().get(0).unwrap();
                let link = page.links().get(0).unwrap();
                assert_eq!(
                    super::resolve_link_target(&document, &link),
                    Some(LinkTarget::Internal {
                        page: 1,
                        top_ratio,
                        left_ratio,
                    }),
                    "{geometry}: {view}",
                );
            }
        }
    }

    #[test]
    fn targets_follow_crop_origin_and_page_rotation() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        for (geometry, view, expected_left, expected_top, width, expected_bounds) in [
            (
                "/CropBox [100 0 400 400]",
                "XYZ 200 300 null",
                Some(1.0 / 3.0),
                Some(0.25),
                600,
                (200, 240),
            ),
            (
                "/MediaBox [100 0 500 400]",
                "XYZ 200 300 null",
                Some(0.25),
                Some(0.25),
                800,
                (200, 240),
            ),
            (
                "/CropBox [100 100 400 400]",
                "XYZ 200 300 null",
                Some(1.0 / 3.0),
                Some(1.0 / 3.0),
                600,
                (200, 240),
            ),
            (
                "/MediaBox [100 100 500 500]",
                "XYZ 200 300 null",
                Some(0.25),
                Some(0.5),
                800,
                (200, 240),
            ),
            (
                "/Rotate 90",
                "XYZ null 300 null",
                Some(0.75),
                None,
                800,
                (200, 240),
            ),
            (
                "/Rotate 90",
                "XYZ 200 null null",
                None,
                Some(0.5),
                800,
                (200, 240),
            ),
            (
                "/Rotate 90 /CropBox [100 100 400 400]",
                "XYZ 200 300 null",
                Some(2.0 / 3.0),
                Some(1.0 / 3.0),
                600,
                (0, 40),
            ),
            (
                "/Rotate 180",
                "XYZ 200 300 null",
                Some(0.5),
                Some(0.75),
                800,
                (360, 400),
            ),
            (
                "/Rotate 180 /CropBox [100 100 400 400]",
                "XYZ 200 300 null",
                Some(2.0 / 3.0),
                Some(2.0 / 3.0),
                600,
                (360, 400),
            ),
            (
                "/Rotate 270",
                "XYZ 200 300 null",
                Some(0.25),
                Some(0.5),
                800,
                (560, 600),
            ),
            (
                "/Rotate 270 /MediaBox [100 100 500 500]",
                "XYZ 200 300 null",
                Some(0.5),
                Some(0.75),
                800,
                (760, 800),
            ),
        ] {
            let document = pdfium
                .load_pdf_from_byte_vec(synthetic_link_pdf_geometry(view, geometry), None)
                .unwrap();
            let source = document.pages().get(0).unwrap();
            let link = source.links().get(0).unwrap();
            let Some(LinkTarget::Internal {
                left_ratio,
                top_ratio,
                ..
            }) = super::resolve_link_target(&document, &link)
            else {
                panic!("missing internal destination: {geometry}");
            };
            for (axis, actual, expected) in [
                ("horizontal", left_ratio, expected_left),
                ("vertical", top_ratio, expected_top),
            ] {
                match (actual, expected) {
                    (Some(actual), Some(expected)) => assert!(
                        (actual - expected).abs() < 0.0001,
                        "{geometry}: {view}: {axis}: {actual} != {expected}"
                    ),
                    (None, None) => {}
                    _ => panic!("wrong optional {axis} position: {geometry}: {view}: {actual:?}"),
                }
            }
            let target = document.pages().get(1).unwrap();
            let config = pdfium_render::prelude::PdfRenderConfig::new().set_target_width(width);
            let (left, right, _, _) = super::page_rect_pixel_bounds(
                &target,
                &config,
                super::SearchRect {
                    left: 200.0,
                    right: 220.0,
                    top: 100.0,
                    bottom: 120.0,
                },
            )
            .unwrap();
            assert_eq!((left, right), expected_bounds, "{geometry}");
        }
    }

    #[test]
    fn optional_destination_axes_survive_subpixel_coordinate_changes() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        for (rotation, expected_left, expected_top) in
            [(0, None, Some(0.25)), (90, Some(0.75), None)]
        {
            let geometry =
                format!("/MediaBox [0 0 14400 400] /CropBox [.4 0 14400 400] /Rotate {rotation}");
            let document = pdfium
                .load_pdf_from_byte_vec(
                    synthetic_link_pdf_geometry("XYZ null 300 null", &geometry),
                    None,
                )
                .unwrap();
            let source = document.pages().get(0).unwrap();
            let link = source.links().get(0).unwrap();
            assert_eq!(
                super::resolve_link_target(&document, &link),
                Some(LinkTarget::Internal {
                    page: 1,
                    left_ratio: expected_left,
                    top_ratio: expected_top,
                }),
                "{geometry}"
            );
        }
    }

    fn synthetic_link_pdf(destination: &str) -> Vec<u8> {
        synthetic_link_pdf_geometry(destination, "")
    }

    fn synthetic_link_pdf_geometry(destination: &str, geometry: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] /Annots [7 0 R 8 0 R] /Contents 5 0 R >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] {geometry} /Contents 6 0 R >>"),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
            format!("<< /Type /Annot /Subtype /Link /Rect [10 10 80 30] /Border [0 0 0] /Dest [4 0 R /{destination}] >>"),
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
    fn dark_mode_recolors_blue_link_ink_against_its_local_background() {
        let style = DarkModeStyle::new([30, 32, 48], [200, 211, 245]);
        let accent = [134, 225, 252];
        let background = [22, 42, 80];
        let edge = [65, 101, 161];
        let ink = [106, 153, 233];
        let mut pixels = [
            background[0],
            background[1],
            background[2],
            255,
            background[0],
            background[1],
            background[2],
            255,
            edge[0],
            edge[1],
            edge[2],
            128,
            ink[0],
            ink[1],
            ink[2],
            64,
            background[0],
            background[1],
            background[2],
            255,
        ];
        let rectangle = PageLinkRect {
            left: 0,
            top: 0,
            right: 4,
            bottom: 1,
        };

        assert_eq!(
            dominant_rectangle_color(&pixels, 5, 1, rectangle),
            Some(background)
        );
        apply_dark_mode_link_contrast(&mut pixels, 5, 1, &[rectangle], style, accent);
        assert_eq!(&pixels[..8], &[22, 42, 80, 255, 22, 42, 80, 255]);
        assert_ne!(&pixels[8..11], &edge);
        assert_eq!(pixels[11], 128);
        assert_eq!(&pixels[12..15], &style.foreground);
        assert_eq!(pixels[15], 64);
        assert_eq!(&pixels[16..], &[22, 42, 80, 255]);
        assert!(contrast_ratio(style.foreground, background) >= 4.5);
    }

    #[test]
    fn dark_mode_link_highlight_uses_a_subtle_fill_and_underline() {
        let color = [134, 225, 252];
        let link = PageLink {
            rect: PageLinkRect {
                left: 1,
                top: 1,
                right: 5,
                bottom: 4,
            },
            label: "reader@example.invalid".into(),
            target: LinkTarget::Uri("mailto:reader@example.invalid".into()),
        };
        let mut pixels = [30, 32, 48, 255].repeat(6 * 5);

        apply_link_highlights(&mut pixels, 6, 5, &[link], color, true);

        let fill = &pixels[(6 + 2) * 4..(6 + 2) * 4 + 3];
        let underline = &pixels[(3 * 6 + 2) * 4..(3 * 6 + 2) * 4 + 3];
        assert_eq!(fill, &[30, 32, 48]);
        assert_eq!(underline, &[69, 104, 124]);
        assert_eq!(&pixels[..4], &[30, 32, 48, 255]);
    }

    #[test]
    fn selected_link_highlight_is_stronger_than_the_link_mode_highlight() {
        let color = [134, 225, 252];
        let rectangle = PageLinkRect {
            left: 1,
            top: 1,
            right: 6,
            bottom: 5,
        };
        let link = PageLink {
            rect: rectangle,
            label: "selected".into(),
            target: LinkTarget::Uri("https://example.invalid/selected".into()),
        };
        let mut regular = [255, 255, 255, 255].repeat(8 * 7);
        let mut selected = regular.clone();

        apply_link_highlights(&mut regular, 8, 7, &[link], color, false);
        apply_selected_link_highlights(&mut selected, 8, 7, &[rectangle], color);

        let interior = (3 * 8 + 3) * 4;
        assert!(selected[interior] < regular[interior]);
        let border = (2 * 8 + 1) * 4;
        assert_eq!(&selected[border..border + 3], &color);
    }

    #[test]
    fn selected_link_ordinal_includes_every_wrapped_fragment() {
        let shared_target = LinkTarget::Internal {
            page: 7,
            top_ratio: None,
            left_ratio: None,
        };
        let links = vec![
            PageLink {
                rect: PageLinkRect {
                    left: 40,
                    top: 10,
                    right: 80,
                    bottom: 20,
                },
                label: "wrapped".into(),
                target: shared_target.clone(),
            },
            PageLink {
                rect: PageLinkRect {
                    left: 10,
                    top: 22,
                    right: 35,
                    bottom: 32,
                },
                label: "link".into(),
                target: shared_target,
            },
            PageLink {
                rect: PageLinkRect {
                    left: 10,
                    top: 50,
                    right: 30,
                    bottom: 60,
                },
                label: "other".into(),
                target: LinkTarget::Uri("https://example.invalid/other".into()),
            },
        ];

        assert_eq!(
            selected_page_link_rectangles(&links, Some(0)),
            vec![links[0].rect, links[1].rect]
        );
        assert_eq!(
            selected_page_link_rectangles(&links, Some(1)),
            vec![links[2].rect]
        );
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
    fn links_are_sorted_top_to_bottom_then_left_to_right() {
        use super::{LinkTarget, PageLink, PageLinkRect, sort_page_links_reading_order};

        let mut links = [(40, 20), (10, 80), (10, 5)]
            .into_iter()
            .map(|(top, left)| PageLink {
                rect: PageLinkRect {
                    left,
                    top,
                    right: left + 5,
                    bottom: top + 5,
                },
                label: format!("{top}:{left}"),
                target: LinkTarget::Internal {
                    page: 0,
                    top_ratio: None,
                    left_ratio: None,
                },
            })
            .collect::<Vec<_>>();

        sort_page_links_reading_order(&mut links);

        assert_eq!(
            links
                .iter()
                .map(|link| link.label.as_str())
                .collect::<Vec<_>>(),
            ["10:5", "10:80", "40:20"]
        );
    }

    #[test]
    fn adjacent_link_fragments_with_the_same_target_are_coalesced() {
        use super::{LinkTarget, PageLink, PageLinkRect, coalesce_document_link_fragments};

        let target = LinkTarget::Internal {
            page: 12,
            top_ratio: Some(0.5),
            left_ratio: None,
        };
        let interleaved_target = LinkTarget::Internal {
            page: 13,
            top_ratio: Some(0.25),
            left_ratio: None,
        };
        let links = vec![
            PageLink {
                rect: PageLinkRect {
                    left: 200,
                    top: 10,
                    right: 300,
                    bottom: 30,
                },
                label: "(Spelke et al.,".into(),
                target: target.clone(),
            },
            PageLink {
                rect: PageLinkRect {
                    left: 200,
                    top: 20,
                    right: 300,
                    bottom: 40,
                },
                label: "(Wiskott and Se".into(),
                target: interleaved_target.clone(),
            },
            PageLink {
                rect: PageLinkRect {
                    left: 10,
                    top: 31,
                    right: 50,
                    bottom: 51,
                },
                label: "1995)".into(),
                target: target.clone(),
            },
            PageLink {
                rect: PageLinkRect {
                    left: 10,
                    top: 41,
                    right: 80,
                    bottom: 61,
                },
                label: "jnowski, 2002)".into(),
                target: interleaved_target,
            },
            PageLink {
                rect: PageLinkRect {
                    left: 10,
                    top: 100,
                    right: 60,
                    bottom: 120,
                },
                label: "later mention".into(),
                target,
            },
        ];

        let source_text = "(Spelke et al., 1995) and (Wiskott and Sejnowski, 2002)";
        let links = coalesce_document_link_fragments(links, Some(source_text));

        assert_eq!(links.len(), 3);
        assert_eq!(links[0].label, "(Spelke et al., 1995)");
        assert_eq!(links[1].label, "(Wiskott and Sejnowski, 2002)");
        assert_eq!(links[2].label, "later mention");
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

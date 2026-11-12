use std::cell::RefCell;
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
    PdfMatrix, PdfPage, PdfPageObject, PdfPageObjectCommon, PdfPageObjectsCommon, PdfPageText,
    PdfPageTextChar, PdfPageTextChars, PdfPageTextRenderMode, PdfPoints, PdfRect, PdfRenderConfig,
    Pdfium, PdfiumError,
};

use crate::synctex::{
    DocumentRevision, ForwardCandidate, ForwardWord, is_spacing_diacritic, words,
};
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    /// Fit the whole page within the viewport (no scrolling).
    #[default]
    Page,
    /// Match the page width to the viewport; scroll vertically when taller.
    Width,
    /// Match the page height to the viewport; scroll horizontally when wider.
    Height,
    /// Fit the content bounds, leaving page margins outside the viewport.
    Content,
}

impl FitMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Page => Self::Width,
            Self::Width => Self::Height,
            Self::Height => Self::Content,
            Self::Content => Self::Page,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Page => "fit-page",
            Self::Width => "fit-width",
            Self::Height => "fit-height",
            Self::Content => "fit-content",
        }
    }

    pub(crate) fn raster_size(
        self,
        width: f32,
        height: f32,
        content: [f32; 4],
        target_width: u32,
        target_height: u32,
    ) -> (u32, u32) {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return (1, 1);
        }
        let mut scale = match self {
            Self::Page | Self::Width => target_width as f32 / width,
            Self::Height => target_height as f32 / height,
            Self::Content => {
                // Reserve a little breathing room without changing page geometry.
                const INNER_FRACTION: f32 = 0.96;
                (target_width as f32 * INNER_FRACTION / (width * (content[2] - content[0]))).min(
                    target_height as f32 * INNER_FRACTION / (height * (content[3] - content[1])),
                )
            }
        };
        if self == Self::Page && height * scale > target_height as f32 {
            scale = target_height as f32 / height;
        }
        const MAX_PIXELS: u64 = 16_777_216;
        const MAX_DIMENSION: u32 = 32_768;
        scale = scale
            .min((MAX_PIXELS as f32 / (width * height)).sqrt())
            .min(MAX_DIMENSION as f32 / width.max(height));
        let pixels = |size: f32| {
            (if self == Self::Content {
                size.floor()
            } else {
                size.round()
            })
            .clamp(1.0, MAX_DIMENSION as f32) as u32
        };
        let (mut raster_width, mut raster_height) = (pixels(width * scale), pixels(height * scale));
        if u64::from(raster_width) * u64::from(raster_height) > MAX_PIXELS {
            raster_width = (width * scale).floor().clamp(1.0, MAX_DIMENSION as f32) as u32;
            raster_height = (height * scale).floor().clamp(1.0, MAX_DIMENSION as f32) as u32;
        }
        (raster_width, raster_height)
    }
}

// Share geometry between rendering and inverse search. Do not use PDFium's
// scale_page_to_display_size(): it rotates landscape pages 90 degrees.
fn build_fit_config(
    base_config: PdfRenderConfig,
    fit: FitMode,
    target_width: i32,
    target_height: i32,
    page: &PdfPage,
    content: [f32; 4],
) -> PdfRenderConfig {
    let (width, height) = fit.raster_size(
        page.width().value,
        page.height().value,
        content,
        target_width.max(1) as u32,
        target_height.max(1) as u32,
    );
    base_config.set_target_size(width as i32, height as i32)
}

fn frame_render_config(
    key: RenderKey,
    page: &PdfPage,
    geometry: &mut CachedPageGeometry,
) -> (PdfRenderConfig, [f32; 4]) {
    let zoom = i32::from(key.zoom.max(1));
    let target_width = (i32::from(key.width) * zoom / 100).max(1);
    let target_height = (i32::from(key.height) * zoom / 100).max(1);
    let content = if key.fit == FitMode::Content {
        cached_page_content_bounds(page, geometry)
    } else {
        [0.0, 0.0, 1.0, 1.0]
    };
    let config = build_fit_config(
        base_render_config(),
        key.fit,
        target_width,
        target_height,
        page,
        content,
    );
    (config, content)
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
    /// Content bounds as fractions of the displayed, rotated page: left/top/right/bottom.
    pub content_bounds: [f32; 4],
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
        Self::for_page_with_bounds(page).map(|(coordinates, _)| coordinates)
    }

    fn for_page_with_bounds(page: &PdfPage) -> Result<(Self, PdfRect), String> {
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
        Ok((
            Self {
                height: media.height().value,
            },
            media,
        ))
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
        revision: DocumentRevision,
    },
    Opened {
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: DocumentRevision,
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
    RenderError {
        key: RenderKey,
        generation: u64,
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
    Command(WorkerCommand),
    IndexLinkPage(LinkIndexJob),
    SearchPage(SearchJob),
    Render(RenderRequest, bool),
}

type TexFontCache = HashMap<String, [Option<bool>; 128]>;

struct LoadedDocument<'a> {
    document: PdfDocument<'a>,
    revision: DocumentRevision,
    text_cache: Vec<Option<CachedPageText>>,
    font_cache: TexFontCache,
    page_cache: HashMap<u32, CachedPageGeometry>,
    search_highlights: Option<SearchHighlights>,
    flash_highlight: Option<(u32, ForwardHighlight)>,
}

impl<'a> LoadedDocument<'a> {
    fn new(document: PdfDocument<'a>, revision: DocumentRevision, pages: u32) -> Self {
        Self {
            document,
            revision,
            text_cache: empty_text_cache(pages),
            font_cache: HashMap::new(),
            page_cache: HashMap::new(),
            search_highlights: None,
            flash_highlight: None,
        }
    }
}

struct CachedBaseRaster {
    key: RenderKey,
    revision: DocumentRevision,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl CachedBaseRaster {
    fn matches(&self, key: RenderKey, revision: DocumentRevision) -> bool {
        self.revision == revision
            && (
                self.key.document_id,
                self.key.page,
                self.key.width,
                self.key.height,
                self.key.zoom,
                self.key.fit,
                self.key.invert,
            ) == (
                key.document_id,
                key.page,
                key.width,
                key.height,
                key.zoom,
                key.fit,
                key.invert,
            )
            && (!key.invert || self.key.dark_mode_style == key.dark_mode_style)
    }
}

struct SearchJob {
    document_id: DocumentId,
    request_id: u64,
    needle: String,
    next_page: u32,
    total_pages: u32,
    pending_matches: Vec<SearchPageMatch>,
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

pub use synctex_parser::SearchRect;

pub type PixelRect = PageLinkRect;

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleMatch {
    pub key: RenderKey,
    pub char_index: usize,
    pub rects: Vec<PixelRect>,
    pub hit: (u32, u32),
    pub anchor: (u32, u32),
    pub next_char: Option<char>,
}
#[derive(Clone, Debug, Default)]
struct CachedPageText {
    raw: String,
    normalized: String,
    source_index_by_byte: Vec<usize>,
    full_text_initialized: bool,
    visible: Option<VisiblePageText>,
}

#[derive(Clone, Debug)]
struct VisiblePageText {
    normalized: String,
    source_index_by_byte: Vec<usize>,
    glyphs: Vec<VisibleGlyph>,
}

#[derive(Clone, Debug)]
struct VisibleGlyph {
    source_index: usize,
    value: char,
    bounds: [Option<PdfRect>; 2],
}

#[derive(Default)]
struct CachedPageGeometry {
    content_bounds: Option<[f32; 4]>,
    links: Option<Vec<CachedPageLink>>,
}

struct CachedPageLink {
    bounds: PdfRect,
    target: Option<LinkTarget>,
}
struct WorkerChannels {
    priority_rx: Receiver<RenderRequest>,
    prefetch_rx: Receiver<RenderRequest>,
    command_rx: Receiver<WorkerCommand>,
    message_tx: Sender<WorkerMessage>,
}

struct WorkerState {
    latest_generation: AtomicU64,
    min_valid_generation: AtomicU64,
    prefetch_bounds: AtomicU64,
    visible_generation: AtomicU64,
}

pub struct RenderWorker {
    priority_tx: Sender<RenderRequest>,
    prefetch_tx: Sender<RenderRequest>,
    command_tx: Sender<WorkerCommand>,
    message_rx: Receiver<WorkerMessage>,
    state: Arc<WorkerState>,
}

impl RenderWorker {
    pub fn spawn(document_id: DocumentId, path: PathBuf, pdfium_library: Option<PathBuf>) -> Self {
        let (priority_tx, priority_rx) = unbounded();
        let (prefetch_tx, prefetch_rx) = unbounded();
        let (command_tx, command_rx) = unbounded();
        let (message_tx, message_rx) = unbounded();
        let state = Arc::new(WorkerState {
            latest_generation: AtomicU64::new(0),
            min_valid_generation: AtomicU64::new(0),
            prefetch_bounds: AtomicU64::new(u64::from(u32::MAX) << 32),
            visible_generation: AtomicU64::new(0),
        });
        let worker_state = Arc::clone(&state);

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
                worker_state,
            );
        });

        Self {
            priority_tx,
            prefetch_tx,
            command_tx,
            message_rx,
            state,
        }
    }

    pub fn wait_until_ready(&self) -> Result<(u32, Vec<OutlineItem>, DocumentRevision), String> {
        match self.message_rx.recv() {
            Ok(WorkerMessage::Ready {
                pages,
                outline,
                revision,
            }) => Ok((pages, outline, revision)),
            Ok(WorkerMessage::Error(error)) => Err(error),
            Ok(_) => Err("renderer sent a frame before initialization".into()),
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
        self.state
            .latest_generation
            .store(generation, Ordering::Release);
    }

    pub fn invalidate_view(&self, min_valid_generation: u64) {
        self.state
            .min_valid_generation
            .store(min_valid_generation, Ordering::Release);
    }

    pub fn set_view_state(&self, min_valid_generation: u64, min_page: u32, max_page: u32) {
        self.invalidate_view(min_valid_generation);
        self.state.prefetch_bounds.store(
            u64::from(min_page) | (u64::from(max_page) << 32),
            Ordering::Release,
        );
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
            .state
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
        self.state.visible_generation.fetch_add(1, Ordering::AcqRel);
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

fn base_render_config() -> PdfRenderConfig {
    PdfRenderConfig::new()
        .set_reverse_byte_order(true)
        .use_lcd_text_rendering(true)
        .force_half_tone(false)
        .use_print_quality(false)
}

fn open_pdf_document<'a>(
    pdfium: &'a Pdfium,
    path: &Path,
    wrap_revision_error: bool,
) -> Result<(PdfDocument<'a>, DocumentRevision, u32, Vec<OutlineItem>), String> {
    let wrap = |error: String| format!("could not open {}: {error}", path.display());
    let map_rev = |error: std::io::Error| {
        let text = error.to_string();
        if wrap_revision_error {
            wrap(text)
        } else {
            text
        }
    };
    let revision = DocumentRevision::read(path).map_err(map_rev)?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|error| wrap(error.to_string()))?;
    let pages = u32::try_from(document.pages().len())
        .map_err(|_| "PDFium returned a negative page count".to_string())?;
    if pages == 0 {
        return Err(format!("{} has no pages", path.display()));
    }
    let outline = extract_outline(&document);
    revision.check(path).map_err(map_rev)?;
    Ok((document, revision, pages, outline))
}

fn run_worker(
    path: PathBuf,
    initial_document_id: DocumentId,
    pdfium_library: Option<&Path>,
    channels: WorkerChannels,
    state: Arc<WorkerState>,
) {
    let WorkerChannels {
        priority_rx,
        prefetch_rx,
        command_rx,
        message_tx,
    } = channels;
    let send = |msg: WorkerMessage| {
        message_tx
            .send(msg)
            .map_err(|_| "viewer stopped".to_string())
    };
    let result = (|| -> Result<(), String> {
        let pdfium = load_pdfium(pdfium_library)?;
        let (document, revision, pages, outline) = open_pdf_document(&pdfium, &path, false)?;
        send(WorkerMessage::Ready {
            pages,
            outline,
            revision,
        })?;
        let mut documents = HashMap::from([(
            initial_document_id,
            LoadedDocument::new(document, revision, pages),
        )]);
        let mut search_jobs: VecDeque<SearchJob> = VecDeque::new();
        let mut link_index_jobs: VecDeque<LinkIndexJob> = VecDeque::new();
        let mut deferred_render: Option<(RenderRequest, bool)> = None;
        let mut base_cache: Option<CachedBaseRaster> = None;

        let is_current = |req: &RenderRequest, is_prefetch: bool| -> bool {
            if is_prefetch {
                if req.generation < state.min_valid_generation.load(Ordering::Acquire) {
                    return false;
                }
                let bounds = state.prefetch_bounds.load(Ordering::Acquire);
                req.key.page >= bounds as u32 && req.key.page <= (bounds >> 32) as u32
            } else {
                req.generation == state.latest_generation.load(Ordering::Acquire)
            }
        };

        loop {
            let task = if let Ok(command) = command_rx.try_recv() {
                WorkerTask::Command(command)
            } else if let Some((req, is_prefetch)) = deferred_render.take() {
                WorkerTask::Render(req, is_prefetch)
            } else {
                match priority_rx.try_recv() {
                    Ok(req) => WorkerTask::Render(req, false),
                    Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {
                        if let Some(job) = link_index_jobs.pop_front() {
                            WorkerTask::IndexLinkPage(job)
                        } else if let Some(job) = search_jobs.pop_front() {
                            WorkerTask::SearchPage(job)
                        } else if let Ok(req) = prefetch_rx.try_recv() {
                            WorkerTask::Render(req, true)
                        } else {
                            select_biased! {
                                recv(command_rx) -> cmd => match cmd {
                                    Ok(c) => WorkerTask::Command(c),
                                    Err(_) => break,
                                },
                                recv(priority_rx) -> req => match req {
                                    Ok(r) => WorkerTask::Render(r, false),
                                    Err(_) => break,
                                },
                                recv(prefetch_rx) -> req => match req {
                                    Ok(r) => WorkerTask::Render(r, true),
                                    Err(_) => break,
                                },
                            }
                        }
                    }
                }
            };

            let (request, is_prefetch) = match task {
                WorkerTask::Command(WorkerCommand::Open {
                    document_id,
                    path: new_path,
                }) => {
                    match open_pdf_document(&pdfium, &new_path, true) {
                        Ok((replacement, revision, pages, outline)) => {
                            documents.insert(
                                document_id,
                                LoadedDocument::new(replacement, revision, pages),
                            );
                            search_jobs.retain(|job| job.document_id != document_id);
                            link_index_jobs.retain(|job| job.document_id != document_id);
                            if base_cache
                                .as_ref()
                                .is_some_and(|c| c.key.document_id == document_id)
                            {
                                base_cache = None;
                            }
                            send(WorkerMessage::Opened {
                                document_id,
                                pages,
                                outline,
                                revision,
                            })?;
                        }
                        Err(error) => send(WorkerMessage::OpenError { document_id, error })?,
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::Close(document_id)) => {
                    documents.remove(&document_id);
                    search_jobs.retain(|job| job.document_id != document_id);
                    link_index_jobs.retain(|job| job.document_id != document_id);
                    if base_cache
                        .as_ref()
                        .is_some_and(|c| c.key.document_id == document_id)
                    {
                        base_cache = None;
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::ExtractText { document_id, page }) => {
                    if let Some(loaded) = documents.get_mut(&document_id) {
                        let content = cached_page_text(
                            &loaded.document,
                            page,
                            &mut loaded.text_cache,
                            &mut loaded.font_cache,
                        )
                        .map_or_else(String::new, |text| text.raw.clone());
                        send(WorkerMessage::Text {
                            document_id,
                            page,
                            content,
                        })?;
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::VisibleMatches {
                    document_id,
                    request_id,
                    revision,
                    query,
                    keys,
                    generation,
                }) => {
                    if generation != state.visible_generation.load(Ordering::Acquire) {
                        continue;
                    }
                    let Some(loaded) = documents.get_mut(&document_id) else {
                        send(WorkerMessage::VisibleMatchesError {
                            document_id,
                            request_id,
                            revision,
                            error: "visible-match document is closed".into(),
                        })?;
                        continue;
                    };
                    if loaded.revision != revision {
                        send(WorkerMessage::VisibleMatches {
                            document_id,
                            request_id,
                            revision,
                            matches: Vec::new(),
                        })?;
                        continue;
                    }
                    let result = find_visible_matches(
                        &loaded.document,
                        document_id,
                        &query,
                        &keys,
                        &mut loaded.text_cache,
                        &mut loaded.page_cache,
                        &mut loaded.font_cache,
                        generation,
                        &state.visible_generation,
                    );
                    if generation != state.visible_generation.load(Ordering::Acquire) {
                        continue;
                    }
                    match result {
                        Ok(Some(matches)) => send(WorkerMessage::VisibleMatches {
                            document_id,
                            request_id,
                            revision,
                            matches,
                        })?,
                        Ok(None) => {}
                        Err(error) => send(WorkerMessage::VisibleMatchesError {
                            document_id,
                            request_id,
                            revision,
                            error,
                        })?,
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::PagePoint {
                    document_id,
                    page,
                    request_id,
                    x,
                    y,
                    key,
                    revision,
                }) => {
                    let result =
                        (|| -> Result<ResolvedClick, String> {
                            let loaded = documents
                                .get_mut(&document_id)
                                .ok_or("hit-test document is closed")?;
                            if loaded.revision != revision {
                                return Err("hit-test document revision is no longer loaded".into());
                            }
                            let page_index = i32::try_from(page).map_err(|e| e.to_string())?;
                            let rendered = loaded
                                .document
                                .pages()
                                .get(page_index)
                                .map_err(|e| e.to_string())?;
                            let (config, _) = frame_render_config(
                                key,
                                &rendered,
                                loaded.page_cache.entry(page).or_default(),
                            );
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
                                text: clicked_text(
                                    &rendered,
                                    pdf_x.value,
                                    pdf_y.value,
                                    &mut loaded.font_cache,
                                ),
                            })
                        })();
                    send(WorkerMessage::PagePoint {
                        document_id,
                        page,
                        request_id,
                        revision,
                        result,
                    })?;
                    continue;
                }
                WorkerTask::Command(WorkerCommand::Flash {
                    document_id,
                    page,
                    rect,
                    word,
                }) => {
                    let Some(loaded) = documents.get_mut(&document_id) else {
                        continue;
                    };
                    let result = (|| -> Result<(SearchRect, bool), String> {
                        let page_index = i32::try_from(page).map_err(|_| {
                            format!("page {} exceeds PDFium's index range", page + 1)
                        })?;
                        let target = loaded
                            .document
                            .pages()
                            .get(page_index)
                            .map_err(|e| e.to_string())?;
                        let coarse = SourcePageCoordinates::for_page(&target)
                            .and_then(|coordinates| coordinates.pdf_rect(rect))?;
                        let Some(word) = word else {
                            return Ok((coarse, false));
                        };
                        let cached = cached_page_text(
                            &loaded.document,
                            page,
                            &mut loaded.text_cache,
                            &mut loaded.font_cache,
                        )
                        .ok_or("could not extract PDF text for word highlighting")?;
                        let refined = forward_word_rect(
                            &target,
                            cached,
                            &word,
                            coarse,
                            &mut loaded.font_cache,
                        )?;
                        Ok((refined.unwrap_or(coarse), refined.is_some()))
                    })();
                    let (rect, word_precise, error) = match result {
                        Ok((rect, precise)) => (rect, precise, None),
                        Err(error) => (rect, false, Some(Arc::<str>::from(error))),
                    };
                    loaded.flash_highlight = Some((
                        page,
                        ForwardHighlight {
                            rect,
                            pixel_bounds: None,
                            word_precise,
                            error,
                        },
                    ));
                    continue;
                }
                WorkerTask::Command(WorkerCommand::ClearFlash { document_id }) => {
                    if let Some(loaded) = documents.get_mut(&document_id) {
                        loaded.flash_highlight = None;
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::Search {
                    document_id,
                    request_id,
                    query,
                }) => {
                    search_jobs.retain(|job| job.document_id != document_id);
                    let needle = normalize_search_text(&query);
                    if let Some(loaded) = documents.get_mut(&document_id) {
                        loaded.search_highlights = None;
                        if !needle.is_empty() {
                            let total_pages =
                                u32::try_from(loaded.text_cache.len()).unwrap_or(u32::MAX);
                            search_jobs.push_front(SearchJob {
                                document_id,
                                request_id,
                                needle,
                                next_page: 0,
                                total_pages,
                                pending_matches: Vec::new(),
                                total_occurrences: 0,
                                highlights: HashMap::new(),
                                last_progress: Instant::now(),
                            });
                            send(WorkerMessage::SearchProgress {
                                document_id,
                                request_id,
                                scanned: 0,
                                total: total_pages,
                                matches: Vec::new(),
                                total_occurrences: 0,
                            })?;
                        }
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::CancelSearch {
                    document_id,
                    request_id,
                }) => {
                    search_jobs.retain(|job| {
                        job.document_id != document_id || job.request_id != request_id
                    });
                    if let Some(loaded) = documents.get_mut(&document_id)
                        && loaded
                            .search_highlights
                            .as_ref()
                            .is_some_and(|highlights| highlights.request_id == request_id)
                    {
                        loaded.search_highlights = None;
                    }
                    continue;
                }
                WorkerTask::Command(WorkerCommand::IndexLinks {
                    document_id,
                    request_id,
                }) => {
                    link_index_jobs.retain(|job| job.document_id != document_id);
                    let Some(loaded) = documents.get(&document_id) else {
                        continue;
                    };
                    let total_pages =
                        u32::try_from(loaded.document.pages().len()).unwrap_or(u32::MAX);
                    send(WorkerMessage::LinkIndexProgress {
                        document_id,
                        request_id,
                        links: Vec::new(),
                        scanned: 0,
                        total: total_pages,
                        complete: total_pages == 0,
                    })?;
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
                    let Some(loaded) = documents.get_mut(&job.document_id) else {
                        continue;
                    };
                    if let Ok(page_index) = i32::try_from(job.next_page)
                        && let Ok(page) = loaded.document.pages().get(page_index)
                    {
                        let annotations = loaded
                            .page_cache
                            .entry(job.next_page)
                            .or_default()
                            .links
                            .get_or_insert_with(|| {
                                extract_page_link_annotations(&loaded.document, &page)
                            });
                        job.pending_links
                            .extend(extract_document_links_with_annotations(
                                &loaded.document,
                                &page,
                                job.next_page,
                                &mut loaded.text_cache,
                                &mut loaded.font_cache,
                                annotations,
                            ));
                    }
                    job.next_page = job.next_page.saturating_add(1);
                    let complete = job.next_page >= job.total_pages;
                    if complete || job.last_progress.elapsed() >= LINK_INDEX_PROGRESS_INTERVAL {
                        send(WorkerMessage::LinkIndexProgress {
                            document_id: job.document_id,
                            request_id: job.request_id,
                            links: std::mem::take(&mut job.pending_links),
                            scanned: job.next_page,
                            total: job.total_pages,
                            complete,
                        })?;
                        job.last_progress = Instant::now();
                    }
                    if !complete {
                        link_index_jobs.push_back(job);
                    }
                    continue;
                }
                WorkerTask::SearchPage(mut job) => {
                    let Some(loaded) = documents.get_mut(&job.document_id) else {
                        continue;
                    };
                    let (occurrences, rectangles, context) = search_page(
                        &loaded.document,
                        job.next_page,
                        &mut loaded.text_cache,
                        &mut loaded.font_cache,
                        &job.needle,
                    );
                    if occurrences > 0 {
                        job.pending_matches.push(SearchPageMatch {
                            page: job.next_page,
                            occurrences,
                            context,
                        });
                        job.total_occurrences = job.total_occurrences.saturating_add(occurrences);
                        job.highlights.insert(job.next_page, rectangles);
                    }
                    job.next_page = job.next_page.saturating_add(1);

                    if job.next_page >= job.total_pages {
                        loaded.search_highlights = Some(SearchHighlights {
                            request_id: job.request_id,
                            pages: job.highlights,
                        });
                        send(WorkerMessage::SearchResults {
                            document_id: job.document_id,
                            request_id: job.request_id,
                            matches: job.pending_matches,
                            total_occurrences: job.total_occurrences,
                        })?;
                    } else {
                        if job.last_progress.elapsed() >= SEARCH_PROGRESS_INTERVAL {
                            send(WorkerMessage::SearchProgress {
                                document_id: job.document_id,
                                request_id: job.request_id,
                                scanned: job.next_page,
                                total: job.total_pages,
                                matches: std::mem::take(&mut job.pending_matches),
                                total_occurrences: job.total_occurrences,
                            })?;
                            job.last_progress = Instant::now();
                        }
                        search_jobs.push_back(job);
                    }
                    continue;
                }
                WorkerTask::Render(req, prefetch) => {
                    if !command_rx.is_empty() {
                        deferred_render = Some((req, prefetch));
                        continue;
                    }
                    (req, prefetch)
                }
            };

            if !is_current(&request, is_prefetch) {
                continue;
            }
            let report_error = |verb: &str, error: &dyn std::fmt::Display| -> Result<(), String> {
                if !is_current(&request, is_prefetch) {
                    return Ok(());
                }
                send(WorkerMessage::RenderError {
                    key: request.key,
                    generation: request.generation,
                    error: format!("could not {verb} page {}: {error}", request.key.page + 1),
                })
            };

            let Ok(page_index) = i32::try_from(request.key.page) else {
                report_error("load", &"page index out of range")?;
                continue;
            };
            let Some(loaded) = documents.get_mut(&request.key.document_id) else {
                report_error("load", &"document is closed")?;
                continue;
            };
            let page = match loaded.document.pages().get(page_index) {
                Ok(page) => page,
                Err(error) => {
                    report_error("load", &error)?;
                    continue;
                }
            };
            let (config, content_bounds) = frame_render_config(
                request.key,
                &page,
                loaded.page_cache.entry(request.key.page).or_default(),
            );
            let mut uncached_rgba = None;
            let (width, height, render_elapsed, dark_mode_elapsed) = if let Some(cached) =
                base_cache
                    .as_ref()
                    .filter(|cached| cached.matches(request.key, loaded.revision))
            {
                (cached.width, cached.height, Duration::ZERO, None)
            } else {
                let render_started = Instant::now();
                let bitmap = match page.render_with_config(&config) {
                    Ok(bitmap) => bitmap,
                    Err(error) => {
                        report_error("render", &error)?;
                        continue;
                    }
                };
                let render_elapsed = render_started.elapsed();
                let (width, height) = (bitmap.width() as u32, bitmap.height() as u32);
                let mut raw_rgba = bitmap.as_raw_bytes();
                drop(bitmap);
                if !is_current(&request, is_prefetch) {
                    continue;
                }
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
                if !is_current(&request, is_prefetch) {
                    continue;
                }
                if !is_prefetch && (width as usize).saturating_mul(height as usize) <= 16_777_216 {
                    base_cache = Some(CachedBaseRaster {
                        key: request.key,
                        revision: loaded.revision,
                        width,
                        height,
                        rgba: raw_rgba,
                    });
                } else {
                    uncached_rgba = Some(raw_rgba);
                }
                (width, height, render_elapsed, dark_mode_elapsed)
            };

            let search_rectangles = (request.key.search_request_id != 0)
                .then(|| {
                    loaded
                        .search_highlights
                        .as_ref()
                        .filter(|highlights| highlights.request_id == request.key.search_request_id)
                        .and_then(|highlights| highlights.pages.get(&request.key.page))
                })
                .flatten();
            let annotations = loaded
                .page_cache
                .entry(request.key.page)
                .or_default()
                .links
                .get_or_insert_with(|| extract_page_link_annotations(&loaded.document, &page));
            let (links, dark_mode_link_rectangles) = project_page_links(
                &page,
                &config,
                width,
                height,
                annotations,
                request.key.invert,
            );
            let selected_link_rectangles =
                selected_page_link_rectangles(&links, request.key.selected_link_ordinal);
            let flash = loaded
                .flash_highlight
                .as_ref()
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
            let has_overlays = search_rectangles.is_some()
                || (request.key.link_mode && !links.is_empty())
                || !selected_link_rectangles.is_empty()
                || !dark_mode_link_rectangles.is_empty()
                || flash_rectangle.is_some();
            let mut overlay_rgba = has_overlays.then(|| {
                uncached_rgba.take().unwrap_or_else(|| {
                    base_cache
                        .as_ref()
                        .expect("cached base raster")
                        .rgba
                        .clone()
                })
            });
            let highlight_elapsed = overlay_rgba.as_mut().map(|raw_rgba| {
                let started = Instant::now();
                if !dark_mode_link_rectangles.is_empty() {
                    apply_dark_mode_link_contrast(
                        raw_rgba,
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
                        raw_rgba,
                        rectangles,
                        request.key.search_highlight,
                    );
                }
                if request.key.link_mode && !links.is_empty() {
                    apply_link_highlights(
                        raw_rgba,
                        width,
                        height,
                        &links,
                        request.key.link_highlight,
                        request.key.invert,
                    );
                }
                if !selected_link_rectangles.is_empty() {
                    apply_selected_link_highlights(
                        raw_rgba,
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
                        raw_rgba,
                        std::slice::from_ref(rect),
                        [255, 0, 0],
                    );
                }
                started.elapsed()
            });
            if !is_current(&request, is_prefetch) {
                continue;
            }
            let rgba_bytes = overlay_rgba
                .as_deref()
                .or(uncached_rgba.as_deref())
                .unwrap_or_else(|| &base_cache.as_ref().expect("cached base raster").rgba);
            let compression_started = Instant::now();
            let compressed_rgba =
                match crate::kitty::compress_rgba_as_rgb(rgba_bytes, width, height) {
                    Ok(compressed) => compressed,
                    Err(error) => {
                        report_error("compress", &error)?;
                        continue;
                    }
                };
            let compression_elapsed = compression_started.elapsed();

            if !is_current(&request, is_prefetch) {
                continue;
            }

            send(WorkerMessage::Frame(Frame {
                key: request.key,
                revision: loaded.revision,
                width,
                height,
                page_width_pt: page.width().value,
                page_height_pt: page.height().value,
                content_bounds,
                compressed_rgba,
                render_elapsed,
                dark_mode_elapsed,
                highlight_elapsed,
                compression_elapsed,
                generation: request.generation,
                links,
                flash,
            }))?;
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
    let sum = usize::from(max_channel) + usize::from(min_channel);
    let new_lightness =
        transform.background_lightness + dark_mode_lightness_lut()[sum] * transform.lightness_range;
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

fn dark_mode_lightness_lut() -> &'static [f32; 511] {
    static LUT: OnceLock<[f32; 511]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|sum| {
            let lightness = sum as f32 / (255.0 * 2.0);
            1.0 - lightness.powf(1.2)
        })
    })
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

fn strip_tex_subset_prefix(font_name: &str) -> &str {
    font_name
        .split_once('+')
        .filter(|(prefix, _)| {
            prefix.len() == 6
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
        .map_or(font_name, |(_, rest)| rest)
}

/// Type 1 TeX math fonts (`CMEX10`, `CMMI10`, `CMSY10`, `MSAM10`, `MSBM10`, `stmary10`, and
/// their Latin Modern / TX / PX counterparts) have incomplete or missing `ToUnicode` CMaps in
/// `pdfTeX`'s built-in `TeX-*-builtin-0` tables, so PDFium exposes raw 1-byte character codes
/// (`0x00..=0x7F`) for unmapped slots.
fn tex_math_font_unicode(font_name: &str, code: u32) -> Option<u32> {
    let raw_base = strip_tex_subset_prefix(font_name);
    let mut lower_buf = [0u8; 64];
    if raw_base.len() > lower_buf.len() || !raw_base.is_ascii() {
        return None;
    }
    for (dst, src) in lower_buf[..raw_base.len()].iter_mut().zip(raw_base.bytes()) {
        *dst = src.to_ascii_lowercase();
    }
    let base = std::str::from_utf8(&lower_buf[..raw_base.len()]).ok()?;
    let is_omx = base.starts_with("cmex")
        || base.starts_with("lcmex")
        || base.starts_with("lmex")
        || base.starts_with("mlmex")
        || base.starts_with("vlmex")
        || base.starts_with("yhcmex")
        || matches!(base, "txex" | "txbex" | "txex-bar" | "pxex" | "pxbex")
        || base.contains("mathextension")
        || base.contains("math-extension")
        || base.contains("-extension-")
        || base.contains("-m-ex-");
    if is_omx {
        return match code {
            0x04 | 0x16 | 0x24 | 0x6a => Some(0x230a), // \lfloor
            0x05 | 0x17 | 0x25 | 0x6b => Some(0x230b), // \rfloor
            0x06 | 0x18 | 0x26 | 0x6c => Some(0x2308), // \lceil
            0x07 | 0x19 | 0x27 | 0x6d => Some(0x2309), // \rceil
            0x0a | 0x1c | 0x2a | 0x44 => Some(0x27e8), // \langle
            0x0b | 0x1d | 0x2b | 0x45 => Some(0x27e9), // \rangle
            0x3a => Some(0x27ee),                      // \lgroup
            0x3b => Some(0x27ef),                      // \rgroup
            0x46 | 0x47 => Some(0x2294),               // \bigsqcup
            0x48 | 0x49 => Some(0x222e),               // \oint
            0x4a | 0x4b => Some(0x2299),               // \bigodot
            0x4c | 0x4d => Some(0x2295),               // \bigoplus
            0x4e | 0x4f => Some(0x2297),               // \bigotimes
            0x50 | 0x58 => Some(0x2211),               // \sum
            0x51 | 0x59 => Some(0x220f),               // \prod
            0x52 | 0x5a => Some(0x222b),               // \int
            0x53 | 0x5b => Some(0x222a),               // \bigcup
            0x54 | 0x5c => Some(0x2229),               // \bigcap
            0x55 | 0x5d => Some(0x228e),               // \biguplus
            0x56 | 0x5e => Some(0x2227),               // \bigwedge
            0x57 | 0x5f => Some(0x2228),               // \bigvee
            0x60 | 0x61 => Some(0x2210),               // \coprod
            0x70..=0x73 => Some(0x221a),               // \sqrt
            _ => None,
        };
    }
    let is_oml = base.starts_with("cmmi")
        || base.starts_with("lcmmi")
        || base.starts_with("lmmi")
        || base.starts_with("mlmmi")
        || base.starts_with("vlmmi")
        || base.starts_with("rtxmi")
        || base.starts_with("rtxbmi")
        || base.starts_with("rpxmi")
        || base.starts_with("rpxbmi")
        || matches!(base, "txmi" | "txbmi" | "pxmi" | "pxbmi")
        || base.contains("mathitalic")
        || base.contains("math-italic");
    if is_oml {
        return match code {
            0x28 => Some(0x21bc), // \leftharpoonup
            0x29 => Some(0x21bd), // \leftharpoondown
            0x2a => Some(0x21c0), // \rightharpoonup
            0x2b => Some(0x21c1), // \rightharpoondown
            0x2d => Some(0x297d), // \hookleftarrow right hook (\rhook)
            0x2c => Some(0x297c), // \hookrightarrow left hook (\lhook)
            _ => None,
        };
    }
    let is_oms = base.starts_with("cmsy")
        || base.starts_with("lcmsy")
        || base.starts_with("lmsy")
        || base.starts_with("mlmsy")
        || base.starts_with("vlmsy")
        || matches!(base, "txsy" | "txbsy" | "pxsy" | "pxbsy")
        || base.contains("mathsymbols")
        || base.contains("math-symbols");
    if is_oms {
        return match code {
            0x34 => Some(0x25b3), // \triangle / \bigtriangleup
            0x35 => Some(0x25bd), // \bigtriangledown
            0x36 => Some(0x20d2), // \not (prefix negation)
            0x37 => Some(0x237f), // \mapstochar
            0x71 => Some(0x2a3f), // \amalg
            _ => None,
        };
    }
    if base.starts_with("msam")
        || base.starts_with("txsya")
        || base.starts_with("txbsya")
        || base.starts_with("pxsya")
        || base.starts_with("pxbsya")
    {
        return match code {
            0x05 => Some(0x22c5), // \centerdot
            0x38 => Some(0x25be), // \blacktriangledown
            0x39 => Some(0x23af), // \dasharrow / \dashrightarrow / \dashleftarrow shaft
            0x4b => Some(0x21e2), // \dashrightarrow head
            0x4c => Some(0x21e0), // \dashleftarrow head
            0x55 => Some(0x00a5), // \yen
            0x70 => Some(0x231c), // \ulcorner
            0x71 => Some(0x231d), // \urcorner
            0x78 => Some(0x231e), // \llcorner
            0x79 => Some(0x231f), // \lrcorner
            _ => None,
        };
    }
    if base.starts_with("msbm")
        || base.starts_with("txsyb")
        || base.starts_with("txbsyb")
        || base.starts_with("pxsyb")
        || base.starts_with("pxbsyb")
    {
        return match code {
            0x12 => Some(0x22e6),   // \lnsim
            0x13 => Some(0x22e7),   // \gnsim
            0x26 => Some(0x2acb),   // \varsubsetneqq
            0x27 => Some(0x2acc),   // \varsupsetneqq
            0x2e => Some(0x2224),   // \nshortmid
            0x2f => Some(0x2226),   // \nshortparallel
            0x70 => Some(0x007c),   // \shortmid
            0x71 => Some(0x2016),   // \shortparallel
            0x7a => Some(0x03dd),   // \digamma
            0xd875 => Some(0xd835), // \Bbbk broken high surrogate in TeX-msbm10-builtin-0
            _ => None,
        };
    }
    if base.starts_with("stmary") {
        return match code {
            0x00 => Some(0x2190),               // \shortleftarrow
            0x01 => Some(0x2192),               // \shortrightarrow
            0x02 => Some(0x2191),               // \shortuparrow
            0x03 => Some(0x2193),               // \shortdownarrow
            0x04 => Some(0x2144),               // \Yup
            0x05 => Some(0x27d9),               // \Ydown
            0x06 => Some(0x2919),               // \Yleft
            0x07 => Some(0x291a),               // \Yright
            0x08 | 0x62 | 0x6a => Some(0x22ce), // \varcurlyvee, \bigcurlyvee, \hugecurlyvee
            0x09 | 0x63 | 0x6b => Some(0x22cf), // \varcurlywedge, \bigcurlywedge, \hugecurlywedge
            0x0a | 0x17 => Some(0x2296),        // \minuso, \varominus
            0x0b | 0x11 | 0x3a => Some(0x29b6), // \baro, \varobar, \obar
            0x0c | 0x28 => Some(0x2afd),        // \sslash, \fatslash
            0x0d | 0x29 => Some(0x29f5),        // \bbslash, \fatbslash
            0x0e => Some(0xe00e),               // \moo
            0x0f => Some(0x2297),               // \varotimes
            0x10 => Some(0x229b),               // \varoast
            0x12 => Some(0x2299),               // \varodot
            0x13 => Some(0x2298),               // \varoslash
            0x14 | 0x3b => Some(0x29b8),        // \varobslash, \obslash
            0x15 => Some(0x229a),               // \varocircle
            0x16 => Some(0x2295),               // \varoplus
            0x18 => Some(0x29c6),               // \boxast
            0x19 => Some(0x25eb),               // \boxbar
            0x1a => Some(0x22a1),               // \boxdot
            0x1b => Some(0x29c4),               // \boxslash
            0x1c => Some(0x29c5),               // \boxbslash
            0x1d => Some(0x29c7),               // \boxcircle
            0x1e => Some(0x29c8),               // \boxbox
            0x1f | 0x65 | 0x6d => Some(0x25a1), // \boxempty, \bigbox, \hugebox
            0x20 => Some(0x21af),               // \lightning
            0x21 => Some(0x2a07),               // \merge
            0x22 => Some(0x00d7),               // \vartimes
            0x23 => Some(0x2a1f),               // \fatsemi
            0x24 => Some(0x2198),               // \ssearrow
            0x26 => Some(0x2199),               // \sswarrow
            0x2a | 0x48 => Some(0x27c5),        // \lbag, \Lbag
            0x2b | 0x49 => Some(0x27c6),        // \rbag, \Rbag
            0x2c => Some(0x25ef),               // \varbigcirc
            0x2d => Some(0x2948),               // \leftrightarroweq
            0x30 => Some(0x2196),               // \nnwarrow
            0x31 => Some(0x2197),               // \nnearrow
            0x32 => Some(0x2aa6),               // \leftslice
            0x33 => Some(0x2aa7),               // \rightslice
            0x34 | 0x3c => Some(0x29c0),        // \varolessthan, \olessthan
            0x35 | 0x3d => Some(0x29c1),        // \varogreaterthan, \ogreaterthan
            0x38 | 0x40 => Some(0x25af),        // \talloblong, \oblong
            0x39 | 0x67 | 0x6f => Some(0x2af4), // \interleave, \biginterleave
            0x41 => Some(0x2a2d),               // \inplus
            0x42 => Some(0x2a2e),               // \niplus
            0x43 | 0x70 | 0x78 => Some(0x228e), // \nplus, \bignplus, \Hugenplus
            0x44 => Some(0x2abf),               // \subsetplus
            0x45 => Some(0x2ac0),               // \supsetplus
            0x46 => Some(0x2ac1),               // \subsetpluseq
            0x47 => Some(0x2ac2),               // \supsetpluseq
            0x4a | 0x71..=0x77 => Some(0x27e6), // \llbracket
            0x4b | 0x79..=0x7f => Some(0x27e7), // \rrbracket
            0x4c => Some(0x2985),               // \llparenthesis
            0x4d => Some(0x2986),               // \rrparenthesis
            0x4e => Some(0x0026),               // \binampersand
            0x4f => Some(0x214b),               // \bindnasrepma
            0x50 => Some(0x22b4),               // \trianglelefteqslant
            0x51 => Some(0x22b5),               // \trianglerighteqslant
            0x52 => Some(0x22ec),               // \ntrianglelefteqslant
            0x53 => Some(0x22ed),               // \ntrianglerighteqslant
            0x54 => Some(0x230a),               // \llfloor
            0x55 => Some(0x230b),               // \rrfloor
            0x56 => Some(0x2308),               // \llceil
            0x57 => Some(0x2309),               // \rrceil
            0x58 | 0x59 => Some(0x20d2),        // \arrownot, \Arrownot (prefix negation)
            0x5a => Some(0x237f),               // \Mapstochar
            0x5b | 0x5c => Some(0x23ae),        // \mapsfromchar, \Mapsfromchar
            0x5d => Some(0x21ff),               // \leftrightarrowtriangle
            0x5e => Some(0x21fd),               // \leftarrowtriangle
            0x5f => Some(0x21fe),               // \rightarrowtriangle
            0x60 | 0x68 => Some(0x25bd),        // \bigtriangledown
            0x61 | 0x69 => Some(0x25b3),        // \bigtriangleup
            0x64 | 0x6c => Some(0x2293),        // \bigsqcap
            0x66 | 0x6e => Some(0x2016),        // \bigparallel
            0x25 | 0x27 | 0x2e | 0x2f | 0x36 | 0x37 | 0x3e | 0x3f => Some(0xe000 + code),
            _ => None,
        };
    }
    None
}

fn tex_type1_slots(data: &[u8], font_name: &str) -> Option<[bool; 128]> {
    let body = if data.starts_with(b"\x80\x01") && data.len() > 6 {
        &data[6..]
    } else {
        data
    };
    if !(body.starts_with(b"%!PS-AdobeFont") || body.starts_with(b"%!FontType1")) {
        return None;
    }
    let header_end = body
        .windows(5)
        .position(|w| w == b"eexec")
        .unwrap_or(body.len());
    let header = String::from_utf8_lossy(&body[..header_end]);
    if !header.contains("/Encoding 256 array") {
        return None;
    }
    let base = strip_tex_subset_prefix(font_name).to_ascii_lowercase();
    if base.starts_with("lm") || base.starts_with("mlm") || base.starts_with("vlm") {
        return Some([true; 128]);
    }
    // Other families require an explicit encoding entry for each recovered slot.
    let mut slots = [false; 128];
    for part in header.split("dup ").skip(1) {
        let digits = part.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && let Some(rest) = part[digits..].trim_ascii_start().strip_prefix('/')
            && !rest.starts_with(".notdef")
            && let Ok(code) = part[..digits].parse::<usize>()
            && let Some(slot) = slots.get_mut(code)
        {
            *slot = true;
        }
    }
    Some(slots)
}

fn is_embedded_tex_type1_char(
    character: &PdfPageTextChar<'_>,
    font_name: &str,
    code: u32,
    cache: &mut TexFontCache,
) -> bool {
    let Some(idx) = usize::try_from(code).ok().filter(|&i| i < 128) else {
        return false;
    };
    if let Some(&Some(present)) = cache.get(font_name).and_then(|slots| slots.get(idx)) {
        return present;
    }
    let Ok(text_object) = character.text_object() else {
        return false;
    };
    let font = text_object.font();
    let slots = match font.is_embedded() {
        Ok(false) => None,
        Ok(true) => {
            let Ok(data) = font.data() else {
                return false;
            };
            tex_type1_slots(&data, font_name)
        }
        Err(_) => return false,
    };
    let entry = cache.entry(font_name.to_owned()).or_insert([None; 128]);
    let Some(slots) = slots else {
        entry.fill(Some(false));
        return false;
    };
    for (dst, &present) in entry.iter_mut().zip(&slots) {
        if present {
            *dst = Some(true);
        }
    }
    let recovered = slots[idx];
    if !recovered && entry[idx].is_none() {
        entry[idx] = Some(false);
    }
    recovered
}

fn unscaled_glyph_box(character: &PdfPageTextChar<'_>) -> Option<(f32, f32, f32, f32)> {
    let bounds = character.tight_bounds().ok()?;
    let (origin_x, origin_y) = character.origin().ok()?;
    let matrix = character.matrix().ok()?;
    let (a, b, c, d) = (matrix.a(), matrix.b(), matrix.c(), matrix.d());
    let norm = a * a + b * b + c * c + d * d;
    let det = a * d - b * c;
    let det_abs = a.abs() * d.abs() - b.abs() * c.abs();
    if norm <= 1e-8 || det.abs() <= 1e-4 * norm {
        return None;
    }
    let (dx, dy) = (bounds.width().value, bounds.height().value);
    let mx = (bounds.left().value + bounds.right().value) * 0.5 - origin_x.value;
    let my = (bounds.bottom().value + bounds.top().value) * 0.5 - origin_y.value;
    let center_y = (-b * mx + a * my) / det;
    if det_abs.abs() <= 0.05 * norm {
        // Near odd multiples of 45°, aspect ratio is ill-conditioned, but
        // (dx + dy) / (2 * (|c| + |d|)) still upper-bounds half the glyph height.
        let denom = 2.0 * (c.abs() + d.abs());
        if denom <= 1e-4 * norm.sqrt() {
            return None;
        }
        let half_span = (dx + dy) / denom;
        return Some((0.0, 0.0, center_y - half_span, center_y + half_span));
    }
    let width = (d.abs() * dx - c.abs() * dy) / det_abs;
    let height = (a.abs() * dy - b.abs() * dx) / det_abs;
    let span = width.max(height);
    if span <= 0.0 || width < -1e-2 * span || height < -1e-2 * span {
        return None;
    }
    let (width, height) = (width.max(0.0), height.max(0.0));
    Some((
        width,
        height,
        center_y - height * 0.5,
        center_y + height * 0.5,
    ))
}

// Recover unmapped Type 1 slots and the known broken TeX high surrogate.
fn pdf_char_unicode(character: &PdfPageTextChar<'_>, cache: &mut TexFontCache) -> u32 {
    let code = character.unicode_value();
    if matches!(code, 0x00..=0x7f | 0xd875) && matches!(character.is_generated(), Ok(false)) {
        let font_name = character.font_name();
        if let Some(mapped) = tex_math_font_unicode(&font_name, code) {
            if code <= 0x7f && !is_embedded_tex_type1_char(character, &font_name, code, cache) {
                return code;
            }
            if matches!(mapped, 0x21bc..=0x21c1) {
                let Some((width, height, _, _)) = unscaled_glyph_box(character) else {
                    return code;
                };
                if width <= height * 2.0 {
                    return code;
                }
            }
            if matches!(mapped, 0x297c | 0x297d) {
                let Some((_, _, bottom, _)) = unscaled_glyph_box(character) else {
                    return code;
                };
                if bottom <= 0.0 {
                    return code;
                }
            }
            if code == 0x26 && mapped == 0x2199 {
                let Some((width, height, _, _)) = unscaled_glyph_box(character) else {
                    return code;
                };
                if height <= width * 1.2 {
                    return code;
                }
            }
            // In CMEX10, ToUnicode-decoded '[', '\\', ']', '^' (0x5b..=0x5e) share codes
            // with raw display-style operators (\bigcup, \bigcap, \biguplus, \bigwedge),
            // which hang below the baseline (B 56 -1400 1054 0) with wide aspect ratio.
            if matches!(code, 0x5b..=0x5e) && matches!(mapped, 0x222a | 0x2229 | 0x228e | 0x2227) {
                let Some((width, height, bottom, top)) = unscaled_glyph_box(character) else {
                    return code;
                };
                if bottom >= 0.0 || top > height * 0.05 || width <= height * 0.5 {
                    return code;
                }
            }
            return mapped;
        }
    }
    code
}

/// Keep a small text neighborhood around the clicked glyph, not the entire page.
fn clicked_text(
    page: &PdfPage,
    x: f32,
    y: f32,
    font_cache: &mut TexFontCache,
) -> Result<Option<(String, usize)>, String> {
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
    let is_word_code = |code: u32, check: fn(char) -> bool| {
        matches!(code, 0xd800..=0xdfff) || char::from_u32(code).is_some_and(check)
    };
    if !is_word_code(
        pdf_char_unicode(&character, font_cache),
        char::is_alphanumeric,
    ) && let Ok(bounds) = character.tight_bounds()
        && bounds.contains(x, y)
    {
        let mut area = bounds.width().value * bounds.height().value;
        for candidate in chars.iter() {
            if !is_word_code(
                pdf_char_unicode(&candidate, font_cache),
                char::is_alphabetic,
            ) {
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
        let character = chars.get(index).map_err(|error| error.to_string())?;
        let code = pdf_char_unicode(&character, font_cache);
        if code == 0x0338
            && let Ok(b1) = character.tight_bounds()
        {
            let overlap = |neighbor: Option<usize>| {
                neighbor
                    .and_then(|i| chars.get(i).ok())
                    .and_then(|other| other.tight_bounds().ok())
                    .map_or(0.0, |b2| {
                        (b1.right().value.min(b2.right().value)
                            - b1.left().value.max(b2.left().value))
                        .max(0.0)
                    })
            };
            if overlap(index.checked_add(1)) > overlap(index.checked_sub(1)) {
                return Ok(0x20d2);
            }
        }
        if matches!(code, 0x21bd | 0x21c0)
            && let Ok(b1) = character.tight_bounds()
        {
            for neighbor in [index.checked_sub(1), index.checked_add(1)]
                .into_iter()
                .flatten()
            {
                if let Ok(other) = chars.get(neighbor)
                    && pdf_char_unicode(&other, font_cache) == (code ^ 0x21bd ^ 0x21c0)
                    && let Ok(b2) = other.tight_bounds()
                {
                    let overlap = b1.right().value.min(b2.right().value)
                        - b1.left().value.max(b2.left().value);
                    let min_width = b1.width().value.min(b2.width().value);
                    let dy = if code == 0x21c0 {
                        b1.bottom().value - b2.bottom().value
                    } else {
                        b2.bottom().value - b1.bottom().value
                    };
                    if min_width > 0.0
                        && overlap > min_width * 0.5
                        && dy > min_width * 0.05
                        && dy < min_width
                    {
                        return Ok(if code == 0x21c0 { 0x296a } else { 0x296b });
                    }
                }
            }
        }
        Ok(code)
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
    best_guess: bool,
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
    let mut font_cache = HashMap::new();
    let mut cached_synctex = crate::synctex::SyncTeXCache::default();
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
            let (context, offset) = match clicked_text(page, pdf_x, pdf_y, &mut font_cache) {
                Ok(Some(context)) => context,
                Ok(None) => return Err("PDFium found no text near point".into()),
                Err(error) => return Err(format!("PDFium text hit-test failed: {error}")),
            };
            let resolution = crate::synctex::resolve_inverse_with_cache(
                pdf,
                coordinates.inverse_point(point.page, pdf_x, pdf_y)?,
                settings.word_precision.then_some((&context, offset)),
                settings.source_context_lines as u32,
                &crate::process::Operation::new(std::time::Duration::from_secs(30)),
                best_guess,
                Some(&mut cached_synctex),
            )
            .map_err(|error| error.to_string())?;
            let tokens = words(&context);
            let pdf_word = crate::synctex::word_index_at(&context, &tokens, offset)
                .map(|index| tokens[index].1.to_owned());
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

#[allow(clippy::too_many_arguments)]
fn find_visible_matches(
    document: &PdfDocument,
    document_id: DocumentId,
    query: &str,
    keys: &[(RenderKey, u32, u32)],
    cache: &mut [Option<CachedPageText>],
    page_cache: &mut HashMap<u32, CachedPageGeometry>,
    font_cache: &mut TexFontCache,
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
        let (config, _) = frame_render_config(key, &page, page_cache.entry(key.page).or_default());
        let Some(cached) = cached_visible_page_text(
            &page,
            key.page,
            cache,
            font_cache,
            generation,
            latest_generation,
        ) else {
            if generation != latest_generation.load(Ordering::Acquire) {
                return Ok(None);
            }
            return Err(format!(
                "could not extract visible text from page {}",
                u64::from(key.page) + 1
            ));
        };
        let source_index_by_byte = &cached.source_index_by_byte;
        let Some(occurrences) =
            overlapping_match_ranges_cancellable(&cached.normalized, &needle, || {
                generation == latest_generation.load(Ordering::Acquire)
            })
        else {
            return Ok(None);
        };
        if occurrences.is_empty() {
            continue;
        }
        for (start, end) in occurrences {
            if generation != latest_generation.load(Ordering::Acquire) {
                return Ok(None);
            }
            let Some(source_ranges) = source_index_by_byte.get(start..end) else {
                continue;
            };
            let Some(&first) = source_ranges.first() else {
                continue;
            };
            let mut rects = Vec::new();
            let mut visible = true;
            let mut previous = None;
            for &glyph_index in source_ranges {
                if generation != latest_generation.load(Ordering::Acquire) {
                    return Ok(None);
                }
                if previous == Some(glyph_index) {
                    continue;
                }
                previous = Some(glyph_index);
                let glyph = &cached.glyphs[glyph_index];
                if glyph.value.is_whitespace() {
                    continue;
                }
                let previous_rect_count = rects.len();
                for &bounds in glyph.bounds.iter().flatten() {
                    if generation != latest_generation.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let Some(rect) =
                        page_rect_to_pixels(&page, &config, frame_width, frame_height, bounds)
                    else {
                        continue;
                    };
                    if rect.right > rect.left && rect.bottom > rect.top {
                        rects.push(rect);
                    }
                }
                if rects.len() == previous_rect_count {
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
            let last_glyph = *source_ranges
                .last()
                .expect("non-empty occurrence source range");
            let next_char = source_index_by_byte
                .get(end)
                .filter(|next_glyph| **next_glyph != last_glyph)
                .map(|&index| cached.glyphs[index].value)
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
                char_index: cached.glyphs[first].source_index,
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

fn init_cached_page_text(
    slot: &mut CachedPageText,
    text: &PdfPageText,
    font_cache: &mut TexFontCache,
) {
    let chars = text.chars();
    let (normalized, source_index_by_byte) = normalize_search_characters(
        decoded_pdf_characters(&chars, font_cache, || true, |_, _| {})
            .map(|(start, _, value)| (start, value)),
    );
    slot.raw = text.all();
    slot.normalized = normalized;
    slot.source_index_by_byte = source_index_by_byte;
    slot.full_text_initialized = true;
}

fn cached_page_text<'a>(
    document: &PdfDocument,
    page: u32,
    cache: &'a mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
) -> Option<&'a CachedPageText> {
    cached_page_text_with_loaded(document, page, None, None, cache, font_cache)
}

fn cached_page_text_with_loaded<'a>(
    document: &PdfDocument,
    page: u32,
    loaded_page: Option<&PdfPage>,
    loaded_text: Option<&PdfPageText>,
    cache: &'a mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
) -> Option<&'a CachedPageText> {
    let index = usize::try_from(page).ok()?;
    let slot = cache
        .get_mut(index)?
        .get_or_insert_with(CachedPageText::default);
    if !slot.full_text_initialized {
        if let Some(text) = loaded_text {
            init_cached_page_text(slot, text, font_cache);
        } else if let Some(pdf_page) = loaded_page {
            let text = pdf_page.text().ok()?;
            init_cached_page_text(slot, &text, font_cache);
        } else {
            let page_index = i32::try_from(page).ok()?;
            let pdf_page = document.pages().get(page_index).ok()?;
            let text = pdf_page.text().ok()?;
            init_cached_page_text(slot, &text, font_cache);
        }
    }
    Some(slot)
}

fn cached_visible_page_text<'a>(
    pdf_page: &PdfPage,
    page: u32,
    cache: &'a mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
    generation: u64,
    latest_generation: &AtomicU64,
) -> Option<&'a VisiblePageText> {
    let index = usize::try_from(page).ok()?;
    let slot = cache
        .get_mut(index)?
        .get_or_insert_with(CachedPageText::default);
    if slot.visible.is_none() {
        slot.visible =
            extract_visible_page_text(pdf_page, font_cache, generation, latest_generation);
    }
    slot.visible.as_ref()
}

fn extract_visible_page_text(
    page: &PdfPage,
    font_cache: &mut TexFontCache,
    generation: u64,
    latest_generation: &AtomicU64,
) -> Option<VisiblePageText> {
    let (clip_left, clip_bottom, clip_right, clip_top) = effective_page_bounds(page)?;
    let text = page.text().ok()?;
    let chars = text.chars();
    let mut glyphs = Vec::new();
    let (normalized, source_index_by_byte) = normalize_search_characters(
        decoded_pdf_characters(
            &chars,
            font_cache,
            || generation == latest_generation.load(Ordering::Acquire),
            |_, _| {},
        )
        .filter_map(|(start, end, value)| {
            let mut bounds = [None; 2];
            if value.is_whitespace() {
                // PDFium inserts separators without a text object. Painted spaces
                // still obey the same transparency/render-mode rules as glyphs.
                let character = chars.get(start).ok()?;
                if !char_has_visible_paint(&character, true) {
                    return None;
                }
            } else {
                for index in start..end {
                    if generation != latest_generation.load(Ordering::Acquire) {
                        return None;
                    }
                    let Ok(character) = chars.get(index) else {
                        continue;
                    };
                    if !char_has_visible_paint(&character, false) {
                        continue;
                    }
                    if let Ok(rect) = character.tight_bounds() {
                        bounds[index - start] = Some(rect);
                    }
                }
                if !bounds.iter().flatten().any(|bounds| {
                    bounds.right().value > clip_left
                        && bounds.left().value < clip_right
                        && bounds.top().value > clip_bottom
                        && bounds.bottom().value < clip_top
                }) {
                    return None;
                }
            }
            let index = glyphs.len();
            glyphs.push(VisibleGlyph {
                source_index: start,
                value,
                bounds,
            });
            Some((index, value))
        }),
    );
    if generation != latest_generation.load(Ordering::Acquire) {
        return None;
    }
    Some(VisiblePageText {
        normalized,
        source_index_by_byte,
        glyphs,
    })
}

fn cached_page_content_bounds(page: &PdfPage, cache: &mut CachedPageGeometry) -> [f32; 4] {
    *cache
        .content_bounds
        .get_or_insert_with(|| page_content_bounds(page))
}

fn search_rect(bounds: PdfRect) -> SearchRect {
    SearchRect {
        left: bounds.left().value,
        right: bounds.right().value,
        top: bounds.top().value,
        bottom: bounds.bottom().value,
    }
}

fn bounds_probe_config(page: &PdfPage) -> Option<(PdfRenderConfig, i32, i32)> {
    const WIDTH: i32 = 100_000;
    let (w, h) = (page.width().value, page.height().value);
    if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scaled_h = (h * WIDTH as f32 / w).round();
    if !scaled_h.is_finite() || !(1.0..=i32::MAX as f32).contains(&scaled_h) {
        return None;
    }
    Some((
        PdfRenderConfig::new().set_target_width(WIDTH),
        WIDTH,
        scaled_h as i32,
    ))
}

/// Measure all page objects, not just text, in the same rotated/cropped device
/// coordinates used by rendering and hit-testing. Empty pages retain page fit.
fn page_content_bounds(page: &PdfPage) -> [f32; 4] {
    let Some((config, width, height)) = bounds_probe_config(page) else {
        return [0.0, 0.0, 1.0, 1.0];
    };
    let mut content = [1.0_f32, 1.0, 0.0, 0.0];
    for object in page.objects().iter() {
        let Ok(bounds) = object.bounds() else {
            continue;
        };
        let Some((left, right, top, bottom)) =
            page_rect_pixel_bounds(page, &config, search_rect(bounds.to_rect()))
        else {
            continue;
        };
        let left = (left as f32 / width as f32).clamp(0.0, 1.0);
        let right = (right as f32 / width as f32).clamp(0.0, 1.0);
        let top = (top as f32 / height as f32).clamp(0.0, 1.0);
        let bottom = (bottom as f32 / height as f32).clamp(0.0, 1.0);
        if left < right && top < bottom {
            content[0] = content[0].min(left);
            content[1] = content[1].min(top);
            content[2] = content[2].max(right);
            content[3] = content[3].max(bottom);
        }
    }
    if content[0] < content[2] && content[1] < content[3] {
        content
    } else {
        [0.0, 0.0, 1.0, 1.0]
    }
}

fn effective_page_bounds(page: &PdfPage) -> Option<(f32, f32, f32, f32)> {
    let (config, width, height) = bounds_probe_config(page)?;
    let points: Vec<_> = [(0, 0), (width, 0), (0, height), (width, height)]
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

fn char_has_visible_paint(
    character: &pdfium_render::prelude::PdfPageTextChar<'_>,
    whitespace_default: bool,
) -> bool {
    character.render_mode().map_or(whitespace_default, |mode| {
        text_mode_has_visible_paint(
            mode,
            || character.fill_color().ok().map(|color| color.alpha()),
            || character.stroke_color().ok().map(|color| color.alpha()),
        )
    })
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

#[cfg(test)]
fn count_search_matches(haystack: &str, needle: &str) -> u32 {
    u32::try_from(haystack.match_indices(needle).count()).unwrap_or(u32::MAX)
}

fn normalize_search_characters<T: Copy>(
    characters: impl IntoIterator<Item = (T, char)>,
) -> (String, Vec<T>) {
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

// Decode PDFium's scalar/UTF-16 representations once per extraction, caching
// parsed Type 1 slot tables by font name across the loaded document.
fn decoded_pdf_characters<'a>(
    chars: &'a PdfPageTextChars<'_>,
    font_cache: &'a mut TexFontCache,
    mut is_current: impl FnMut() -> bool + 'a,
    mut on_invalid: impl FnMut(usize, u32) + 'a,
) -> impl Iterator<Item = (usize, usize, char)> + 'a {
    let len = chars.len();
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < len {
            if !is_current() {
                return None;
            }
            let start = index;
            index += 1;
            let Ok(character) = chars.get(start) else {
                continue;
            };
            let first = pdf_char_unicode(&character, font_cache);
            let second = if (0xd800..=0xdbff).contains(&first) && index < len {
                chars
                    .get(index)
                    .ok()
                    .map(|character| pdf_char_unicode(&character, font_cache))
            } else {
                None
            };
            if second.is_some_and(|unit| (0xdc00..=0xdfff).contains(&unit)) {
                index += 1;
            }
            if let Some(value) = source_range_char(start, index, |unit| {
                if unit == start { Some(first) } else { second }
            }) {
                return Some((start, index, value));
            }
            on_invalid(start, first);
        }
        None
    })
}

fn push_normalized_character<T: Copy>(
    normalized: &mut String,
    source_index_by_byte: &mut Vec<T>,
    source_index: T,
    character: char,
) {
    normalized.push(character);
    source_index_by_byte.extend(std::iter::repeat_n(source_index, character.len_utf8()));
}

fn same_forward_word(a: &str, b: &str) -> bool {
    a.chars()
        .filter(|character| !is_spacing_diacritic(*character))
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .eq(b
            .chars()
            .filter(|character| !is_spacing_diacritic(*character))
            .nfkd()
            .filter(|character| !is_combining_mark(*character))
            .flat_map(char::to_lowercase))
}

fn forward_neighbor_score(tokens: &[(usize, &str)], hint: &ForwardWord, index: usize) -> u32 {
    let radius = 3;
    let mut score = 0;
    for distance in 1..=radius {
        for direction in [-1, 1] {
            let delta = distance * direction;
            if let (Some(a), Some(b)) = (
                index.checked_add_signed(delta).and_then(|i| tokens.get(i)),
                hint.selected
                    .checked_add_signed(delta)
                    .and_then(|i| hint.words.get(i)),
            ) && same_forward_word(a.1, b)
            {
                score += (radius + 1 - distance) as u32;
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
fn visible_forward_text(
    page: &PdfPage,
    font_cache: &mut TexFontCache,
) -> Result<(String, Vec<usize>), String> {
    let bounds =
        effective_page_bounds(page).ok_or("could not determine visible PDF page bounds")?;
    painted_forward_text(page, bounds, font_cache)
}

fn painted_forward_text(
    page: &PdfPage,
    (clip_left, clip_bottom, clip_right, clip_top): (f32, f32, f32, f32),
    font_cache: &mut TexFontCache,
) -> Result<(String, Vec<usize>), String> {
    let text = page
        .text()
        .map_err(|error| format!("could not extract visible PDF text: {error}"))?;
    let chars = text.chars();
    let mut unicode_error = None;
    let normalized = normalize_search_characters(
        decoded_pdf_characters(
            &chars,
            font_cache,
            || true,
            |index, first| {
                unicode_error.get_or_insert_with(|| {
                    format!("invalid PDF Unicode value {first:#x} at character {index}")
                });
            },
        )
        .filter_map(|(index, _, value)| {
            let character = chars.get(index).ok()?;
            if value.is_whitespace() {
                return char_has_visible_paint(&character, true)
                    .then_some((character.index(), value));
            }
            let bounds = character.tight_bounds().ok()?;
            let within_crop = bounds.right().value > clip_left
                && bounds.left().value < clip_right
                && bounds.top().value > clip_bottom
                && bounds.bottom().value < clip_top;
            (within_crop && char_has_visible_paint(&character, false))
                .then_some((character.index(), value))
        }),
    );
    match unicode_error {
        Some(error) => Err(error),
        None => Ok(normalized),
    }
}

fn forward_overlay_page(
    document: &PdfDocument,
    pages: u32,
    first: u32,
    candidates: &[ForwardCandidate],
    page_hint: &ForwardWord,
) -> (u32, Option<(String, Vec<usize>)>) {
    let mut selected_page = first;
    let mut selected_text = None;
    let mut later_pages: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.rect.right > candidate.rect.left || candidate.rect.bottom > candidate.rect.top
        })
        .map(|candidate| candidate.page)
        .filter(|page| *page != first)
        .collect();
    later_pages.sort_unstable();
    later_pages.dedup();
    if !later_pages.is_empty() && !page_hint.words.is_empty() {
        // An unreadable page never overrides the initial result. Preserve the
        // original first-plus-63-later-page scan bound.
        let mut font_cache = HashMap::new();
        let mut visible = |page: u32| {
            let index = page.checked_sub(1)?;
            if index >= pages {
                return None;
            }
            let page = document.pages().get(i32::try_from(index).ok()?).ok()?;
            visible_forward_text(&page, &mut font_cache).ok()
        };
        selected_text = visible(first);
        let first_score = selected_text
            .as_ref()
            .and_then(|(text, _)| forward_context_score(text, page_hint));
        let min_score = u32::from(page_hint.words.len() >= 2);
        if !first_score.is_some_and(|score| score >= min_score) {
            let mut fallback = None;
            for page in later_pages.into_iter().take(63) {
                let Some(text) = visible(page) else {
                    continue;
                };
                let score = forward_context_score(&text.0, page_hint);
                if score.is_some_and(|score| score >= min_score) {
                    selected_page = page;
                    selected_text = Some(text);
                    fallback = None;
                    break;
                }
                if first_score.is_none() && fallback.is_none() && score.is_some() {
                    fallback = Some((page, text));
                }
            }
            if let Some((page, text)) = fallback {
                selected_page = page;
                selected_text = Some(text);
            }
        }
    }
    (selected_page, selected_text)
}

/// Overlay selection is independent of WORD qualification. Retain only a
/// compiler line region when the source's literal context is unsupported.
pub(crate) fn select_forward_line_candidate(
    pdf: &Path,
    candidates: &[ForwardCandidate],
    page_hint: &ForwardWord,
    anchor: ForwardCandidate,
    library: Option<&Path>,
) -> Result<ForwardCandidate, String> {
    if page_hint.words.is_empty()
        || candidates
            .iter()
            .all(|candidate| candidate.page == anchor.page)
    {
        return Ok(anchor);
    }
    let pdfium = load_pdfium(library)?;
    let document = pdfium
        .load_pdf_from_file(pdf, None)
        .map_err(|error| format!("could not open {}: {error}", pdf.display()))?;
    let pages = u32::try_from(document.pages().len())
        .map_err(|_| "PDFium returned a negative page count".to_string())?;
    let (page, _) = forward_overlay_page(&document, pages, anchor.page, candidates, page_hint);
    if page == 0 || page > pages {
        return Err("selected SyncTeX page is outside the PDF".into());
    }
    candidates
        .iter()
        .find(|candidate| {
            candidate.page == page
                && (candidate.rect.right > candidate.rect.left
                    || candidate.rect.bottom > candidate.rect.top)
        })
        .copied()
        .ok_or_else(|| "selected SyncTeX page has no usable compiler line geometry".into())
}

#[derive(Clone, Copy)]
struct ForwardWordOccurrence {
    first: usize,
    rect: SearchRect,
}

fn forward_word_occurrence(
    text: &PdfPageText,
    source_index_by_byte: &[usize],
    start: usize,
    word: &str,
    font_cache: &mut TexFontCache,
) -> Option<ForwardWordOccurrence> {
    let end = start + word.len();
    let first = *source_index_by_byte.get(start)?;
    let last = *source_index_by_byte.get(end.checked_sub(1)?)?;
    // Reject partial case-expanded glyphs and require a contiguous unit span.
    if start > 0 && source_index_by_byte[start - 1] == first
        || source_index_by_byte.get(end) == Some(&last)
    {
        return None;
    }
    let chars = text.chars();
    let mut next = first;
    for (byte, value) in word.char_indices() {
        if source_index_by_byte.get(start + byte) != Some(&next) {
            return None;
        }
        if value as u32 > 0xffff
            && (0xd800..=0xdbff).contains(&pdf_char_unicode(&chars.get(next).ok()?, font_cache))
        {
            let stop = next + 2;
            let decoded = source_range_char(next, stop, |index| {
                chars
                    .get(index)
                    .ok()
                    .map(|character| pdf_char_unicode(&character, font_cache))
            })?;
            if !decoded.to_lowercase().eq(std::iter::once(value)) {
                return None;
            }
            next = stop;
        } else {
            next += 1;
        }
    }
    let segments = text.segments_subset(first, next - first);
    let mut rectangles = segments.iter();
    let segment = rectangles.next()?;
    if rectangles.next().is_some() {
        return None;
    }
    Some(ForwardWordOccurrence {
        first,
        rect: search_rect(segment.bounds()),
    })
}

fn forward_region_contains(region: SearchRect, word: SearchRect) -> bool {
    let x = (word.left + word.right) * 0.5;
    let y = (word.bottom + word.top) * 0.5;
    x >= region.left - 2.0
        && x <= region.right + 2.0
        && y >= region.top.min(region.bottom) - 2.0
        && y <= region.top.max(region.bottom) + 2.0
}

/// Match complete PDF words inside the SyncTeX region. Point-only anchors also
/// allow a unique complete source-context match across text runs and wrapped
/// lines; weak context stays in the anchored run. Ties retain the coarse region.
fn forward_word_rect(
    page: &PdfPage,
    cached: &CachedPageText,
    hint: &ForwardWord,
    region: SearchRect,
    font_cache: &mut TexFontCache,
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
        let Some(occurrence) =
            forward_word_occurrence(&text, &cached.source_index_by_byte, start, word, font_cache)
        else {
            continue;
        };
        let rect = occurrence.rect;
        let score = forward_neighbor_score(&tokens, hint, index);
        if point_only {
            // Typst anchors a source span's first glyph, even if the selected
            // word follows a font switch or a soft wrap. Complete context is
            // evidence across those boundaries; without it, require the
            // actual PDF text run and baseline rather than an arbitrary radius.
            if complete_context_score != Some(score) {
                let character = text
                    .chars()
                    .get(occurrence.first)
                    .map_err(|e| e.to_string())?;
                let object = character.text_object().map_err(|e| e.to_string())?;
                let run = object.bounds().map_err(|e| e.to_string())?.to_rect();
                let origin = object.matrix().map_err(|e| e.to_string())?;
                let baseline = character.origin_y().map_err(|e| e.to_string())?.value;
                if region.left < run.left().value.min(origin.e()) - 2.0
                    || region.left > run.right().value.max(origin.e()) + 2.0
                    || region.top < rect.bottom.min(baseline) - 2.0
                    || region.top > rect.top.max(baseline) + 2.0
                {
                    continue;
                }
            }
        } else if !forward_region_contains(region, rect) {
            continue;
        }
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

fn search_page_ranges(cached: &CachedPageText, needle: &str) -> Vec<(usize, usize)> {
    cached
        .normalized
        .match_indices(needle)
        .filter_map(|(start, _)| {
            let end = start.checked_add(needle.len())?;
            let first = *cached.source_index_by_byte.get(start)?;
            let last = *cached.source_index_by_byte.get(end.checked_sub(1)?)?;
            Some((first, last.saturating_sub(first).saturating_add(1)))
        })
        .collect()
}

fn search_page_segments(text: &PdfPageText, source_ranges: &[(usize, usize)]) -> Vec<SearchRect> {
    let mut rectangles = Vec::new();
    for &(start, count) in source_ranges {
        let segments = text.segments_subset(start, count);
        rectangles.extend(segments.iter().map(|s| search_rect(s.bounds())));
    }
    rectangles
}

fn finish_search_page(
    slot: &CachedPageText,
    ranges: &[(usize, usize)],
    text: Option<&PdfPageText>,
) -> (u32, Vec<SearchRect>, String) {
    let occurrences = u32::try_from(ranges.len()).unwrap_or(u32::MAX);
    let rects = text.map_or_else(Vec::new, |t| search_page_segments(t, ranges));
    (occurrences, rects, search_match_context(slot, ranges[0]))
}

fn search_page(
    document: &PdfDocument,
    page: u32,
    cache: &mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
    needle: &str,
) -> (u32, Vec<SearchRect>, String) {
    let Some(entry) = usize::try_from(page).ok().and_then(|i| cache.get_mut(i)) else {
        return (0, Vec::new(), String::new());
    };
    if let Some(cached) = entry.as_ref().filter(|slot| slot.full_text_initialized) {
        let ranges = search_page_ranges(cached, needle);
        if ranges.is_empty() {
            return (0, Vec::new(), String::new());
        }
        let pdf_page = i32::try_from(page)
            .ok()
            .and_then(|i| document.pages().get(i).ok());
        let text = pdf_page.as_ref().and_then(|p| p.text().ok());
        return finish_search_page(cached, &ranges, text.as_ref());
    }
    let Some(pdf_page) = i32::try_from(page)
        .ok()
        .and_then(|i| document.pages().get(i).ok())
    else {
        return (0, Vec::new(), String::new());
    };
    let Ok(text) = pdf_page.text() else {
        return (0, Vec::new(), String::new());
    };
    let slot = entry.get_or_insert_with(CachedPageText::default);
    init_cached_page_text(slot, &text, font_cache);
    let ranges = search_page_ranges(slot, needle);
    if ranges.is_empty() {
        return (0, Vec::new(), String::new());
    }
    finish_search_page(slot, &ranges, Some(&text))
}

fn search_match_context(cached: &CachedPageText, range: (usize, usize)) -> String {
    let (start, count) = range;
    let end_utf16 = start.saturating_add(count);
    let mut utf16_pos = 0usize;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte_index, ch) in cached.raw.char_indices() {
        if start_byte.is_none() && utf16_pos >= start {
            start_byte = Some(byte_index);
        }
        if utf16_pos >= end_utf16 {
            end_byte = Some(byte_index);
            break;
        }
        utf16_pos += ch.len_utf16();
    }
    let start_byte = start_byte.unwrap_or(cached.raw.len());
    let end_byte = end_byte.unwrap_or(cached.raw.len()).max(start_byte);
    normalize_context_text(&context_window(&cached.raw, start_byte..end_byte, 70, 90))
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
        if let Some(rect) = padded_pixel_rect(page, config, width, height, *rectangle) {
            blend_highlight_rectangle(rgba, width, height, rect, color);
        }
    }
}

#[cfg(test)]
fn extract_page_links(
    document: &PdfDocument,
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
) -> Vec<PageLink> {
    let annotations = extract_page_link_annotations(document, page);
    extract_page_links_with_annotations(page, config, width, height, &annotations, None)
}

fn extract_page_links_with_annotations(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    annotations: &[CachedPageLink],
    loaded_text: Option<&PdfPageText>,
) -> Vec<PageLink> {
    let mut lazy_text = None;
    let mut links: Vec<_> = annotations
        .iter()
        .filter_map(|annotation| {
            let target = annotation.target.as_ref()?.clone();
            let rect = page_rect_to_pixels(page, config, width, height, annotation.bounds)?;
            let text =
                loaded_text.or_else(|| lazy_text.get_or_insert_with(|| page.text().ok()).as_ref());
            let label = text
                .map(|text| link_text_label(&text.inside_rect(annotation.bounds)))
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| link_target_label(&target));
            Some(PageLink {
                rect,
                label,
                target,
            })
        })
        .collect();
    sort_page_links_reading_order(&mut links);
    links
}

const DESTINATION_RATIO_SIZE: i32 = 10_000;

fn extract_page_link_annotations<'a>(
    document: &PdfDocument<'a>,
    page: &PdfPage<'_>,
) -> Vec<CachedPageLink> {
    let config =
        PdfRenderConfig::new().set_fixed_size(DESTINATION_RATIO_SIZE, DESTINATION_RATIO_SIZE);
    let mut target_pages: HashMap<u32, Option<PdfPage<'a>>> = HashMap::new();
    page.links()
        .iter()
        .filter_map(|link| {
            Some(CachedPageLink {
                bounds: link.rect().ok()?,
                target: resolve_link_target(document, &link, &config, &mut target_pages),
            })
        })
        .collect()
}

fn project_page_links(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    annotations: &[CachedPageLink],
    include_contrast_rectangles: bool,
) -> (Vec<PageLink>, Vec<PageLinkRect>) {
    let mut links = Vec::new();
    let mut rectangles = Vec::new();
    for annotation in annotations {
        let Some(rect) = page_rect_to_pixels(page, config, width, height, annotation.bounds) else {
            continue;
        };
        if include_contrast_rectangles {
            rectangles.push(rect);
        }
        if let Some(target) = &annotation.target {
            links.push(PageLink {
                rect,
                label: String::new(),
                target: target.clone(),
            });
        }
    }
    sort_page_links_reading_order(&mut links);
    (links, rectangles)
}

#[cfg(test)]
fn extract_document_links(
    document: &PdfDocument,
    page: &PdfPage,
    source_page: u32,
    text_cache: &mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
) -> Vec<DocumentLink> {
    let annotations = extract_page_link_annotations(document, page);
    extract_document_links_with_annotations(
        document,
        page,
        source_page,
        text_cache,
        font_cache,
        &annotations,
    )
}

fn extract_document_links_with_annotations(
    document: &PdfDocument,
    page: &PdfPage,
    source_page: u32,
    text_cache: &mut [Option<CachedPageText>],
    font_cache: &mut TexFontCache,
    annotations: &[CachedPageLink],
) -> Vec<DocumentLink> {
    if annotations.is_empty() {
        return Vec::new();
    }
    const INDEX_WIDTH: u32 = 1_000;
    let config = PdfRenderConfig::new().set_target_width(i32::try_from(INDEX_WIDTH).unwrap());
    let page_width = page.width().value.max(1.0);
    let index_height =
        ((INDEX_WIDTH as f32 * page.height().value / page_width).ceil() as u32).max(1);
    let page_text = page.text().ok();
    let links = extract_page_links_with_annotations(
        page,
        &config,
        INDEX_WIDTH,
        index_height,
        annotations,
        page_text.as_ref(),
    );
    if links.is_empty() {
        return Vec::new();
    }
    let source_text = cached_page_text_with_loaded(
        document,
        source_page,
        Some(page),
        page_text.as_ref(),
        text_cache,
        font_cache,
    )
    .map(|text| normalize_context_text(&text.raw));
    let mut target_texts = HashMap::<u32, Option<String>>::new();
    let mut label_occurrences = HashMap::<String, usize>::new();
    coalesce_document_link_fragments(links, source_text.as_deref())
        .into_iter()
        .enumerate()
        .map(|(ordinal, link)| {
            let occurrence = label_occurrences.entry(link.label.clone()).or_default();
            let source_context = source_text
                .as_deref()
                .and_then(|text| context_around_label(text, &link.label, *occurrence, 90, 110));
            *occurrence += 1;
            let reference_context = match &link.target {
                LinkTarget::Internal { page, .. } => {
                    citation_number(&link.label).and_then(|number| {
                        let target_text = target_texts.entry(*page).or_insert_with(|| {
                            cached_page_text(document, *page, text_cache, font_cache)
                                .map(|text| normalize_context_text(&text.raw))
                        });
                        target_text
                            .as_deref()
                            .and_then(|text| reference_context(text, number))
                    })
                }
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
    let bits = |v: Option<f32>| v.map(|x| if x == 0.0 { 0 } else { x.to_bits() });
    let is_sorted = links.windows(2).all(|w| w[0].rect.top <= w[1].rect.top);
    let max_top_gap = links
        .iter()
        .map(|l| l.rect.bottom.saturating_sub(l.rect.top).max(1))
        .max()
        .unwrap_or(1)
        .saturating_mul(2)
        .max(2);
    let mut groups: Vec<Vec<usize>> = Vec::with_capacity(links.len());
    let mut by_target = HashMap::with_capacity(links.len());
    for (index, link) in links.iter().enumerate() {
        let key = match &link.target {
            LinkTarget::Internal {
                page,
                top_ratio,
                left_ratio,
            } => (false, *page, bits(*top_ratio), bits(*left_ratio), ""),
            LinkTarget::Uri(uri) => (true, 0, None, None, uri.as_str()),
        };
        let target_groups: &mut Vec<usize> = by_target.entry(key).or_default();
        if is_sorted {
            target_groups.retain(|&group_index| {
                let prev = &links[*groups[group_index].last().expect("link group is not empty")];
                link.rect.top.saturating_sub(prev.rect.top) <= max_top_gap
            });
        }
        if let Some(&group_index) = target_groups.iter().rev().find(|&&group_index| {
            let previous = &links[*groups[group_index].last().expect("link group is not empty")];
            link_fragments_are_adjacent(previous.rect, link.rect)
        }) {
            groups[group_index].push(index);
        } else {
            let group_index = groups.len();
            groups.push(vec![index]);
            target_groups.push(group_index);
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
    let mut normalized = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    normalized
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
    let mut rev = text.get(..range.start).unwrap_or("").char_indices().rev();
    let mut start = (0..before)
        .try_fold(range.start, |_, _| rev.next().map(|(i, _)| i))
        .unwrap_or(0);
    if start > 0 {
        for (i, _) in rev.take_while(|(_, c)| !c.is_whitespace()) {
            start = i;
        }
    }
    let suffix = text.get(range.end..).unwrap_or("");
    let mut fwd = suffix.char_indices();
    let end_rel = match (0..after).try_fold(0, |_, _| fwd.next().map(|(i, c)| i + c.len_utf8())) {
        Some(mut rel) => {
            for (i, c) in fwd.take_while(|(_, c)| !c.is_whitespace()) {
                rel = i + c.len_utf8();
            }
            rel
        }
        None => suffix.len(),
    };
    let end = (range.end + end_rel).min(text.len());
    let trimmed = text.get(start..end).unwrap_or("").trim();
    format!(
        "{}{trimmed}{}",
        if start > 0 { "…" } else { "" },
        if end < text.len() { "…" } else { "" }
    )
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
    let normalized = normalize_context_text(text);
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

fn resolve_link_target<'a>(
    document: &PdfDocument<'a>,
    link: &PdfLink<'_>,
    config: &PdfRenderConfig,
    target_pages: &mut HashMap<u32, Option<PdfPage<'a>>>,
) -> Option<LinkTarget> {
    if let Some(destination) = link.destination() {
        return resolve_internal_destination(document, destination, config, target_pages);
    }
    match link.action()? {
        PdfAction::LocalDestination(action) => {
            resolve_internal_destination(document, action.destination().ok()?, config, target_pages)
        }
        PdfAction::Uri(action) => action.uri().ok().map(LinkTarget::Uri),
        _ => None,
    }
}

fn resolve_internal_destination<'a>(
    document: &PdfDocument<'a>,
    destination: PdfDestination<'_>,
    config: &PdfRenderConfig,
    target_pages: &mut HashMap<u32, Option<PdfPage<'a>>>,
) -> Option<LinkTarget> {
    let page_index = destination.page_index().ok()?;
    let page = u32::try_from(page_index).ok()?;
    let view = destination.view_settings().ok();
    if matches!(
        view,
        Some(
            PdfDestinationViewSettings::FitPageToWindow
                | PdfDestinationViewSettings::FitBoundsToWindow
        )
    ) {
        return Some(LinkTarget::Internal {
            page,
            top_ratio: Some(0.0),
            left_ratio: None,
        });
    }
    let (target_x, target_y) = match view {
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
        .then(|| {
            target_pages
                .entry(page)
                .or_insert_with(|| document.pages().get(page_index).ok())
                .as_ref()
        })
        .flatten();
    let (left_ratio, top_ratio) = target_page
        .and_then(|page| {
            // A fixed square maps both rendered axes directly to normalized positions,
            // including page rotation and the visible crop/media-box origin.
            let (left, top) = page_destination_position(page, config, target_x, target_y)?;
            let ratio =
                |position: i32| (position as f32 / DESTINATION_RATIO_SIZE as f32).clamp(0.0, 1.0);
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

fn padded_pixel_rect(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    bounds: SearchRect,
) -> Option<PixelRect> {
    let (left, right, top, bottom) = page_rect_pixel_bounds(page, config, bounds)?;
    Some(PixelRect {
        left: left.saturating_sub(1).clamp(0, width as i32) as u32,
        right: right.saturating_add(2).clamp(0, width as i32) as u32,
        top: top.saturating_sub(1).clamp(0, height as i32) as u32,
        bottom: bottom.saturating_add(2).clamp(0, height as i32) as u32,
    })
}

fn page_rect_to_pixels(
    page: &PdfPage,
    config: &PdfRenderConfig,
    width: u32,
    height: u32,
    bounds: PdfRect,
) -> Option<PixelRect> {
    padded_pixel_rect(page, config, width, height, search_rect(bounds))
}

fn blend_outlined_rectangle(
    rgba: &mut [u8],
    (width, height): (u32, u32),
    rect: PixelRect,
    color: [u8; 3],
    (fill_alpha, border, border_alpha): (u16, u32, u16),
) {
    blend_rectangle(rgba, width, height, rect, color, fill_alpha);
    blend_rectangle_border(rgba, width, height, rect, border, color, border_alpha);
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
        let rect = link.rect;
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
            blend_outlined_rectangle(
                rgba,
                (width, height),
                rect,
                color,
                (LINK_HIGHLIGHT_ALPHA, 2, LINK_BORDER_ALPHA),
            );
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
    for &rect in rectangles {
        blend_outlined_rectangle(
            rgba,
            (width, height),
            rect,
            color,
            (SELECTED_LINK_HIGHLIGHT_ALPHA, 3, SELECTED_LINK_BORDER_ALPHA),
        );
    }
}

fn blend_rectangle_border(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    rect: PixelRect,
    border: u32,
    color: [u8; 3],
    alpha: u16,
) {
    for edge in [
        PixelRect {
            bottom: rect.top.saturating_add(border).min(rect.bottom),
            ..rect
        },
        PixelRect {
            top: rect.bottom.saturating_sub(border).max(rect.top),
            ..rect
        },
        PixelRect {
            right: rect.left.saturating_add(border).min(rect.right),
            ..rect
        },
        PixelRect {
            left: rect.right.saturating_sub(border).max(rect.left),
            ..rect
        },
    ] {
        blend_rectangle(rgba, width, height, edge, color, alpha);
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
    for rectangle in rectangles {
        let left = rectangle.left.min(width);
        let right = rectangle.right.min(width);
        let top = rectangle.top.min(height);
        let bottom = rectangle.bottom.min(height);
        let Some(background) = dominant_rectangle_color(rgba, width, height, *rectangle) else {
            continue;
        };
        let background_luminance = relative_luminance(background);
        let Some(ink) = darkest_blue_link_ink(
            rgba,
            width,
            height,
            *rectangle,
            background,
            background_luminance,
        ) else {
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
            let Some(row) = rectangle_row_range(rgba.len(), width, y, left, right) else {
                continue;
            };
            for pixel in rgba[row].chunks_exact_mut(4) {
                let original = [pixel[0], pixel[1], pixel[2]];
                if !is_blue_link_ink(original, background, background_luminance) {
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

// Validate a row once, including a possibly truncated final pixel buffer.
fn rectangle_row_range(
    rgba_len: usize,
    width: u32,
    y: u32,
    left: u32,
    right: u32,
) -> Option<std::ops::Range<usize>> {
    if left >= right {
        return None;
    }
    let stride = usize::try_from(width).ok()?.checked_mul(4)?;
    let base = usize::try_from(y).ok()?.checked_mul(stride)?;
    let start = base.checked_add(usize::try_from(left).ok()?.checked_mul(4)?)?;
    let end = base
        .checked_add(usize::try_from(right).ok()?.checked_mul(4)?)?
        .min(rgba_len / 4 * 4);
    (start < end).then_some(start..end)
}

thread_local! {
    static RECTANGLE_COLOR_BUCKETS: RefCell<RectangleColorBuckets> =
        RefCell::new(RectangleColorBuckets::new());
}

struct RectangleColorBuckets {
    values: Vec<(u32, [u64; 3])>,
    touched: Vec<u16>,
}

impl RectangleColorBuckets {
    fn new() -> Self {
        Self {
            values: vec![(0u32, [0u64; 3]); 32_768],
            touched: Vec::with_capacity(32_768),
        }
    }

    fn dominant_color(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        rectangle: PageLinkRect,
    ) -> Option<[u8; 3]> {
        let left = rectangle.left.min(width);
        let right = rectangle.right.min(width);
        let top = rectangle.top.min(height);
        let bottom = rectangle.bottom.min(height);
        for y in top..bottom {
            let Some(row) = rectangle_row_range(rgba.len(), width, y, left, right) else {
                continue;
            };
            for pixel in rgba[row].chunks_exact(4) {
                let key = (u16::from(pixel[0] >> 3) << 10)
                    | (u16::from(pixel[1] >> 3) << 5)
                    | u16::from(pixel[2] >> 3);
                let entry = &mut self.values[usize::from(key)];
                if entry.0 == 0 {
                    self.touched.push(key);
                }
                entry.0 += 1;
                for (sum, channel) in entry.1.iter_mut().zip(pixel) {
                    *sum += u64::from(*channel);
                }
            }
        }
        let mut dominant = (0, [0; 3]);
        for key in self.touched.drain(..) {
            let entry = std::mem::take(&mut self.values[usize::from(key)]);
            if entry.0 > dominant.0 {
                dominant = entry;
            }
        }
        let (count, sums) = dominant;
        (count > 0).then(|| std::array::from_fn(|channel| (sums[channel] / u64::from(count)) as u8))
    }
}

fn dominant_rectangle_color(
    rgba: &[u8],
    width: u32,
    height: u32,
    rectangle: PageLinkRect,
) -> Option<[u8; 3]> {
    RECTANGLE_COLOR_BUCKETS
        .with_borrow_mut(|buckets| buckets.dominant_color(rgba, width, height, rectangle))
}

fn darkest_blue_link_ink(
    rgba: &[u8],
    width: u32,
    height: u32,
    rectangle: PageLinkRect,
    background: [u8; 3],
    background_luminance: f32,
) -> Option<[u8; 3]> {
    let left = rectangle.left.min(width);
    let right = rectangle.right.min(width);
    let top = rectangle.top.min(height);
    let bottom = rectangle.bottom.min(height);
    let mut farthest = None;
    let mut farthest_distance = 0_u32;
    for y in top..bottom {
        let Some(row) = rectangle_row_range(rgba.len(), width, y, left, right) else {
            continue;
        };
        for pixel in rgba[row].chunks_exact(4) {
            let pixel = [pixel[0], pixel[1], pixel[2]];
            if !is_blue_link_ink(pixel, background, background_luminance) {
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

fn is_blue_link_ink(pixel: [u8; 3], background: [u8; 3], background_luminance: f32) -> bool {
    pixel[2].saturating_sub(pixel[0].max(pixel[1])) >= DARK_LINK_BLUE_DOMINANCE
        && relative_luminance(pixel) > background_luminance + 0.005
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

fn srgb_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|channel| {
            let channel = channel as f32 / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        })
    })
}

fn relative_luminance(color: [u8; 3]) -> f32 {
    let lut = srgb_linear_lut();
    lut[usize::from(color[0])] * 0.2126
        + lut[usize::from(color[1])] * 0.7152
        + lut[usize::from(color[2])] * 0.0722
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
    for y in top..bottom {
        let Some(row) = rectangle_row_range(rgba.len(), width, y, left, right) else {
            continue;
        };
        for pixel in rgba[row].chunks_exact_mut(4) {
            for (channel, highlight) in pixel[..3].iter_mut().zip(color) {
                let original = u16::from(*channel);
                *channel = ((original * (255 - alpha) + u16::from(highlight) * alpha) / 255) as u8;
            }
        }
    }
}

fn load_pdfium(library: Option<&Path>) -> Result<Pdfium, String> {
    // pdfium-render binds once per process; afterwards skip cache materialization and dlopen.
    static BOUND: OnceLock<()> = OnceLock::new();
    if library.is_none() && BOUND.get().is_some() {
        return Ok(Pdfium::default());
    }
    let path = match library {
        Some(path) => path.to_path_buf(),
        None => crate::embedded_pdfium::materialize().map_err(|error| {
            format!("could not extract embedded PDFium to the user cache: {error}")
        })?,
    };
    let pdfium = match Pdfium::bind_to_library(&path) {
        Ok(bindings) => Pdfium::new(bindings),
        Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Pdfium::default(),
        Err(error) => {
            return Err(format!(
                "could not load PDFium from {}: {error}",
                path.display()
            ));
        }
    };
    let _ = BOUND.set(());
    Ok(pdfium)
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
        let mut rgb = Vec::new();
        flate2::read::ZlibDecoder::new(frame.compressed_rgba.as_slice())
            .read_to_end(&mut rgb)
            .unwrap();
        assert_eq!(rgb.len(), (frame.width * frame.height * 3) as usize);
        rgb.chunks_exact(3)
            .flat_map(|px| [px[0], px[1], px[2], 255])
            .collect()
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
             \\pdfcompresslevel=0\\relax\\pdfobjcompresslevel=0\\relax\n\
             \\usepackage[paperwidth=400bp,paperheight=600bp,margin=72bp]{geometry}\n\
             \\pagestyle{empty}\n\
             \\newbox\\coordinateform\n\
             \\setbox\\coordinateform=\\hbox{\\vrule width10bp height5bp depth0bp}\n\
             \\immediate\\pdfxform\\coordinateform\n\
             \\edef\\coordinateformid{\\the\\pdflastxform}\n\
             \\begin{document}\n",
        );
        let body = "\\noindent ZenithAlpha confirms the original page origin.\\par\n\
                    \\vspace{110bp}\n\
                    \\noindent MeridianBeta identifies a different source line.\\par\n\
                    \\vspace{110bp}\n\
                    \\noindent NadirGamma identifies the bottom region.\\par\n";
        // Identical input bodies must retain their own native page geometry.
        let mut sources = Vec::with_capacity(cases.len());
        for (index, (rotation, crop, media)) in cases.iter().enumerate() {
            if index != 0 {
                tex.push_str("\\newpage\n");
            }
            writeln!(tex, "\\pdfpageattr{{/Rotate {rotation} {crop} {media}}}").unwrap();
            let source_name = format!("coordinates-{index}.tex");
            let input = directory.path().join(&source_name);
            std::fs::write(&input, body).unwrap();
            sources.push(input.canonicalize().unwrap());
            writeln!(tex, "\\input{{{source_name}}}").unwrap();
            tex.push_str("\\pdfrefxform\\coordinateformid\\par\n");
            if index == 1 {
                tex.push_str("\\noindent OverlayTwoNeedle distinctive\\par\n");
            }
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
        let mut navigation = NavigationWorker::new();
        let revision = synctex::DocumentRevision::read(&pdf).unwrap();
        worker.begin_generation(1);
        let mut batch_input = String::new();
        for (index, source) in sources.iter().enumerate() {
            let line = 1;
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
            let column = body
                .lines()
                .nth(line as usize - 1)
                .unwrap()
                .find("ZenithAlpha")
                .unwrap() as u32
                + 1;
            let request = synctex::resolve_forward(&pdf, source, line, column).unwrap();
            assert_eq!(request.page, index as u32 + 1);
            let hint = request.word.as_ref().unwrap();
            assert_eq!(hint.words[hint.selected], "ZenithAlpha");
            if index == 0 {
                let anchor = synctex::ForwardCandidate {
                    page: request.page,
                    source_line: line,
                    rect: request.rect(),
                };
                let empty_later = synctex::ForwardCandidate {
                    page: 2,
                    source_line: line,
                    rect: super::SearchRect {
                        left: 72.0,
                        right: 72.0,
                        top: 100.0,
                        bottom: 100.0,
                    },
                };
                let selected = super::select_forward_line_candidate(
                    &pdf,
                    &[anchor, empty_later],
                    &synctex::ForwardWord {
                        words: ["OverlayTwoNeedle", "distinctive"].map(String::from).into(),
                        selected: 0,
                    },
                    anchor,
                    None,
                )
                .unwrap();
                assert_eq!(selected.page, anchor.page);
                assert_eq!(selected.rect, anchor.rect);
            }
            for (fit, zoom) in [
                (super::FitMode::Page, 100),
                (super::FitMode::Width, 125),
                (super::FitMode::Content, 125),
            ] {
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
                    &page,
                    super::page_content_bounds(&page),
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
                        best_guess: false,
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
                assert_eq!(inverse.location.file.as_str(), source.to_str().unwrap());
                assert!(inverse.location.precise, "{:?}", inverse.location);
                assert_eq!(inverse.location.byte_column, (column - 1) as usize);
                assert!(inverse.warning.is_none(), "{:?}", inverse.warning);

                // Compare actual flashed pixels with PDFium's painted text bounds,
                // not merely the forward/inverse conversion's own round trip.
                worker.flash(
                    1,
                    index as u32,
                    request.rect(),
                    Some(synctex::ForwardWord {
                        words: ["ZenithAlpha", "confirms", "the", "original"]
                            .map(String::from)
                            .into(),
                        selected: 0,
                    }),
                );
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
            false,
        )
        .unwrap();
        let records = String::from_utf8(batch_output).unwrap();
        assert_eq!(records.lines().count(), cases.len());
        for (record, source) in records.lines().zip(sources) {
            let record: serde_json::Value = serde_json::from_str(record).unwrap();
            assert_eq!(record["ok"], true, "{record}");
            assert_eq!(record["pdf_word"], "ZenithAlpha");
            assert_eq!(record["location"]["file"], source.to_str().unwrap());
            assert_eq!(record["location"]["line"], 1);
            assert_eq!(record["location"]["precise"], true);
            assert!(record["warning"].is_null(), "{record}");
        }
    }

    #[test]
    fn inter_row_gap_click_keeps_coarse_line_when_glyph_and_synctex_rows_disagree() {
        let _native = super::pdfium_test_lock();
        use crate::{
            editor::Editor,
            navigation::{InverseTask, NavigationWorker},
            process::Operation,
            synctex,
        };
        use pdfium_render::prelude::{PdfPoints, PdfRenderConfig};
        use std::{fmt::Write, process::Command, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("rows.tex");
        let rows = [
            "\\noindent\\fontsize{8}{10}\\selectfont Amber Quartz Copper Zenith Birch Maple.\\par",
            "\\noindent\\fontsize{10}{12}\\selectfont Cedar Violet Indigo Silver Saffron Willow.\\par",
        ];
        std::fs::write(
            &source,
            format!(
                "\\documentclass{{article}}\n\
                 \\usepackage[paperwidth=360pt,paperheight=120pt,margin=24pt]{{geometry}}\n\
                 \\pagestyle{{empty}}\n\\begin{{document}}\n{}\n{}\n\\end{{document}}\n",
                rows[0], rows[1],
            ),
        )
        .unwrap();
        let output = crate::process::output(
            Command::new("pdflatex")
                .current_dir(directory.path())
                .args([
                    "-interaction=nonstopmode",
                    "-halt-on-error",
                    "-synctex=1",
                    "rows.tex",
                ]),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for real coordinate regressions");
        assert!(output.status.success());
        let pdf = directory.path().join("rows.pdf");
        let source = source.canonicalize().unwrap();
        // Original-paper points in the gap between rows, outside every glyph's
        // layout box: PDFium's nearest glyph lies on one row while the point's
        // SyncTeX box belongs to the other. Neither owns the click, so a word must
        // not be claimed; moving the query into the PDFium glyph stole neighboring
        // words at real viewer cell centers. An interior point is the control.
        let cases = [
            (36.0, 37.75, "Cedar", 5, None),
            (175.0, 36.2, "Maple", 6, None),
            (36.0, 45.0, "Cedar", 6, rows[1].find("Cedar")),
        ];

        let pdfium = super::load_pdfium(None).unwrap();
        let document = pdfium.load_pdf_from_file(&pdf, None).unwrap();
        let page = document.pages().get(0).unwrap();
        let worker = super::RenderWorker::spawn(1, pdf.clone(), None);
        assert_eq!(worker.wait_until_ready().unwrap().0, 1);
        let mut navigation = NavigationWorker::new();
        let revision = synctex::DocumentRevision::read(&pdf).unwrap();
        worker.begin_generation(1);
        let key = super::RenderKey {
            document_id: 1,
            page: 0,
            width: 800,
            height: 600,
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
        let config = super::build_fit_config(
            PdfRenderConfig::new(),
            key.fit,
            800,
            600,
            &page,
            super::page_content_bounds(&page),
        );
        worker
            .render(super::RenderRequest { key, generation: 1 })
            .unwrap();
        coordinate_frame(&worker);
        let mut batch_input = String::new();
        for (request_id, &(x, y, word, line, byte)) in (1..).zip(&cases) {
            writeln!(
                batch_input,
                "{}",
                serde_json::json!({"page": 1, "x": x, "y": y})
            )
            .unwrap();
            let (pixel_x, pixel_y) = page
                .points_to_pixels(PdfPoints::new(x), PdfPoints::new(120.0 - y), &config)
                .unwrap();
            worker.page_point(revision, request_id, pixel_x as u32, pixel_y as u32, key);
            let super::WorkerMessage::PagePoint { result, .. } = worker
                .message_rx
                .recv_timeout(Duration::from_secs(15))
                .unwrap()
            else {
                panic!("missing inter-row inverse point");
            };
            let click = result.unwrap();
            let (context, offset) = click.text.clone().unwrap().unwrap();
            assert!(
                context
                    .match_indices(word)
                    .any(|(start, _)| (start..start + word.len()).contains(&offset)),
                "hit offset {offset} in {context:?} is not inside {word}"
            );
            let raw = synctex::resolve_inverse(
                &pdf,
                click.synctex,
                None,
                0,
                &Operation::default(),
                false,
            )
            .unwrap();
            assert_eq!(raw.location.line, line, "{word} at ({x}, {y})");
            navigation
                .submit(InverseTask {
                    request_id,
                    path: pdf.clone(),
                    revision,
                    page: 0,
                    click,
                    word_precision: true,
                    radius: 4,
                    editor: Editor::None,
                    inverse_search: None,
                    best_guess: false,
                    operation: Operation::default(),
                })
                .unwrap();
            let inverse = navigation
                .replies
                .recv_timeout(Duration::from_secs(15))
                .unwrap()
                .result
                .unwrap();
            assert_eq!(inverse.location.file.as_str(), source.to_str().unwrap());
            assert_eq!(
                (inverse.location.line, inverse.location.byte_column),
                (line, byte.unwrap_or(0)),
                "{word} at ({x}, {y})"
            );
            assert_eq!(
                inverse.location.precise,
                byte.is_some(),
                "{word} at ({x}, {y})"
            );
        }
        close_test_worker(&worker, revision);
        let mut batch_output = Vec::new();
        super::synctex_edit_batch(
            &pdf,
            None,
            &crate::config::ViewerSettings::default(),
            std::io::Cursor::new(batch_input),
            &mut batch_output,
            false,
        )
        .unwrap();
        let records = String::from_utf8(batch_output).unwrap();
        assert_eq!(records.lines().count(), cases.len());
        for (record, &(_, _, word, line, byte)) in records.lines().zip(&cases) {
            let record: serde_json::Value = serde_json::from_str(record).unwrap();
            assert_eq!(record["pdf_word"], word, "{record}");
            assert_eq!(record["location"]["line"], line, "{record}");
            assert_eq!(
                record["location"]["byte_column"],
                byte.unwrap_or(0),
                "{record}"
            );
            assert_eq!(record["location"]["precise"], byte.is_some(), "{record}");
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
    fn visible_text_cache_is_independent_and_reprojects_cached_glyphs() {
        let _native = super::pdfium_test_lock();
        let pdf = synthetic_forward_pdf(
            "/MediaBox [0 0 400 600]",
            "BT /F1 12 Tf 20 300 Td (zorp suv) Tj ET\n\
             BT /F1 12 Tf 3 Tr 20 200 Td (miv) Tj ET\n\
             BT /F1 12 Tf 7 Tr 20 100 Td (vup) Tj ET\n\
             BT /F1 12 Tf 0 Tr 1000 50 Td (nux) Tj ET",
        );
        let pdfium = super::load_pdfium(None).unwrap();
        let document = pdfium.load_pdf_from_file(pdf.path(), None).unwrap();
        let page = document.pages().get(0).unwrap();
        let mut cache = super::empty_text_cache(1);
        let mut font_cache = std::collections::HashMap::new();
        let generation = super::AtomicU64::new(4);
        assert!(
            super::cached_visible_page_text(&page, 0, &mut cache, &mut font_cache, 3, &generation)
                .is_none()
        );
        assert!(cache[0].as_ref().unwrap().visible.is_none());
        let visible =
            super::cached_visible_page_text(&page, 0, &mut cache, &mut font_cache, 4, &generation)
                .unwrap();
        assert_eq!(visible.normalized, "zorp 𝑠uv");
        assert!(
            visible
                .glyphs
                .iter()
                .filter(|glyph| !glyph.value.is_whitespace())
                .all(|glyph| glyph.bounds.iter().any(Option::is_some))
        );
        let glyphs = visible.glyphs.as_ptr();
        let slot = cache[0].as_ref().unwrap();
        assert!(!slot.full_text_initialized);
        assert!(slot.raw.is_empty() && slot.normalized.is_empty());

        let key = super::RenderKey {
            document_id: 1,
            page: 0,
            width: 400,
            height: 600,
            zoom: 100,
            fit: super::FitMode::Page,
            invert: false,
            dark_mode_style: NEUTRAL_DARK_MODE,
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [255, 255, 0],
            selected_link_ordinal: None,
        };
        let mut page_cache = std::collections::HashMap::new();
        let matches = super::find_visible_matches(
            &document,
            1,
            "zo",
            &[
                (key, 400, 600),
                (super::RenderKey { zoom: 200, ..key }, 800, 1200),
            ],
            &mut cache,
            &mut page_cache,
            &mut font_cache,
            4,
            &generation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].next_char, Some('r'));
        assert_eq!(matches[0].char_index, matches[1].char_index);
        assert!((matches[1].rects[0].left + 1).abs_diff((matches[0].rects[0].left + 1) * 2) <= 1);
        assert!((matches[1].rects[0].top + 1).abs_diff((matches[0].rects[0].top + 1) * 2) <= 1);
        let supplementary = super::find_visible_matches(
            &document,
            1,
            "𝑠",
            &[(key, 400, 600)],
            &mut cache,
            &mut page_cache,
            &mut font_cache,
            4,
            &generation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(supplementary.len(), 1);
        assert_eq!(supplementary[0].next_char, Some('u'));

        let full = super::cached_page_text(&document, 0, &mut cache, &mut font_cache).unwrap();
        assert!(full.full_text_initialized && full.raw.contains("miv"));
        assert_eq!(full.visible.as_ref().unwrap().glyphs.as_ptr(), glyphs);
        assert_eq!(full.visible.as_ref().unwrap().normalized, "zorp 𝑠uv");
    }

    #[test]
    fn type1_inspection_preserves_explicit_slots_and_header_boundary() {
        let data = b"%!FontType1\n/Encoding 256 array\ndup 96 /.notdef put\ndup 97 /zorp put\ndup 98/vup put\neexec\ndup 99 /nux put";
        let slots = super::tex_type1_slots(data, "ABCDEF+fixture").unwrap();
        assert!(slots[97] && slots[98]);
        assert!(!slots[96] && !slots[99]);
        assert_eq!(super::tex_type1_slots(data, "AB+lmfixture"), Some(slots));
        assert!(
            super::tex_type1_slots(data, "ABCDEF+lmfixture")
                .unwrap()
                .into_iter()
                .all(|slot| slot)
        );
        let mut pfb = b"\x80\x01\0\0\0\0".to_vec();
        pfb.extend_from_slice(data);
        assert_eq!(super::tex_type1_slots(&pfb, "ABCDEF+fixture"), Some(slots));
        assert!(super::tex_type1_slots(b"not a font", "lmfixture").is_none());
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
        assert!(super::same_forward_word("Žora", "Zora"));
        assert!(super::same_forward_word("Cafe\u{301}", "café"));
        assert!(!super::same_forward_word("Zora", "Alpha"));
    }

    fn synthetic_forward_pdf(geometry: &str, contents: &str) -> tempfile::NamedTempFile {
        // Exercise PDFium's two-unit supplementary Unicode representation in
        // real text runs, including mathematical letters inside context words.
        let cmap = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
            /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
            /CMapName /SupplementaryS def\n/CMapType 2 def\n\
            1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
            1 beginbfchar\n<73> <D835DC60>\nendbfchar\n\
            endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R {geometry} /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            ),
            format!(
                "<< /Length {} >>\nstream\n{contents}\nendstream",
                contents.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /ToUnicode 6 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len()),
        ];
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = bytes.len();
        bytes.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        bytes.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        let pdf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(pdf.path(), bytes).unwrap();
        pdf
    }

    const FORWARD_ROWS: &str = "BT /F1 12 Tf 20 300 Td (alpha needle beta) Tj ET\n\
        BT /F1 12 Tf 20 200 Td (gamma needle delta) Tj ET";

    fn forward_row(baseline: f32) -> crate::synctex::ForwardCandidate {
        crate::synctex::ForwardCandidate {
            page: 1,
            source_line: 0,
            rect: super::SearchRect {
                left: 0.0,
                right: 390.0,
                top: 600.0 - (baseline + 15.0),
                bottom: 600.0 - (baseline - 5.0),
            },
        }
    }

    fn assert_forward_candidate_paints(
        pdf: &std::path::Path,
        selected: crate::synctex::ForwardCandidate,
        first_value: u32,
        unit_count: usize,
        baseline: f32,
        hint: crate::synctex::ForwardWord,
    ) {
        let key = super::RenderKey {
            document_id: 1,
            page: selected.page - 1,
            width: 400,
            height: 600,
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
        // Locate the expected painted glyph span independently of the raw
        // request and its conversion; every fixture has 600bp original paper.
        let expected = {
            let pdfium = super::load_pdfium(None).unwrap();
            let document = pdfium.load_pdf_from_file(pdf, None).unwrap();
            let page = document.pages().get(key.page as i32).unwrap();
            let text = page.text().unwrap();
            let first = text
                .chars()
                .iter()
                .position(|character| {
                    character.unicode_value() == first_value
                        && character
                            .origin_y()
                            .is_ok_and(|y| (y.value - baseline).abs() < 0.1)
                })
                .unwrap();
            let segments = text.segments_subset(first, unit_count);
            assert_eq!(segments.len(), 1);
            let bounds = segments.iter().next().unwrap().bounds();
            let config = super::build_fit_config(
                pdfium_render::prelude::PdfRenderConfig::new(),
                key.fit,
                i32::from(key.width),
                i32::from(key.height),
                &page,
                super::page_content_bounds(&page),
            );
            super::page_rect_pixel_bounds(
                &page,
                &config,
                super::SearchRect {
                    left: bounds.left().value,
                    right: bounds.right().value,
                    top: bounds.top().value,
                    bottom: bounds.bottom().value,
                },
            )
            .unwrap()
        };
        let worker = super::RenderWorker::spawn(1, pdf.to_path_buf(), None);
        worker.wait_until_ready().unwrap();
        let revision = super::DocumentRevision::read(pdf).unwrap();
        worker.begin_generation(1);
        worker
            .render(super::RenderRequest { key, generation: 1 })
            .unwrap();
        let plain = coordinate_frame(&worker);
        worker.flash(1, key.page, selected.rect, Some(hint));
        worker
            .render(super::RenderRequest { key, generation: 1 })
            .unwrap();
        let painted = coordinate_frame(&worker);
        let flash = painted.flash.as_ref().unwrap();
        assert!(flash.error.is_none(), "{:?}", flash.error);
        assert!(flash.word_precise);
        assert_eq!(flash.pixel_bounds, Some(expected));
        let before = coordinate_rgba(&plain);
        let after = coordinate_rgba(&painted);
        let mut changed = (u32::MAX, 0, u32::MAX, 0);
        for (pixel, (before, after)) in before
            .as_chunks::<4>()
            .0
            .iter()
            .zip(after.as_chunks::<4>().0)
            .enumerate()
        {
            if before != after {
                let x = pixel as u32 % painted.width;
                let y = pixel as u32 / painted.width;
                changed.0 = changed.0.min(x);
                changed.1 = changed.1.max(x + 1);
                changed.2 = changed.2.min(y);
                changed.3 = changed.3.max(y + 1);
            }
        }
        assert_eq!(
            changed,
            (
                expected.0.saturating_sub(1).clamp(0, painted.width as i32) as u32,
                expected.1.saturating_add(2).clamp(0, painted.width as i32) as u32,
                expected.2.saturating_sub(1).clamp(0, painted.height as i32) as u32,
                expected.3.saturating_add(2).clamp(0, painted.height as i32) as u32,
            )
        );
        close_test_worker(&worker, revision);
    }

    #[test]
    fn forward_context_keeps_supplementary_letters_between_repeated_words() {
        let _native = super::pdfium_test_lock();
        let pdf = synthetic_forward_pdf(
            "/MediaBox [0 0 400 600]",
            "BT /F1 12 Tf 20 500 Td (Rung mostly s bramble turnip lanterns) Tj ET\n\
             BT /F1 12 Tf 20 400 Td (Rung at once s lanterns tilted near d) Tj ET\n\
             BT /F1 12 Tf 20 300 Td (Rung mostly s pebble crate of the bramble kettle) Tj ET\n\
             BT /F1 12 Tf 20 200 Td (Rung at once s pebble crate of d) Tj ET\n\
             BT /F1 12 Tf 20 100 Td (Rung mostly s plover lanterns for the copper widgets) Tj ET",
        );
        for (baseline, context) in [
            (500.0, "Rung mostly s bramble turnip lanterns"),
            (400.0, "Rung at once s lanterns tilted near"),
            (300.0, "Rung mostly s pebble crate of the"),
            (200.0, "Rung at once s pebble crate of"),
            (100.0, "Rung mostly s plover lanterns for the"),
        ] {
            let selection = crate::synctex::ForwardWord {
                words: context.split_whitespace().map(str::to_owned).collect(),
                selected: 0,
            };
            assert_forward_candidate_paints(
                pdf.path(),
                forward_row(baseline),
                u32::from('R'),
                4,
                baseline,
                selection,
            );
        }
    }

    #[test]
    fn forward_decoded_words_include_complete_supplementary_glyphs() {
        let _native = super::pdfium_test_lock();
        for (line, unit_count) in [("alpha squared beta", 8), ("alpha betas gamma", 6)] {
            let pdf = synthetic_forward_pdf(
                "/MediaBox [0 0 400 600]",
                &format!("BT /F1 12 Tf 20 200 Td ({line}) Tj ET"),
            );
            let pdfium = super::load_pdfium(None).unwrap();
            let document = pdfium.load_pdf_from_file(pdf.path(), None).unwrap();
            let page = document.pages().get(0).unwrap();
            let text = page.text().unwrap();
            let mut font_cache = std::collections::HashMap::new();
            let (normalized, indexes) =
                super::visible_forward_text(&page, &mut font_cache).unwrap();
            let (start, word) = super::words(&normalized)[1];
            let selected =
                super::forward_word_occurrence(&text, &indexes, start, word, &mut font_cache)
                    .unwrap();
            let segments = text.segments_subset(6, unit_count);
            let expected = segments.iter().next().unwrap().bounds();
            assert_eq!(selected.first, 6);
            assert_eq!(
                selected.rect,
                super::SearchRect {
                    left: expected.left().value,
                    right: expected.right().value,
                    top: expected.top().value,
                    bottom: expected.bottom().value,
                },
            );
        }
    }

    #[test]
    fn forward_worker_preserves_original_paper_coordinates_and_transformed_paint() {
        let _native = super::pdfium_test_lock();
        let hint = crate::synctex::ForwardWord {
            words: ["gamma", "needle", "delta"].map(String::from).into(),
            selected: 1,
        };
        for rotation in [0, 90, 180, 270] {
            let pdf = synthetic_forward_pdf(
                &format!("/MediaBox [20 30 420 630] /CropBox [40 60 390 590] /Rotate {rotation}"),
                FORWARD_ROWS,
            );
            let selected = forward_row(200.0);
            assert_forward_candidate_paints(
                pdf.path(),
                selected,
                u32::from('n'),
                6,
                200.0,
                hint.clone(),
            );
        }
    }

    #[test]
    fn forward_worker_word_hints_stay_in_line_and_retain_uncertain_regions() {
        let _native = super::pdfium_test_lock();
        let pdf = synthetic_forward_pdf(
            "/MediaBox [0 0 400 600]",
            "BT /F1 12 Tf 20 300 Td (alpha needle beta) Tj ET\n\
             BT /F1 12 Tf 20 200 Td (alpha needle beta gamma needle delta) Tj ET",
        );
        let key = super::RenderKey {
            document_id: 1,
            page: 0,
            width: 400,
            height: 600,
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
        let worker = super::RenderWorker::spawn(1, pdf.path().to_path_buf(), None);
        worker.wait_until_ready().unwrap();
        let revision = super::DocumentRevision::read(pdf.path()).unwrap();
        worker.begin_generation(1);
        for (baseline, context, selected, precise) in [
            // Better context on another line must not beat the compiler region.
            (300.0, "gamma needle delta", 1, true),
            // Neighbor context may distinguish two occurrences on the same line.
            (200.0, "gamma needle delta", 1, true),
            (200.0, "needle", 0, false),
            (300.0, "delta", 0, false),
            (300.0, "missing", 0, false),
        ] {
            let region = forward_row(baseline).rect;
            worker.flash(
                1,
                0,
                region,
                Some(crate::synctex::ForwardWord {
                    words: context.split_whitespace().map(str::to_owned).collect(),
                    selected,
                }),
            );
            worker
                .render(super::RenderRequest { key, generation: 1 })
                .unwrap();
            let frame = coordinate_frame(&worker);
            let flash = frame.flash.as_ref().unwrap();
            assert!(flash.error.is_none(), "{context}: {:?}", flash.error);
            assert_eq!(flash.word_precise, precise, "{baseline}: {context}");
            assert!(flash.pixel_bounds.is_some());
            if precise {
                assert!(flash.rect.bottom > baseline - 5.0, "{flash:?}");
                assert!(flash.rect.top < baseline + 15.0, "{flash:?}");
                assert!(flash.rect.right - flash.rect.left < 50.0, "{flash:?}");
                if baseline == 200.0 {
                    assert!(flash.rect.left > 150.0, "{flash:?}");
                } else {
                    assert!(flash.rect.right < 150.0, "{flash:?}");
                }
            } else {
                assert_eq!(
                    flash.rect,
                    super::SearchRect {
                        left: region.left,
                        right: region.right,
                        top: baseline + 15.0,
                        bottom: baseline - 5.0,
                    },
                    "{context}",
                );
            }
        }
        close_test_worker(&worker, revision);
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
        DarkModeStyle, DarkModeTransform, LinkTarget, PageLink, PageLinkRect, PixelRect,
        apply_dark_mode_link_contrast, apply_link_highlights, apply_selected_link_highlights,
        blend_highlight_rectangle, context_around_label, contrast_ratio, count_search_matches,
        dark_mode_pixel, darken_rgba, dominant_rectangle_color, mask_quadrilateral,
        normalize_search_text, reference_context, selected_page_link_rectangles,
    };

    const NEUTRAL_DARK_MODE: DarkModeStyle = DarkModeStyle::new([30, 30, 30], [209, 209, 209]);

    #[test]
    fn link_context_includes_surrounding_text() {
        let text = "Earlier crates hold copper turnips. A lopsided kettle simmers in damp gravel [23] beside bramble widgets. Later lanterns follow.";

        let context = context_around_label(text, "[23]", 0, 40, 20).expect("context");

        assert!(context.contains("kettle simmers in damp gravel [23]"));
        assert!(context.starts_with('…'));
        assert!(context.ends_with('…'));
    }

    #[test]
    fn link_context_expands_truncated_edges_to_complete_words() {
        let text = "alpha turnip teakettle omega tail";

        let context = context_around_label(text, "teakettle", 0, 4, 2).expect("context");

        assert_eq!(context, "…turnip teakettle omega…");
    }

    #[test]
    fn reference_context_stops_before_the_next_numbered_reference() {
        let text = "[22] Prior et al. Prior notes. [23] Zabbo et al. On lopsided teakettles of gravel. [24] Later et al. Later notes.";

        let context = reference_context(text, "23").expect("reference context");

        assert!(context.starts_with("[23] Zabbo et al."), "{context:?}");
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
        let mut font_cache = std::collections::HashMap::new();
        let cached = super::cached_page_text(&document, 0, &mut cache, &mut font_cache).unwrap();
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
        let rect = super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
            .unwrap()
            .unwrap();
        assert!(rect.left > 100.0 && rect.right < 155.0, "{rect:?}");
        assert!(rect.bottom >= 198.0 && rect.top < 215.0, "{rect:?}");

        // Matching only part of the context must not escape the anchored run.
        hint.words[1] = "missing".into();
        assert!(
            super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
                .unwrap()
                .is_none()
        );
        hint.words = ["anchor", "after"].map(String::from).into();
        hint.selected = 0;
        assert!(
            super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
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
        let wrapped =
            super::forward_word_rect(&page, cached, &hint, wrapped_anchor, &mut font_cache)
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
                &mut font_cache,
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
                &mut font_cache,
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
        let mut font_cache = std::collections::HashMap::new();
        let cached = super::cached_page_text(&document, 0, &mut cache, &mut font_cache).unwrap();
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
        let first = super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
            .unwrap()
            .unwrap();
        assert!(first.left > 60.0 && first.right < 115.0, "{first:?}");
        assert!(first.bottom >= 98.0 && first.top < 115.0, "{first:?}");

        // The span's start can precede the selected word by several words.
        // Context selects the second occurrence without reaching other runs.
        hint.words = ["gamma", "needle", "delta"].map(String::from).into();
        let second = super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
            .unwrap()
            .unwrap();
        assert!(second.left > 190.0 && second.right < 245.0, "{second:?}");
        assert!(second.bottom >= 98.0 && second.top < 115.0, "{second:?}");

        hint.selected = 0;
        for word in ["needle", "need", "elsewhere"] {
            hint.words = vec![word.into()];
            assert!(
                super::forward_word_rect(&page, cached, &hint, anchor, &mut font_cache)
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
                super::forward_word_rect(&page, cached, &hint, point, &mut font_cache)
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
            &mut font_cache,
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
        let mut font_cache = std::collections::HashMap::new();
        let text = cached_page_text(&text_document, 0, &mut cache, &mut font_cache)
            .expect("cached page text");
        assert_eq!(
            count_search_matches(&text.normalized, "synthetic needle"),
            2
        );
        let (occurrences, rectangles, context) = search_page(
            &text_document,
            0,
            &mut cache,
            &mut font_cache,
            "synthetic needle",
        );
        assert_eq!(occurrences, 2);
        assert!(!rectangles.is_empty());
        assert!(context.contains("Synthetic Needle"));

        let page = text_document.pages().get(0).expect("text page");
        let cached = cached_page_text(&text_document, 0, &mut cache, &mut font_cache).unwrap();
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
            super::forward_word_rect(&page, cached, &hint, region, &mut font_cache)
                .unwrap()
                .is_none()
        );
        hint.words = ["synthetic", "needle", "synthetic", "needle"]
            .map(String::from)
            .into();
        let mut previous = 0.0;
        for selected in 0..4 {
            hint.selected = selected;
            let rect = super::forward_word_rect(&page, cached, &hint, region, &mut font_cache)
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
        let rect = super::forward_word_rect(&page, cached, &hint, right_half, &mut font_cache)
            .unwrap()
            .unwrap();
        assert!(rect.left > 200.0);
        hint.words[0] = "need".into();
        assert!(
            super::forward_word_rect(&page, cached, &hint, region, &mut font_cache)
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
        let mut link_font_cache = std::collections::HashMap::new();
        let document_links = extract_document_links(
            &link_document,
            &link_page,
            6,
            &mut link_text_cache,
            &mut link_font_cache,
        );
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

    #[test]
    fn content_fit_bounds_sparse_page_bitmap_sizes() {
        for (width, height) in [(600.0, 600.0), (1.0, 100_000.0)] {
            let (raster_width, raster_height) = super::FitMode::Content.raster_size(
                width,
                height,
                [0.5, 0.5, 0.500_001, 0.500_001],
                1000,
                1000,
            );
            assert!(raster_width > 0 && raster_height > 0);
            assert!(raster_width <= 32_768 && raster_height <= 32_768);
            assert!(u64::from(raster_width) * u64::from(raster_height) <= 16_777_216);
        }
    }

    #[test]
    fn content_fit_includes_images_and_empty_pages_keep_page_fit() {
        let _native = super::pdfium_test_lock();
        use pdfium_render::prelude::*;
        let pdfium = super::load_pdfium(None).unwrap();
        let document = pdfium
            .load_pdf_from_byte_vec(synthetic_image_pdf(), None)
            .unwrap();
        let page = document.pages().get(0).unwrap();
        let content = super::page_content_bounds(&page);
        for (actual, expected) in content.into_iter().zip([0.25, 0.25, 0.75, 0.75]) {
            assert!((actual - expected).abs() < 0.0001);
        }
        let config = super::build_fit_config(
            PdfRenderConfig::new(),
            super::FitMode::Content,
            400,
            300,
            &page,
            content,
        );
        let bitmap = page.render_with_config(&config).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (576, 576));
        let mut empty = pdfium.create_new_pdf().unwrap();
        let page = empty
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::from_points(
                PdfPoints::new(400.0),
                PdfPoints::new(600.0),
            ))
            .unwrap();
        let content = super::page_content_bounds(&page);
        assert_eq!(content, [0.0, 0.0, 1.0, 1.0]);
        let config = super::build_fit_config(
            PdfRenderConfig::new(),
            super::FitMode::Content,
            400,
            300,
            &page,
            content,
        );
        let bitmap = page.render_with_config(&config).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (192, 288));
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
        let config = build_fit_config(
            PdfRenderConfig::new(),
            FitMode::Page,
            400,
            400,
            &page,
            super::page_content_bounds(&page),
        );
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
    fn cached_annotations_project_targets_and_unresolved_contrast_rectangles() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        let document = pdfium
            .load_pdf_from_byte_vec(
                synthetic_link_pdf_geometry(
                    "XYZ 200 300 null",
                    "/Rotate 270 /CropBox [100 100 400 400]",
                ),
                None,
            )
            .unwrap();
        let page = document.pages().get(0).unwrap();
        let mut annotations = super::extract_page_link_annotations(&document, &page);
        assert_eq!(annotations.len(), 2);
        annotations.push(super::CachedPageLink {
            bounds: annotations[0].bounds,
            target: None,
        });
        for width in [400, 800] {
            let config =
                pdfium_render::prelude::PdfRenderConfig::new().set_target_width(width as i32);
            let indexed = super::extract_page_links(&document, &page, &config, width, width);
            let (links, rectangles) =
                super::project_page_links(&page, &config, width, width, &annotations, true);
            assert_eq!(links.len(), 2);
            assert_eq!(rectangles.len(), 3);
            for (link, indexed) in links.iter().zip(indexed) {
                assert!(link.label.is_empty());
                assert!(!indexed.label.is_empty());
                assert_eq!(link.target, indexed.target);
                assert_eq!(link.rect, indexed.rect);
            }
            let (plain, rectangles) =
                super::project_page_links(&page, &config, width, width, &annotations, false);
            assert_eq!(plain, links);
            assert!(rectangles.is_empty());
        }
    }

    #[test]
    fn reopening_a_document_invalidates_cached_link_destinations() {
        let _native = super::pdfium_test_lock();
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(first.path(), synthetic_link_pdf("XYZ null 300 null")).unwrap();
        std::fs::write(second.path(), synthetic_link_pdf("XYZ null 100 null")).unwrap();
        let worker = super::RenderWorker::spawn(1, first.path().to_path_buf(), None);
        let (_, _, mut revision) = worker.wait_until_ready().unwrap();
        worker.begin_generation(1);
        let key = super::RenderKey {
            document_id: 1,
            page: 0,
            width: 400,
            height: 400,
            zoom: 100,
            fit: super::FitMode::Content,
            invert: false,
            dark_mode_style: NEUTRAL_DARK_MODE,
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [255, 255, 0],
            selected_link_ordinal: None,
        };
        for top in [0.25, 0.75] {
            worker
                .render(super::RenderRequest { key, generation: 1 })
                .unwrap();
            let frame = coordinate_frame(&worker);
            assert_eq!(frame.revision, revision);
            assert!(frame.links.iter().all(|link| link.label.is_empty()));
            assert!(frame.links.iter().any(|link| matches!(link.target,
                LinkTarget::Internal { top_ratio: Some(ratio), .. } if ratio == top)));
            if top == 0.25 {
                worker.open(1, second.path().to_path_buf()).unwrap();
                match worker
                    .message_rx
                    .recv_timeout(std::time::Duration::from_secs(15))
                    .unwrap()
                {
                    super::WorkerMessage::Opened {
                        revision: loaded, ..
                    } => revision = loaded,
                    message => panic!("expected reopened fixture, got {message:?}"),
                }
            }
        }
        close_test_worker(&worker, revision);
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
                ("Fit", (None, Some(0.0))),
                ("FitB", (None, Some(0.0))),
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
                    resolve_test_link_target(&document, &link),
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

    fn resolve_test_link_target(
        document: &pdfium_render::prelude::PdfDocument<'_>,
        link: &pdfium_render::prelude::PdfLink<'_>,
    ) -> Option<LinkTarget> {
        let config = pdfium_render::prelude::PdfRenderConfig::new()
            .set_fixed_size(super::DESTINATION_RATIO_SIZE, super::DESTINATION_RATIO_SIZE);
        let mut target_pages = std::collections::HashMap::new();
        super::resolve_link_target(document, link, &config, &mut target_pages)
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
            }) = resolve_test_link_target(&document, &link)
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
                resolve_test_link_target(&document, &link),
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
    fn rectangle_rows_clamp_truncated_buffers_and_histogram_buckets_reset() {
        let rectangle = PageLinkRect {
            left: 0,
            top: 0,
            right: 3,
            bottom: 1,
        };
        let pixels = [16, 32, 48, 255, 23, 39, 55, 128, 31, 47, 63, 64];
        let mut buckets = super::RectangleColorBuckets::new();
        assert_eq!(
            buckets.dominant_color(&pixels, 3, 1, rectangle),
            Some([19, 35, 51])
        );
        assert_eq!(
            buckets.dominant_color(&pixels[8..], 1, 1, rectangle),
            Some([31, 47, 63])
        );
        assert_eq!(buckets.dominant_color(&[], 3, 1, rectangle), None);
        assert!(buckets.touched.is_empty());

        let rectangle = PageLinkRect {
            left: 1,
            top: 0,
            right: u32::MAX,
            bottom: u32::MAX,
        };
        let mut pixels = [10, 20, 30, 40, 50, 60, 70, 80, 91];
        assert_eq!(
            dominant_rectangle_color(&pixels, 2, 2, rectangle),
            Some([50, 60, 70])
        );
        super::blend_rectangle(&mut pixels, 2, 2, rectangle, [1, 2, 3], 255);
        assert_eq!(pixels, [10, 20, 30, 40, 1, 2, 3, 80, 91]);
        assert_eq!(super::rectangle_row_range(pixels.len(), 2, 0, 2, 1), None);
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
                label: "(Zabbo et al.,".into(),
                target: target.clone(),
            },
            PageLink {
                rect: PageLinkRect {
                    left: 200,
                    top: 20,
                    right: 300,
                    bottom: 40,
                },
                label: "(Plover and Br".into(),
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
                label: "ambling, 2002)".into(),
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

        let source_text = "(Zabbo et al., 1995) and (Plover and Brambling, 2002)";
        let links = coalesce_document_link_fragments(links, Some(source_text));

        assert_eq!(links.len(), 3);
        assert_eq!(links[0].label, "(Zabbo et al., 1995)");
        assert_eq!(links[1].label, "(Plover and Brambling, 2002)");
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

    #[test]
    fn tex_math_font_fallbacks_map_to_unicode() {
        use super::tex_math_font_unicode;
        for font in [
            "CMEX10",
            "ABCDEF+CMEX10",
            "LMMathExtension10-Regular",
            "XYZABC+LMMathExtension10-Regular",
            "txex",
            "pxex",
            "Fourier-Math-Extension",
        ] {
            assert_eq!(tex_math_font_unicode(font, 0x04), Some(0x230a));
            assert_eq!(tex_math_font_unicode(font, 0x0a), Some(0x27e8));
            assert_eq!(tex_math_font_unicode(font, 0x28), None);
            assert_eq!(tex_math_font_unicode(font, 0x29), None);
            assert_eq!(tex_math_font_unicode(font, 0x2f), None);
            assert_eq!(tex_math_font_unicode(font, 0x4c), Some(0x2295));
            assert_eq!(tex_math_font_unicode(font, 0x4d), Some(0x2295));
            assert_eq!(tex_math_font_unicode(font, 0x4e), Some(0x2297));
            assert_eq!(tex_math_font_unicode(font, 0x50), Some(0x2211));
            assert_eq!(tex_math_font_unicode(font, 0x58), Some(0x2211));
            assert_eq!(tex_math_font_unicode(font, 0x52), Some(0x222b));
            assert_eq!(tex_math_font_unicode(font, 0x5a), Some(0x222b));
            assert_eq!(tex_math_font_unicode(font, 0x70), Some(0x221a));
        }
        assert_eq!(tex_math_font_unicode("ABCDEF+CMMI10", 0x2a), Some(0x21c0));
        assert_eq!(tex_math_font_unicode("rtxmi", 0x2d), Some(0x297d));
        assert_eq!(tex_math_font_unicode("Rpxmi", 0x28), Some(0x21bc));
        assert_eq!(tex_math_font_unicode("txmia", 0x28), None);
        assert_eq!(tex_math_font_unicode("CMMI10", 0x2e), None);
        assert_eq!(tex_math_font_unicode("CMMI10", 0x2f), None);
        assert_eq!(tex_math_font_unicode("CMMI10", 0x2c), Some(0x297c));
        assert_eq!(tex_math_font_unicode("CMSY10", 0x37), Some(0x237f));
        assert_eq!(tex_math_font_unicode("CMSY10", 0x71), Some(0x2a3f));
        assert_eq!(tex_math_font_unicode("MSAM10", 0x05), Some(0x22c5));
        assert_eq!(tex_math_font_unicode("MSAM10", 0x39), Some(0x23af));
        assert_eq!(tex_math_font_unicode("MSAM10", 0x4b), Some(0x21e2));
        assert_eq!(tex_math_font_unicode("MSAM10", 0x4c), Some(0x21e0));
        assert_eq!(tex_math_font_unicode("CMEX10", 0x3a), Some(0x27ee));
        assert_eq!(tex_math_font_unicode("CMEX10", 0x3b), Some(0x27ef));
        assert_eq!(tex_math_font_unicode("CMEX10", 0x62), None);
        assert_eq!(tex_math_font_unicode("MSBM10", 0x26), Some(0x2acb));
        assert_eq!(tex_math_font_unicode("MSBM10", 0x5b), None);
        assert_eq!(tex_math_font_unicode("MSBM10", 0x5e), None);
        assert_eq!(tex_math_font_unicode("MSBM10", 0x7a), Some(0x03dd));
        assert_eq!(tex_math_font_unicode("MSBM10", 0xd875), Some(0xd835));
        assert_eq!(tex_math_font_unicode("stmary10", 0x00), Some(0x2190));
        assert_eq!(tex_math_font_unicode("stmary10", 0x0c), Some(0x2afd));
        assert_eq!(tex_math_font_unicode("stmary10", 0x24), Some(0x2198));
        assert_eq!(tex_math_font_unicode("stmary10", 0x25), Some(0xe025));
        assert_eq!(tex_math_font_unicode("stmary10", 0x26), Some(0x2199));
        assert_eq!(tex_math_font_unicode("stmary10", 0x3e), Some(0xe03e));
        assert_eq!(tex_math_font_unicode("stmary10", 0x4a), Some(0x27e6));
        assert_eq!(tex_math_font_unicode("stmary10", 0x5a), Some(0x237f));
        assert_eq!(tex_math_font_unicode("stmary10", 0x5b), Some(0x23ae));
        assert_eq!(tex_math_font_unicode("stmary10", 0x74), Some(0x27e6));
        assert_eq!(tex_math_font_unicode("CMR10", 0x4c), None);
        assert_eq!(tex_math_font_unicode("LMRoman10-Regular", 0x4d), None);
        assert_eq!(tex_math_font_unicode("CMEX10", 0x30), None);
    }

    // Minimal synthetic Type 1 subsets with deliberately unmapped PDF slots.
    fn synthetic_type1_pdf(fonts: &[(&str, u8, &str, Option<&str>)]) -> Vec<u8> {
        synthetic_type1_pdf_with_contents(fonts, None)
    }

    fn synthetic_type1_pdf_with_contents(
        fonts: &[(&str, u8, &str, Option<&str>)],
        custom_contents: Option<&str>,
    ) -> Vec<u8> {
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            String::new(),
            String::new(),
        ];
        let mut resources = String::new();
        let mut contents = String::new();
        for (i, &(name, slot, glyph, unicode)) in fonts.iter().enumerate() {
            let font = objects.len() + 1;
            resources.push_str(&format!("/F{i} {font} 0 R "));
            contents.push_str(&format!(
                "BT /F{i} 12 Tf {} 72 Td <{slot:02x}> Tj ET\n",
                20 + i * 25
            ));
            let program_name = name.split_once('+').map_or(name, |(_, base)| base);
            let header = format!(
                "%!PS-AdobeFont-1.0: {program_name} 1.0\n\
                 10 dict begin\n/FontName /{program_name} def\n/FontType 1 def\n\
                 /FontMatrix [0.001 0 0 0.001 0 0] def\n/FontBBox [0 0 500 700] def\n\
                 /PaintType 0 def\n/Encoding 256 array\n\
                 0 1 255 {{1 index exch /.notdef put}} for\ndup {slot} /{glyph} put\nreadonly def\n\
                 currentdict end\ncurrentfile eexec\n"
            );
            let mut private = b"\0\0\0\0dup /Private 8 dict dup begin\n\
                /RD {string currentfile exch readstring pop} executeonly def\n\
                /ND {noaccess def} executeonly def\n/lenIV -1 def\n/Subrs 0 array def\n\
                2 index /CharStrings 2 dict dup begin\n/.notdef 5 RD "
                .to_vec();
            private.extend_from_slice(&[139, 248, 136, 13, 14]);
            let charstring: &[u8] = match glyph {
                "wide" => &[
                    139, 248, 136, 13, 139, 139, 21, 249, 80, 139, 5, 139, 239, 5, 9, 14,
                ],
                "hook" => &[
                    139, 248, 136, 13, 139, 239, 21, 239, 139, 5, 139, 239, 5, 9, 14,
                ],
                _ => &[
                    139, 248, 136, 13, 139, 139, 21, 248, 136, 139, 5, 139, 249, 80, 5, 9, 14,
                ],
            };
            private.extend_from_slice(format!(" ND\n/{glyph} {} RD ", charstring.len()).as_bytes());
            private.extend_from_slice(charstring);
            private.extend_from_slice(
                b" ND\nend\nend\nreadonly put\nnoaccess put\n\
                dup /FontName get exch definefont pop\nmark currentfile closefile\n",
            );
            let mut cipher = 55665u16;
            let encrypted: String = private
                .into_iter()
                .map(|byte| {
                    let byte = byte ^ (cipher >> 8) as u8;
                    cipher = u16::from(byte)
                        .wrapping_add(cipher)
                        .wrapping_mul(52845)
                        .wrapping_add(22719);
                    format!("{byte:02x}")
                })
                .collect();
            let program = format!("{header}{encrypted}\n{}\ncleartomark\n", "0".repeat(512));
            let to_unicode =
                unicode.map_or(String::new(), |_| format!("/ToUnicode {} 0 R", font + 3));
            objects.push(format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /{name} /FirstChar {slot} /LastChar {slot} /Widths [500] /Encoding << /Differences [{slot} /{glyph}] >> /FontDescriptor {} 0 R {to_unicode} >>", font + 1
            ));
            objects.push(format!(
                "<< /Type /FontDescriptor /FontName /{program_name} /Flags 4 /FontBBox [0 0 500 700] /ItalicAngle 0 /Ascent 700 /Descent 0 /CapHeight 700 /StemV 80 /FontFile {} 0 R >>", font + 2
            ));
            objects.push(format!(
                "<< /Length {} /Length1 {} >>\nstream\n{program}endstream",
                program.len(),
                program.len()
            ));
            if let Some(unicode) = unicode {
                let cmap = format!(
                    "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                     /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
                     /CMapName /Zorp def\n/CMapType 2 def\n\
                     1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                     1 beginbfchar\n<{slot:02x}> <{unicode}>\nendbfchar\n\
                     endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend"
                );
                objects.push(format!(
                    "<< /Length {} >>\nstream\n{cmap}\nendstream",
                    cmap.len()
                ));
            }
        }
        let contents = custom_contents.unwrap_or(&contents);
        objects[2] = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << {resources} >> >> /Contents 4 0 R >>"
        );
        objects[3] = format!(
            "<< /Length {} >>\nstream\n{contents}endstream",
            contents.len()
        );
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn embedded_type1_zero_slot_and_broken_bbbk_surrogate_decode() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        for (font, expected) in [
            (("ABCDEF+msbm10", 1, "zorp", Some("D875DD5C")), '𝕜'),
            (("ABCDEF+stmary10", 0, "nux", None), '←'),
        ] {
            let document = pdfium
                .load_pdf_from_byte_vec(synthetic_type1_pdf(&[font]), None)
                .unwrap();
            let page = document.pages().get(0).unwrap();
            let text = page.text().unwrap();
            let chars = text.chars();
            let mut font_cache = std::collections::HashMap::new();
            let decoded: String =
                super::decoded_pdf_characters(&chars, &mut font_cache, || true, |_, _| {})
                    .map(|(_, _, c)| c)
                    .collect();
            assert_eq!(decoded, expected.to_string());
        }
        let document = pdfium
            .load_pdf_from_byte_vec(
                synthetic_type1_pdf(&[("ABCDEF+stmary10", 0, "nux", None)]),
                None,
            )
            .unwrap();
        let page = document.pages().get(0).unwrap();
        let text = page.text().unwrap();
        let ch = text.chars().get(0).unwrap();
        let mut cache = std::collections::HashMap::new();
        assert_eq!(super::pdf_char_unicode(&ch, &mut cache), '←' as u32);
        assert_eq!(super::pdf_char_unicode(&ch, &mut cache), '←' as u32);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn rotated_and_scaled_type1_glyphs_keep_unscaled_geometry_checks() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        let pdf = synthetic_type1_pdf_with_contents(
            &[
                ("ABCDEF+stmary10", 0x26, "zorp", None),
                ("BCDEFG+cmmi10", 0x28, "vup", Some("0028")),
                ("CDEFGH+cmmi10", 0x28, "wide", None),
                ("ABCDEF+lmmi10", 0x2c, "nux", Some("002C")),
                ("DEFGHI+cmmi10", 0x2c, "hook", None),
                ("EFGHIJ+cmmi10", 0x2d, "hook", None),
            ],
            Some(
                "BT /F0 1 Tf 0.8660254 2 -0.5 3.4641016 20 72 Tm <26> Tj ET\n\
                 BT /F1 1 Tf 0.5 2 -3.4641016 0.8660254 50 72 Tm <28> Tj ET\n\
                 BT /F2 1 Tf 0 1 -1 0 80 72 Tm <28> Tj ET\n\
                 BT /F3 1 Tf 0 1 -1 0 110 72 Tm <2c> Tj ET\n\
                 BT /F4 1 Tf 1 0 0 -1 140 72 Tm <2c> Tj ET\n\
                 BT /F5 1 Tf 0.70710677 0.70710677 -0.70710677 0.70710677 170 72 Tm <2d> Tj ET\n",
            ),
        );
        let document = pdfium.load_pdf_from_byte_vec(pdf, None).unwrap();
        let page = document.pages().get(0).unwrap();
        let text = page.text().unwrap();
        let chars = text.chars();
        let mut font_cache = std::collections::HashMap::new();
        let decoded: String =
            super::decoded_pdf_characters(&chars, &mut font_cache, || true, |_, _| {})
                .map(|(_, _, c)| c)
                .filter(|c| !c.is_whitespace())
                .collect();
        assert_eq!(decoded, "↙(↼,⥼⥽");
    }

    #[test]
    fn decoded_ascii_in_tex_named_font_is_preserved_in_clicks_and_search() {
        let _native = super::pdfium_test_lock();
        let pdfium = super::load_pdfium(None).unwrap();
        let stream = b"BT /F1 12 Tf 72 72 Td (&) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /stmary10 >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                stream.len(),
                std::str::from_utf8(stream).unwrap()
            ),
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
        let document = pdfium.load_pdf_from_byte_vec(pdf, None).unwrap();
        let page = document.pages().get(0).unwrap();
        let text = page.text().unwrap();
        let ch = text.chars().get(0).unwrap();
        let bounds = ch.tight_bounds().unwrap();
        let mut font_cache = std::collections::HashMap::new();
        let (context, byte) = super::clicked_text(
            &page,
            (bounds.left().value + bounds.right().value) * 0.5,
            (bounds.bottom().value + bounds.top().value) * 0.5,
            &mut font_cache,
        )
        .unwrap()
        .unwrap();
        assert_eq!(&context[byte..byte + 1], "&");
        let mut cache = super::empty_text_cache(1);
        assert_eq!(
            super::search_page(&document, 0, &mut cache, &mut font_cache, "&").0,
            1
        );
        assert_eq!(
            super::search_page(&document, 0, &mut cache, &mut font_cache, "↙").0,
            0
        );
    }

    #[test]
    fn fit_modes_clamp_excessive_raster_dimensions_and_pixel_budget() {
        for fit in [
            super::FitMode::Page,
            super::FitMode::Width,
            super::FitMode::Height,
            super::FitMode::Content,
        ] {
            for (pw, ph) in [(612.0, 792.0), (612.0, 793.0), (10.0, 5_000.0)] {
                let (w, h) = fit.raster_size(pw, ph, [0.0, 0.0, 1.0, 1.0], 15_360, 8_640);
                assert!((1..=32_768).contains(&w) && (1..=32_768).contains(&h));
                assert!(u64::from(w) * u64::from(h) <= 16_777_216);
            }
        }
    }

    #[test]
    fn dark_mode_base_cache_roundtrips_link_overlay_cleanly() {
        let pdf = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        std::fs::write(pdf.path(), synthetic_link_pdf("XYZ null 300 null")).unwrap();
        let worker = super::RenderWorker::spawn(1, pdf.path().to_path_buf(), None);
        worker.wait_until_ready().unwrap();
        worker.begin_generation(1);
        let base = super::RenderKey {
            document_id: 1,
            page: 0,
            width: 240,
            height: 240,
            zoom: 100,
            fit: super::FitMode::Page,
            invert: true,
            dark_mode_style: NEUTRAL_DARK_MODE,
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [80, 160, 255],
            selected_link_ordinal: None,
        };
        worker
            .render(super::RenderRequest {
                key: base,
                generation: 1,
            })
            .unwrap();
        let plain1 = coordinate_frame(&worker);
        worker
            .render(super::RenderRequest {
                key: super::RenderKey {
                    link_mode: true,
                    ..base
                },
                generation: 1,
            })
            .unwrap();
        let highlighted = coordinate_frame(&worker);
        assert_ne!(plain1.compressed_rgba, highlighted.compressed_rgba);
        worker
            .render(super::RenderRequest {
                key: base,
                generation: 1,
            })
            .unwrap();
        let plain2 = coordinate_frame(&worker);
        assert_eq!(plain1.compressed_rgba, plain2.compressed_rgba);
        assert_eq!(plain2.render_elapsed, std::time::Duration::ZERO);
    }
}

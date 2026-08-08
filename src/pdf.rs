use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased, unbounded};
use pdfium_render::prelude::{
    PdfBookmark, PdfDocument, PdfMatrix, PdfPage, PdfPageObject, PdfPageObjectCommon,
    PdfPageObjectsCommon, PdfRenderConfig, Pdfium,
};

const LOW_CHROMA_THRESHOLD: u8 = 10;
const MAX_DARK_MODE_WORKERS: usize = 8;
const MAX_FORM_DEPTH: u8 = 32;
const IMAGE_MASK_SAMPLES: usize = 4;
const PARALLEL_DARK_MODE_PIXELS: usize = 250_000;

pub type DocumentId = u64;

/// One entry in a document's outline (table of contents).
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub page: u32,
    pub depth: u16,
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
    pub compression_elapsed: Duration,
    pub generation: u64,
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
    Render(RenderRequest),
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

        loop {
            let task = match command_rx.try_recv() {
                Ok(command) => command.into(),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => match priority_rx
                    .try_recv()
                {
                    Ok(request) => WorkerTask::Render(request),
                    Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => match prefetch_rx.try_recv() {
                        Ok(request) => WorkerTask::Render(request),
                        Err(TryRecvError::Disconnected | TryRecvError::Empty) => select_biased! {
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
                        },
                    },
                },
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
                    continue;
                }
                WorkerTask::ExtractText { document_id, page } => {
                    if let Some(document) = documents.get(&document_id) {
                        let content = extract_page_text(document, page);
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
                    compression_elapsed,
                    generation: request.generation,
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
        }
    }
}

/// Extracts all selectable text from a page, returning an empty string when the
/// page has no text layer or cannot be loaded.
fn extract_page_text(document: &PdfDocument, page: u32) -> String {
    let Ok(index) = i32::try_from(page) else {
        return String::new();
    };
    let Ok(page) = document.pages().get(index) else {
        return String::new();
    };
    match page.text() {
        Ok(text) => text.all(),
        Err(_) => String::new(),
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
        DarkModeStyle, DarkModeTransform, FitMode, dark_mode_pixel, darken_rgba, mask_quadrilateral,
    };

    const NEUTRAL_DARK_MODE: DarkModeStyle = DarkModeStyle::new([30, 30, 30], [209, 209, 209]);

    #[test]
    fn image_mask_finds_images_nested_in_transformed_forms() {
        use super::{image_mask, load_pdfium};
        use pdfium_render::prelude::{PdfPageObjectsCommon, PdfPagePaperSize, PdfRenderConfig};

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
}

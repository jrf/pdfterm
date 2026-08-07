use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased, unbounded};
use pdfium_render::prelude::{PdfBookmark, PdfDocument, PdfRenderConfig, Pdfium};

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
pub struct RenderKey {
    pub document_id: DocumentId,
    pub page: u32,
    pub width: u16,
    pub height: u16,
    pub fit: FitMode,
    pub invert: bool,
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
    Frame(Frame),
    Error(String),
}

enum WorkerCommand {
    Open {
        document_id: DocumentId,
        path: PathBuf,
    },
    Close(DocumentId),
}

enum WorkerTask {
    Open {
        document_id: DocumentId,
        path: PathBuf,
    },
    Close(DocumentId),
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
            if request.key.invert {
                invert_rgb(&mut raw_rgba);
            }
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

/// Inverts the red, green, and blue channels of an RGBA buffer, leaving alpha
/// untouched, so light documents render comfortably on a dark terminal.
fn invert_rgb(rgba: &mut [u8]) {
    for pixel in rgba.chunks_mut(4) {
        if let [r, g, b, _alpha] = pixel {
            *r = 255 - *r;
            *g = 255 - *g;
            *b = 255 - *b;
        }
    }
}

impl From<WorkerCommand> for WorkerTask {
    fn from(command: WorkerCommand) -> Self {
        match command {
            WorkerCommand::Open { document_id, path } => Self::Open { document_id, path },
            WorkerCommand::Close(document_id) => Self::Close(document_id),
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
    use super::{FitMode, invert_rgb};

    #[test]
    fn invert_flips_color_channels_but_keeps_alpha() {
        let mut pixels = [0, 10, 245, 128, 255, 255, 255, 64];
        invert_rgb(&mut pixels);
        assert_eq!(pixels, [255, 245, 10, 128, 0, 0, 0, 64]);
    }

    #[test]
    fn fit_mode_cycles_page_width_height() {
        assert_eq!(FitMode::Page.cycle(), FitMode::Width);
        assert_eq!(FitMode::Width.cycle(), FitMode::Height);
        assert_eq!(FitMode::Height.cycle(), FitMode::Page);
    }
}

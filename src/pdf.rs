use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased, unbounded};
use pdfium_render::prelude::{PdfRenderConfig, Pdfium};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderKey {
    pub page: u32,
    pub width: u16,
    pub height: u16,
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
    Ready { pages: u32 },
    Frame(Frame),
    Error(String),
}

pub struct RenderWorker {
    priority_tx: Sender<RenderRequest>,
    prefetch_tx: Sender<RenderRequest>,
    message_rx: Receiver<WorkerMessage>,
    latest_generation: Arc<AtomicU64>,
}

impl RenderWorker {
    pub fn spawn(path: PathBuf, pdfium_library: Option<PathBuf>) -> Self {
        let (priority_tx, priority_rx) = unbounded();
        let (prefetch_tx, prefetch_rx) = unbounded();
        let (message_tx, message_rx) = unbounded();
        let latest_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&latest_generation);

        thread::spawn(move || {
            run_worker(
                &path,
                pdfium_library.as_deref(),
                priority_rx,
                prefetch_rx,
                message_tx,
                worker_generation,
            );
        });

        Self {
            priority_tx,
            prefetch_tx,
            message_rx,
            latest_generation,
        }
    }

    pub fn wait_until_ready(&self) -> Result<u32, String> {
        match self.message_rx.recv() {
            Ok(WorkerMessage::Ready { pages }) => Ok(pages),
            Ok(WorkerMessage::Error(error)) => Err(error),
            Ok(WorkerMessage::Frame(_)) => {
                Err("renderer sent a frame before initialization".into())
            }
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

    pub fn try_recv(&self) -> Result<WorkerMessage, TryRecvError> {
        self.message_rx.try_recv()
    }
}

fn run_worker(
    path: &Path,
    pdfium_library: Option<&Path>,
    priority_rx: Receiver<RenderRequest>,
    prefetch_rx: Receiver<RenderRequest>,
    message_tx: Sender<WorkerMessage>,
    latest_generation: Arc<AtomicU64>,
) {
    let result = (|| -> Result<(), String> {
        let pdfium = load_pdfium(pdfium_library)?;
        let document = pdfium
            .load_pdf_from_file(path, None)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let pages = u32::try_from(document.pages().len())
            .map_err(|_| "PDFium returned a negative page count".to_string())?;
        if pages == 0 {
            return Err(format!("{} has no pages", path.display()));
        }
        message_tx
            .send(WorkerMessage::Ready { pages })
            .map_err(|_| "viewer stopped".to_string())?;

        loop {
            let request = match priority_rx.try_recv() {
                Ok(request) => request,
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => match prefetch_rx.try_recv() {
                    Ok(request) => request,
                    Err(TryRecvError::Disconnected | TryRecvError::Empty) => select_biased! {
                        recv(priority_rx) -> request => match request {
                            Ok(request) => request,
                            Err(_) => break,
                        },
                        recv(prefetch_rx) -> request => match request {
                            Ok(request) => request,
                            Err(_) => break,
                        },
                    },
                },
            };

            if request.generation != latest_generation.load(Ordering::Acquire) {
                continue;
            }

            let page_index = i32::try_from(request.key.page).map_err(|_| {
                format!("page {} exceeds PDFium's index range", request.key.page + 1)
            })?;
            let page = document.pages().get(page_index).map_err(|error| {
                format!("could not load page {}: {error}", request.key.page + 1)
            })?;
            let render_started = Instant::now();
            let bitmap = page
                .render_with_config(
                    &PdfRenderConfig::new()
                        .scale_page_to_display_size(
                            i32::from(request.key.width),
                            i32::from(request.key.height),
                        )
                        .set_reverse_byte_order(true)
                        .use_lcd_text_rendering(true)
                        .force_half_tone(false)
                        .use_print_quality(false),
                )
                .map_err(|error| {
                    format!("could not render page {}: {error}", request.key.page + 1)
                })?;
            let render_elapsed = render_started.elapsed();

            let width = bitmap.width() as u32;
            let height = bitmap.height() as u32;
            let compression_started = Instant::now();
            let compressed_rgba =
                crate::kitty::compress_rgba(&bitmap.as_raw_bytes()).map_err(|error| {
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

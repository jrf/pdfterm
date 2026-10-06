//! External navigation owns its cancellation/deadline, never the render thread.
use crate::{
    editor::Editor,
    pdf::{DocumentId, ResolvedClick},
    process::Operation,
    synctex::{self, DocumentRevision, InverseResolution},
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    thread::{self, JoinHandle},
};

pub(crate) struct Coordinator {
    // Drop cancels the active request before joining the external worker.
    pub inverse: Option<PendingInverse>,
    pub worker: NavigationWorker,
    pub forward: Option<PendingForward>,
    pub flash: Option<PendingFlash>,
    pub next_request_id: u64,
    // The retained resolver validates each displayed revision, including reloads.
    pub source_maps: HashMap<PathBuf, String>,
}
impl Coordinator {
    pub fn new() -> Self {
        Self {
            inverse: None,
            worker: NavigationWorker::new(),
            forward: None,
            flash: None,
            next_request_id: 1,
            source_maps: HashMap::new(),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardStage {
    AwaitingDocument,
    AwaitingFrame,
}
pub(crate) struct PendingForward {
    pub request: synctex::ForwardRequest,
    pub reply: crate::ipc::ForwardReply,
    pub deadline: std::time::Instant,
    pub stage: ForwardStage,
}
pub(crate) struct PendingFlash {
    pub document_id: DocumentId,
    pub revision: synctex::PdfRevision,
    pub page: u32,
    pub positioning_pending: bool,
    pub expires_at: Option<std::time::Instant>,
}

pub(crate) enum InverseStage {
    HitTest,
    Resolving,
}
pub(crate) struct PendingInverse {
    pub document_id: DocumentId,
    pub page: u32,
    pub request_id: u64,
    pub revision: DocumentRevision,
    pub operation: Operation,
    pub stage: InverseStage,
}
impl Drop for PendingInverse {
    fn drop(&mut self) {
        self.operation.cancel();
    }
}
pub(crate) struct InverseTask {
    pub request_id: u64,
    pub path: PathBuf,
    pub revision: DocumentRevision,
    pub page: u32,
    pub click: ResolvedClick,
    pub word_precision: bool,
    pub radius: u32,
    pub editor: Editor,
    pub inverse_search: Option<String>,
    pub operation: Operation,
}
pub(crate) struct InverseReply {
    pub request_id: u64,
    pub result: io::Result<InverseResolution>,
}
impl InverseTask {
    fn resolve(&self) -> io::Result<InverseResolution> {
        self.operation.check()?;
        self.revision.check(&self.path)?;
        let word = if self.word_precision {
            self.click
                .text
                .as_ref()
                .ok()
                .and_then(|v| v.as_ref())
                .map(|(s, i)| (s.as_str(), *i))
        } else {
            None
        };
        let point = synctex::InversePoint {
            page: self.page + 1,
            ..if self.inverse_search.is_some() {
                self.click.typst
            } else {
                self.click.synctex
            }
        };
        let mut result = if let Some(endpoint) = &self.inverse_search {
            crate::typst::resolve_inverse(endpoint, self.revision.pdf, point, &self.operation)?
        } else {
            synctex::resolve_inverse(&self.path, point, word, self.radius, &self.operation)?
        };
        if self.inverse_search.is_none()
            && self.word_precision
            && let Err(error) = &self.click.text
        {
            result.warning = Some(format!(
                "line-only navigation: PDF text refinement unavailable: {error}"
            ));
        }
        self.operation.check()?;
        self.revision.check(&self.path)?;
        self.editor.deliver(&result.location, &self.operation)?;
        Ok(result)
    }
}

pub(crate) struct NavigationWorker {
    requests: Option<Sender<InverseTask>>,
    queued: Receiver<InverseTask>,
    pub replies: Receiver<InverseReply>,
    thread: Option<JoinHandle<()>>,
}
impl NavigationWorker {
    pub fn new() -> Self {
        let (requests, receiver) = bounded::<InverseTask>(1);
        let (sender, replies) = bounded(1);
        let queued = receiver.clone();
        let thread = thread::spawn(move || {
            while let Ok(task) = receiver.recv() {
                let reply = InverseReply {
                    request_id: task.request_id,
                    result: task.resolve(),
                };
                if sender.send(reply).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: Some(requests),
            queued,
            replies,
            thread: Some(thread),
        }
    }
    pub fn submit(&self, task: InverseTask) -> io::Result<()> {
        let requests = self.requests.as_ref().expect("live worker");
        let mut task = task;
        loop {
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                return Err(io::Error::other("navigation worker stopped"));
            }
            match requests.try_send(task) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(latest)) => {
                    // The UI already cancels the superseded active request.
                    // Replace its queued successor instead of losing this click.
                    if let Ok(obsolete) = self.queued.try_recv() {
                        obsolete.operation.cancel();
                    }
                    task = latest;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other("navigation worker stopped"));
                }
            }
        }
    }
}
impl Drop for NavigationWorker {
    fn drop(&mut self) {
        while let Ok(task) = self.queued.try_recv() {
            task.operation.cancel();
        }
        self.requests.take();
        // Drain completions while shutting down so the bounded reply queue
        // cannot keep the worker alive. Active operations have a hard deadline.
        if let Some(thread) = self.thread.take() {
            while !thread.is_finished() {
                let _ = self
                    .replies
                    .recv_timeout(std::time::Duration::from_millis(5));
            }
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        process::Command,
        time::{Duration, Instant},
    };

    struct InverseHarness {
        directory: tempfile::TempDir,
        path: PathBuf,
        map: crate::ipc::Listener,
        editor: crate::ipc::Listener,
    }
    impl InverseHarness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.path().join("navigation.pdf");
            fs::write(&path, b"stable inverse-navigation revision").unwrap();
            let map = crate::ipc::Listener::bind(&directory.path().join("map.sock")).unwrap();
            let editor = crate::ipc::Listener::bind(&directory.path().join("editor.sock")).unwrap();
            Self {
                directory,
                path,
                map,
                editor,
            }
        }
        fn task(&self, request_id: u64) -> InverseTask {
            InverseTask {
                request_id,
                path: self.path.clone(),
                revision: DocumentRevision::read(&self.path).unwrap(),
                page: 0,
                click: ResolvedClick {
                    synctex: crate::synctex::InversePoint {
                        page: 1,
                        x: request_id as f32,
                        y_from_top: 580.,
                        page_height_pt: 600.,
                    },
                    typst: crate::synctex::InversePoint {
                        page: 1,
                        x: request_id as f32,
                        y_from_top: 580.,
                        page_height_pt: 600.,
                    },
                    text: Ok(None),
                },
                word_precision: false,
                radius: 4,
                editor: Editor::Socket {
                    path: self
                        .directory
                        .path()
                        .join("editor.sock")
                        .to_str()
                        .unwrap()
                        .into(),
                },
                inverse_search: Some(
                    self.directory
                        .path()
                        .join("map.sock")
                        .to_str()
                        .unwrap()
                        .into(),
                ),
                operation: Operation::new(Duration::from_secs(5)),
            }
        }
        fn accept(listener: &crate::ipc::Listener) -> UnixStream {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.socket.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).unwrap();
                        return stream;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "inverse IPC did not connect");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("inverse IPC accept failed: {error}"),
                }
            }
        }
        fn read(stream: &mut UnixStream) -> serde_json::Value {
            let mut bytes = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match stream.read_to_end(&mut bytes) {
                    Ok(_) => return serde_json::from_slice(&bytes).unwrap(),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "inverse IPC did not finish");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("inverse IPC read failed: {error}"),
                }
            }
        }
        fn stalled_worker(&self) -> (NavigationWorker, Operation, UnixStream) {
            let worker = NavigationWorker::new();
            let completed = self.task(1);
            completed.operation.cancel();
            worker.submit(completed).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while worker.replies.is_empty() {
                assert!(Instant::now() < deadline, "cancellation did not complete");
                thread::sleep(Duration::from_millis(1));
            }

            // Keep the first completion queued. The next completion therefore
            // blocks the worker before it can take any queued replacement.
            let active = self.task(2);
            let operation = active.operation.clone();
            worker.submit(active).unwrap();
            let mut stream = Self::accept(&self.map);
            assert_eq!(Self::read(&mut stream)["x"].as_f64(), Some(2.));
            (worker, operation, stream)
        }
    }

    #[test]
    fn inverse_burst_replaces_canceled_queue_and_only_delivers_latest_click() {
        let harness = InverseHarness::new();
        let (worker, active, stalled) = harness.stalled_worker();
        active.cancel();

        let queued = harness.task(3);
        let obsolete = queued.operation.clone();
        worker.submit(queued).unwrap();
        let replacement = harness.task(4);
        let mut operation = replacement.operation.clone();
        worker.submit(replacement).unwrap();
        assert!(obsolete.is_cancelled());
        for request_id in 5..=12 {
            operation.cancel();
            let latest = harness.task(request_id);
            operation = latest.operation.clone();
            worker.submit(latest).unwrap();
        }
        drop(stalled);

        for request_id in [1, 2] {
            let reply = worker.replies.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(reply.request_id, request_id);
            assert!(
                matches!(reply.result, Err(error) if error.kind() == io::ErrorKind::Interrupted)
            );
        }
        let mut mapped = InverseHarness::accept(&harness.map);
        assert_eq!(InverseHarness::read(&mut mapped)["x"].as_f64(), Some(12.));
        let source = harness.directory.path().join("latest.typ");
        mapped
            .write_all(
                &serde_json::to_vec(&serde_json::json!({
                    "ok": true,
                    "location": {
                        "file": source,
                        "line": 37,
                        "byte_column": 0,
                        "column": 1,
                        "column_char": 1,
                        "precise": true
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        drop(mapped);

        let mut editor = InverseHarness::accept(&harness.editor);
        let delivered = InverseHarness::read(&mut editor);
        assert_eq!(delivered["file"].as_str(), source.to_str());
        assert_eq!(delivered["line"].as_u64(), Some(37));
        let reply = worker.replies.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(reply.request_id, 12);
        assert_eq!(reply.result.unwrap().location.line, 37);
        assert!(
            matches!(harness.map.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn inverse_shutdown_discards_queued_work_and_drains_blocked_completions() {
        let harness = InverseHarness::new();
        let (worker, active, stalled) = harness.stalled_worker();
        let queued = harness.task(3);
        let operation = queued.operation.clone();
        worker.submit(queued).unwrap();
        // Coordinator drops PendingInverse before the worker.
        active.cancel();
        let started = Instant::now();
        drop(worker);
        drop(stalled);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(operation.is_cancelled());
        assert!(
            matches!(harness.map.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn failed_pdf_text_preserves_real_synctex_navigation_and_stale_pairs_fail() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("navigation.tex");
        fs::write(&source, include_str!("../tests/fixtures/navigation.tex")).unwrap();
        let output = crate::process::output(
            Command::new("pdflatex")
                .current_dir(directory.path())
                .args([
                    "-interaction=nonstopmode",
                    "-halt-on-error",
                    "-synctex=1",
                    "navigation.tex",
                ]),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for the navigation fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let path = directory.path().join("navigation.pdf");
        let forward = synctex::resolve_forward(&path, &source, 6, 1).unwrap();
        let task = InverseTask {
            request_id: 1,
            revision: DocumentRevision::read(&path).unwrap(),
            path,
            page: forward.page - 1,
            click: ResolvedClick {
                synctex: crate::synctex::InversePoint {
                    page: forward.page,
                    x: forward.h + forward.width / 2.,
                    y_from_top: forward.v - forward.height / 2.,
                    page_height_pt: 792.,
                },
                typst: crate::synctex::InversePoint {
                    page: forward.page,
                    x: forward.h + forward.width / 2.,
                    y_from_top: forward.v - forward.height / 2.,
                    page_height_pt: 792.,
                },
                text: Err("unsupported glyph encoding".into()),
            },
            word_precision: true,
            radius: 4,
            editor: Editor::None,
            inverse_search: None,
            operation: Operation::default(),
        };
        let resolution = task.resolve().unwrap();
        assert_eq!(resolution.location.line, 6);
        assert!(!resolution.location.precise);
        assert!(
            resolution
                .warning
                .unwrap()
                .contains("PDF text refinement unavailable")
        );
        // A changed companion must reject before any source refinement/delivery.
        fs::remove_file(task.path.with_extension("synctex.gz")).unwrap();
        assert!(matches!(task.resolve(), Err(error) if error.to_string().contains("revision")));
        task.operation.cancel();
        assert!(matches!(task.resolve(), Err(error) if error.kind() == io::ErrorKind::Interrupted));
    }
}

//! External navigation owns its cancellation/deadline, never the render thread.
use crate::{
    editor::Editor,
    pdf::{DocumentId, ResolvedClick},
    process::Operation,
    synctex::{self, DocumentRevision, InverseResolution},
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::{
    collections::{HashMap, HashSet},
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
    pub best_guess: HashSet<PathBuf>,
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
            best_guess: HashSet::new(),
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
impl PendingForward {
    pub fn check_revision(&self) -> io::Result<()> {
        self.request.revision.check(&self.request.pdf)
    }
}
pub(crate) struct PendingFlash {
    pub document_id: DocumentId,
    pub revision: synctex::PdfRevision,
    pub page: u32,
    pub positioning_pending: bool,
    pub expires_at: Option<std::time::Instant>,
}

pub(crate) struct PendingInverse {
    pub document_id: DocumentId,
    pub page: u32,
    pub request_id: u64,
    pub revision: DocumentRevision,
    pub operation: Operation,
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
    pub best_guess: bool,
    pub operation: Operation,
}
pub(crate) struct InverseReply {
    pub request_id: u64,
    pub result: io::Result<InverseResolution>,
}
impl InverseTask {
    fn resolve(&self, cache: Option<&mut synctex::SyncTeXCache>) -> io::Result<InverseResolution> {
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
            synctex::resolve_inverse_with_cache(
                &self.path,
                point,
                word,
                self.radius,
                &self.operation,
                self.best_guess,
                cache,
            )?
        };
        if self.inverse_search.is_none()
            && self.word_precision
            && let Err(error) = &self.click.text
        {
            let warning = format!("line-only navigation: PDF text refinement unavailable: {error}");
            result.warning = Some(match result.warning.take() {
                Some(previous) => format!("{previous}; {warning}"),
                None => warning,
            });
        }
        if let Some(warning) = &mut result.warning
            && warning.len() > 4096
        {
            let mut end = 4096;
            while !warning.is_char_boundary(end) {
                end -= 1;
            }
            warning.truncate(end);
        }
        self.operation.check()?;
        self.revision.check(&self.path)?;
        self.editor
            .deliver(&result.location, &self.operation, result.warning.as_deref())?;
        Ok(result)
    }
}

pub(crate) struct NavigationWorker {
    requests: Option<Sender<InverseTask>>,
    queued: Receiver<InverseTask>,
    pub replies: Receiver<InverseReply>,
    thread: Option<JoinHandle<()>>,
    operation: Option<Operation>,
}
impl NavigationWorker {
    pub fn new() -> Self {
        let (requests, receiver) = bounded::<InverseTask>(1);
        let (sender, replies) = bounded(1);
        let queued = receiver.clone();
        let thread = thread::spawn(move || {
            let mut cache = synctex::SyncTeXCache::default();
            while let Ok(task) = receiver.recv() {
                let reply = InverseReply {
                    request_id: task.request_id,
                    result: task.resolve(Some(&mut cache)),
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
            operation: None,
        }
    }
    pub fn submit(&mut self, task: InverseTask) -> io::Result<()> {
        let requests = self.requests.as_ref().expect("live worker");
        if let Some(previous) = self.operation.replace(task.operation.clone()) {
            previous.cancel();
        }
        let mut task = task;
        loop {
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                task.operation.cancel();
                return Err(io::Error::other("navigation worker stopped"));
            }
            match requests.try_send(task) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(latest)) => {
                    // Replace its queued successor instead of losing this click.
                    if let Ok(obsolete) = self.queued.try_recv() {
                        obsolete.operation.cancel();
                    }
                    task = latest;
                }
                Err(TrySendError::Disconnected(stopped)) => {
                    stopped.operation.cancel();
                    return Err(io::Error::other("navigation worker stopped"));
                }
            }
        }
    }
}
impl Drop for NavigationWorker {
    fn drop(&mut self) {
        if let Some(operation) = self.operation.take() {
            operation.cancel();
        }
        while let Ok(task) = self.queued.try_recv() {
            task.operation.cancel();
        }
        self.requests.take();
        // Drain completions so the bounded reply queue cannot block the join.
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
        path::Path,
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
                best_guess: false,
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
            let mut worker = NavigationWorker::new();
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
        let (mut worker, active, stalled) = harness.stalled_worker();
        assert!(!active.is_cancelled());

        let queued = harness.task(3);
        let obsolete = queued.operation.clone();
        worker.submit(queued).unwrap();
        assert!(active.is_cancelled());
        let replacement = harness.task(4);
        let mut operation = replacement.operation.clone();
        worker.submit(replacement).unwrap();
        assert!(obsolete.is_cancelled());
        for request_id in 5..=12 {
            let latest = harness.task(request_id);
            let replacement = latest.operation.clone();
            worker.submit(latest).unwrap();
            assert!(operation.is_cancelled());
            operation = replacement;
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
        let (mut worker, active, stalled) = harness.stalled_worker();
        let queued = harness.task(3);
        let operation = queued.operation.clone();
        worker.submit(queued).unwrap();
        assert!(active.is_cancelled());
        let started = Instant::now();
        drop(worker);
        drop(stalled);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(operation.is_cancelled());
        assert!(
            matches!(harness.map.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn standalone_inverse_shutdown_cancels_active_work_without_pending_inverse() {
        let harness = InverseHarness::new();
        let mut worker = NavigationWorker::new();
        let task = harness.task(1);
        let operation = task.operation.clone();
        worker.submit(task).unwrap();
        let mut stalled = InverseHarness::accept(&harness.map);
        assert_eq!(InverseHarness::read(&mut stalled)["x"].as_f64(), Some(1.));
        assert!(!operation.is_cancelled());
        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(operation.is_cancelled());
        let mut byte = [0];
        assert_eq!(stalled.read(&mut byte).unwrap(), 0);
        assert!(
            matches!(harness.editor.socket.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn native_source_cpu_work_is_superseded_and_joined_on_shutdown() {
        let harness = InverseHarness::new();
        let contents = format!(
            "\\documentclass{{beamer}}\n\
             \\newcommand{{\\eqA}}{{\\begin{{equation}}x\\end{{equation}}}}\n\
             \\newcommand{{\\eqB}}{{\\eqA{{}} \\eqA{{}} \\eqA{{}} \\eqA{{}}}}\n\
             \\newcommand{{\\eqC}}{{\\eqB{{}} \\eqB{{}} \\eqB{{}} \\eqB{{}}}}\n\
             \\newcommand{{\\eqD}}{{\\eqC{{}} \\eqC{{}} \\eqC{{}} \\eqC{{}}}}\n\
             \\newcommand{{\\unused}}{{\n{}}}\n\
             \\begin{{document}}\n\\begin{{frame}}[shrink=1]\n\
             Quartz \\eqD{{}} zephyr.\nCopper nimbus.\n\
             \\end{{frame}}\n\\end{{document}}\n",
            "Neutral filler prose remains unrelated to selected target.\n".repeat(18_644),
        );
        fs::write(harness.directory.path().join("navigation.tex"), &contents).unwrap();
        let output = crate::process::output(
            Command::new("pdflatex")
                .current_dir(harness.directory.path())
                .args([
                    "-interaction=batchmode",
                    "-halt-on-error",
                    "-synctex=1",
                    "navigation.tex",
                ]),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for the native CPU cancellation fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let cpu_task = |request_id| {
            let mut task = harness.task(request_id);
            task.editor = Editor::None;
            task.inverse_search = None;
            task.word_precision = true;
            task.radius = 10;
            task.click.synctex = synctex::InversePoint {
                page: 1,
                x: 35.,
                y_from_top: 265.,
                page_height_pt: 272.126,
            };
            task.click.text = Ok(Some(("Copper nimbus.".into(), 0)));
            task
        };
        let expected_line = contents
            .lines()
            .position(|line| line == "Copper nimbus.")
            .unwrap() as u32
            + 1;
        let control = cpu_task(0).resolve(None).unwrap();
        assert_eq!(control.location.line, expected_line);
        assert!(control.location.precise);

        let mut worker = NavigationWorker::new();
        let active = cpu_task(1);
        let superseded = active.operation.clone();
        worker.submit(active).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert!(
            worker.replies.is_empty(),
            "CPU fixture completed before supersession"
        );
        let mut latest = cpu_task(2);
        latest.word_precision = false;
        let started = Instant::now();
        worker.submit(latest).unwrap();
        let canceled = worker
            .replies
            .recv_timeout(Duration::from_millis(500))
            .unwrap();
        assert_eq!(canceled.request_id, 1);
        assert!(
            matches!(canceled.result, Err(error) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert!(superseded.is_cancelled());
        let latest = worker
            .replies
            .recv_timeout(Duration::from_millis(500))
            .unwrap();
        assert_eq!(latest.request_id, 2);
        let location = latest.result.unwrap().location;
        assert_eq!(
            Path::new(&location.file),
            harness
                .directory
                .path()
                .join("navigation.tex")
                .canonicalize()
                .unwrap()
        );
        assert!(!location.precise);
        assert!(started.elapsed() < Duration::from_millis(500));

        let active = cpu_task(3);
        let shutdown = active.operation.clone();
        worker.submit(active).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert!(
            worker.replies.is_empty(),
            "CPU fixture completed before shutdown"
        );
        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn failed_pdf_text_preserves_real_synctex_navigation_and_stale_pairs_fail() {
        let _native = crate::pdf::pdfium_test_lock();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = root.join("navigation.tex");
        let contents = include_str!("../tests/fixtures/navigation.tex");
        fs::write(&source, contents).unwrap();
        let output = crate::process::output(
            Command::new("pdflatex").current_dir(&root).args([
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
        let path = root.join("navigation.pdf");
        let forward = synctex::resolve_forward(&path, &source, 6, 1).unwrap();
        let mut task = InverseTask {
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
            best_guess: false,
            operation: Operation::default(),
        };
        let mut synctex_cache = synctex::SyncTeXCache::default();
        let resolution = task.resolve(Some(&mut synctex_cache)).unwrap();
        assert_eq!(resolution.location.line, 6);
        assert!(!resolution.location.precise);
        assert!(
            resolution
                .warning
                .unwrap()
                .contains("PDF text refinement unavailable")
        );
        fs::write(&source, "\\relax\n").unwrap();
        assert!(
            matches!(task.resolve(Some(&mut synctex_cache)), Err(error) if error.to_string().contains("out of range"))
        );
        task.best_guess = true;
        let approximate = task.resolve(Some(&mut synctex_cache)).unwrap();
        assert_eq!(approximate.location.line, 6);
        assert!(!approximate.location.precise);
        assert!(
            approximate
                .warning
                .unwrap()
                .contains("best-guess navigation")
        );

        // A changed companion must reject before any source refinement/delivery.
        fs::remove_file(task.path.with_extension("synctex.gz")).unwrap();
        assert!(
            matches!(task.resolve(Some(&mut synctex_cache)), Err(error) if error.to_string().contains("revision"))
        );
        task.operation.cancel();
        assert!(
            matches!(task.resolve(Some(&mut synctex_cache)), Err(error) if error.kind() == io::ErrorKind::Interrupted)
        );
    }
}

use pdfterm::{
    pdf::{DarkModeStyle, FitMode, RenderKey, RenderRequest, RenderWorker, WorkerMessage},
    process::{self, Operation},
    synctex::{self, DocumentRevision},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn pdfium_test_lock() -> std::sync::MutexGuard<'static, ()> {
    // This integration binary has its own process-global PDFium bindings.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().expect("native PDFium fixture lock poisoned")
}

fn message(worker: &RenderWorker) -> WorkerMessage {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(message) = worker.try_recv() {
            return message;
        }
        assert!(Instant::now() < deadline, "worker never completed request");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn beamer_overlay_forward_uses_visible_source_context() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("overlay.tex");
    fs::write(
        &source,
        r"\documentclass{beamer}
\begin{document}
\begin{frame}{Overlay}
\begin{itemize}
\item Base words remain visible.
\pause
\item UniqueZephyr appears after the first overlay.
\end{itemize}
\only<3->{AnotherNebula appears on the third overlay.}
\end{frame}
\end{document}
",
    )
    .unwrap();
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(directory.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "overlay.tex",
            ]),
        &Operation::new(Duration::from_secs(30)),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let pdf = directory.path().join("overlay.pdf");
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 5, 7).unwrap().page,
        1
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 7, 7).unwrap().page,
        2
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 9, 12).unwrap().page,
        3
    );
    assert_eq!(
        synctex::resolve_forward(&pdf, &source, 9, 1).unwrap().page,
        1
    );
}

#[test]
fn real_synctex_revisions_failed_hit_tests_and_coarse_refinement() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("navigation.tex");
    fs::write(&source, include_str!("fixtures/navigation.tex")).unwrap();
    let output = process::output(
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
    .expect("pdflatex is required for the real SyncTeX fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let pdf = directory.path().join("navigation.pdf");
    let request = synctex::resolve_forward(&pdf, &source, 6, 1).unwrap();
    let point = (
        request.h + request.width / 2.,
        request.v - request.height / 2.,
    );
    let inverse = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        Some(("target zephyr", 7)),
        4,
        &Operation::default(),
    )
    .unwrap();
    assert_eq!(inverse.location.line, 6);
    assert!(inverse.location.precise);
    let expected = fs::read_to_string(&source)
        .unwrap()
        .lines()
        .nth(5)
        .unwrap()
        .find("zephyr")
        .unwrap();
    assert_eq!(inverse.location.byte_column, expected);
    fs::write(&source, [0xff]).unwrap();
    assert!(synctex::resolve_forward(&pdf, &source, 6, 1).is_err());
    let coarse = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        Some(("zephyr", 0)),
        4,
        &Operation::default(),
    )
    .unwrap();
    assert_eq!(coarse.location.line, 6);
    assert!(!coarse.location.precise);
    assert!(
        coarse
            .warning
            .unwrap()
            .contains("source refinement unavailable")
    );
    for special in ["oversized", "fifo"] {
        fs::remove_file(&source).unwrap();
        if special == "oversized" {
            fs::write(&source, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
        } else {
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        assert!(synctex::resolve_forward(&pdf, &source, 6, 1).is_err());
        let result = synctex::resolve_inverse(
            &pdf,
            synctex::InversePoint {
                page: request.page,
                x: point.0,
                y_from_top: point.1,
                page_height_pt: 792.0,
            },
            Some(("zephyr", 0)),
            4,
            &Operation::default(),
        )
        .unwrap();
        assert_eq!(result.location.line, 6);
        assert!(!result.location.precise);
        assert!(result.warning.is_some());
    }
    fs::remove_file(&source).unwrap();
    fs::write(&source, include_str!("fixtures/navigation.tex")).unwrap();
    let word_request = synctex::resolve_forward(&pdf, &source, 6, expected as u32 + 1).unwrap();

    // One PDFium lifetime in this process, including both reload revisions.
    let _native = pdfium_test_lock();
    let worker = RenderWorker::spawn(1, pdf.clone(), None);
    assert_eq!(worker.wait_until_ready().unwrap().0, 2);
    let key = RenderKey {
        document_id: 1,
        page: 0,
        width: 600,
        height: 800,
        zoom: 100,
        fit: FitMode::Width,
        invert: false,
        dark_mode_style: DarkModeStyle::new([0; 3], [255; 3]),
        search_request_id: 0,
        search_highlight: [255, 255, 0],
        link_mode: false,
        link_highlight: [255, 255, 0],
        selected_link_ordinal: None,
    };
    worker.begin_generation(1);
    worker.flash(
        1,
        word_request.page - 1,
        word_request.rect(),
        word_request.word,
    );
    worker.render(RenderRequest { key, generation: 1 }).unwrap();
    let WorkerMessage::Frame(frame) = message(&worker) else {
        panic!("expected displayed frame")
    };
    let displayed = frame.revision;
    assert_eq!(displayed, DocumentRevision::read(&pdf).unwrap());
    let highlight = frame.flash.as_ref().unwrap();
    assert!(highlight.error.is_none());
    assert!(highlight.word_precise);
    assert!(highlight.rect.right - highlight.rect.left < request.width / 2.0);
    assert!(highlight.rect.left > request.h);
    worker.flash(1, request.page - 1, request.rect(), None);
    worker.render(RenderRequest { key, generation: 1 }).unwrap();
    let WorkerMessage::Frame(coarse) = message(&worker) else {
        panic!("expected coarse forward frame")
    };
    let coarse = coarse.flash.as_ref().unwrap();
    assert!(!coarse.word_precise);
    assert!(coarse.error.is_none());
    assert!((coarse.rect.right - coarse.rect.left - request.width).abs() < 0.001);
    for (id, bad_key, success) in [
        (1, RenderKey { page: 99, ..key }, false),
        (
            2,
            RenderKey {
                document_id: 99,
                ..key
            },
            false,
        ),
        (3, key, true),
    ] {
        worker.page_point(displayed, id, 100, 100, bad_key);
        let WorkerMessage::PagePoint {
            request_id, result, ..
        } = message(&worker)
        else {
            panic!("missing hit-test completion")
        };
        assert_eq!(request_id, id);
        assert_eq!(result.is_ok(), success);
    }
    let companion = pdf.with_extension("synctex.gz");
    let contents = fs::read(&companion).unwrap();
    fs::remove_file(&companion).unwrap();
    fs::write(&companion, contents).unwrap();
    assert!(
        displayed.check(&pdf).is_err(),
        "companion-only replacement must invalidate old pixels"
    );
    worker.open(1, pdf.clone()).unwrap();
    assert!(matches!(message(&worker), WorkerMessage::Opened { .. }));
    worker.page_point(displayed, 4, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing stale reply")
    };
    assert!(result.unwrap_err().contains("revision"));
    worker.page_point(DocumentRevision::read(&pdf).unwrap(), 5, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing recovered reply")
    };
    assert!(result.is_ok());
    worker.close(1);
    worker.page_point(DocumentRevision::read(&pdf).unwrap(), 6, 100, 100, key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("missing closed-document reply")
    };
    assert!(result.is_err());
}

fn compile_out_of_tree_fixture(root: &Path, contents: &str) -> (PathBuf, PathBuf) {
    fs::create_dir(root.join("sources")).unwrap();
    fs::create_dir(root.join("build")).unwrap();
    let source = root.join("sources/original.tex");
    fs::write(&source, contents).unwrap();
    let output = process::output(
        Command::new("pdflatex")
            .current_dir(root)
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the real SyncTeX fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    (root.join("build/deck.pdf"), source)
}

fn painted_word_occurrences(
    pdf: &Path,
    word: &str,
) -> Vec<(synctex::InversePoint, synctex::InversePoint)> {
    let _native = pdfium_test_lock();
    let worker = RenderWorker::spawn(1, pdf.to_owned(), None);
    let (pages, _, _) = worker.wait_until_ready().unwrap();
    let revision = DocumentRevision::read(pdf).unwrap();
    let mut keys = Vec::with_capacity(pages as usize);
    for page in 0..pages {
        let key = RenderKey {
            document_id: 1,
            page,
            width: 800,
            height: 600,
            zoom: 100,
            fit: FitMode::Page,
            invert: false,
            dark_mode_style: DarkModeStyle::new([0; 3], [255; 3]),
            search_request_id: 0,
            search_highlight: [255, 255, 0],
            link_mode: false,
            link_highlight: [255, 255, 0],
            selected_link_ordinal: None,
        };
        worker.render(RenderRequest { key, generation: 0 }).unwrap();
        let WorkerMessage::Frame(frame) = message(&worker) else {
            panic!("expected fixture page frame");
        };
        keys.push((key, frame.width, frame.height));
    }
    worker
        .find_visible(1, 1, revision, word.to_owned(), keys)
        .unwrap();
    let WorkerMessage::VisibleMatches { matches, .. } = message(&worker) else {
        panic!("expected painted fixture word matches");
    };
    assert!(!matches.is_empty(), "fixture word {word} must be painted");
    let mut occurrences = Vec::with_capacity(matches.len());
    for matched in &matches {
        let left = matched.rects.iter().map(|rect| rect.left).min().unwrap();
        let right = matched.rects.iter().map(|rect| rect.right).max().unwrap();
        let top = matched.rects.iter().map(|rect| rect.top).min().unwrap();
        let bottom = matched.rects.iter().map(|rect| rect.bottom).max().unwrap();
        let mut points = [None, None];
        for (index, (x, y)) in [matched.hit, ((left + right) / 2, (top + bottom) / 2)]
            .into_iter()
            .enumerate()
        {
            worker.page_point(revision, index as u64 + 2, x, y, matched.key);
            let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
                panic!("expected fixture glyph point");
            };
            let click = result.unwrap();
            let (context, offset) = click.text.unwrap().expect("fixture glyph must be hit");
            assert!(
                context
                    .match_indices(word)
                    .any(|(start, word)| { (start..start + word.len()).contains(&offset) }),
                "hit offset {offset} in {context:?} is not inside fixture word {word}"
            );
            points[index] = Some(click.synctex);
        }
        occurrences.push((points[0].unwrap(), points[1].unwrap()));
    }
    // Finish native document destruction before releasing the fixture lock.
    worker.close(1);
    worker.page_point(revision, 4, 0, 0, matches[0].key);
    let WorkerMessage::PagePoint { result, .. } = message(&worker) else {
        panic!("expected closed native fixture reply");
    };
    assert!(result.is_err());
    occurrences
}

fn painted_word_points(pdf: &Path, word: &str) -> (synctex::InversePoint, synctex::InversePoint) {
    let mut occurrences = painted_word_occurrences(pdf, word);
    assert_eq!(occurrences.len(), 1, "fixture word {word} must be unique");
    occurrences.pop().unwrap()
}

#[test]
fn real_fragile_frames_remap_by_clicked_page_with_separate_output_directory() {
    let contents = r"\documentclass{beamer}
\title{GlobalAurora Metadata Example}
\begin{document}
\begin{frame}
\titlepage
\end{frame}
\begin{frame}[fragile]{First Original Frame}
EarlyZephyr belongs only to the first frame.
FirstCopper is another distinctive sentence here.
\end{frame}
\begin{frame}[fragile]{Second Original Frame}
LateNebula belongs only to the second frame.
SecondSilver is another distinctive sentence here.
\end{frame}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    // A same-basename sibling is not source-map/project evidence.
    fs::write(
        pdf.with_extension("tex"),
        "\\title{Wrong GlobalAurora Metadata Example}\n",
    )
    .unwrap();
    for (word, page) in [("EarlyZephyr", 2), ("LateNebula", 3)] {
        let (point, center) = painted_word_points(&pdf, word);
        assert_eq!(point.page, page);
        let inverse =
            synctex::resolve_inverse(&pdf, point, Some((word, 0)), 4, &Operation::default())
                .unwrap();
        assert_eq!(
            Path::new(&inverse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        let expected = contents.lines().position(|row| row.contains(word)).unwrap() as u32 + 1;
        assert_eq!(inverse.location.line, expected);
        assert_eq!(inverse.location.byte_column, 0);
        assert!(inverse.location.precise);
        assert!(inverse.warning.is_none());
        let coarse =
            synctex::resolve_inverse(&pdf, center, None, 4, &Operation::default()).unwrap();
        assert_eq!(
            Path::new(&coarse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        let closing = contents
            .lines()
            .skip(expected as usize - 1)
            .position(|row| row == "\\end{frame}")
            .unwrap() as u32
            + expected;
        assert_eq!(coarse.location.line, closing);
        assert!(!coarse.location.precise);
        assert!(coarse.warning.is_none());
    }
    let (point, _) = painted_word_points(&pdf, "GlobalAurora");
    assert_eq!(point.page, 1);
    let metadata = synctex::resolve_inverse(
        &pdf,
        point,
        Some(("GlobalAurora Metadata Example", 0)),
        4,
        &Operation::default(),
    )
    .unwrap();
    assert_eq!(
        Path::new(&metadata.location.file),
        fs::canonicalize(&source).unwrap()
    );
    assert_eq!(metadata.location.line, 2);
    assert!(metadata.location.precise);
    assert!(metadata.warning.is_none());
}

#[test]
fn real_macro_definition_and_same_word_label_stay_coarse() {
    let contents = r"\documentclass{article}
\usepackage{hyperref}
\newcommand{\Hidden}{Zephyr}
\begin{document}
\Hidden\label{Zephyr}

Printed prose is visible here.
éé \href{https://example.test/DisplayAmber}{DisplayAmber stays printed.}
\begin{figure}[h]
\caption{PrintedCopper remains visible.}
\end{figure}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let (point, _) = painted_word_points(&pdf, "Zephyr");
    let coarse = synctex::resolve_inverse(&pdf, point, None, 4, &Operation::default()).unwrap();
    let inverse =
        synctex::resolve_inverse(&pdf, point, Some(("Zephyr", 0)), 4, &Operation::default())
            .unwrap();
    assert_eq!(
        Path::new(&inverse.location.file),
        fs::canonicalize(&source).unwrap()
    );
    assert_eq!(inverse.location.line, coarse.location.line);
    assert!(
        !inverse.location.precise,
        "macro body/label key cannot prove printed text"
    );
    assert_eq!(
        (
            inverse.location.byte_column,
            inverse.location.column,
            inverse.location.column_char
        ),
        (0, 1, 1)
    );
    assert!(inverse.warning.is_none());
    for word in ["DisplayAmber", "PrintedCopper"] {
        let (point, _) = painted_word_points(&pdf, word);
        let inverse =
            synctex::resolve_inverse(&pdf, point, Some((word, 0)), 4, &Operation::default())
                .unwrap();
        let (row, text) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let byte = text.rfind(word).unwrap();
        assert_eq!(inverse.location.line, row as u32 + 1);
        assert_eq!(inverse.location.byte_column, byte);
        assert_eq!(
            inverse.location.column,
            text[..byte].encode_utf16().count() + 1
        );
        assert_eq!(
            inverse.location.column_char,
            text[..byte].chars().count() + 1
        );
        assert!(inverse.location.precise);
        assert!(inverse.warning.is_none());
    }
}

#[test]
fn real_fragile_macro_title_and_footer_do_not_refine_to_duplicate_body() {
    let contents = r"\documentclass{beamer}
\newcommand{\DocTitle}{FooterZephyr}
\title[\DocTitle]{\DocTitle}
\setbeamertemplate{footline}{%
  \leavevmode%
  \hbox{%
    \begin{beamercolorbox}[wd=\paperwidth,ht=2.5ex,dp=1.125ex,right]{title in head/foot}%
      \usebeamerfont{title in head/foot}\insertshorttitle\hspace*{2em}%
    \end{beamercolorbox}%
  }%
}
\begin{document}
\begin{frame}
\titlepage
\end{frame}
\begin{frame}[fragile]{\DocTitle}
\centering Body repeats FooterZephyr as a literal word.
\end{frame}
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let mut occurrences: Vec<_> = painted_word_occurrences(&pdf, "FooterZephyr")
        .into_iter()
        .filter(|(_, point)| point.page == 2)
        .collect();
    occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
    assert_eq!(
        occurrences.len(),
        3,
        "frame title, literal body, and running footer"
    );
    for (index, (_, point)) in occurrences.iter().enumerate() {
        let inverse = synctex::resolve_inverse(
            &pdf,
            *point,
            Some(("FooterZephyr", 0)),
            4,
            &Operation::default(),
        )
        .unwrap();
        assert_eq!(
            Path::new(&inverse.location.file),
            fs::canonicalize(&source).unwrap()
        );
        assert!(inverse.warning.is_none());
        if index == 1 {
            assert_eq!(inverse.location.line, 17);
            assert_eq!(inverse.location.byte_column, 24);
            assert!(inverse.location.precise);
        } else {
            assert_eq!(inverse.location.line, 18);
            assert_eq!(inverse.location.byte_column, 0);
            assert!(
                !inverse.location.precise,
                "macro title/footer must not borrow body prose"
            );
        }
    }
}

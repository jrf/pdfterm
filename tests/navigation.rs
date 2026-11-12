use pdfterm::{
    pdf::{DarkModeStyle, FitMode, RenderKey, RenderRequest, RenderWorker, WorkerMessage},
    process::{self, Operation},
    synctex::{self, DocumentRevision, ForwardSource},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn resolve_forward(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
) -> std::io::Result<synctex::ForwardRequest> {
    synctex::resolve_forward_with_library(pdf, file, line, column, None, ForwardSource::default())
}

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
fn unmapped_hook_does_not_resolve_to_a_separate_comma_and_arrow() {
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(
        directory.path(),
        "\\documentclass{article}\n\
         \\pdfgentounicode=1\n\
         \\pagestyle{empty}\n\
         \\begin{document}\n\
         $a\\hookrightarrow b$\\\\\n\
         $a,\\rightarrow b$\n\
         \\end{document}\n",
    );
    let source = source.canonicalize().unwrap();
    let points = directory.path().join("points.jsonl");
    fs::write(
        &points,
        "{\"page\":1,\"x\":158.12168,\"y\":131.30801}\n\
         {\"page\":1,\"x\":162.80908,\"y\":132.26938}\n\
         {\"page\":1,\"x\":140.46785,\"y\":147.14844}\n",
    )
    .unwrap();
    // Reuse bounded process-group supervision while supplying the fixture's batch stdin.
    let output = process::output(
        Command::new("sh")
            .args([
                "-c",
                "exec \"$1\" \"$2\" --synctex-edit-batch < \"$3\"",
                "hook-probe",
            ])
            .arg(env!("CARGO_BIN_EXE_pdfterm"))
            .arg(&pdf)
            .arg(points)
            .env("XDG_CONFIG_HOME", directory.path().join("cli-config")),
        &Operation::default(),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    for (result, line) in results.iter().zip([5, 5, 6]) {
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            result["location"]["file"],
            source.to_str().unwrap(),
            "{result}"
        );
        assert_eq!(result["location"]["line"], line, "{result}");
        assert_eq!(result["location"]["byte_column"], 2, "{result}");
        assert_eq!(result["location"]["precise"], true, "{result}");
    }
}

#[test]
fn decoded_math_unicode_is_preserved_without_losing_unmapped_operators() {
    let _native = pdfium_test_lock();
    let directory = tempfile::tempdir().unwrap();
    // Both slots share one embedded font, so raw-slot presence cannot prove ownership.
    let (pdf, _) = compile_out_of_tree_fixture(
        directory.path(),
        "\\documentclass{article}\n\
         \\usepackage{graphicx,stmaryrd}\n\
         \\pdfgentounicode=1\n\
         \\pdfglyphtounicode{binampersand}{0026}\n\
         \\pagestyle{empty}\n\
         \\begin{document}\n\
         Native \\scalebox{0.5}[1]{$a\\binampersand b\\curlywedgeuparrow c$} marker\\par\n\
         \\[\\sum_{i=1}^n i\\quad\\int_0^1 u\\,du\\quad\\bigcup_i A_i\\quad u.v/w\\]\n\
         \\end{document}\n",
    );
    let worker = RenderWorker::spawn(1, pdf, None);
    let (_, _, revision) = worker.wait_until_ready().unwrap();
    for (request_id, query, expected) in [
        (1, "&", 1),
        (3, "∑", 1),
        (4, "∫", 1),
        (5, "∪", 1),
        (6, ".", 1),
        (7, "/", 1),
        (8, "X", 0),
        (9, "Z", 0),
        (11, "[", 0),
    ] {
        worker.search(1, request_id, query.into());
        loop {
            match message(&worker) {
                WorkerMessage::SearchResults {
                    request_id: actual,
                    total_occurrences,
                    ..
                } if actual == request_id => {
                    assert_eq!(total_occurrences, expected, "search {query:?}");
                    break;
                }
                WorkerMessage::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    worker.close(1);
    worker
        .find_visible(1, 10, revision, String::new(), Vec::new())
        .unwrap();
    assert!(matches!(
        message(&worker),
        WorkerMessage::VisibleMatchesError { request_id: 10, .. }
    ));
}

#[test]
fn inverse_native_near_boundary_preserves_the_clicked_source_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let child = root.join("child.tex");
    fs::write(
        &child,
        format!("{}ChildZephyr paragraph.\\par\n", "\n".repeat(40)),
    )
    .unwrap();
    let (pdf, _) = compile_out_of_tree_fixture(
        &root,
        "\\documentclass{article}\n\
         \\begin{document}\n\
         MainCopper paragraph.\\par\n\
         \\input{child}\n\
         \\end{document}\n",
    );
    // This point lies on the child's side of the native nearest-anchor boundary.
    // Rounding y to 137.28 selects MainCopper in the other input instead.
    let result = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: 1,
            x: 140.0,
            y_from_top: 137.28305,
            page_height_pt: 792.0,
        },
        None,
        0,
        &Operation::default(),
        false,
    )
    .unwrap();
    assert_eq!(
        Path::new(&result.location.file),
        child.canonicalize().unwrap()
    );
    assert_eq!(result.location.line, 41);
    assert!(!result.location.precise);
    assert!(result.warning.is_none());
}

#[test]
fn inverse_starred_subparagraph_heading_keeps_its_word_column() {
    let contents = "\\documentclass{article}\n\\begin{document}\n\
                    Preceding blorple sentence.\n\n\
                    \\subparagraph*{Frobnitz quux on digital snazzles.}\n\
                    In many wazzit applications such as snorfle\n\
                    and cellular blimble,\n\
                    zibble flumple,\n\
                    blorple scans,\n\
                    quux flow,\n\
                    and glorp fields,\n\
                    or nimbus networks,\n\
                    we examine\n\
                    initial snazzles as\n\
                    regular grids.\n\\end{document}\n";
    for contents in [
        contents.to_owned(),
        contents.replacen("In many", "\\textbf{In} many", 1),
        contents.replacen("snazzles.}", "snazzles.}\\label{heading}", 1),
        contents.replacen("snazzles.}", "snazzles.}\n\\label{heading}", 1),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (pdf, source) = compile_out_of_tree_fixture(directory.path(), &contents);
        let mut occurrences = painted_word_occurrences(&pdf, "snazzles");
        occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
        assert_eq!(occurrences.len(), 2);
        let lines = contents
            .lines()
            .enumerate()
            .filter_map(|(index, row)| row.contains("snazzles").then_some(index + 1));
        for ((hit, center), line) in occurrences.into_iter().zip(lines) {
            let byte = contents
                .lines()
                .nth(line - 1)
                .unwrap()
                .find("snazzles")
                .unwrap();
            for point in [hit, center] {
                for best_guess in [false, true] {
                    let result = synctex::resolve_inverse(
                        &pdf,
                        point,
                        Some(("snazzles", 0)),
                        3,
                        &Operation::default(),
                        best_guess,
                    )
                    .unwrap();
                    assert_eq!(
                        Path::new(&result.location.file).canonicalize().unwrap(),
                        source.canonicalize().unwrap()
                    );
                    assert_eq!(result.location.line, line as u32, "{result:?}");
                    assert_eq!(result.location.byte_column, byte, "{result:?}");
                    assert!(result.location.precise, "{result:?}");
                }
            }
        }
    }
}

#[test]
fn inverse_generated_section_number_does_not_borrow_next_lines_literal() {
    for (numbering, word) in [
        ("", "1"),
        ("\\renewcommand{\\thesection}{\\roman{section}}\n", "i"),
        ("\\renewcommand{\\thesection}{\\alph{section}}\n", "a"),
    ] {
        for (separator, title) in [
            ("", "Copper"),
            ("", "Copper\nore"),
            ("", "Copper\n\\{ore"),
            ("\n", "Copper"),
        ] {
            let contents = format!(
                "\\documentclass{{article}}\n\
                 \\usepackage[paperwidth=420pt,paperheight=300pt,margin=35pt]{{geometry}}\n\
                 \\pagestyle{{empty}}\n\
                 \\begin{{document}}\n\
                 {numbering}\\section{separator}{{{title}}}\n\
                 {word} Copper.\n\
                 \\end{{document}}\n"
            );
            let directory = tempfile::tempdir().unwrap();
            let (pdf, source) = compile_out_of_tree_fixture(directory.path(), &contents);
            let source = source.canonicalize().unwrap();
            let heading_line = contents
                .lines()
                .position(|row| row.starts_with("\\section"))
                .unwrap() as u32
                + u32::from(separator == "\n")
                + title.lines().count() as u32;
            let mut occurrences = painted_word_occurrences(&pdf, word);
            occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
            assert_eq!(occurrences.len(), 2, "{word}, {title}");
            for ((hit, center), generated) in occurrences.into_iter().zip([true, false]) {
                for point in [hit, center] {
                    for best_guess in [false, true] {
                        let result = synctex::resolve_inverse(
                            &pdf,
                            point,
                            Some((word, 0)),
                            3,
                            &Operation::default(),
                            best_guess,
                        )
                        .unwrap();
                        assert_eq!(
                            Path::new(&result.location.file).canonicalize().unwrap(),
                            source
                        );
                        assert_eq!(
                            result.location.line,
                            heading_line + u32::from(!generated),
                            "{word}, {title}, best={best_guess}: {result:?}"
                        );
                        assert_eq!(result.location.byte_column, 0, "{result:?}");
                        assert_eq!(result.location.precise, !generated, "{result:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn inverse_generated_contents_heading_does_not_borrow_next_lines_literal() {
    let contents = "\\documentclass{article}\n\
                    \\usepackage[paperwidth=420pt,paperheight=300pt,margin=35pt]{geometry}\n\
                    \\pagestyle{empty}\n\
                    \\begin{document}\n\
                    \\tableofcontents\n\
                    Contents Wazzit.\n\
                    \\end{document}\n";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let source = source.canonicalize().unwrap();
    let mut occurrences = painted_word_occurrences(&pdf, "Contents");
    occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
    assert_eq!(occurrences.len(), 2);
    for ((hit, center), generated) in occurrences.into_iter().zip([true, false]) {
        for point in [hit, center] {
            for best_guess in [false, true] {
                let result = synctex::resolve_inverse(
                    &pdf,
                    point,
                    Some(("Contents", 0)),
                    3,
                    &Operation::default(),
                    best_guess,
                )
                .unwrap();
                assert_eq!(
                    Path::new(&result.location.file).canonicalize().unwrap(),
                    source
                );
                assert_eq!(
                    result.location.line,
                    5 + u32::from(!generated),
                    "best={best_guess}: {result:?}"
                );
                assert_eq!(result.location.byte_column, 0, "{result:?}");
                assert_eq!(result.location.precise, !generated, "{result:?}");
            }
        }
    }
}

#[test]
fn inverse_unbraced_section_keeps_shared_native_anchor_for_duplicate_numbers() {
    for (prelude, prefix, title, heading_line) in [
        ("", "", "Copper", 6),
        ("", "[Short]\n", "C", 7),
        (
            "",
            "% skipped comment\n[Short]\n% skipped comment\n",
            "C",
            9,
        ),
        ("\\newcommand{\\Unused}{\\verb}\n", "", "C", 7),
    ] {
        let contents = format!(
            "\\documentclass{{article}}\n\
             \\usepackage[paperwidth=420pt,paperheight=300pt,margin=35pt]{{geometry}}\n\
             \\pagestyle{{empty}}\n\
             \\begin{{document}}\n\
             {prelude}\\section\n\
             {prefix}{title}\n\
             1 Wazzit neighboring laterword end.\n\
             \\end{{document}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let (pdf, source) = compile_out_of_tree_fixture(directory.path(), &contents);
        let source = source.canonicalize().unwrap();
        let mut occurrences = painted_word_occurrences(&pdf, "1");
        occurrences.sort_by(|a, b| a.1.y_from_top.total_cmp(&b.1.y_from_top));
        assert_eq!(occurrences.len(), 2);
        for ((hit, center), generated) in occurrences.into_iter().zip([true, false]) {
            for point in [hit, center] {
                for best_guess in [false, true] {
                    let result = synctex::resolve_inverse(
                        &pdf,
                        point,
                        Some(("1", 0)),
                        3,
                        &Operation::default(),
                        best_guess,
                    )
                    .unwrap();
                    assert_eq!(
                        Path::new(&result.location.file).canonicalize().unwrap(),
                        source
                    );
                    let direct_body = !generated && title == "C";
                    assert_eq!(
                        result.location.line,
                        heading_line + u32::from(direct_body),
                        "best={best_guess}: {result:?}"
                    );
                    assert_eq!(result.location.byte_column, 0, "{result:?}");
                    assert_eq!(result.location.precise, direct_body, "{result:?}");
                }
            }
        }
    }
}

#[test]
fn cli_wrapped_paragraph_first_word_keeps_its_source_line_and_column() {
    let contents = "\\documentclass{article}\n\
                    \\usepackage[paperwidth=420pt,paperheight=300pt,margin=35pt]{geometry}\n\
                    \\pagestyle{empty}\n\
                    \\newcommand{\\xref}[1]{\\ref{#1}}\n\
                    \\begin{document}\n\
                    \\section{Copper widgets}\\label{widget}\n\
                    Hauling \\xref{widget} slowly past a lopsided\n\
                    turnip barrow with inline $V_\\star$ beside damp gravel nudges an\n\
                    brambling porridges teakettle (\\xref{widget}):\n\
                    \\end{document}\n";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let source = source.canonicalize().unwrap();
    let mut requests = String::new();
    let mut expected = Vec::new();
    for (word, precise) in [
        ("brambling", true),
        ("nudges", true),
        ("porridges", true),
        ("teakettle", true),
        // The preceding macro argument still requires separate scope proof.
        ("turnip", false),
    ] {
        let (line, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let byte = row.find(word).unwrap();
        let (hit, center) = painted_word_points(&pdf, word);
        for point in [hit, center] {
            requests.push_str(
                &serde_json::json!({
                    "page": point.page,
                    "x": point.x,
                    "y": point.y_from_top,
                })
                .to_string(),
            );
            requests.push('\n');
            expected.push((
                word,
                line + usize::from(precise),
                if precise { byte } else { 0 },
                precise,
            ));
        }
    }
    let points = directory.path().join("points.jsonl");
    fs::write(&points, requests).unwrap();
    for best_guess in [false, true] {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec \"$@\" < \"$0\""])
            .arg(&points)
            .arg(env!("CARGO_BIN_EXE_pdfterm"))
            .arg(&pdf)
            .arg("--synctex-edit-batch")
            .env("XDG_CONFIG_HOME", directory.path().join("cli-config"));
        if best_guess {
            cmd.arg("--synctex-best-guess");
        }
        let output = process::output(&mut cmd, &Operation::default()).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(results.len(), expected.len());
        for (result, &(word, line, byte, precise)) in results.iter().zip(&expected) {
            assert_eq!(result["ok"], true, "{word}, best={best_guess}: {result}");
            assert_eq!(result["pdf_word"], word, "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], line, "{result}");
            assert_eq!(result["location"]["byte_column"], byte, "{result}");
            assert_eq!(result["location"]["precise"], precise, "{result}");
        }
    }
}

#[test]
fn inverse_multiline_formatting_keeps_each_words_source_column() {
    for (wrapper, math) in [
        ("textbf", false),
        ("textit", false),
        ("emph", false),
        ("textnormal", false),
        ("mathbf", true),
        ("mathrm", true),
        ("mathsf", true),
        ("mathbb", true),
    ] {
        let styled = if math {
            format!("$\\{wrapper}{{J+\nX}}$")
        } else {
            format!("\\{wrapper}{{Snazzles frobnitz quux are\ninherently wazzit.}}")
        };
        let contents = format!(
            "\\documentclass{{article}}\n\\usepackage{{amssymb}}\n\\begin{{document}}\n\
             Preceding blorple sentence.\n\\begin{{enumerate}}\n\
             \\item {styled}\nNormal blimble body.\n\
             \\end{{enumerate}}\n\\end{{document}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let (pdf, source) = compile_out_of_tree_fixture(directory.path(), &contents);
        let words: &[&str] = if math {
            &["J", "X", "blimble"]
        } else {
            &["Snazzles", "frobnitz", "inherently", "wazzit", "blimble"]
        };
        for &word in words {
            let (hit, center) = painted_word_points(&pdf, word);
            let (line, row) = contents
                .lines()
                .enumerate()
                .find(|(_, row)| row.contains(word))
                .unwrap();
            let byte = row.find(word).unwrap();
            for point in [hit, center] {
                for best_guess in [false, true] {
                    let result = synctex::resolve_inverse(
                        &pdf,
                        point,
                        Some((word, 0)),
                        3,
                        &Operation::default(),
                        best_guess,
                    )
                    .unwrap();
                    assert_eq!(
                        Path::new(&result.location.file).canonicalize().unwrap(),
                        source.canonicalize().unwrap()
                    );
                    assert_eq!(
                        (
                            result.location.line,
                            result.location.byte_column,
                            result.location.precise
                        ),
                        (line as u32 + 1, byte, true),
                        "{wrapper}, {word}: {result:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn beamer_overlay_forward_uses_visible_source_context() {
    let _native = pdfium_test_lock();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("overlay.tex");
    fs::write(
        &source,
        r"\documentclass{beamer}
\begin{document}
\begin{frame}Cover page.\end{frame}
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
    assert_eq!(resolve_forward(&pdf, &source, 6, 7).unwrap().page, 2);
    assert_eq!(resolve_forward(&pdf, &source, 8, 7).unwrap().page, 3);
    assert_eq!(resolve_forward(&pdf, &source, 10, 12).unwrap().page, 4);
    let line_anchor = resolve_forward(&pdf, &source, 10, 7).unwrap();
    assert_eq!(line_anchor.page, 2);
    assert!(line_anchor.word.is_none());
    assert_eq!(resolve_forward(&pdf, &source, 10, 1).unwrap().page, 2);
}

#[test]
fn forward_multiline_caption_uses_caption_region_instead_of_graphic_legend() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let graphic_root = root.join("graphic");
    fs::create_dir(&graphic_root).unwrap();
    compile_out_of_tree_fixture(
        &graphic_root,
        "\\documentclass{article}\n\
         \\usepackage[paperwidth=220pt,paperheight=80pt,margin=10pt]{geometry}\n\
         \\pagestyle{empty}\n\
         \\begin{document}\n\
         \\noindent LegendQuartz labels the graphic.\n\
         \\end{document}\n",
    );
    let contents = r"\documentclass{article}
\usepackage{graphicx}
\usepackage[paperwidth=500pt,paperheight=400pt,margin=30pt]{geometry}
\pagestyle{empty}
\begin{document}
OutsideAmber remains ordinary prose.
\begin{figure}[h!]
\centering
\includegraphics{graphic/build/deck.pdf}
\caption[ShortQuartz]{CaptionCopper illustrates this literal caption.
InteriorNickel belongs to the caption rather than the legend above.
The final literal sentence closes the caption.
}
\end{figure}
\end{document}
";
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    let caption = painted_word_points(&pdf, "CaptionCopper").1;
    let legend = painted_word_points(&pdf, "LegendQuartz").1;
    let outside = painted_word_points(&pdf, "OutsideAmber").1;
    assert_eq!(caption.page, legend.page);
    assert!(legend.y_from_top < caption.y_from_top);
    let cursor = |word: &str| {
        let (index, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        (index as u32 + 1, row.find(word).unwrap() as u32 + 1)
    };
    let _native = pdfium_test_lock();
    let (line, column) = cursor("OutsideAmber");
    let prose = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        line,
        column,
        None,
        ForwardSource::default(),
    )
    .unwrap();
    assert_eq!(prose.page, outside.page);
    assert!((prose.h..=prose.h + prose.width).contains(&outside.x));
    assert!((prose.v - prose.height..=prose.v).contains(&outside.y_from_top));
    assert!(!(prose.v - prose.height..=prose.v).contains(&caption.y_from_top));
    // Anchor-only requests keep the native query without adding a word hint.
    let (line, column) = cursor("CaptionCopper");
    let raw = synctex::resolve_forward_with_library(
        &pdf,
        &source,
        line,
        column,
        None,
        ForwardSource {
            anchor_only: true,
            ..ForwardSource::default()
        },
    )
    .unwrap();
    assert_eq!(raw.page, legend.page);
    assert!((raw.h..=raw.h + raw.width).contains(&legend.x));
    assert!((raw.v - raw.height..=raw.v).contains(&legend.y_from_top));
    assert!(!(raw.v - raw.height..=raw.v).contains(&caption.y_from_top));
    assert!(raw.word.is_none());
    let context = ForwardSource::default();
    for word in ["CaptionCopper", "InteriorNickel"] {
        let (line, column) = cursor(word);
        let request =
            synctex::resolve_forward_with_library(&pdf, &source, line, column, None, context)
                .unwrap();
        assert_eq!(request.page, caption.page, "{word}: {request:?}");
        assert!(
            (request.h..=request.h + request.width).contains(&caption.x)
                && (request.v - request.height..=request.v).contains(&caption.y_from_top),
            "caption body must reach its printed first row: {word}, {request:?}, {caption:?}"
        );
        assert!(
            !(request.v - request.height..=request.v).contains(&legend.y_from_top),
            "caption body must not highlight the graphic legend: {word}, {request:?}, {legend:?}"
        );
    }
}

#[test]
fn inverse_subfigure_images_use_native_geometry_without_caption_or_sibling_confusion() {
    use pdfium_render::prelude::{PdfPageObjectCommon, PdfPageObjectsCommon, Pdfium};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let graphic_root = root.join("graphic");
    fs::create_dir(&graphic_root).unwrap();
    // Downscaled graphics exercise native boxes that can reach neighboring subfigures.
    let (graphic, _) = compile_out_of_tree_fixture(
        &graphic_root,
        "\\documentclass{article}\n\
         \\usepackage[paperwidth=252pt,paperheight=151.2pt,margin=0pt]{geometry}\n\
         \\pagestyle{empty}\n\
         \\begin{document}\n\
         \\noindent\\rule{252pt}{140pt}\n\
         \\end{document}\n",
    );
    fs::copy(graphic, graphic_root.join("build/other.pdf")).unwrap();
    let contents = r"\documentclass{article}
\usepackage{graphicx,subcaption}
\usepackage[paperwidth=500pt,paperheight=600pt,margin=30pt]{geometry}
\pagestyle{empty}
\begin{document}
\begin{figure}[!t]
\centering
\begin{subfigure}[t]{0.48\textwidth}
\centering
\includegraphics[width=0.92\linewidth]{graphic/build/deck.pdf}
\subcaption{CaptionCopper is beneath the first panel.}
\label{fig:copper}
\end{subfigure}\hfill
\begin{subfigure}[t]{0.48\textwidth}
\centering
\includegraphics[width=0.92\linewidth]{graphic/build/other.pdf}
\subcaption{CaptionNickel is beneath the second panel.}
\label{fig:nickel}
\end{subfigure}

\begin{subfigure}[t]{0.48\textwidth}
\centering
\includegraphics[width=0.92\linewidth]{graphic/build/deck.pdf}
\subcaption{CaptionSilver is beneath the third panel.}
\label{fig:silver}
\end{subfigure}\hfill
\begin{subfigure}[t]{0.48\textwidth}
\centering
\includegraphics[width=0.92\linewidth]{graphic/build/deck.pdf}
\subcaption{CaptionBronze is beneath the fourth panel.}
\label{fig:bronze}
\end{subfigure}

\begin{subfigure}[t]{0.32\textwidth}
\centering
\includegraphics[width=\linewidth]{graphic/build/deck.pdf}
\subcaption{CaptionCobalt is beneath the fifth panel.}
\label{fig:cobalt}
\end{subfigure}\hfill
\begin{subfigure}[t]{0.32\textwidth}
\centering
\includegraphics[width=\linewidth]{graphic/build/other.pdf}
\subcaption{CaptionIndigo is beneath the sixth panel.}
\label{fig:indigo}
\end{subfigure}\hfill
\begin{subfigure}[t]{0.32\textwidth}
\centering
\includegraphics[width=\linewidth]{graphic/build/other.pdf}
\subcaption{CaptionViolet is beneath the seventh panel.}
\label{fig:violet}
\end{subfigure}
\caption{CaptionQuartz describes the complete figure.}
\end{figure}
\end{document}
";
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    let source = source.canonicalize().unwrap();
    // These helpers finish their native workers before the next PDFium transaction.
    let captions = [
        "CaptionCopper",
        "CaptionNickel",
        "CaptionSilver",
        "CaptionBronze",
        "CaptionCobalt",
        "CaptionIndigo",
        "CaptionViolet",
        "CaptionQuartz",
    ]
    .map(|word| (word, painted_word_points(&pdf, word).1));
    let (boxes, page_height) = {
        let _native = pdfium_test_lock();
        let pdfium = Pdfium::default();
        let document = pdfium.load_pdf_from_file(&pdf, None).unwrap();
        assert_eq!(document.pages().len(), 1);
        let page = document.pages().get(0).unwrap();
        let page_height = page.boundaries().media().unwrap().bounds.height().value;
        let mut boxes: Vec<_> = page
            .objects()
            .iter()
            .filter(|object| object.as_x_object_form_object().is_some())
            .map(|object| object.bounds().unwrap().to_rect())
            .collect();
        assert_eq!(
            boxes.len(),
            7,
            "each imported graphic must have native bounds"
        );
        boxes.sort_by(|left, right| {
            right
                .top()
                .value
                .total_cmp(&left.top().value)
                .then_with(|| left.left().value.total_cmp(&right.left().value))
        });
        for pair in boxes.windows(2) {
            if (pair[0].top().value - pair[1].top().value).abs() < 0.01 {
                assert!(pair[0].right().value < pair[1].left().value);
            }
        }
        (boxes, page_height)
    };
    let graphics: Vec<_> = contents
        .lines()
        .enumerate()
        .filter(|(_, row)| row.starts_with(r"\includegraphics"))
        .collect();
    for (index, bounds) in boxes.iter().enumerate() {
        let (line, row) = graphics[index];
        let filename = row.rsplit_once('{').unwrap().1.strip_suffix('}').unwrap();
        assert_eq!(captions[index].1.page, 1);
        assert!(captions[index].1.y_from_top > page_height - bounds.bottom().value);
        for (fx, fy) in [(0.5, 0.5), (0.1, 0.1), (0.9, 0.1), (0.1, 0.9), (0.9, 0.9)] {
            let point = synctex::InversePoint {
                page: 1,
                x: bounds.left().value + bounds.width().value * fx,
                y_from_top: page_height - (bounds.bottom().value + bounds.height().value * fy),
                page_height_pt: page_height,
            };
            for best_guess in [false, true] {
                let inverse = synctex::resolve_inverse(
                    &pdf,
                    point,
                    None,
                    4,
                    &Operation::default(),
                    best_guess,
                )
                .unwrap();
                assert_eq!(Path::new(&inverse.location.file), source);
                assert_eq!(
                    (inverse.location.line, inverse.location.byte_column),
                    (line as u32 + 1, row.find(filename).unwrap()),
                    "image {index}, point {point:?}, best_guess={best_guess}: {inverse:?}"
                );
                assert!(inverse.location.precise, "image {index}: {inverse:?}");
            }
        }
    }
    for (word, point) in captions {
        let (line, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let inverse = synctex::resolve_inverse(
            &pdf,
            point,
            Some((word, 0)),
            4,
            &Operation::default(),
            false,
        )
        .unwrap();
        assert_eq!(Path::new(&inverse.location.file), source);
        assert_eq!(
            (inverse.location.line, inverse.location.byte_column),
            (line as u32 + 1, row.find(word).unwrap()),
            "{word}: {inverse:?}"
        );
        assert!(inverse.location.precise, "{word}: {inverse:?}");
    }
}

#[test]
fn forward_fancyvrb_literal_captions_keep_their_original_line_regions() {
    for environment in ["Verbatim", "Verbatim*"] {
        let contents = format!(
            "\\documentclass{{article}}\n\
             \\usepackage{{fancyvrb}}\n\
             \\usepackage[paperwidth=500pt,paperheight=600pt,margin=30pt]{{geometry}}\n\
             \\pagestyle{{empty}}\n\
             \\begin{{document}}\n\
             \\begin{{{environment}}}\n\
             \\end{{other}} ignored \\end{{{environment}}}\n\
             \\caption{{LiteralCopper originally owns this printed line.\n\
             LiteralNickel owns a separate printed line.\n\
             }}\n\
             \\end{{{environment}}}\n\
             AfterQuartz ordinary prose.\n\
             \\begin{{figure}}[h]\n\
             \\caption{{VisibleCopper begins an actual caption.\n\
             VisibleNickel continues the actual caption.\n\
             }}\n\
             \\end{{figure}}\n\
             \\end{{document}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (pdf, source) = compile_out_of_tree_fixture(&root, &contents);
        let copper = painted_word_points(&pdf, "LiteralCopper").1;
        let nickel = painted_word_points(&pdf, "LiteralNickel").1;
        let caption = painted_word_points(&pdf, "VisibleCopper").1;
        let _native = pdfium_test_lock();
        for (line, column, expected) in [(8, 10, copper), (9, 1, nickel), (15, 1, caption)] {
            let request = synctex::resolve_forward_with_library(
                &pdf,
                &source,
                line,
                column,
                None,
                ForwardSource::default(),
            )
            .unwrap();
            assert_eq!(request.page, expected.page);
            assert!(
                (request.h..=request.h + request.width).contains(&expected.x)
                    && (request.v - request.height..=request.v).contains(&expected.y_from_top),
                "{environment}, source {line}:{column}: {request:?}, {expected:?}"
            );
        }
    }
}

#[test]
fn forward_discarded_arguments_keep_native_page_support() {
    let contents = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\newcommand{\Opaque}{Intruder}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\begin{document}
Cover page.
\newpage
needle alpha beta gamma \Opaque delta epsilon zeta \Duplicate
\newcommand{\Discard}[1]{}
\Discard{needle alpha beta gamma delta epsilon zeta}
\def\DiscardUntil#1\Stop{}

\DiscardUntil needle alpha beta gamma delta epsilon zeta\Stop\Duplicate

Independent Copper owner
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    assert_eq!(painted_word_occurrences(&pdf, "needle").len(), 3);
    let expected = painted_word_points(&pdf, "Copper").1;
    assert_eq!(expected.page, 2);
    let _native = pdfium_test_lock();
    for (prefix, selected) in [(r"\Discard{", "zeta"), (r"\DiscardUntil ", "gamma")] {
        let (index, text) = contents
            .lines()
            .enumerate()
            .find(|(_, text)| text.starts_with(prefix))
            .unwrap();
        let column = text.find(selected).unwrap() as u32 + 1;
        let coarse = resolve_forward(&pdf, &source, index as u32 + 1, column).unwrap();
        assert_eq!(
            coarse.page, expected.page,
            "word failure must retain the line's page instead of opening page one: {coarse:?}"
        );
        assert!(coarse.width > 0.0 || coarse.height > 0.0);
    }
    let literal_line = contents
        .lines()
        .position(|text| text.starts_with("Independent"))
        .unwrap() as u32
        + 1;
    let literal = resolve_forward(&pdf, &source, literal_line, 13).unwrap();
    assert_eq!(literal.page, expected.page);
    assert!((literal.h..=literal.h + literal.width).contains(&expected.x));
    assert!((literal.v - literal.height..=literal.v).contains(&expected.y_from_top));
}

#[test]
fn forward_discarded_multiline_body_retains_the_compiler_line_anchor() {
    let contents = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\newcommand{\Discard}[1]{}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\begin{document}
\input{control.tex}
Cover page.
\newpage
\Discard{
needle alpha beta gamma delta epsilon zeta
}\Duplicate

Independent Copper owner
\end{document}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let control_source = root.join("control.tex");
    fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(&root, contents);
    // Only \Duplicate prints gamma; the queried \Discard argument prints none.
    let printed = painted_word_occurrences(&pdf, "gamma");
    assert_eq!(printed.len(), 1);
    assert_eq!(printed[0].1.page, 2);
    let copper = painted_word_points(&pdf, "Copper").1;
    let expected_control = painted_word_points(&pdf, "NickelControl").1;
    let _native = pdfium_test_lock();
    let query_line = contents
        .lines()
        .position(|row| row == "needle alpha beta gamma delta epsilon zeta")
        .unwrap() as u32
        + 1;
    let coarse = resolve_forward(&pdf, &source, query_line, 19).unwrap();
    assert_eq!(coarse.page, 2);
    assert!(coarse.word.is_none(), "{coarse:?}");
    for anchor_only in [false, true] {
        let anchor = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            query_line,
            19,
            None,
            ForwardSource {
                best_guess: true,
                anchor_only,
                ..ForwardSource::default()
            },
        )
        .unwrap();
        assert!(anchor.word.is_none(), "{anchor:?}");
        assert_eq!(
            (coarse.page, coarse.h, coarse.v, coarse.width, coarse.height),
            (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height)
        );
    }
    let literal_line = contents
        .lines()
        .position(|row| row.starts_with("Independent Copper owner"))
        .unwrap() as u32
        + 1;
    let literal = resolve_forward(&pdf, &source, literal_line, 13).unwrap();
    assert_eq!(literal.page, copper.page);
    assert!((literal.h..=literal.h + literal.width).contains(&copper.x));
    assert!((literal.v - literal.height..=literal.v).contains(&copper.y_from_top));
    let control = resolve_forward(&pdf, &control_source, 1, 13).unwrap();
    assert_eq!(control.page, expected_control.page);
    assert!((control.h..=control.h + control.width).contains(&expected_control.x));
    assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
}

#[test]
fn forward_scoped_and_numeric_expansions_cannot_claim_discarded_input() {
    let main = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\usepackage{beamerarticle}
\newcommand{\Take}[2]{}
\newcommand{\TakeNumber}[1]{0}
\newcommand{\Duplicate}{needle alpha beta gamma delta epsilon zeta}
\newcommand{\ExternalGlobalMutator}{\global\def\Take##1##2{}}
\let\SavedNoindent\noindent
\let\SavedRelax\relax
\begin{document}
\input{control.tex}
Cover page.
\newpage
\input{scoped.tex}
\end{document}
";
    let ordinary = [
        r"Independent Copper owner

\begin{center}
\def\Take#1{}
Center marker.
\end{center}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\pdfcompresslevel=0\TakeNumber{
needle alpha beta gamma delta epsilon zeta
}\relax\Duplicate
",
        r"Independent Copper owner

$\def\Take#1{}x$
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

$$\def\Take#1{}x$$
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\(\def\Take#1{}x\)
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\[\def\Take#1{}x\]
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
\input{redefine.tex}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
{\ExternalGlobalMutator}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\def\Take#1{}
\textbf{\ExternalGlobalMutator}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

\only<presentation>\def\Take#1{}
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"\title{\ExternalGlobalMutator}
\def\Take#1{}
\maketitle
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        r"Independent Copper owner

{\global\let\noindent\Take}
\noindent{first}{
needle alpha beta gamma delta epsilon zeta
}\global\let\noindent\SavedNoindent\Duplicate
",
        r"Independent Copper owner

{\global\let\relax\TakeNumber}
\pdfcompresslevel=0\relax{
needle alpha beta gamma delta epsilon zeta
}\global\let\relax\SavedRelax\Duplicate
",
    ];
    let inherited = [
        (
            "{",
            r"\def\Take#1{}
}
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\begin{center}",
            r"\def\Take#1{}
\end{center}
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            "$",
            r"\def\Take#1{}
$
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            "$$",
            r"\def\Take#1{}
$$
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\(",
            r"\def\Take#1{}
\)
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
        (
            r"\[",
            r"\def\Take#1{}
\]
Independent Copper owner
\Take{first}{
needle alpha beta gamma delta epsilon zeta
}\Duplicate
",
        ),
    ];
    for (opening, contents) in ordinary
        .into_iter()
        .map(|contents| ("", contents))
        .chain(inherited)
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let included = root.join("scoped.tex");
        fs::write(&included, contents).unwrap();
        fs::write(root.join("redefine.tex"), "\\def\\Take#1#2{}\n").unwrap();
        let control_source = root.join("control.tex");
        fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
        let main_with_opening;
        let fixture_main = if opening.is_empty() {
            main
        } else {
            main_with_opening = main.replace(
                "\\input{scoped.tex}",
                &format!("{opening}\n\\input{{scoped.tex}}"),
            );
            &main_with_opening
        };
        let (pdf, _) = compile_out_of_tree_fixture(&root, fixture_main);
        // Neither captured argument prints gamma; only the main's Duplicate does.
        let printed = painted_word_occurrences(&pdf, "gamma");
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].1.page, 2);
        let copper = painted_word_points(&pdf, "Copper").1;
        let (index, body) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains("needle alpha"))
            .unwrap();
        let line = index as u32 + 1;
        let column = body.find("gamma").unwrap() as u32 + 1;
        let expected_control = painted_word_points(&pdf, "NickelControl").1;
        let _native = pdfium_test_lock();
        let coarse = resolve_forward(&pdf, &included, line, column).unwrap();
        assert_eq!(coarse.page, 2);
        assert!(coarse.word.is_none(), "{coarse:?}");
        let anchor = synctex::resolve_forward_with_library(
            &pdf,
            &included,
            line,
            column,
            None,
            ForwardSource {
                best_guess: true,
                anchor_only: true,
                ..ForwardSource::default()
            },
        )
        .unwrap();
        assert_eq!(
            (coarse.page, coarse.h, coarse.v, coarse.width, coarse.height),
            (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height)
        );
        let (copper_index, copper_body) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains("Independent Copper owner"))
            .unwrap();
        let copper_column = copper_body.find("Copper").unwrap() as u32 + 1;
        let literal =
            resolve_forward(&pdf, &included, copper_index as u32 + 1, copper_column).unwrap();
        assert_eq!(literal.page, copper.page);
        assert!((literal.h..=literal.h + literal.width).contains(&copper.x));
        assert!((literal.v - literal.height..=literal.v).contains(&copper.y_from_top));
        let control = resolve_forward(&pdf, &control_source, 1, 13).unwrap();
        assert_eq!(control.page, expected_control.page);
        assert!((control.h..=control.h + control.width).contains(&expected_control.x));
        assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
    }
}

#[test]
fn forward_inherited_controls_and_joined_words_keep_native_line_geometry() {
    let main = r"\documentclass{article}
\usepackage[paperwidth=900pt,paperheight=300pt,margin=20pt]{geometry}
\def\TakeTwo#1#2{}
\def\Duplicate{needle alpha beta gamma delta epsilon zeta}
\def\Opaque{Intruder}
\let\SavedNoindent\noindent
\let\SavedLabel\label
\def\Restore{\global\let\noindent\SavedNoindent\global\let\label\SavedLabel}
\begin{document}
\input{control.tex}
Cover page.
\newpage
% inherited binding
\input{original.tex}
\end{document}
";
    for (name, binding, contents, printed_count) in [
        (
            "noindent-bare",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent needle alpha beta gamma delta epsilon zeta\n\\Restore\\Duplicate\n",
            2,
        ),
        (
            "noindent-group",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent{first}{needle alpha beta gamma delta epsilon zeta}\\Restore\\Duplicate\n",
            1,
        ),
        (
            "noindent-restore-same-row",
            "\\global\\let\\noindent\\TakeTwo",
            "\\noindent needle alpha beta gamma delta epsilon zeta\\Restore\\Duplicate\n",
            2,
        ),
        (
            "label-bare",
            "\\global\\let\\label\\TakeTwo",
            "\\label{first}needle alpha beta gamma delta epsilon zeta\\Restore\\Duplicate\n",
            2,
        ),
        (
            "label-extra-group",
            "\\global\\let\\label\\TakeTwo",
            "\\label{first}{needle alpha beta gamma delta epsilon zeta}\\Restore\\Duplicate\n",
            1,
        ),
        (
            "joined-label-suffix",
            "",
            "needle alpha beta gamma delta epsilon zeta\\label{owner}Suffix \\Duplicate\n",
            2,
        ),
        (
            "opaque-separated",
            "",
            "needle alpha beta gamma delta epsilon zeta\\Opaque \\Duplicate\n",
            2,
        ),
        (
            "everypar-plain-prefix",
            "\\everypar{\\global\\everypar{}\\TakeTwo}",
            "needle alpha beta gamma delta epsilon zeta \\Duplicate\n",
            2,
        ),
        (
            "active-tilde-group",
            "\\global\\let~\\TakeTwo",
            "~{first}{needle alpha beta gamma delta epsilon zeta}\\Duplicate\n",
            1,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        // Deliberately share the compiler root's basename, not its identity.
        let included = root.join("original.tex");
        fs::write(&included, contents).unwrap();
        let control_source = root.join("control.tex");
        fs::write(&control_source, "Independent NickelControl owner\n\n").unwrap();
        let (pdf, main_source) =
            compile_out_of_tree_fixture(&root, &main.replace("% inherited binding", binding));
        assert_ne!(main_source, included);
        let printed = painted_word_occurrences(&pdf, "gamma");
        assert_eq!(printed.len(), printed_count, "{name}");
        assert!(printed.iter().all(|(_, point)| point.page == 2), "{name}");
        let expected_control = painted_word_points(&pdf, "NickelControl").1;
        let column = contents.lines().next().unwrap().find("gamma").unwrap() as u32 + 1;
        let _native = pdfium_test_lock();
        for best_guess in [false, true] {
            let request = synctex::resolve_forward_with_library(
                &pdf,
                &included,
                1,
                column,
                None,
                ForwardSource {
                    best_guess,
                    ..ForwardSource::default()
                },
            )
            .unwrap();
            assert_eq!(request.page, 2, "{name}: {request:?}");
            assert_eq!(request.best_guess, best_guess);
            assert!(request.width > 0.0 || request.height > 0.0, "{name}");
        }
        let control = resolve_forward(&pdf, &control_source, 1, 13).unwrap();
        assert_eq!(control.page, expected_control.page);
        assert!((control.h..=control.h + control.width).contains(&expected_control.x));
        assert!((control.v - control.height..=control.v).contains(&expected_control.y_from_top));
    }
}

#[test]
fn beamer_included_controls_keep_honest_compiler_line_geometry() {
    let main = "\\documentclass{beamer}\n\\begin{document}\n\
                \\begin{frame}RootCover supplies the first page.\\end{frame}\n\
                \\input{controls.tex}\n\\end{document}\n";
    let contents = r"\begin{frame}{Inherited controls}
LiteralCopper keeps ordinary body context available.
\noindent LayoutNickel keeps ordinary layout context available.
\label{owner} LabelQuartz keeps metadata context available.
\only<1>{OverlayAurora keeps visible overlay context available.}
\uncover<1>{RevealNebula keeps visible overlay context available.}
\onslide<1>{ShiftCobalt keeps visible overlay context available.}
\begin{itemize}
\item<1> ItemSilver keeps visible item context available.
\end{itemize}
\end{frame}
";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let source = root.join("controls.tex");
    fs::write(&source, contents).unwrap();
    let (pdf, main_source) = compile_out_of_tree_fixture(&root, main);
    // These are genuine printed controls, not evidence that hashes trace TeX.
    // The existing multi-overlay fixture separately pins pages 2, 3, and 4.
    for word in [
        "LiteralCopper",
        "LayoutNickel",
        "LabelQuartz",
        "OverlayAurora",
        "RevealNebula",
        "ShiftCobalt",
        "ItemSilver",
    ] {
        let printed = painted_word_points(&pdf, word).1;
        assert_eq!(printed.page, 2, "{word}");
        let (index, row) = contents
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(word))
            .unwrap();
        let line = index as u32 + 1;
        let column = row.find(word).unwrap() as u32 + 1;
        let _native = pdfium_test_lock();
        let context = ForwardSource::default();
        let request =
            synctex::resolve_forward_with_library(&pdf, &source, line, column, None, context)
                .unwrap();
        assert_eq!(request.page, printed.page, "{word}: {request:?}");
        assert!(!request.best_guess);
        assert!(request.width > 0.0 || request.height > 0.0);
        let anchor = synctex::resolve_forward_with_library(
            &pdf,
            &source,
            line,
            column,
            None,
            ForwardSource {
                anchor_only: true,
                ..context
            },
        )
        .unwrap();
        assert_eq!(
            (
                request.page,
                request.h,
                request.v,
                request.width,
                request.height
            ),
            (anchor.page, anchor.h, anchor.v, anchor.width, anchor.height),
            "{word}: word refinement must preserve the compiler's line rectangle"
        );
    }
    let _native = pdfium_test_lock();
    let row = main.lines().nth(2).unwrap();
    let root_request = synctex::resolve_forward_with_library(
        &pdf,
        &main_source,
        3,
        row.find("RootCover").unwrap() as u32 + 1,
        None,
        ForwardSource::default(),
    )
    .unwrap();
    assert_eq!(root_request.page, 1);
    assert!(!root_request.best_guess);
}

#[test]
fn real_synctex_revisions_failed_hit_tests_and_coarse_refinement() {
    let _native = pdfium_test_lock();
    let directory = tempfile::tempdir().unwrap();
    let source_contents = include_str!("fixtures/navigation.tex");
    let source = directory.path().join("navigation.tex");
    fs::write(&source, source_contents).unwrap();
    let source = source.canonicalize().unwrap();
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
    let request = resolve_forward(&pdf, &source, 6, 1).unwrap();
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
        false,
    )
    .unwrap();
    assert_eq!(inverse.location.line, 6);
    assert_eq!(inverse.location.file, source.to_str().unwrap());
    assert!(inverse.location.precise);
    let expected = fs::read_to_string(&source)
        .unwrap()
        .lines()
        .nth(5)
        .unwrap()
        .find("zephyr")
        .unwrap();
    assert_eq!(inverse.location.byte_column, expected);
    let compiled_source_time = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, "\\relax\n").unwrap();
    let invalid_anchor = synctex::resolve_inverse(
        &pdf,
        synctex::InversePoint {
            page: request.page,
            x: point.0,
            y_from_top: point.1,
            page_height_pt: 792.0,
        },
        None,
        4,
        &Operation::default(),
        false,
    );
    assert!(
        invalid_anchor.is_err(),
        "a readable source shorter than its raw SyncTeX anchor must not navigate"
    );
    fs::write(&source, [0xff]).unwrap();
    assert!(resolve_forward(&pdf, &source, 6, 1).is_err());
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
        false,
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
        assert!(resolve_forward(&pdf, &source, 6, 1).is_err());
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
            false,
        )
        .unwrap();
        assert_eq!(result.location.line, 6);
        assert!(!result.location.precise);
        assert!(result.warning.is_some());
    }
    fs::remove_file(&source).unwrap();
    fs::write(&source, source_contents).unwrap();
    fs::File::open(&source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(compiled_source_time))
        .unwrap();
    let word_request = resolve_forward(&pdf, &source, 6, expected as u32 + 1).unwrap();
    let hint = word_request.word.as_ref().unwrap();
    assert_eq!(hint.words[hint.selected], "zephyr");

    // Finish this native transaction, including both reload revisions, before
    // the fixture lock allows another resolver or worker to open PDFium.
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
        let inverse = synctex::resolve_inverse(
            &pdf,
            point,
            Some((word, 0)),
            4,
            &Operation::default(),
            false,
        )
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
            synctex::resolve_inverse(&pdf, center, None, 4, &Operation::default(), false).unwrap();
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
        false,
    )
    .unwrap();
    assert_eq!(
        Path::new(&metadata.location.file),
        fs::canonicalize(&source).unwrap()
    );
    assert_eq!(metadata.location.line, 6);
    assert_eq!(metadata.location.byte_column, 0);
    assert!(!metadata.location.precise);
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
    let coarse =
        synctex::resolve_inverse(&pdf, point, None, 4, &Operation::default(), false).unwrap();
    let inverse = synctex::resolve_inverse(
        &pdf,
        point,
        Some(("Zephyr", 0)),
        4,
        &Operation::default(),
        false,
    )
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
        let inverse = synctex::resolve_inverse(
            &pdf,
            point,
            Some((word, 0)),
            4,
            &Operation::default(),
            false,
        )
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
            false,
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

#[test]
fn cli_saved_sources_preserve_relative_input_identity_across_directories() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let client = root.join("client");
    let build = root.join("build");
    fs::create_dir(&client).unwrap();
    fs::create_dir(&build).unwrap();

    let build_source = build.join("main.tex");
    let build_contents =
        "\\documentclass{article}\n\\begin{document}\nAlpha Copper Omega\n\\end{document}\n";
    fs::write(&build_source, build_contents).unwrap();
    let compile = process::output(
        Command::new("pdflatex").current_dir(&build).args([
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-synctex=-1",
            "main.tex",
        ]),
        &Operation::new(Duration::from_secs(30)),
    )
    .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stdout)
    );
    let sidecar = build.join("main.synctex");
    let raw_synctex = fs::read_to_string(&sidecar).unwrap();
    fs::write(
        &sidecar,
        raw_synctex.replace(&format!("{}/", build.display()), ""),
    )
    .unwrap();
    let pdf = build.join("main.pdf");
    let (point, _) = painted_word_points(&pdf, "Copper");

    let forward = process::output(
        Command::new(env!("CARGO_BIN_EXE_pdfterm"))
            .current_dir(&client)
            .arg(&pdf)
            .arg("--synctex-view")
            .arg(&build_source)
            .args(["--line", "3", "--column", "7"]),
        &Operation::default(),
    )
    .unwrap();
    assert!(
        forward.status.success(),
        "{}",
        String::from_utf8_lossy(&forward.stderr)
    );
    let forward_json: serde_json::Value = serde_json::from_slice(&forward.stdout).unwrap();
    assert_eq!(forward_json["page"], 1);

    let click_line = format!(
        "{}\n",
        serde_json::json!({
            "page": point.page,
            "x": point.x,
            "y": point.y_from_top,
        })
    );
    let run_batch = || -> serde_json::Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pdfterm"));
        cmd.current_dir(&client)
            .arg(&pdf)
            .arg("--synctex-edit-batch")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), click_line.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let inverse_json = run_batch();
    assert_eq!(inverse_json["ok"], true, "{inverse_json}");
    assert_eq!(
        inverse_json["location"]["file"],
        build_source.to_str().unwrap()
    );
    assert_eq!(inverse_json["location"]["line"], 3);
    assert_eq!(inverse_json["location"]["precise"], true);

    let client_source = client.join("main.tex");
    fs::write(&client_source, build_contents).unwrap();
    let both_inverse = run_batch();
    assert_eq!(both_inverse["ok"], true, "{both_inverse}");
    assert_eq!(
        both_inverse["location"]["file"],
        client_source.to_str().unwrap()
    );
    assert_eq!(both_inverse["location"]["precise"], true);
}

#[test]
fn synctex_inverse_reference_commands_select_label_argument_start() {
    let contents = "\\documentclass{article}\n\
                    \\usepackage[paperwidth=420pt,paperheight=300pt,margin=35pt]{geometry}\n\
                    \\usepackage{tabularx}\n\
                    \\usepackage[colorlinks=true]{hyperref}\n\
                    \\usepackage[nameinlink]{cleveref}\n\
                    \\begin{document}\n\
                    \\thispagestyle{empty}\n\
                    \\section{First}\\label{sec:first}\n\
                    \\begin{enumerate}\n\
                    \\item\\label{itm:one} Alpha.\n\
                    \\end{enumerate}\n\
                    \\newcommand{\\unused}{𐐀} Café check \\cref{sec:first} tail.\n\
                    \n\
                    Lead \\Cref{itm:one} done.\n\
                    \n\
                    \\begin{tabularx}{\\linewidth}{lXl}\n\
                    Row & $-$ & end \\\\\n\
                    Row & \\cref{sec:first} & end \\\\\n\
                    Row & $77$ tableword & end \\\\\n\
                    \\end{tabularx}\n\
                    \n\
                    See $66$ for details.\\footnote{See \\cref{sec:first} for details.} Following this sentence is deliberately long ordinary body text so that the rendered footnote at the bottom of the page is well beyond ninety six characters from the body formula.\\par\\clearpage\n\
                    \\thispagestyle{plain}\\setcounter{page}{99}\n\
                    Distinctfooter \\cref{sec:first}\\par\\clearpage\n\
                    \\cref{sec:first} runningtail. Headerbody \\cref{sec:first} headertail.\\thispagestyle{myheadings}\\setcounter{page}{\\numexpr100-2\\relax}\\par\\clearpage\n\
                    \\thispagestyle{plain}\\setcounter{page}{97}\n\
                    Endfn.\\footnote{One two three see \\cref{sec:first}.}\\clearpage\n\
                    \\thispagestyle{empty}\n\
                    Page five text.\\footnote{Note see \\cref{sec:first} for details.}\\par\\clearpage\n\
                    \\thispagestyle{empty}\n\
                    See \\cite{kpar} for details.\\footnote{Note see \\cref{sec:first} for details.} Following material provides enough distinct visible characters to keep the distant footnote outside the ninety-six-character native glyph context. This additional sentence extends the body text even further for reliable separation.\\par\\clearpage\n\
                    \\thispagestyle{empty}\n\
                    \\begin{itemize}\n\
                    \\item See \\cite{kitm} for details.\\footnote{Note see \\cref{sec:first} for details.} Following material provides enough distinct visible characters to keep the distant footnote outside the ninety-six-character native glyph context. This additional sentence extends the body text even further for reliable separation.\n\
                    \\end{itemize}\\par\\clearpage\n\
                    \\thispagestyle{empty}\n\
                    See $85$ for details.\\footnote{Note see \\cref{sec:first} for details.} Following material provides enough distinct visible characters to keep the distant footnote outside the ninety-six-character native glyph context. This additional sentence extends the body text even further for reliable separation.\n\
                    \n\
                    See $\\alpha$ for details.\\footnote{Note see \\cref{sec:first} for details.} Following material provides enough distinct visible characters to keep the distant footnote outside the ninety-six-character native glyph context. This additional sentence extends the body text even further for reliable separation.\\par\\clearpage\n\
                    \\thispagestyle{empty}\n\
                    \\begin{thebibliography}{99}\n\
                    \\bibitem[73]{kpar} Reference entry one.\n\
                    \\bibitem[74]{kitm} Reference entry two.\n\
                    \\end{thebibliography}\n\
                    \\end{document}\n";
    let directory = tempfile::tempdir().unwrap();
    let (pdf, source) = compile_out_of_tree_fixture(directory.path(), contents);
    let second_pass = process::output(
        Command::new("pdflatex")
            .current_dir(directory.path())
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
        second_pass.status.success(),
        "{}",
        String::from_utf8_lossy(&second_pass.stdout)
    );
    let source = source.canonicalize().unwrap();
    let (sec_line_idx, sec_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Café check"))
        .unwrap();
    let sec_line = sec_line_idx + 1;
    let sec_expected_byte = sec_row.find("sec:first").unwrap();
    let sec_prefix = &sec_row[..sec_expected_byte];
    let sec_expected_utf16 = sec_prefix.encode_utf16().count() + 1;
    let sec_expected_char = sec_prefix.chars().count() + 1;
    assert!(sec_expected_byte > sec_expected_utf16 && sec_expected_utf16 > sec_expected_char);
    let lit_expected_byte = sec_row.find("check").unwrap();
    let lit_prefix = &sec_row[..lit_expected_byte];
    let lit_expected_utf16 = lit_prefix.encode_utf16().count() + 1;
    let lit_expected_char = lit_prefix.chars().count() + 1;

    let (itm_line_idx, itm_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("\\Cref{itm:one}"))
        .unwrap();
    let itm_line = itm_line_idx + 1;
    let itm_expected_byte = itm_row.find("itm:one").unwrap();
    let itm_prefix = &itm_row[..itm_expected_byte];
    let itm_expected_utf16 = itm_prefix.encode_utf16().count() + 1;
    let itm_expected_char = itm_prefix.chars().count() + 1;

    let (tbl_sec_idx, tbl_sec_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Row & \\cref{sec:first}"))
        .unwrap();
    let tbl_sec_line = tbl_sec_idx + 1;
    let tbl_sec_byte = tbl_sec_row.find("sec:first").unwrap();

    let (tbl_lit_idx, tbl_lit_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("tableword"))
        .unwrap();
    let tbl_lit_line = tbl_lit_idx + 1;
    let tbl_lit_byte = tbl_lit_row.find("tableword").unwrap();
    let tbl_lit_prefix = &tbl_lit_row[..tbl_lit_byte];
    let tbl_lit_utf16 = tbl_lit_prefix.encode_utf16().count() + 1;
    let tbl_lit_char = tbl_lit_prefix.chars().count() + 1;

    let (fn_line_idx, fn_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("\\footnote{See \\cref{sec:first}"))
        .unwrap();
    let fn_line = fn_line_idx + 1;
    let fn_sec_byte = fn_row.find("sec:first").unwrap();

    let (fn2_line_idx, fn2_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Page five text.\\footnote{Note see \\cref{sec:first}"))
        .unwrap();
    let fn2_line = fn2_line_idx + 1;
    let fn2_sec_byte = fn2_row.find("sec:first").unwrap();
    let fn2_prefix = &fn2_row[..fn2_sec_byte];
    let fn2_utf16 = fn2_prefix.encode_utf16().count() + 1;
    let fn2_char = fn2_prefix.chars().count() + 1;

    let (hdr_line_idx, hdr_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Headerbody \\cref{sec:first}"))
        .unwrap();
    let hdr_line = hdr_line_idx + 1;
    let hdr_sec_byte = hdr_row.rfind("sec:first").unwrap();
    let hdr_prefix = &hdr_row[..hdr_sec_byte];
    let hdr_utf16 = hdr_prefix.encode_utf16().count() + 1;
    let hdr_char = hdr_prefix.chars().count() + 1;

    let (short_math_idx, short_math_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("See $85$ for details."))
        .unwrap();
    let short_math_line = short_math_idx + 1;
    let short_math_byte = short_math_row.find("85").unwrap();
    let short_math_sec_byte = short_math_row.find("sec:first").unwrap();

    let (short_alpha_idx, short_alpha_row) = contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("See $\\alpha$ for details."))
        .unwrap();
    let short_alpha_line = short_alpha_idx + 1;
    let short_alpha_byte = short_alpha_row.find("\\alpha").unwrap();
    let short_alpha_utf16 = short_alpha_row[..short_alpha_byte].encode_utf16().count() + 1;
    let short_alpha_char = short_alpha_row[..short_alpha_byte].chars().count() + 1;

    let sec_occurrences = painted_word_occurrences(&pdf, "section");
    let (sec_hit, sec_center) = sec_occurrences[0];
    let (hdr_sec_hit, hdr_sec_center) = sec_occurrences
        .iter()
        .copied()
        .filter(|(hit, _)| hit.page == 3)
        .max_by(|(a, _), (b, _)| a.x.total_cmp(&b.x))
        .expect("expected page-3 body section reference");
    let &(fn2_sec_hit, fn2_sec_center) = sec_occurrences
        .iter()
        .find(|(hit, _)| hit.page == 5)
        .expect("expected page-5 footnote section reference");
    let (itm_hit, itm_center) = painted_word_points(&pdf, "Item");
    let (lit_hit, lit_center) = painted_word_points(&pdf, "check");
    let (tbl_lit_hit, tbl_lit_center) = painted_word_points(&pdf, "tableword");
    let (math_hit, math_center) = painted_word_points(&pdf, "77");
    let (fn_math_hit, fn_math_center) = painted_word_points(&pdf, "66");
    let (page_hit, page_center) = painted_word_points(&pdf, "99");
    let (hdr_hit, hdr_center) = painted_word_points(&pdf, "98");
    let (fn_ftr_hit, fn_ftr_center) = painted_word_points(&pdf, "97");
    let &(cite_par_hit, cite_par_center) = painted_word_occurrences(&pdf, "73")
        .iter()
        .find(|(hit, _)| hit.page == 6)
        .expect("expected page-6 paragraph-start citation");
    let &(cite_itm_hit, cite_itm_center) = painted_word_occurrences(&pdf, "74")
        .iter()
        .find(|(hit, _)| hit.page == 7)
        .expect("expected page-7 item-start citation");
    let (short_math_hit, short_math_center) = painted_word_points(&pdf, "85");
    let (short_alpha_hit, short_alpha_center) = painted_word_points(&pdf, "\u{03b1}");
    let mut requests = String::new();
    for point in [
        sec_hit,
        sec_center,
        itm_hit,
        itm_center,
        lit_hit,
        lit_center,
        tbl_lit_hit,
        tbl_lit_center,
        math_hit,
        math_center,
        fn_math_hit,
        fn_math_center,
        page_hit,
        page_center,
        fn2_sec_hit,
        fn2_sec_center,
        hdr_hit,
        hdr_center,
        fn_ftr_hit,
        fn_ftr_center,
        cite_par_hit,
        cite_par_center,
        cite_itm_hit,
        cite_itm_center,
        short_math_hit,
        short_math_center,
        short_alpha_hit,
        short_alpha_center,
        hdr_sec_hit,
        hdr_sec_center,
    ] {
        requests.push_str(
            &serde_json::json!({
                "page": point.page,
                "x": point.x,
                "y": point.y_from_top,
            })
            .to_string(),
        );
        requests.push('\n');
    }
    let points = directory.path().join("points.jsonl");
    fs::write(&points, requests).unwrap();
    for best_guess in [false, true] {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec \"$@\" < \"$0\""])
            .arg(&points)
            .arg(env!("CARGO_BIN_EXE_pdfterm"))
            .arg(&pdf)
            .arg("--synctex-edit-batch")
            .env("XDG_CONFIG_HOME", directory.path().join("cli-config"));
        if best_guess {
            cmd.arg("--synctex-best-guess");
        }
        let output = process::output(&mut cmd, &Operation::default()).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(results.len(), 30);
        for (idx, (word, line, byte, utf16, ch)) in [
            (
                "section",
                sec_line,
                sec_expected_byte,
                sec_expected_utf16,
                sec_expected_char,
            ),
            (
                "section",
                sec_line,
                sec_expected_byte,
                sec_expected_utf16,
                sec_expected_char,
            ),
            (
                "Item",
                itm_line,
                itm_expected_byte,
                itm_expected_utf16,
                itm_expected_char,
            ),
            (
                "Item",
                itm_line,
                itm_expected_byte,
                itm_expected_utf16,
                itm_expected_char,
            ),
            (
                "check",
                sec_line,
                lit_expected_byte,
                lit_expected_utf16,
                lit_expected_char,
            ),
            (
                "check",
                sec_line,
                lit_expected_byte,
                lit_expected_utf16,
                lit_expected_char,
            ),
            (
                "tableword",
                tbl_lit_line,
                tbl_lit_byte,
                tbl_lit_utf16,
                tbl_lit_char,
            ),
            (
                "tableword",
                tbl_lit_line,
                tbl_lit_byte,
                tbl_lit_utf16,
                tbl_lit_char,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], word, "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], line, "{result}");
            assert_eq!(result["location"]["byte_column"], byte, "{result}");
            assert_eq!(result["location"]["column"], utf16, "{result}");
            assert_eq!(result["location"]["column_char"], ch, "{result}");
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for idx in [8, 9] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "77", "{result}");
            assert!(
                (result["location"]["line"] != tbl_sec_line
                    || result["location"]["byte_column"] != tbl_sec_byte)
                    && (result["location"]["line"] != sec_line
                        || result["location"]["byte_column"] != sec_expected_byte),
                "tabularx ordinal-shifted math stole reference argument: {result}"
            );
        }
        for idx in [10, 11] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "66", "{result}");
            assert!(
                result["location"]["line"] != fn_line
                    || result["location"]["byte_column"] != fn_sec_byte,
                "body math stole footnote reference argument: {result}"
            );
        }
        for (idxs, word, label) in [
            (
                [12usize, 13],
                "99",
                "unmasked same-line clearpage footer number",
            ),
            (
                [16, 17],
                "98",
                "unmasked same-line clearpage running header number",
            ),
            ([18, 19], "97", "ending-footnote clearpage footer number"),
            (
                [20, 21],
                "73",
                "paragraph-start citation beside Note-see footnote",
            ),
            (
                [22, 23],
                "74",
                "item-start citation beside Note-see footnote",
            ),
        ] {
            for idx in idxs {
                let result = &results[idx];
                assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
                assert_eq!(result["pdf_word"], word, "{result}");
                assert_eq!(
                    result["location"]["precise"], false,
                    "{label} must remain coarse: {result}"
                );
            }
        }
        for idx in [14, 15] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "section", "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], fn2_line, "{result}");
            assert_eq!(result["location"]["byte_column"], fn2_sec_byte, "{result}");
            assert_eq!(result["location"]["column"], fn2_utf16, "{result}");
            assert_eq!(result["location"]["column_char"], fn2_char, "{result}");
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for idx in [24, 25] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "85", "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], short_math_line, "{result}");
            let byte = result["location"]["byte_column"].as_u64().unwrap() as usize;
            assert!(
                byte == short_math_byte || byte == short_math_byte + 1,
                "short-left body math must resolve to one of its math digit atoms (not footnote key {short_math_sec_byte}): {result}"
            );
            assert_eq!(result["location"]["column"], byte + 1, "{result}");
            assert_eq!(result["location"]["column_char"], byte + 1, "{result}");
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for idx in [26, 27] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "\u{03b1}", "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], short_alpha_line, "{result}");
            assert_eq!(
                result["location"]["byte_column"], short_alpha_byte,
                "{result}"
            );
            assert_eq!(result["location"]["column"], short_alpha_utf16, "{result}");
            assert_eq!(
                result["location"]["column_char"], short_alpha_char,
                "{result}"
            );
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for idx in [28, 29] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "section", "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                source,
            );
            assert_eq!(result["location"]["line"], hdr_line, "{result}");
            assert_eq!(result["location"]["byte_column"], hdr_sec_byte, "{result}");
            assert_eq!(result["location"]["column"], hdr_utf16, "{result}");
            assert_eq!(result["location"]["column_char"], hdr_char, "{result}");
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
    }

    let beamer_contents = "\\documentclass{beamer}\n\
                           \\newcounter{myctr}\n\
                           \\renewcommand{\\themyctr}{71}\n\
                           \\begin{document}\n\
                           \\begin{frame}\n\
                           \\refstepcounter{myctr}\\label{sec:frame}\n\
                           Unique \\ref{sec:frame} here.\n\
                           \n\
                           By \\ref{sec:frame} then.\n\
                           \n\
                           By $88$ then.\n\
                           \\end{frame}\n\
                           \\end{document}\n";
    let beamer_dir = tempfile::tempdir().unwrap();
    let (beamer_pdf, beamer_source) =
        compile_out_of_tree_fixture(beamer_dir.path(), beamer_contents);
    let beamer_second = process::output(
        Command::new("pdflatex")
            .current_dir(beamer_dir.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&beamer_source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the real beamer SyncTeX fixture");
    assert!(
        beamer_second.status.success(),
        "{}",
        String::from_utf8_lossy(&beamer_second.stdout)
    );
    let beamer_source = beamer_source.canonicalize().unwrap();
    let (frame_ref_idx, frame_ref_row) = beamer_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Unique \\ref{sec:frame}"))
        .unwrap();
    let frame_ref_line = frame_ref_idx + 1;
    let frame_ref_byte = frame_ref_row.find("sec:frame").unwrap();
    let (rep_ref_idx, rep_ref_row) = beamer_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("By \\ref{sec:frame}"))
        .unwrap();
    let rep_ref_line = rep_ref_idx + 1;
    let rep_ref_byte = rep_ref_row.find("sec:frame").unwrap();
    let (b_sec_hit, b_sec_center) = painted_word_occurrences(&beamer_pdf, "71")[0];
    let (b_math_hit, b_math_center) = painted_word_points(&beamer_pdf, "88");
    let mut beamer_requests = String::new();
    for point in [b_sec_hit, b_sec_center, b_math_hit, b_math_center] {
        beamer_requests.push_str(
            &serde_json::json!({
                "page": point.page,
                "x": point.x,
                "y": point.y_from_top,
            })
            .to_string(),
        );
        beamer_requests.push('\n');
    }
    let beamer_points = beamer_dir.path().join("points.jsonl");
    fs::write(&beamer_points, beamer_requests).unwrap();
    for best_guess in [false, true] {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec \"$@\" < \"$0\""])
            .arg(&beamer_points)
            .arg(env!("CARGO_BIN_EXE_pdfterm"))
            .arg(&beamer_pdf)
            .arg("--synctex-edit-batch")
            .env("XDG_CONFIG_HOME", beamer_dir.path().join("cli-config"));
        if best_guess {
            cmd.arg("--synctex-best-guess");
        }
        let output = process::output(&mut cmd, &Operation::default()).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(results.len(), 4);
        for idx in [0, 1] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "71", "{result}");
            assert_eq!(
                Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                beamer_source,
            );
            assert_eq!(result["location"]["line"], frame_ref_line, "{result}");
            assert_eq!(
                result["location"]["byte_column"], frame_ref_byte,
                "{result}"
            );
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for idx in [2, 3] {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "88", "{result}");
            assert!(
                (result["location"]["line"] != frame_ref_line
                    || result["location"]["byte_column"] != frame_ref_byte)
                    && (result["location"]["line"] != rep_ref_line
                        || result["location"]["byte_column"] != rep_ref_byte),
                "beamer frame repeated-neighbor math stole reference argument: {result}"
            );
        }
    }

    let beamer_footline_contents = "\\documentclass{beamer}\n\
                                    \\setbeamertemplate{footline}[frame number]\n\
                                    \\newcounter{myctr}\n\
                                    \\renewcommand{\\themyctr}{71}\n\
                                    \\begin{document}\n\
                                    \\refstepcounter{myctr}\\label{sec:frame}\n\
                                    \\begin{frame}\n\
                                    \\ref{sec:frame} done.\n\
                                    \n\
                                    Middle prose filler sentence keeps the top reference far from the bottom footline while remaining on the same slide.\n\
                                    \n\
                                    Bottom tail.\n\
                                    \\end{frame}\n\
                                    \\begin{frame}\n\
                                    Top lead.\n\
                                    \n\
                                    Middle prose filler sentence keeps the top paragraph far from the bottom reference.\n\
                                    \n\
                                    \\begin{itemize}\n\
                                    \\item See \\ref{sec:frame}\n\
                                    \\end{itemize}\n\
                                    \\end{frame}\n\
                                    \\end{document}\n";
    let beamer_foot_dir = tempfile::tempdir().unwrap();
    let (beamer_foot_pdf, beamer_foot_source) =
        compile_out_of_tree_fixture(beamer_foot_dir.path(), beamer_footline_contents);
    let beamer_foot_second = process::output(
        Command::new("pdflatex")
            .current_dir(beamer_foot_dir.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&beamer_foot_source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the beamer footline SyncTeX fixture");
    assert!(
        beamer_foot_second.status.success(),
        "{}",
        String::from_utf8_lossy(&beamer_foot_second.stdout)
    );
    let &(foot_first_1_hit, foot_first_1_center) = painted_word_occurrences(&beamer_foot_pdf, "1")
        .iter()
        .find(|(hit, _)| hit.page == 1)
        .expect("expected page-1 footline frame number 1");
    let foot_twos: Vec<_> = painted_word_occurrences(&beamer_foot_pdf, "2")
        .into_iter()
        .filter(|(hit, _)| hit.page == 2)
        .collect();
    assert_eq!(foot_twos.len(), 2, "expected 2 / 2 footline on page 2");
    let (foot_last_2a_hit, foot_last_2a_center) = foot_twos[0];
    let (foot_last_2b_hit, foot_last_2b_center) = foot_twos[1];

    let beamer_headline_contents = "\\documentclass{beamer}\n\
                                    \\setbeamertemplate{headline}{Headerword}\n\
                                    \\newcounter{myctr}\n\
                                    \\renewcommand{\\themyctr}{71}\n\
                                    \\begin{document}\n\
                                    \\refstepcounter{myctr}\\label{sec:frame}\n\
                                    \\begin{frame}\n\
                                    \\ref{sec:frame} done.\n\
                                    \n\
                                    Middle prose filler sentence keeps the top reference far from the bottom tail.\n\
                                    \n\
                                    Bottom tail.\n\
                                    \\end{frame}\n\
                                    \\begin{frame}\n\
                                    Top lead.\n\
                                    \n\
                                    Middle prose filler sentence keeps the top paragraph far from the bottom reference.\n\
                                    \n\
                                    \\begin{itemize}\n\
                                    \\item See \\ref{sec:frame}\n\
                                    \\end{itemize}\n\
                                    \\end{frame}\n\
                                    \\end{document}\n";
    let beamer_head_dir = tempfile::tempdir().unwrap();
    let (beamer_head_pdf, beamer_head_source) =
        compile_out_of_tree_fixture(beamer_head_dir.path(), beamer_headline_contents);
    let beamer_head_second = process::output(
        Command::new("pdflatex")
            .current_dir(beamer_head_dir.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&beamer_head_source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the beamer headline SyncTeX fixture");
    assert!(
        beamer_head_second.status.success(),
        "{}",
        String::from_utf8_lossy(&beamer_head_second.stdout)
    );
    let head_words = painted_word_occurrences(&beamer_head_pdf, "Headerword");
    let &(head_first_hit, head_first_center) = head_words
        .iter()
        .find(|(hit, _)| hit.page == 1)
        .expect("expected page-1 headline word");
    let &(head_last_hit, head_last_center) = head_words
        .iter()
        .find(|(hit, _)| hit.page == 2)
        .expect("expected page-2 headline word");

    let beamer_pause_contents = "\\documentclass{beamer}\n\
                                 \\setbeamertemplate{navigation symbols}{}\n\
                                 \\setbeamertemplate{footline}[frame number]\n\
                                 \\begin{document}\n\
                                 \\setcounter{section}{76}\\refstepcounter{section}\\label{sec:frame}\n\
                                 \\begin{frame}\n\
                                 \\begin{itemize}\n\
                                 \\item See \\ref{sec:frame}\\pause then.\n\
                                 \\end{itemize}\n\
                                 \\end{frame}\n\
                                 \\end{document}\n";
    let beamer_pause_dir = tempfile::tempdir().unwrap();
    let (beamer_pause_pdf, beamer_pause_source) =
        compile_out_of_tree_fixture(beamer_pause_dir.path(), beamer_pause_contents);
    let beamer_pause_second = process::output(
        Command::new("pdflatex")
            .current_dir(beamer_pause_dir.path())
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-output-directory=build",
                "-jobname=deck",
            ])
            .arg(&beamer_pause_source),
        &Operation::new(Duration::from_secs(30)),
    )
    .expect("pdflatex is required for the beamer pause SyncTeX fixture");
    assert!(
        beamer_pause_second.status.success(),
        "{}",
        String::from_utf8_lossy(&beamer_pause_second.stdout)
    );
    let pause_ones = painted_word_occurrences(&beamer_pause_pdf, "1");
    let pause_p1: Vec<_> = pause_ones
        .iter()
        .copied()
        .filter(|(hit, _)| hit.page == 1)
        .collect();
    let pause_p2: Vec<_> = pause_ones
        .iter()
        .copied()
        .filter(|(hit, _)| hit.page == 2)
        .collect();
    assert!(
        !pause_p1.is_empty() && !pause_p2.is_empty(),
        "expected footline frame numbers on both pause overlay pages"
    );
    let mut pause_pts = Vec::new();
    for (hit, center) in pause_p1.into_iter().chain(pause_p2) {
        pause_pts.push(hit);
        pause_pts.push(center);
    }

    for (dir_path, pdf_path, pts) in [
        (
            beamer_foot_dir.path(),
            &beamer_foot_pdf,
            vec![
                foot_first_1_hit,
                foot_first_1_center,
                foot_last_2a_hit,
                foot_last_2a_center,
                foot_last_2b_hit,
                foot_last_2b_center,
            ],
        ),
        (
            beamer_head_dir.path(),
            &beamer_head_pdf,
            vec![
                head_first_hit,
                head_first_center,
                head_last_hit,
                head_last_center,
            ],
        ),
        (beamer_pause_dir.path(), &beamer_pause_pdf, pause_pts),
    ] {
        let mut reqs = String::new();
        for pt in &pts {
            reqs.push_str(
                &serde_json::json!({
                    "page": pt.page,
                    "x": pt.x,
                    "y": pt.y_from_top,
                })
                .to_string(),
            );
            reqs.push('\n');
        }
        let pts_file = dir_path.join("points.jsonl");
        fs::write(&pts_file, reqs).unwrap();
        for best_guess in [false, true] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exec \"$@\" < \"$0\""])
                .arg(&pts_file)
                .arg(env!("CARGO_BIN_EXE_pdfterm"))
                .arg(pdf_path)
                .arg("--synctex-edit-batch")
                .env("XDG_CONFIG_HOME", dir_path.join("cli-config"));
            if best_guess {
                cmd.arg("--synctex-best-guess");
            }
            let output = process::output(&mut cmd, &Operation::default()).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(results.len(), pts.len());
            for (idx, result) in results.iter().enumerate() {
                assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
                assert_eq!(
                    result["location"]["precise"], false,
                    "beamer slide furniture must remain coarse (best={best_guess} idx={idx}): {result}"
                );
            }
        }
    }
}

#[test]
fn imported_and_local_zero_arg_prose_macros_refine_to_invocation_without_stealing_ties() {
    let directory = tempfile::tempdir().unwrap();
    let src_dir = directory.path().join("src");
    let build_dir = directory.path().join("build");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&build_dir).unwrap();

    let defs_contents = "\\newcommand{\\QX}{QX}\n\
                         \\newcommand{\\Thm}{Theorem}\n\
                         \\newcommand{\\Fig}{Figure}\n\
                         \\newcommand{\\gap}{2}\n";
    let body_contents = "\\thispagestyle{empty}\n\
                         \\section{Overview}\n\
                         𐐀 Café beside damp gravel nudges \\QX{} in this kettle.\n\
                         \n\
                         Intermediate paragraph.\n\
                         Our \\texttt{\\QX{}+ZYW} kettle simmers quietly.\n\
                         \n\
                         Separate \\QX{} clause.\n\
                         Separate QX clause.\n\
                         \n\
                         Same \\QX{} tail.\\newline Same QX tail.\\newline Same \\QX{} tail.\\newline Same QX tail.\\newline Same \\QX{} tail.\\newline Same QX tail.\\newline Same \\QX{} tail.\\newline Same QX tail.\\newline Same \\QX{} tail.\\newline Same QX tail.\n\
                         \n\
                         \\begin{thm} By \\Thm{} we have the result.\\end{thm}\n\
                         \n\
                         \\begin{figure}[ht]\\centering\\rule{10pt}{10pt}\\caption{By \\Fig{} we have the result.}\\end{figure}\n\
                         \n\
                         See \\hspace{\\gap pt} \\cite{kgap} here.\n\
                         \n\
                         See \\vspace{\\gap pt} \\cite{kgap} here.\n\
                         \n\
                         See \\index{\\gap} \\cite{kgap} here.\n\
                         \\clearpage\n\
                         \\begin{thebibliography}{9}\n\
                         \\bibitem[2]{kgap} Reference entry.\n\
                         \\end{thebibliography}\n";
    let main_contents = "\\documentclass{article}\n\
                         \\usepackage[utf8]{inputenc}\n\
                         \\usepackage[T1]{fontenc}\n\
                         \\usepackage{makeidx}\n\
                         \\makeindex\n\
                         \\newtheorem{thm}{Theorem}\n\
                         \\DeclareUnicodeCharacter{10400}{A}\n\
                         \\input{src/defs.tex}\n\
                         \\begin{document}\n\
                         \\input{src/sub.tex}\n\
                         \\end{document}\n";
    let defs_file = src_dir.join("defs.tex");
    let body_file = src_dir.join("sub.tex");
    let main_file = src_dir.join("main.tex");
    fs::write(&defs_file, defs_contents).unwrap();
    fs::write(&body_file, body_contents).unwrap();
    fs::write(&main_file, main_contents).unwrap();

    for _ in 0..2 {
        let compile = process::output(
            Command::new("pdflatex")
                .current_dir(directory.path())
                .args([
                    "-interaction=nonstopmode",
                    "-halt-on-error",
                    "-synctex=1",
                    "-output-directory=build",
                    "-jobname=deck",
                ])
                .arg(&main_file),
            &Operation::new(Duration::from_secs(30)),
        )
        .expect("pdflatex is required for the real imported macro fixture");
        assert!(
            compile.status.success(),
            "{}",
            String::from_utf8_lossy(&compile.stdout)
        );
    }
    let pdf = build_dir.join("deck.pdf");
    let body_source = body_file.canonicalize().unwrap();
    let defs_source = defs_file.canonicalize().unwrap();

    let (plain_idx, plain_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("beside damp gravel"))
        .unwrap();
    let plain_line = plain_idx + 1;
    let plain_byte = plain_row.find("\\QX").unwrap();
    let plain_prefix = &plain_row[..plain_byte];
    let plain_utf16 = plain_prefix.encode_utf16().count() + 1;
    let plain_char = plain_prefix.chars().count() + 1;
    assert!(plain_byte > plain_utf16 && plain_utf16 > plain_char);

    let (wrap_idx, wrap_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("\\texttt{\\QX{}+ZYW}"))
        .unwrap();
    let wrap_line = wrap_idx + 1;
    let wrap_byte = wrap_row.find("\\QX").unwrap();
    let wrap_prefix = &wrap_row[..wrap_byte];
    let wrap_utf16 = wrap_prefix.encode_utf16().count() + 1;
    let wrap_char = wrap_prefix.chars().count() + 1;

    let (sep_macro_idx, sep_macro_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Separate \\QX{} clause."))
        .unwrap();
    let sep_macro_line = sep_macro_idx + 1;
    let sep_macro_byte = sep_macro_row.find("\\QX").unwrap();
    let sep_macro_utf16 = sep_macro_row[..sep_macro_byte].encode_utf16().count() + 1;
    let sep_macro_char = sep_macro_row[..sep_macro_byte].chars().count() + 1;

    let (sep_lit_idx, sep_lit_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("Separate QX clause."))
        .unwrap();
    let sep_lit_line = sep_lit_idx + 1;
    let sep_lit_byte = sep_lit_row.find("QX").unwrap();
    let sep_lit_utf16 = sep_lit_row[..sep_lit_byte].encode_utf16().count() + 1;
    let sep_lit_char = sep_lit_row[..sep_lit_byte].chars().count() + 1;

    let (thm_idx, thm_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("By \\Thm{} we have"))
        .unwrap();
    let thm_line = thm_idx + 1;
    let thm_byte = thm_row.find("\\Thm").unwrap();
    let thm_utf16 = thm_row[..thm_byte].encode_utf16().count() + 1;
    let thm_char = thm_row[..thm_byte].chars().count() + 1;

    let (fig_idx, fig_row) = body_contents
        .lines()
        .enumerate()
        .find(|(_, row)| row.contains("By \\Fig{} we have"))
        .unwrap();
    let fig_line = fig_idx + 1;
    let fig_byte = fig_row.find("\\Fig").unwrap();
    let fig_utf16 = fig_row[..fig_byte].encode_utf16().count() + 1;
    let fig_char = fig_row[..fig_byte].chars().count() + 1;

    let qx_occurrences = painted_word_occurrences(&pdf, "QX");
    assert!(
        qx_occurrences.len() >= 14,
        "expected plain, wrapped, separate-line, and tied QX occurrences, found {}",
        qx_occurrences.len()
    );
    let (plain_hit, plain_center) = qx_occurrences[0];
    let (wrap_hit, wrap_center) = qx_occurrences[1];
    let (sep_macro_hit, sep_macro_center) = qx_occurrences[2];
    let (sep_lit_hit, sep_lit_center) = qx_occurrences[3];
    let (tie1_hit, tie1_center) = qx_occurrences[8];
    let (tie2_hit, tie2_center) = qx_occurrences[9];

    let thm_occurrences = painted_word_occurrences(&pdf, "Theorem");
    assert_eq!(
        thm_occurrences.len(),
        2,
        "expected generated Theorem heading and body \\Thm"
    );
    let (thm_hdr_hit, thm_hdr_center) = thm_occurrences[0];
    let (thm_body_hit, thm_body_center) = thm_occurrences[1];

    let fig_occurrences = painted_word_occurrences(&pdf, "Figure");
    assert_eq!(
        fig_occurrences.len(),
        2,
        "expected generated Figure caption and body \\Fig"
    );
    let (fig_hdr_hit, fig_hdr_center) = fig_occurrences[0];
    let (fig_body_hit, fig_body_center) = fig_occurrences[1];

    let two_page1: Vec<_> = painted_word_occurrences(&pdf, "2")
        .into_iter()
        .filter(|(hit, _)| hit.page == 1)
        .collect();
    assert_eq!(
        two_page1.len(),
        3,
        "expected 3 citation [2] occurrences on page 1"
    );
    let (hspace_cite_hit, hspace_cite_center) = two_page1[0];
    let (vspace_cite_hit, vspace_cite_center) = two_page1[1];
    let (index_cite_hit, index_cite_center) = two_page1[2];

    let mut requests = String::new();
    for point in [
        plain_hit,
        plain_center,
        wrap_hit,
        wrap_center,
        sep_macro_hit,
        sep_macro_center,
        sep_lit_hit,
        sep_lit_center,
        tie1_hit,
        tie1_center,
        tie2_hit,
        tie2_center,
        thm_body_hit,
        thm_body_center,
        fig_body_hit,
        fig_body_center,
        thm_hdr_hit,
        thm_hdr_center,
        fig_hdr_hit,
        fig_hdr_center,
        hspace_cite_hit,
        hspace_cite_center,
        vspace_cite_hit,
        vspace_cite_center,
        index_cite_hit,
        index_cite_center,
    ] {
        requests.push_str(
            &serde_json::json!({
                "page": point.page,
                "x": point.x,
                "y": point.y_from_top,
            })
            .to_string(),
        );
        requests.push('\n');
    }
    let points = directory.path().join("points.jsonl");
    fs::write(&points, requests).unwrap();

    for best_guess in [false, true] {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec \"$@\" < \"$0\""])
            .arg(&points)
            .arg(env!("CARGO_BIN_EXE_pdfterm"))
            .arg(&pdf)
            .arg("--synctex-edit-batch")
            .env("XDG_CONFIG_HOME", directory.path().join("cli-config"));
        if best_guess {
            cmd.arg("--synctex-best-guess");
        }
        let output = process::output(&mut cmd, &Operation::default()).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(results.len(), 26);
        for (idx, (word, line, byte, utf16, ch)) in [
            ("QX", plain_line, plain_byte, plain_utf16, plain_char),
            ("QX", plain_line, plain_byte, plain_utf16, plain_char),
            ("QX", wrap_line, wrap_byte, wrap_utf16, wrap_char),
            ("QX", wrap_line, wrap_byte, wrap_utf16, wrap_char),
            (
                "QX",
                sep_macro_line,
                sep_macro_byte,
                sep_macro_utf16,
                sep_macro_char,
            ),
            (
                "QX",
                sep_macro_line,
                sep_macro_byte,
                sep_macro_utf16,
                sep_macro_char,
            ),
            (
                "QX",
                sep_lit_line,
                sep_lit_byte,
                sep_lit_utf16,
                sep_lit_char,
            ),
            (
                "QX",
                sep_lit_line,
                sep_lit_byte,
                sep_lit_utf16,
                sep_lit_char,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let result = &results[idx];
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], word, "{result}");
            let resolved_file = Path::new(result["location"]["file"].as_str().unwrap())
                .canonicalize()
                .unwrap();
            assert_eq!(resolved_file, body_source, "{result}");
            assert_ne!(resolved_file, defs_source, "{result}");
            assert_eq!(result["location"]["line"], line, "{result}");
            assert_eq!(result["location"]["byte_column"], byte, "{result}");
            assert_eq!(result["location"]["column"], utf16, "{result}");
            assert_eq!(result["location"]["column_char"], ch, "{result}");
            assert_eq!(result["location"]["precise"], true, "{result}");
        }
        for (idx, result) in results.iter().enumerate().take(12).skip(8) {
            assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
            assert_eq!(result["pdf_word"], "QX", "{result}");
            assert_eq!(
                result["location"]["precise"], false,
                "equal-context macro/literal tie must remain coarse: {result}"
            );
        }
        for (idxs, word, line, byte, utf16, ch) in [
            (
                [12usize, 13],
                "Theorem",
                thm_line,
                thm_byte,
                thm_utf16,
                thm_char,
            ),
            ([14, 15], "Figure", fig_line, fig_byte, fig_utf16, fig_char),
        ] {
            for idx in idxs {
                let result = &results[idx];
                assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
                assert_eq!(result["pdf_word"], word, "{result}");
                let resolved_file = Path::new(result["location"]["file"].as_str().unwrap())
                    .canonicalize()
                    .unwrap();
                assert_eq!(resolved_file, body_source, "{result}");
                assert_ne!(resolved_file, defs_source, "{result}");
                assert_eq!(result["location"]["line"], line, "{result}");
                assert_eq!(result["location"]["byte_column"], byte, "{result}");
                assert_eq!(result["location"]["column"], utf16, "{result}");
                assert_eq!(result["location"]["column_char"], ch, "{result}");
                assert_eq!(result["location"]["precise"], true, "{result}");
            }
        }
        for (idxs, word, label) in [
            ([16usize, 17], "Theorem", "generated theorem heading"),
            ([18, 19], "Figure", "generated figure caption heading"),
            ([20, 21], "2", "citation [2] beside \\hspace{\\gap pt}"),
            ([22, 23], "2", "citation [2] beside \\vspace{\\gap pt}"),
            ([24, 25], "2", "citation [2] beside \\index{\\gap}"),
        ] {
            for idx in idxs {
                let result = &results[idx];
                assert_eq!(result["ok"], true, "best={best_guess} idx={idx}: {result}");
                assert_eq!(result["pdf_word"], word, "{result}");
                assert_eq!(
                    result["location"]["precise"], false,
                    "{label} must remain coarse: {result}"
                );
            }
        }
    }
}

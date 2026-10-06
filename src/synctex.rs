mod math;

use crate::pdf::SearchRect;
use crate::process::Operation;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Source coordinates: one-based line, zero-based UTF-8 byte offset;
/// column is one-based UTF-16 (VS Code), column_char is one-based Unicode scalar.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub byte_column: usize,
    pub column: usize,
    pub column_char: usize,
    pub precise: bool,
}

/// Identity of a local PDF revision, including atomic replacement and in-place writes.
/// Not a content digest: the protocol assumes a non-adversarial local filesystem.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PdfRevision {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl PdfRevision {
    pub fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    pub fn check(self, path: &Path) -> io::Result<()> {
        if Self::read(path)? != self {
            return Err(io::Error::other(
                "PDF revision changed; repeat forward search",
            ));
        }
        Ok(())
    }
}

/// Metadata identity of both files observed when PDFium opens a document.
/// Stability does not prove a common build; producers must publish a completed
/// PDF/SyncTeX pair before requesting navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentRevision {
    pub pdf: PdfRevision,
    companion: Option<(bool, PdfRevision)>,
}
impl DocumentRevision {
    pub fn read(path: &Path) -> io::Result<Self> {
        let pdf = PdfRevision::read(path)?;
        let mut companion = None;
        for (extension, compressed) in [("synctex.gz", true), ("synctex", false)] {
            match PdfRevision::read(&path.with_extension(extension)) {
                Ok(revision) => {
                    companion = Some((compressed, revision));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Self { pdf, companion })
    }
    pub fn check(self, path: &Path) -> io::Result<()> {
        if Self::read(path)? != self {
            return Err(io::Error::other(
                "displayed PDF/SyncTeX revision changed; wait for reload and click again",
            ));
        }
        Ok(())
    }
}

pub struct InverseResolution {
    pub location: SourceLocation,
    pub warning: Option<String>,
}

/// Literal source word and nearby prose tokens; not TeX macro expansion.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardWord {
    pub words: Vec<String>,
    pub selected: usize,
}

impl ForwardWord {
    fn at(text: &str, column: u32) -> Option<Self> {
        let text = source_line_text(text);
        let byte = text.char_indices().nth(column.checked_sub(1)? as usize)?.0;
        let words: Vec<_> = words(text)
            .into_iter()
            .filter(|(start, _)| *start == 0 || !text[..*start].ends_with('\\'))
            .collect();
        Self::from_tokens(&words, byte)
    }

    fn from_tokens(words: &[(usize, &str)], byte: usize) -> Option<Self> {
        let selected = words
            .iter()
            .position(|(start, word)| *start <= byte && byte < start + word.len())?;
        let start = selected.saturating_sub(3);
        let end = (selected + 4).min(words.len());
        if words[start..end].iter().any(|(_, word)| word.len() > 128) {
            return None;
        }
        Some(Self {
            words: words[start..end]
                .iter()
                .map(|(_, word)| (*word).to_owned())
                .collect(),
            selected: selected - start,
        })
    }

    fn valid(&self) -> bool {
        self.words.len() <= 7
            && self.selected < self.words.len()
            && self.words.iter().all(|word| {
                word.len() <= 128
                    && word.chars().next().is_some_and(char::is_alphanumeric)
                    && word
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || is_combining_mark(ch))
            })
    }
}

// Editor adapters can supply a literal saved line and UTF-8 cursor offset.
// Tokenize here so every adapter shares PDF matching's Unicode word rules.
fn deserialize_forward_word<'de, D>(deserializer: D) -> Result<Option<ForwardWord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Literal {
        text: String,
        byte_column: usize,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Context(ForwardWord),
        Literal(Literal),
    }
    match Option::<Input>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Input::Context(word)) => Ok(Some(word)),
        Some(Input::Literal(Literal { text, byte_column })) => {
            if !text.is_char_boundary(byte_column) {
                return Err(serde::de::Error::custom("invalid source word byte column"));
            }
            Ok(ForwardWord::from_tokens(&words(&text), byte_column))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub pdf: std::path::PathBuf,
    pub revision: PdfRevision,
    pub page: u32,
    pub h: f32,
    pub v: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default, deserialize_with = "deserialize_forward_word")]
    pub word: Option<ForwardWord>,
    /// Revision-bound compiler source-map service supplied by an editor adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_search: Option<String>,
}

impl ForwardRequest {
    pub fn rect(&self) -> SearchRect {
        SearchRect {
            left: self.h,
            top: self.v - self.height,
            right: self.h + self.width,
            bottom: self.v,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        if self
            .inverse_search
            .as_ref()
            .is_some_and(|path| !Path::new(path).is_absolute() || path.as_bytes().contains(&0))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inverse source-map endpoint must be an absolute Unix socket path",
            ));
        }
        if self.word.as_ref().is_some_and(|word| !word.valid()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid forward word context",
            ));
        }
        if !self.pdf.is_absolute()
            || self.page == 0
            || self.width < 0.0
            || self.height < 0.0
            || ![
                self.h,
                self.v,
                self.width,
                self.height,
                self.h + self.width,
                self.v - self.height,
            ]
            .into_iter()
            .all(f32::is_finite)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "forward request requires an absolute PDF path, positive page, and finite nonnegative box dimensions",
            ));
        }
        Ok(())
    }
}

pub fn parse_forward_request(payload: &str) -> io::Result<ForwardRequest> {
    let request: ForwardRequest = serde_json::from_str(payload)?;
    request.validate()?;
    Ok(request)
}

pub fn resolve_forward(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
) -> io::Result<ForwardRequest> {
    resolve_forward_with_library(pdf, file, line, column, None)
}

pub fn resolve_forward_with_library(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
    library: Option<&Path>,
) -> io::Result<ForwardRequest> {
    if line == 0 || column == 0 {
        return Err(io::Error::other("source line and column must be positive"));
    }
    let pdf = fs::canonicalize(pdf)?;
    let file = fs::canonicalize(file)?;
    let revision = PdfRevision::read(&pdf)?;
    let source = read_source(&file)?;
    let word = source
        .lines()
        .nth(line as usize - 1)
        .and_then(|text| ForwardWord::at(text, column));
    // Beamer records frame bodies at the closing line; query that line so the
    // resolver cannot pick an earlier frame's mapping.
    let view_line = frame_closing_line(&source, line).unwrap_or(line);
    let spec = format!(
        "{view_line}:{column}:{}",
        file.to_str()
            .ok_or_else(|| io::Error::other("source path is not UTF-8"))?
    );
    let stdout = run(
        &Operation::default(),
        &[
            "view",
            "-i",
            &spec,
            "-o",
            pdf.to_str()
                .ok_or_else(|| io::Error::other("PDF path is not UTF-8"))?,
        ],
    )?;
    let (mut page, mut h, mut v, mut width, mut height) = (None, None, None, None, None);
    let mut candidates = Vec::new();
    let mut captured = false;
    for row in stdout.lines() {
        if let Some((key, value)) = row.split_once(':') {
            match key {
                "Page" => {
                    page = value.trim().parse::<u32>().ok();
                    h = None;
                    v = None;
                    width = None;
                    height = None;
                    captured = false;
                }
                "h" => h = value.trim().parse::<f32>().ok(),
                "v" => v = value.trim().parse::<f32>().ok(),
                "W" => width = value.trim().parse::<f32>().ok(),
                "H" => height = value.trim().parse::<f32>().ok(),
                _ => {}
            }
        }
        if !captured
            && let (Some(page), Some(h), Some(v), Some(width), Some(height)) =
                (page, h, v, width, height)
        {
            let result = ForwardRequest {
                revision,
                pdf: pdf.clone(),
                page,
                h,
                v,
                width,
                height,
                word: word.clone(),
                inverse_search: None,
            };
            match result.validate() {
                Ok(()) => candidates.push(result),
                Err(error) if candidates.is_empty() => return Err(error),
                Err(_) => {}
            }
            captured = true;
        }
    }
    let mut result = candidates
        .first()
        .cloned()
        .ok_or_else(|| io::Error::other("synctex view returned no complete match"))?;
    if let Some(hint) = &word {
        let mut pages = Vec::new();
        for candidate in &candidates {
            if !pages.contains(&candidate.page) {
                pages.push(candidate.page);
            }
        }
        // SyncTeX returns overlay matches in traversal order, which need not
        // be page order. Keep its first result as fallback, then inspect the
        // remaining overlays in the order a reader sees them.
        if pages.len() > 1 {
            pages[1..].sort_unstable();
        }
        if let Some(page) = crate::pdf::first_visible_forward_page(&pdf, &pages, hint, library)
            .map_err(io::Error::other)?
            && let Some(candidate) = candidates.iter().find(|candidate| candidate.page == page)
        {
            result = candidate.clone();
        }
    }
    revision.check(&pdf)?;
    Ok(result)
}

fn run(operation: &Operation, args: &[&str]) -> io::Result<String> {
    let output = crate::process::output(Command::new("synctex").args(args), operation)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "synctex failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[derive(Clone, Copy, Debug)]
pub struct InversePoint {
    pub page: u32,
    pub x: f32,
    pub y_from_top: f32,
    pub page_height_pt: f32,
}

pub fn resolve_inverse(
    pdf: &Path,
    point: InversePoint,
    word: Option<(&str, usize)>,
    radius: u32,
    operation: &Operation,
) -> io::Result<InverseResolution> {
    let pdf = std::path::absolute(pdf)?;
    let spec = format!(
        "{}:{:.2}:{:.2}:{}",
        point.page,
        point.x,
        point.y_from_top,
        pdf.display()
    );
    let stdout = run(operation, &["edit", "-o", &spec])?;
    let mut target = parse_synctex_edit(&stdout)
        .ok_or_else(|| io::Error::other("synctex edit returned no match"))?;
    let path = Path::new(&target.file);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        pdf.parent().unwrap_or(Path::new(".")).join(path)
    };
    let mut warning = None;
    let resolved = match fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(error) => {
            warning = Some(format!(
                "line-only navigation: source path unavailable: {error}"
            ));
            absolute
        }
    };
    target.file = resolved
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("source path is not UTF-8"))?;
    let generated = Path::new(&target.file)
        .extension()
        .is_some_and(|ext| ext == "vrb")
        && Path::new(&target.file).file_stem() == pdf.file_stem();
    // A fragile-body hit can instead be reported at unrelated document
    // metadata. Keep that raw coarse location, but allow the clicked sheet's
    // original frame as a bounded literal-word refinement candidate.
    let generated_path = if generated {
        PathBuf::from(&target.file)
    } else {
        pdf.with_extension("vrb")
    };
    let source_map = (generated || word.is_some())
        .then(|| read_source_map(&pdf, point, &generated_path, !generated, operation));
    let mut remapped_source = None;
    if generated && let Some(map) = &source_map {
        match map
            .as_ref()
            .map_err(|error| error.to_string())
            .and_then(|map| original_source_from_verbatim(map).map_err(|error| error.to_string()))
        {
            Ok((file, original, anchor)) => {
                target.file = file;
                target.line = anchor;
                remapped_source = Some(original);
            }
            Err(error) => {
                warning = Some(format!(
                    "line-only navigation: original fragile frame unavailable: {error}"
                ));
            }
        }
    }
    // The final .vrb may belong to a different frame. Never refine against it.
    if let Some((context, offset)) = word
        && (!generated || remapped_source.is_some())
    {
        operation.check()?;
        let source = remapped_source.map_or_else(|| read_source(Path::new(&target.file)), Ok);
        if let Err(error) = &source {
            warning = Some(format!(
                "line-only navigation: source refinement unavailable: {error}"
            ));
        }
        if let Ok(mut source) = source {
            // A fragile sheet also contains nongenerated headings and footers.
            // Sheet-wide .vrb participation does not prove this point is body text.
            let scope_proven = fragile_frame_range(&source, target.line).is_none_or(|frame| {
                generated
                    || source_map
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .is_some_and(|map| {
                            map.generated_at_point
                                && map.page_source.as_ref().is_some_and(|(path, line)| {
                                    *line == frame.end as u32
                                        && fs::canonicalize(path)
                                            .is_ok_and(|path| path == Path::new(&target.file))
                                })
                        })
            });
            let mut location = if scope_proven {
                source_word_location(&source, target.line, context, offset, radius)
            } else {
                source_word_location_in_range(
                    &source,
                    target.line,
                    context,
                    offset,
                    target.line.saturating_sub(1) as usize..target.line as usize,
                    false,
                )
            };
            if location.is_none()
                && !generated
                && let Some(map) = source_map
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .filter(|map| map.generated_at_point && map.page_source.is_some())
            {
                match original_source_from_verbatim(map) {
                    Ok((file, original, anchor)) => {
                        if let Some(candidate) =
                            source_word_location(&original, anchor, context, offset, radius)
                        {
                            target.file = file;
                            target.line = anchor;
                            source = original;
                            location = Some(candidate);
                        }
                    }
                    Err(error) => {
                        warning = Some(format!(
                            "line-only navigation: original fragile frame unavailable: {error}"
                        ));
                    }
                }
            }
            if let Some(Err(error)) = &source_map {
                warning = Some(format!("source-map refinement unavailable: {error}"));
            }
            if let Some((file, original, line, byte, score)) = source_map
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|map| document_metadata_word_location(&map.main, context, offset))
                && (location.is_none()
                    || score >= 6
                        && metadata_beats_frame(
                            &source,
                            target.line,
                            context,
                            offset,
                            point.y_from_top,
                            point.page_height_pt,
                            score,
                        ))
            {
                target.file = file;
                source = original;
                location = Some((line, byte));
            }
            if let Some((line, byte)) = location {
                let text = source
                    .lines()
                    .nth(line as usize - 1)
                    .ok_or_else(|| io::Error::other("source line is missing"))?;
                let prefix = text
                    .get(..byte)
                    .ok_or_else(|| io::Error::other("source column is not a UTF-8 boundary"))?;
                target.line = line;
                target.byte_column = byte;
                target.column = prefix.encode_utf16().count() + 1;
                target.column_char = prefix.chars().count() + 1;
                target.precise = true;
            }
        }
    }
    operation.check()?;
    Ok(InverseResolution {
        location: target,
        warning,
    })
}

/// A frame title and the running document title may contain the same words.
/// Preserve a local frame hit except when a footer has stronger metadata context.
fn metadata_beats_frame(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    y_from_top: f32,
    page_height_pt: f32,
    metadata_score: isize,
) -> bool {
    let Some(frame) = source_frame_range(source, line) else {
        return true;
    };
    y_from_top >= page_height_pt / 2.0
        && source_prose_scored_location(
            source,
            line,
            context,
            offset,
            frame,
            true,
            &nonprinting_source_ranges(source),
        )
        .is_some_and(|(_, _, local_score)| metadata_score >= local_score)
}

fn read_source(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = Vec::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "source refinement requires a regular file",
        ));
    }
    file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("source exceeds 2 MiB refinement limit"));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

struct SourceMap {
    main: PathBuf,
    page_source: Option<(PathBuf, u32)>,
    generated_at_point: bool,
}

#[derive(Clone, Copy)]
struct SourceBox {
    bounds: [f64; 4],
    generated: bool,
}

fn source_box_bounds(row: &str, position: [f64; 2]) -> io::Result<Option<[f64; 4]>> {
    let parse = || {
        let size = row.split(':').nth(2)?;
        let mut size = size.split(',');
        let [width, height, depth] = [size.next()?, size.next()?, size.next()?]
            .map(|value| value.parse::<f64>().ok().filter(|value| value.is_finite()));
        Some([position[0], position[1], width?, height?, depth?])
    };
    let [x, y, width, height, depth] =
        parse().ok_or_else(|| io::Error::other("SyncTeX horizontal box has invalid geometry"))?;
    Ok((width > 0.0 && height + depth > 0.0).then_some([x, y - height, x + width, y + depth]))
}

fn source_map_position(row: &str, last_v: &mut f64) -> io::Result<[f64; 2]> {
    let position = row
        .split(':')
        .nth(1)
        .and_then(|position| position.split_once(','))
        .ok_or_else(|| io::Error::other("SyncTeX node has invalid position"))?;
    let x = source_map_number(position.0)?;
    let y = if position.1 == "=" {
        *last_v
    } else {
        let y = source_map_number(position.1)?;
        *last_v = y;
        y
    };
    Ok([x, y])
}

fn generated_box_at_point(
    boxes: &[SourceBox],
    x: f64,
    y: f64,
    operation: &Operation,
) -> io::Result<bool> {
    let contains =
        |a: [f64; 4], b: [f64; 4]| a[0] <= b[0] && a[1] <= b[1] && a[2] >= b[2] && a[3] >= b[3];
    let mut nearest: Vec<SourceBox> = Vec::new();
    for candidate in boxes {
        operation.check()?;
        if !contains(candidate.bounds, [x, y, x, y]) {
            continue;
        }
        if let Some(equal) = nearest
            .iter_mut()
            .find(|old| old.bounds == candidate.bounds)
        {
            equal.generated &= candidate.generated;
        } else if !nearest
            .iter()
            .any(|old| contains(candidate.bounds, old.bounds))
        {
            nearest.retain(|old| !contains(old.bounds, candidate.bounds));
            nearest.push(*candidate);
        }
    }
    // Conflicting nonnested overlaps and equal boxes with mixed ownership abstain.
    Ok(nearest.len() == 1 && nearest[0].generated)
}

fn source_map_number(value: &str) -> io::Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| io::Error::other("SyncTeX transform has an invalid number"))
}

fn source_map_dimension(value: &str) -> io::Result<f64> {
    let units = [
        ("in", 72.27 * 65536.0),
        ("cm", 72.27 * 65536.0 / 2.54),
        ("mm", 72.27 * 65536.0 / 25.4),
        ("pt", 65536.0),
        ("bp", 72.27 / 72.0 * 65536.0),
        ("pc", 12.0 * 65536.0),
        ("sp", 1.0),
        ("dd", 1238.0 / 1157.0 * 65536.0),
        ("cc", 14856.0 / 1157.0 * 65536.0),
        ("nd", 685.0 / 642.0 * 65536.0),
        ("nc", 1370.0 / 107.0 * 65536.0),
    ];
    for (suffix, scale) in units {
        if let Some(number) = value.trim().strip_suffix(suffix) {
            return source_map_number(number).map(|number| number * scale);
        }
    }
    source_map_number(value)
}

fn read_source_map(
    pdf: &Path,
    point: InversePoint,
    generated: &Path,
    geometry_required: bool,
    operation: &Operation,
) -> io::Result<SourceMap> {
    let compressed = pdf.with_extension("synctex.gz");
    match fs::File::open(&compressed) {
        Ok(file) => source_map_from_reader(
            BufReader::new(flate2::read::GzDecoder::new(file).take(128 * 1024 * 1024)),
            pdf,
            point,
            generated,
            geometry_required,
            operation,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => source_map_from_reader(
            BufReader::new(fs::File::open(pdf.with_extension("synctex"))?.take(128 * 1024 * 1024)),
            pdf,
            point,
            generated,
            geometry_required,
            operation,
        ),
        Err(error) => Err(error),
    }
}

fn source_map_from_reader(
    mut reader: impl BufRead,
    pdf: &Path,
    point: InversePoint,
    generated: &Path,
    geometry_required: bool,
    operation: &Operation,
) -> io::Result<SourceMap> {
    let mut inputs = std::collections::HashMap::new();
    let mut generated_tags = std::collections::HashSet::new();
    let mut row = String::new();
    let mut in_page = false;
    let mut page_complete = false;
    let mut origin = None;
    let mut generated_on_page = false;
    let mut stack: Vec<(u8, Option<[f64; 4]>, bool)> = Vec::new();
    let mut boxes = Vec::new();
    let mut unit = None;
    let mut magnification = None;
    let mut x_offset = None;
    let mut y_offset = None;
    let mut post_magnification = 1.0;
    let mut post_x_offset = None;
    let mut post_y_offset = None;
    let mut post_scriptum = false;
    let mut postamble = false;
    let mut postamble_count = false;
    let mut bytes_read = 0;
    let mut last_v = -1.0;
    loop {
        operation.check()?;
        row.clear();
        let count = reader.read_line(&mut row)?;
        bytes_read += count;
        if bytes_read >= 128 * 1024 * 1024 {
            return Err(io::Error::other("SyncTeX source map exceeds 128 MiB"));
        }
        if count == 0 {
            break;
        }
        let row = row.trim_end();
        if geometry_required && row == "Postamble:" {
            postamble = true;
        } else if geometry_required
            && postamble
            && let Some(value) = row.strip_prefix("Count:")
        {
            value
                .parse::<u32>()
                .map_err(|_| io::Error::other("SyncTeX postamble has invalid Count"))?;
            postamble_count = true;
        }
        if geometry_required && row == "Post scriptum:" {
            post_scriptum = true;
        } else if geometry_required && let Some(value) = row.strip_prefix("Unit:") {
            unit = Some(source_map_number(value)?);
        } else if geometry_required && let Some(value) = row.strip_prefix("Magnification:") {
            let value = source_map_number(value)?;
            if post_scriptum {
                post_magnification = value;
            } else {
                magnification = Some(value);
            }
        } else if geometry_required && let Some(value) = row.strip_prefix("X Offset:") {
            if post_scriptum {
                post_x_offset = Some(source_map_dimension(value)?);
            } else {
                x_offset = Some(source_map_number(value)?);
            }
        } else if geometry_required && let Some(value) = row.strip_prefix("Y Offset:") {
            if post_scriptum {
                post_y_offset = Some(source_map_dimension(value)?);
            } else {
                y_offset = Some(source_map_number(value)?);
            }
        }
        // Compressed ",=" reuses the scanner's global last vertical value,
        // including records from earlier sheets and form definitions.
        let position = if geometry_required
            && matches!(
                row.as_bytes().first(),
                Some(b'[' | b'(' | b'v' | b'h' | b'x' | b'k' | b'g' | b'r' | b'$' | b'f')
            ) {
            Some(source_map_position(row, &mut last_v)?)
        } else {
            None
        };
        if let Some(input) = row.strip_prefix("Input:") {
            if let Some((tag, path)) = input.split_once(':')
                && let Ok(tag) = tag.parse::<u32>()
            {
                let path = Path::new(path);
                let path = if path.is_absolute() {
                    path.to_owned()
                } else {
                    pdf.parent().unwrap_or(Path::new(".")).join(path)
                };
                // TeX can emit .vrb in the build cwd while the PDF is in a
                // separate output directory. Jobname is the map identity;
                // the final generated file is neither read nor trusted.
                if path.file_name() == generated.file_name() {
                    generated_tags.insert(tag);
                }
                inputs.insert(tag, path);
            }
        } else if let Some(page) = row.strip_prefix('{') {
            in_page = page.parse::<u32>() == Ok(point.page);
            if in_page && page_complete {
                return Err(io::Error::other(
                    "SyncTeX source map repeats the clicked sheet",
                ));
            }
        } else if in_page && row.starts_with('}') {
            if row[1..].parse::<u32>() != Ok(point.page) || !stack.is_empty() {
                return Err(io::Error::other(
                    "SyncTeX source map has mismatched sheet bounds",
                ));
            }
            page_complete = true;
            in_page = false;
            if !geometry_required {
                let main = inputs
                    .get(&1)
                    .cloned()
                    .ok_or_else(|| io::Error::other("SyncTeX source map has no main input"))?;
                let page_source = origin
                    .filter(|_| generated_on_page)
                    .and_then(|(tag, line)| inputs.remove(&tag).map(|path| (path, line)));
                return Ok(SourceMap {
                    main,
                    page_source,
                    generated_at_point: false,
                });
            }
        } else if in_page {
            if geometry_required && matches!(row.as_bytes().first(), Some(b'<' | b'>' | b'f')) {
                return Err(io::Error::other(
                    "SyncTeX fragile body ownership cannot resolve sheet form transforms",
                ));
            }
            let link = row
                .get(1..)
                .and_then(|row| row.split_once(':'))
                .and_then(|(link, _)| link.split_once(','))
                .and_then(|(tag, rest)| {
                    Some((
                        tag.parse::<u32>().ok()?,
                        rest.split(',').next()?.parse::<u32>().ok()?,
                    ))
                });
            // Only the first box enclosing the whole sheet proves provenance.
            if origin.is_none() {
                if !matches!(row.as_bytes().first(), Some(b'[' | b'(')) || link.is_none() {
                    return Err(io::Error::other(
                        "SyncTeX sheet has no enclosing source box",
                    ));
                }
                origin = link;
            }
            let generated_node = link.is_some_and(|(tag, _)| generated_tags.contains(&tag));
            if geometry_required {
                match row.as_bytes().first() {
                    Some(kind @ (b'[' | b'(')) => {
                        let bounds = if *kind == b'(' {
                            source_box_bounds(
                                row,
                                position.ok_or_else(|| {
                                    io::Error::other("SyncTeX horizontal box has no position")
                                })?,
                            )?
                        } else {
                            None
                        };
                        stack.push((*kind, bounds, generated_node));
                    }
                    Some(b']' | b')') => {
                        let (kind, bounds, generated) = stack.pop().ok_or_else(|| {
                            io::Error::other("SyncTeX sheet has an unmatched box close")
                        })?;
                        if (kind == b'(') != row.starts_with(')') {
                            return Err(io::Error::other(
                                "SyncTeX sheet has mismatched box bounds",
                            ));
                        }
                        if let Some(bounds) = bounds {
                            boxes.push(SourceBox { bounds, generated });
                        }
                        if let Some(parent) = stack.last_mut() {
                            parent.2 |= generated;
                        }
                    }
                    _ => {
                        if generated_node && let Some(parent) = stack.last_mut() {
                            parent.2 = true;
                        }
                    }
                }
            }
            if matches!(
                row.as_bytes().first(),
                Some(b'[' | b'(' | b'v' | b'h' | b'k' | b'g' | b'x' | b'$' | b'r')
            ) && let Some((tag, _)) = link
            {
                generated_on_page |= generated_tags.contains(&tag);
            }
        }
    }
    if !page_complete || !postamble_count {
        return Err(io::Error::other("SyncTeX source map is incomplete"));
    }
    // Match SyncTeX's effective preamble/Post scriptum transform. The offsets
    // are not magnified; post offsets are dimensions in scaled TeX points.
    let unit = unit
        .filter(|unit| *unit > 0.0)
        .ok_or_else(|| io::Error::other("SyncTeX source map has no positive Unit"))?;
    let magnification = magnification
        .filter(|magnification| *magnification > 0.0)
        .ok_or_else(|| io::Error::other("SyncTeX source map has no positive Magnification"))?;
    if post_magnification <= 0.0 {
        return Err(io::Error::other(
            "SyncTeX post magnification is not positive",
        ));
    }
    let pre_unit = unit / 65781.76;
    let visible_unit = pre_unit * magnification / 1000.0 * post_magnification;
    let (x_offset, y_offset) = if let Some(x) = post_x_offset {
        let y = post_y_offset
            .ok_or_else(|| io::Error::other("SyncTeX post X Offset has no Y Offset"))?;
        (x / 65781.76, y / 65781.76)
    } else {
        (
            x_offset.ok_or_else(|| io::Error::other("SyncTeX source map has no X Offset"))?
                * pre_unit,
            y_offset.ok_or_else(|| io::Error::other("SyncTeX source map has no Y Offset"))?
                * pre_unit,
        )
    };
    if !visible_unit.is_finite()
        || visible_unit <= 0.0
        || !x_offset.is_finite()
        || !y_offset.is_finite()
    {
        return Err(io::Error::other(
            "SyncTeX effective transform is not finite",
        ));
    }
    let main = inputs
        .get(&1)
        .cloned()
        .ok_or_else(|| io::Error::other("SyncTeX source map has no main input"))?;
    let page_source = origin
        .filter(|_| generated_on_page)
        .and_then(|(tag, line)| inputs.remove(&tag).map(|path| (path, line)));
    let generated_at_point = generated_box_at_point(
        &boxes,
        (f64::from(point.x) - x_offset) / visible_unit,
        (f64::from(point.y_from_top) - y_offset) / visible_unit,
        operation,
    )?;
    Ok(SourceMap {
        main,
        page_source,
        generated_at_point,
    })
}

/// Beamer reopens the same .vrb for successive fragile frames. The clicked
/// sheet's enclosing source-map box identifies the original frame; the final
/// generated file's contents cannot establish that identity.
fn original_source_from_verbatim(map: &SourceMap) -> io::Result<(String, String, u32)> {
    let (original, line) = map.page_source.as_ref().ok_or_else(|| {
        io::Error::other("clicked sheet has no original generated-file provenance")
    })?;
    let original = fs::canonicalize(original)?;
    let source = read_source(&original)?;
    fragile_frame_range(&source, *line)
        .filter(|frame| *line == frame.end as u32)
        .ok_or_else(|| {
            io::Error::other("clicked sheet does not identify a fragile frame's closing line")
        })?;
    let file = original
        .to_str()
        .ok_or_else(|| io::Error::other("original fragile source path is not UTF-8"))?
        .to_owned();
    Ok((file, source, *line))
}

fn fragile_frame_range(source: &str, line: u32) -> Option<std::ops::Range<usize>> {
    let frame = source_frame_range(source, line)?;
    let row = source_line_text(source.lines().nth(frame.start)?);
    let row_start: usize = source
        .split_inclusive('\n')
        .take(frame.start)
        .map(str::len)
        .sum();
    let mut at = row
        .match_indices("\\begin")
        .filter(|(at, _)| {
            row[..*at]
                .bytes()
                .rev()
                .take_while(|ch| *ch == b'\\')
                .count()
                % 2
                == 0
        })
        .find_map(|(at, _)| {
            let (environment, end) = math::group(source, row_start + at + "\\begin".len())?;
            (&source[environment] == "frame").then_some(end)
        })?;
    at = skip_source_space(source, at);
    if source.as_bytes().get(at) == Some(&b'<') {
        let end = source[at..].find('>')?;
        at = skip_source_space(source, at + end + 1);
    }
    let end = optional_argument_end(source, at)?;
    let options = &source[at + 1..end - 1];
    options
        .split(',')
        .flat_map(str::lines)
        .any(|option| {
            let option = source_line_text(option).trim();
            option == "fragile" || option.starts_with("fragile=")
        })
        .then_some(frame)
}

/// Title pages and running headers can be produced from declarations far
/// outside SyncTeX's reported frame. Search only literal document metadata,
/// including direct preamble inputs, and require neighboring PDF words.
fn document_metadata_word_location(
    main: &Path,
    context: &str,
    offset: usize,
) -> Option<(String, String, u32, usize, isize)> {
    let main = fs::canonicalize(main).ok()?;
    let main_source = read_source(&main).ok()?;
    let mut files = vec![(main.clone(), main_source)];
    let inputs: Vec<_> = files[0]
        .1
        .lines()
        .take_while(|row| !source_line_text(row).contains("\\begin{document}"))
        .filter_map(|row| {
            source_line_text(row)
                .trim()
                .strip_prefix("\\input{")
                .and_then(|rest| rest.split_once('}'))
                .map(|(name, _)| name.to_owned())
        })
        .take(8)
        .collect();
    for name in inputs {
        let mut path = main.parent()?.join(name);
        if path.extension().is_none() {
            path.set_extension("tex");
        }
        if let Ok(path) = fs::canonicalize(path)
            && !files.iter().any(|(known, _)| *known == path)
            && let Ok(source) = read_source(&path)
        {
            files.push((path, source));
        }
    }
    let mut best = None;
    let mut tied = false;
    for (file_index, (_, source)) in files.iter().enumerate() {
        let hidden = nonprinting_source_ranges(source);
        let mut start = 0;
        for raw_row in source.split_inclusive('\n') {
            let row = raw_row.strip_suffix('\n').unwrap_or(raw_row);
            let row = row.strip_suffix('\r').unwrap_or(row);
            let visible = source_line_text(row);
            for command in ["\\title", "\\subtitle", "\\author", "\\date", "\\institute"] {
                let Some(at) = visible.find(command) else {
                    continue;
                };
                let Some((body, end)) = math::group(source, start + at + command.len()) else {
                    continue;
                };
                let first = source[..body.start]
                    .bytes()
                    .filter(|ch| *ch == b'\n')
                    .count();
                let last = source[..end].bytes().filter(|ch| *ch == b'\n').count();
                let Some((line, byte, score)) = source_prose_scored_location(
                    source,
                    1,
                    context,
                    offset,
                    first..last + 1,
                    true,
                    &hidden,
                ) else {
                    continue;
                };
                if score < 3 {
                    continue;
                }
                match best {
                    Some((old, _, _, _)) if score < old => {}
                    Some((old, index, old_line, old_byte)) if score == old => {
                        tied |= (file_index, line, byte) != (index, old_line, old_byte);
                    }
                    _ => {
                        best = Some((score, file_index, line, byte));
                        tied = false;
                    }
                }
            }
            start += raw_row.len();
        }
    }
    let (score, index, line, byte) = best.filter(|_| !tied)?;
    let (path, source) = files.swap_remove(index);
    Some((path.to_str()?.to_owned(), source, line, byte, score))
}

pub(crate) fn parse_synctex_edit(stdout: &str) -> Option<SourceLocation> {
    let (mut input, mut line) = (None, None);
    let mut result = None;
    for row in stdout.lines() {
        if let Some(value) = row.strip_prefix("Input:") {
            input = Some(value.trim().to_owned());
            line = None;
        } else if let Some(value) = row.strip_prefix("Line:") {
            line = value.trim().parse::<u32>().ok().filter(|line| *line > 0);
        }
        if let (Some(file), Some(line)) = (&input, line)
            && !file.is_empty()
        {
            result = Some(SourceLocation {
                file: file.clone(),
                line,
                byte_column: 0,
                column: 1,
                column_char: 1,
                precise: false,
            });
        }
    }
    result
}
/// TeX comments start at an unescaped percent sign.
fn source_line_text(text: &str) -> &str {
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'%' {
            return &text[..index];
        }
    }
    text
}

/// Beamer attributes frame bodies to the frame's last source line: forward
/// search refines against the closing line, inverse search against the range.
fn frame_bounds(source: &str, anchor: usize) -> Option<(usize, usize)> {
    let mut start = None;
    for (index, text) in source.lines().enumerate() {
        let mut text = source_line_text(text);
        while let Some((_, rest)) = text.split_once('\\') {
            text = rest;
            if let Some(rest) = text.strip_prefix('\\') {
                text = rest;
                continue;
            }
            if let Some(rest) = text
                .strip_prefix("begin")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                text = rest;
                // Nested frames are not a reliable source boundary.
                if start.replace(index).is_some() {
                    return None;
                }
            } else if let Some(rest) = text
                .strip_prefix("end")
                .and_then(|rest| rest.trim_start().strip_prefix("{frame}"))
            {
                text = rest;
                if let Some(begin) = start.take() {
                    if begin <= anchor && anchor <= index {
                        return Some((begin, index));
                    }
                } else if anchor <= index {
                    return Some((index, index));
                }
            }
        }
        if index >= anchor && start.is_none() {
            return None;
        }
    }
    None
}

/// Find the literal frame whose body closes at or after the one-based
/// SyncTeX line. Beamer forwards map interior lines to the closing line.
pub(crate) fn frame_closing_line(source: &str, line: u32) -> Option<u32> {
    frame_bounds(source, line.checked_sub(1)? as usize).map(|(_, end)| end as u32 + 1)
}

/// Find a literal frame environment enclosing the one-based SyncTeX line.
fn source_frame_range(source: &str, line: u32) -> Option<std::ops::Range<usize>> {
    frame_bounds(source, line.checked_sub(1)? as usize).map(|(start, end)| start..end + 1)
}

pub(crate) fn words(text: &str) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let mut start = None;
    for (index, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if ch.is_alphanumeric() || start.is_some() && is_combining_mark(ch) {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            result.push((start, &text[start..index]));
        }
    }
    result
}

fn normalized_word(word: &str) -> String {
    word.nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .collect::<String>()
        .to_lowercase()
        .replace('ﬁ', "fi")
        .replace('ﬂ', "fl")
        .replace('ﬀ', "ff")
        .replace('ﬃ', "ffi")
        .replace('ﬄ', "ffl")
}

fn ordinary_word_source_start(
    source: &str,
    context: &str,
    offset: usize,
    line: u32,
    byte: usize,
) -> Option<usize> {
    let Some((_, pdf_word)) = words(context)
        .into_iter()
        .find(|(start, word)| *start <= offset && offset < start + word.len())
    else {
        return Some(byte);
    };
    let pdf_word = normalized_word(pdf_word);
    if pdf_word.len() < 2 || !pdf_word.bytes().all(|ch| ch.is_ascii_alphabetic()) {
        return Some(byte);
    }
    source
        .lines()
        .nth(line as usize - 1)
        .and_then(|text| {
            words(text)
                .into_iter()
                .find(|(start, word)| *start <= byte && byte < start + word.len())
        })
        .and_then(|(start, word)| (normalized_word(word) == pdf_word).then_some(start))
}

/// Refine prose and mathematical atoms without expanding arbitrary TeX macros.
fn source_word_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    radius: u32,
) -> Option<(u32, usize)> {
    let scope = math::caption_lines(source, line).or_else(|| source_frame_range(source, line));
    let within_scope = scope.is_some();
    let lines = scope.unwrap_or_else(|| {
        line.saturating_sub(radius.saturating_add(1)) as usize
            ..(line as usize).saturating_add(radius as usize)
    });
    source_word_location_in_range(source, line, context, offset, lines, within_scope)
}

fn source_word_location_in_range(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_scope: bool,
) -> Option<(u32, usize)> {
    let hidden = nonprinting_source_ranges(source);
    let prose = source_prose_scored_location(
        source,
        line,
        context,
        offset,
        lines.clone(),
        within_scope,
        &hidden,
    );
    math::source_location(source, line, lines, within_scope, context, offset, prose).and_then(
        |(row, byte)| {
            if !hidden.is_empty() {
                let absolute = source
                    .split_inclusive('\n')
                    .take(row as usize - 1)
                    .map(str::len)
                    .sum::<usize>()
                    + byte;
                if hidden.iter().any(|range| range.contains(&absolute)) {
                    return None;
                }
            }
            ordinary_word_source_start(source, context, offset, row, byte).map(|start| (row, start))
        },
    )
}

fn skip_source_space(source: &str, mut at: usize) -> usize {
    loop {
        at += source[at..].len() - source[at..].trim_start().len();
        if source.as_bytes().get(at) != Some(&b'%') {
            return at;
        }
        at += source[at..].find('\n').unwrap_or(source.len() - at);
    }
}

fn optional_argument_end(source: &str, start: usize) -> Option<usize> {
    let start = skip_source_space(source, start);
    if source.as_bytes().get(start) != Some(&b'[') {
        return None;
    }
    let mut depth = 1;
    let mut at = start + 1;
    while at < source.len() {
        match source.as_bytes()[at] {
            b'\\' => {
                at += 1;
                at += source[at..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            b'%' => {
                at += source[at..].find('\n').unwrap_or(source.len() - at);
                continue;
            }
            b'{' => {
                at = math::group(source, at)?.1;
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
        at += source[at..].chars().next()?.len_utf8();
    }
    None
}

/// Only known nonprinting arguments are masked. This is not macro expansion;
/// ordinary formatting/caption arguments and href display text remain literal.
fn nonprinting_source_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while at < source.len() {
        if source.as_bytes()[at] == b'%' {
            at += source[at..].find('\n').unwrap_or(source.len() - at);
        } else if source.as_bytes()[at] == b'\\' {
            let start = at;
            at += 1;
            let end = at
                + source[at..]
                    .bytes()
                    .take_while(|ch| ch.is_ascii_alphabetic() || *ch == b'@')
                    .count();
            if end == at {
                at += source[at..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            let command = &source[at..end];
            at = end + usize::from(source.as_bytes().get(end) == Some(&b'*'));
            let definition = matches!(
                command,
                "newcommand"
                    | "renewcommand"
                    | "providecommand"
                    | "DeclareRobustCommand"
                    | "NewDocumentCommand"
                    | "RenewDocumentCommand"
                    | "ProvideDocumentCommand"
                    | "DeclareDocumentCommand"
                    | "newenvironment"
                    | "renewenvironment"
                    | "def"
                    | "gdef"
                    | "edef"
                    | "xdef"
            );
            if definition {
                if matches!(command, "def" | "gdef" | "edef" | "xdef") {
                    while at < source.len() && source.as_bytes()[at] != b'{' {
                        if source.as_bytes()[at] == b'\\' {
                            at += 1;
                            at += source[at..].chars().next().map_or(0, char::len_utf8);
                        } else if source.as_bytes()[at] == b'%' {
                            at = skip_source_space(source, at);
                        } else {
                            at += source[at..].chars().next().unwrap().len_utf8();
                        }
                    }
                } else {
                    at = skip_source_space(source, at);
                    if let Some((_, end)) = math::group(source, at) {
                        at = end;
                    } else if source.as_bytes().get(at) == Some(&b'\\') {
                        at += 1;
                        at += source[at..]
                            .bytes()
                            .take_while(|ch| ch.is_ascii_alphabetic() || *ch == b'@')
                            .count();
                    }
                    while let Some(end) = optional_argument_end(source, at) {
                        at = end;
                    }
                    if command.contains("Document") {
                        let Some((_, end)) = math::group(source, skip_source_space(source, at))
                        else {
                            continue;
                        };
                        at = end;
                    }
                }
                let Some((_, end)) = math::group(source, skip_source_space(source, at)) else {
                    ranges.push(start..source.len());
                    break;
                };
                at = end;
                if command.ends_with("environment")
                    && let Some((_, end)) = math::group(source, skip_source_space(source, at))
                {
                    at = end;
                }
                ranges.push(start..at);
            } else if matches!(
                command,
                "label"
                    | "ref"
                    | "eqref"
                    | "pageref"
                    | "autoref"
                    | "cref"
                    | "Cref"
                    | "cite"
                    | "citep"
                    | "citet"
                    | "parencite"
                    | "textcite"
                    | "autocite"
                    | "footcite"
                    | "nocite"
                    | "href"
                    | "includegraphics"
                    | "input"
                    | "include"
                    | "bibliography"
                    | "addbibresource"
            ) {
                while let Some(end) = optional_argument_end(source, at) {
                    at = end;
                }
                if let Some((body, end)) = math::group(source, skip_source_space(source, at)) {
                    ranges.push(if command == "includegraphics" {
                        start..end
                    } else {
                        body
                    });
                    at = end;
                }
            }
        } else {
            at += source[at..].chars().next().unwrap().len_utf8();
        }
    }
    ranges
}

fn source_prose_scored_location(
    source: &str,
    line: u32,
    context: &str,
    offset: usize,
    lines: std::ops::Range<usize>,
    within_frame: bool,
    hidden: &[std::ops::Range<usize>],
) -> Option<(u32, usize, isize)> {
    let pdf = words(context);
    let selected = pdf
        .iter()
        .position(|(start, word)| *start <= offset && offset < start + word.len())?;
    let pdf: Vec<_> = pdf.iter().map(|(_, word)| normalized_word(word)).collect();
    let frame_header = within_frame
        && source
            .lines()
            .nth(lines.start)
            .is_some_and(|row| source_line_text(row).contains("\\begin{frame}"));
    let mut candidates = Vec::new();
    let mut line_start = 0;
    for (index, raw_row) in source.split_inclusive('\n').enumerate().take(lines.end) {
        let text = source_line_text(raw_row.trim_end_matches(['\r', '\n']));
        if index >= lines.start {
            for (byte, word) in words(text) {
                if (byte == 0 || !text[..byte].ends_with('\\'))
                    && !hidden
                        .iter()
                        .any(|range| range.contains(&(line_start + byte)))
                {
                    candidates.push((index as u32 + 1, byte, normalized_word(word)));
                }
            }
        }
        line_start += raw_row.len();
    }
    let mut best = None;
    let mut tied = false;
    for (index, (row, byte, word)) in candidates.iter().enumerate() {
        if *word != pdf[selected] {
            continue;
        }
        let mut score = 0;
        for distance in 1..=3 {
            for direction in [-1isize, 1] {
                let delta = distance * direction;
                if let (Some(a), Some(b)) = (
                    selected.checked_add_signed(delta).and_then(|i| pdf.get(i)),
                    index
                        .checked_add_signed(delta)
                        .and_then(|i| candidates.get(i)),
                ) && *a == b.2
                {
                    score += 4 - distance;
                }
            }
        }
        // TeX math can insert source tokens that PDF text extraction omits.
        // Use nearby longer words as a tie break for repeated prose.
        let mut context_bonus = 0;
        for direction in [-1isize, 1] {
            for distance in 1..=8 {
                let Some(neighbor) = selected
                    .checked_add_signed(direction * distance)
                    .and_then(|i| pdf.get(i))
                else {
                    break;
                };
                if neighbor.chars().count() < 4 {
                    continue;
                }
                if (1..=10).any(|step| {
                    index
                        .checked_add_signed(direction * step)
                        .and_then(|i| candidates.get(i))
                        .is_some_and(|candidate| candidate.2 == *neighbor)
                }) {
                    context_bonus += 1;
                }
            }
        }
        // A collected frame/caption boundary does not favor its last occurrence.
        let proximity = if within_frame { 0 } else { row.abs_diff(line) };
        let rank = (
            score,
            frame_header && *row as usize == lines.start + 1,
            context_bonus,
            std::cmp::Reverse(proximity),
        );
        match best {
            Some((old, _, _)) if rank < old => {}
            Some((old, _, _)) if rank == old => tied = true,
            _ => {
                best = Some((rank, *row, *byte));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied)
        .map(|(rank, row, byte)| (row, byte, rank.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_revision_rejects_rewrites_and_atomic_replacements() {
        let root = tempfile::tempdir().unwrap();
        let pdf = root.path().join("paper.pdf");
        fs::write(&pdf, b"first").unwrap();
        let first = PdfRevision::read(&pdf).unwrap();
        first.check(&pdf).unwrap();
        fs::write(&pdf, b"second").unwrap();
        assert!(first.check(&pdf).is_err());
        let second = PdfRevision::read(&pdf).unwrap();
        let replacement = root.path().join("replacement.pdf");
        fs::write(&replacement, b"second").unwrap();
        fs::rename(replacement, &pdf).unwrap();
        assert!(second.check(&pdf).is_err());
    }
    #[test]
    fn inverse_word_matching_resolves_context_and_utf8_byte_columns() {
        let source = "A repeated word far away.\nÉlie uses \\emph{repeated} maps near fibers.\nRepeated noise.\n";
        let context = "Élie uses repeated maps near ﬁbers.";
        assert_eq!(
            super::source_word_location(source, 2, context, context.find("repeated").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("repeated").unwrap()))
        );
        assert_eq!(
            super::source_word_location(source, 3, context, context.find("ﬁbers").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("fibers").unwrap()))
        );
        assert_eq!(
            super::source_word_location("word word", 1, "word", 1, 4),
            None
        );
        assert_eq!(
            super::source_word_location("\\word % word", 1, "word", 1, 4),
            None
        );
        assert_eq!(
            super::source_word_location("word", 1, "missing", 1, 4),
            None
        );
        let boundary = "one\ntwo\nthree\nfour\nfive\nsix";
        assert_eq!(
            super::source_word_location(boundary, 1, "five", 1, 4),
            Some((5, 0))
        );
        assert_eq!(super::source_word_location(boundary, 1, "six", 1, 4), None);
    }

    #[test]
    fn inverse_frame_matching_reaches_prose_without_crossing_frames() {
        let source = "\\begin{frame}\nSome datasets convey geometry.\n\\pause\n\
                      \\includegraphics{datasets/image.pdf}\n\n\n\n\n\
                      \\only<2>{Other visible text.}\n\\end{frame}\n\
                      \\begin{frame}\nSome datasets convey geometry.\n\\end{frame}";
        assert_eq!(
            source_word_location(source, 10, "Some datasets convey geometry.", 5, 4),
            Some((2, 5))
        );
        assert_eq!(
            source_word_location(source, 13, "Some datasets convey geometry.", 5, 4),
            Some((12, 5))
        );
        assert_eq!(
            source_word_location(source, 10, "datasets", 1, 4),
            Some((2, 5))
        );
        assert_eq!(source_word_location(source, 13, "Other", 1, 4), None);
    }

    #[test]
    fn inverse_frame_matching_keeps_duplicate_overlays_line_only() {
        let source = "\\begin{frame}\n\\only<1>{Identical visible phrase.}\n\
                      \\only<2>{Identical visible phrase.}\n\\end{frame}";
        assert_eq!(
            source_word_location(source, 4, "Identical visible phrase.", 10, 4),
            None
        );
    }

    #[test]
    fn inverse_title_word_survives_unrelated_opaque_math() {
        let source = "\\begin{frame}[fragile]{Let's revisit this example}\n\
                      Ordinary body words:\n\
                      \\begin{equation*}\n\
                      \\custom{x}\n\
                      \\end{equation*}\n\
                      \\end{frame}";
        let context = "Let’s revisit this example\r\nOrdinary body words:";
        assert_eq!(
            source_prose_scored_location(
                source,
                6,
                context,
                0,
                0..6,
                true,
                &nonprinting_source_ranges(source)
            )
            .map(|(row, byte, _)| (row, byte)),
            Some((1, 23))
        );
        assert_eq!(
            source_word_location(source, 6, context, 0, 4),
            Some((1, 23))
        );
    }

    #[test]
    fn inverse_title_words_prefer_the_frame_header_when_body_repeats_them() {
        let source = "\\begin{frame}{Overview}\n\
                      An overview follows in the body.\n\
                      $\\custom{x}$\n\
                      \\end{frame}";
        assert_eq!(
            source_word_location(source, 4, "Overview\r\n0.0 0.2", 6, 4),
            Some((1, 14))
        );
        assert_eq!(
            source_word_location(source, 4, "An overview follows", 3, 4),
            Some((2, 3))
        );
        let source = "\\begin{frame}{Primary Topic}{A small example}\n\
                      A different body sentence.\n\
                      $\\custom{x}$\n\
                      \\end{frame}";
        let context = "Primary Topic\r\nA small example\r\nTheorem";
        assert_eq!(
            source_word_location(source, 4, context, context.find("A small").unwrap(), 4),
            Some((
                1,
                source.lines().next().unwrap().find("{A small").unwrap() + 1
            ))
        );
    }

    #[test]
    fn inverse_heading_matches_tex_accent_and_pdf_unicode() {
        let source = "\\begin{frame}{An example from \\v Cech's construction}\n\
                      $x$\n\
                      \\end{frame}";
        let context = "An example from Čech’s construction";
        assert_eq!(
            source_word_location(source, 3, context, context.find('Č').unwrap(), 4),
            Some((1, source.lines().next().unwrap().find("Cech").unwrap()))
        );
    }

    #[test]
    fn inverse_literal_words_inside_boxed_math_keep_their_source_spans() {
        let source = "\\begin{frame}{Example}\n\
                      \\[\\boxed{\\textnormal{Visible sentence inside the box.}}\\]\n\
                      \\[\\begin{aligned}N\\to\\infty\\end{aligned}\\]\n\
                      \\end{frame}";
        let context = "Example\r\nVisible sentence inside the box.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("Visible").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("Visible").unwrap()))
        );
    }

    #[test]
    fn inverse_prose_word_does_not_match_inside_longer_math_word() {
        let source = "\\begin{frame}{Example}\n\
                      Pick an option here.\n\
                      $\\mathrm{constant}(x)$ $\\custom{q}$\n\
                      \\end{frame}";
        let context = "Pick an option here. constant(x)";
        assert_eq!(
            source_word_location(source, 4, context, context.find("an option").unwrap(), 4),
            Some((2, 5))
        );
    }

    #[test]
    fn inverse_caption_word_ignores_image_filename() {
        let source = "\\begin{frame}{Example}\n\
                      \\includegraphics[width=\\textwidth]{assets/sample_green_blue_shapes--[Maker].jpg}\n\
                      \\caption{Sample green blue shapes, Maker.}\n\
                      \\end{frame}";
        let context = "Figure 1: Sample green blue shapes, Maker.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("green").unwrap(), 4),
            Some((3, source.lines().nth(2).unwrap().find("green").unwrap()))
        );
        assert_eq!(source_word_location(source, 4, "assets", 0, 4), None);
    }

    #[test]
    fn inverse_prose_excludes_nonprinting_keys_and_multiline_definitions() {
        let source = "\\newcommand{\\Hidden}{\nZephyr\n}\n\
                      \\Hidden\\label% keep key on the next line\n{Zephyr}\n\
                      \\cite[PrintedNote]{Zephyr}\\ref{Zephyr}\\href{Zephyr}{DisplayAmber}\n\
                      \\newcommand{\\Unused}{Zephyr} éé PrintedCopper\n";
        assert_eq!(source_word_location(source, 5, "Zephyr", 0, 10), None);
        for (word, row) in [
            ("PrintedNote", 6),
            ("DisplayAmber", 6),
            ("PrintedCopper", 7),
        ] {
            let byte = source.lines().nth(row - 1).unwrap().find(word).unwrap();
            assert_eq!(
                source_word_location(source, row as u32, word, 0, 10),
                Some((row as u32, byte))
            );
        }
        let source = "\\def\\Hidden#1{Zephyr {NestedKey}} PrintedCopper\n";
        for word in ["Zephyr", "NestedKey"] {
            assert_eq!(source_word_location(source, 1, word, 0, 4), None);
        }
        assert_eq!(
            source_word_location(source, 1, "PrintedCopper", 0, 4),
            Some((1, source.find("PrintedCopper").unwrap()))
        );
        let source = "\\newcommand{\\Hidden}{$Zephyr$}\n\\Hidden\\label{Zephyr}\n";
        assert_eq!(source_word_location(source, 2, "Zephyr", 0, 4), None);
    }

    #[test]
    fn inverse_repeated_prose_uses_words_past_adjacent_math() {
        let source = "\\begin{frame}{Example}\n\
                      \\item the \\blue{$X_i$}s are random variables in $\\mathcal{X}$, a space of inputs,\n\
                      \\item the \\blue{$Y_i$}s are random variables in $\\mathcal{Y}$, a space of labels,\n\
                      \\end{frame}";
        let context = "the Xis are random variables in X, a space of inputs,\r\n\
                       the Yis are random variables in Y, a space of labels,";
        for (needle, line) in [("inputs", 2), ("labels", 3)] {
            let at = context.find(needle).unwrap();
            let selected = context[..at].rfind("variables").unwrap();
            assert_eq!(
                source_word_location(source, 4, context, selected, 4),
                Some((
                    line,
                    source
                        .lines()
                        .nth(line as usize - 1)
                        .unwrap()
                        .find("variables")
                        .unwrap()
                ))
            );
        }
    }

    #[test]
    fn inverse_prose_context_beats_unrelated_math_word() {
        let source = "\\begin{frame}{Example}\n\
                      The map defines an \\emph{order} $(a_1, a_2)$ on a set.\n\
                      \\begin{equation*}\\mathrm{order}(f)=\\mathrm{order}(g)\\end{equation*}\n\
                      \\end{frame}";
        let context = "The map defines an order (a1, a2) on a set.";
        assert_eq!(
            source_word_location(source, 4, context, context.find("order").unwrap(), 4),
            Some((2, source.lines().nth(1).unwrap().find("order").unwrap()))
        );
    }

    #[test]
    fn inverse_repeated_word_uses_short_context_across_math() {
        let source = "\\begin{frame}{Example}\n\
                      It is smooth on $\\mathrm{cell}(f)$ as soon\n\
                      as $\\mathrm{t}(g)$ is smooth.\n\
                      \\end{frame}";
        let context = "It is smooth on cell(f) as soon\r\nas t(g) is smooth.";
        let selected = context.rfind("smooth").unwrap();
        assert_eq!(
            source_word_location(source, 4, context, selected, 4),
            Some((3, source.lines().nth(2).unwrap().find("smooth").unwrap()))
        );
    }

    #[test]
    fn inverse_ordinary_word_rejects_a_different_source_word() {
        let source = "The map is smooth and has a finite image.";
        let byte = source.find("smooth").unwrap();
        assert_eq!(
            ordinary_word_source_start(
                source,
                "The map is regular and has a finite image.",
                "The map is ".len(),
                1,
                byte,
            ),
            None
        );
        assert_eq!(
            ordinary_word_source_start(
                source,
                "The map is smooth and has a finite image.",
                "The map is ".len(),
                1,
                byte + 2,
            ),
            Some(byte)
        );
    }

    #[test]
    fn inverse_generated_verbatim_uses_clicked_sheet_original_frame() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("original.tex");
        let source = "\\begin {frame}\n[fragile]{First}\nEarlyZephyr.\nFirstCopper.\n\\end{frame}\n\
                      \\begin{frame}[fragile]{Second}\nLateNebula.\nSecondSilver.\n\\end{frame}\n";
        fs::write(&source_path, source).unwrap();
        let pdf = directory.path().join("build/deck.pdf");
        let generated = directory.path().join("build/deck.vrb");
        let contents = format!(
            "SyncTeX Version:1\nInput:1:{}\nInput:2:{}\n\
             Magnification:1000\nUnit:1\nX Offset:0\nY Offset:0\nContent:\n\
             {{1\n[1,5:0,0:1,1,0\nx2,2:0,0\n]\n}}1\n\
             Input:3:{}\n{{2\n[1,9:0,0:1,1,0\nx3,2:0,0\n]\n}}2\n\
             Postamble:\nCount:8\nPost scriptum:\n",
            source_path.display(),
            generated.display(),
            generated.display()
        );
        for (page, anchor) in [(1, 5), (2, 9)] {
            let map = source_map_from_reader(
                contents.as_bytes(),
                &pdf,
                InversePoint {
                    page,
                    x: 0.0,
                    y_from_top: 0.0,
                    page_height_pt: 1.0,
                },
                &generated,
                false,
                &Operation::default(),
            )
            .unwrap();
            let remapped = original_source_from_verbatim(&map).unwrap();
            assert_eq!(
                remapped.0,
                fs::canonicalize(&source_path).unwrap().to_str().unwrap()
            );
            assert_eq!(remapped.1, source);
            assert_eq!(remapped.2, anchor);
        }
        let missing = source_map_from_reader(
            contents.as_bytes(),
            &pdf,
            InversePoint {
                page: 1,
                x: 0.0,
                y_from_top: 0.0,
                page_height_pt: 1.0,
            },
            Path::new("/unrelated.vrb"),
            false,
            &Operation::default(),
        )
        .unwrap();
        assert!(original_source_from_verbatim(&missing).is_err());
    }

    #[test]
    fn inverse_body_ownership_uses_specific_boxes_and_effective_transform() {
        let sheet = "SyncTeX Version:1\nInput:1:/source.tex\nInput:2:/deck.vrb\n\
                     Magnification:1000\nUnit:65536\nX Offset:0\nY Offset:0\nContent:\n\
                     {2\n[1,18:0,100:100,100,0\n\
                     (1,18:0,100:100,100,0\n(1,18:0,100:100,100,0\n)\n)\n\
                     (1,18:10,20:30,10,0\nx2,2:20,20\n)\n\
                     (1,18:70,90:20,10,0\nx1,18:80,90\n)\n\
                     ]\n}2\nPostamble:\nCount:12\nPost scriptum:\n";
        for (has_post_scriptum, post, magnification, x_offset, y_offset) in [
            (true, "", 1.0, 0.0, 0.0),
            (false, "", 1.0, 0.0, 0.0),
            (
                true,
                "Magnification:2\nX Offset:10bp\nY Offset:20bp\n",
                2.0,
                10.0,
                20.0,
            ),
        ] {
            let header = if has_post_scriptum {
                sheet
            } else {
                sheet.strip_suffix("Post scriptum:\n").unwrap()
            };
            let contents = format!("{header}{post}");
            let compressed = contents
                .replacen(
                    "{2\n[1,18:0,100",
                    "{1\n[1,1:0,200:100,200,0\nx1,1:0,100\n]\n}1\n{2\n[1,18:0,=",
                    1,
                )
                .replace("(1,18:10,20:", "$1,18:0,20\n(1,18:10,=:");
            for (contents, x, y, body) in [
                (&contents, 20.0, 15.0, true),
                (&contents, 80.0, 85.0, false),
                (&contents, 50.0, 50.0, false),
                (&compressed, 20.0, 15.0, true),
            ] {
                let map = source_map_from_reader(
                    contents.as_bytes(),
                    Path::new("/deck.pdf"),
                    InversePoint {
                        page: 2,
                        x: (x * 65536.0 / 65781.76 * magnification + x_offset) as f32,
                        y_from_top: (y * 65536.0 / 65781.76 * magnification + y_offset) as f32,
                        page_height_pt: 200.0,
                    },
                    Path::new("/deck.vrb"),
                    true,
                    &Operation::default(),
                )
                .unwrap();
                assert_eq!(
                    map.generated_at_point, body,
                    "post={post:?}, point=({x},{y})"
                );
            }
        }
        for contents in [
            sheet.replace("Unit:65536\n", ""),
            sheet.replace("(1,18:10,20:30,10,0", "(1,18:invalid"),
            sheet.replace("Count:12\n", ""),
        ] {
            assert!(
                source_map_from_reader(
                    contents.as_bytes(),
                    Path::new("/deck.pdf"),
                    InversePoint {
                        page: 2,
                        x: 20.0,
                        y_from_top: 15.0,
                        page_height_pt: 200.0
                    },
                    Path::new("/deck.vrb"),
                    true,
                    &Operation::default(),
                )
                .is_err()
            );
        }
        let operation = Operation::default();
        let generated = SourceBox {
            bounds: [0.0, 0.0, 20.0, 20.0],
            generated: true,
        };
        for other in [
            SourceBox {
                bounds: generated.bounds,
                generated: false,
            },
            SourceBox {
                bounds: [10.0, 0.0, 30.0, 20.0],
                generated: false,
            },
        ] {
            assert!(!generated_box_at_point(&[generated, other], 15.0, 15.0, &operation).unwrap());
        }
    }

    #[test]
    fn inverse_document_metadata_resolves_title_and_direct_input() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let main = "\\input{preamble.tex}\n\
                    \\title{Example Report and Results}\n\
                    \\author{Ada Example}\n\
                    \\begin{document}\n\
                    \\begin{frame}{Other title}\n\
                    Body text.\n\
                    \\end{frame}\n";
        fs::write(&main_path, main).unwrap();
        fs::write(
            directory.path().join("preamble.tex"),
            "\\institute{North Research Center\\\\\nExample School for Mathematics}\n",
        )
        .unwrap();
        let found = document_metadata_word_location(
            &main_path,
            "Example Report and Results",
            "Example ".len(),
        )
        .unwrap();
        assert_eq!((found.2, found.3), (2, 15));
        assert!(found.4 >= 6);
        let found = document_metadata_word_location(
            &main_path,
            "North Research Center\r\nExample School for Mathematics",
            "North Research Center\r\nExample School for ".len(),
        )
        .unwrap();
        assert!(found.0.ends_with("preamble.tex"));
        assert_eq!((found.2, found.3), (2, 19));
        assert!(document_metadata_word_location(&main_path, "Report", 0).is_none());
    }

    #[test]
    fn inverse_metadata_preserves_crlf_and_unicode_byte_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let declaration = "\\newcommand{\\unused}{éééé}\\title{Example Report}";
        for newline in ["\n", "\r\n"] {
            let mut source = format!("\\documentclass{{beamer}}{newline}");
            for _ in 0..7 {
                source.push_str(&format!("% padding{newline}"));
            }
            source.push_str(declaration);
            fs::write(&main_path, &source).unwrap();
            let found = document_metadata_word_location(&main_path, "Example Report", 8).unwrap();
            assert_eq!((found.2, found.3), (9, declaration.find("Report").unwrap()));
        }
    }

    #[test]
    fn inverse_frame_title_beats_same_words_in_document_title() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let source = "\\title{Persistent Homology and Applications in Topological Data Analysis}\n\
                      \\begin{document}\n\
                      \\begin{frame}{Topological Data Analysis}{Cell cycle TDA application}\n\
                      \\begin{figure}\\includegraphics{cycle.png}\\end{figure}\n\
                      \\end{frame}\n";
        fs::write(&main_path, source).unwrap();
        let title = "Topological Data Analysis\nCell cycle TDA application\n\
                     Persistent Homology and Applications in Topological Data Analysis";
        let title_offset = title.find("Data").unwrap();
        let metadata_score = document_metadata_word_location(&main_path, title, title_offset)
            .unwrap()
            .4;
        assert!(metadata_score >= 6);
        assert_eq!(
            source_word_location(source, 5, title, title_offset, 4),
            Some((3, source.lines().nth(2).unwrap().find("Data").unwrap()))
        );
        assert!(!metadata_beats_frame(
            source,
            5,
            title,
            title_offset,
            10.0,
            272.0,
            metadata_score
        ));
        let footer = "Topological Data Analysis\nCell cycle TDA application\n\
                      Persistent Homology and Applications in Topological Data Analysis";
        let footer_offset = footer.rfind("Data").unwrap();
        let footer_score = document_metadata_word_location(&main_path, footer, footer_offset)
            .unwrap()
            .4;
        assert!(metadata_beats_frame(
            source,
            5,
            footer,
            footer_offset,
            228.0,
            272.0,
            footer_score
        ));
    }

    #[test]
    fn inverse_identical_running_title_uses_click_height_for_tie() {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("original.tex");
        let source = "\\title{Topological Data Analysis}\n\
                      \\begin{frame}{Topological Data Analysis}\n\
                      \\end{frame}\n";
        fs::write(&main_path, source).unwrap();
        let context = "Topological Data Analysis";
        let offset = context.find("Data").unwrap();
        let metadata_score = document_metadata_word_location(&main_path, context, offset)
            .unwrap()
            .4;
        assert_eq!(
            metadata_score,
            source_prose_scored_location(
                source,
                3,
                context,
                offset,
                1..3,
                true,
                &nonprinting_source_ranges(source)
            )
            .unwrap()
            .2
        );
        assert!(!metadata_beats_frame(
            source,
            3,
            context,
            offset,
            10.0,
            272.0,
            metadata_score
        ));
        assert!(metadata_beats_frame(
            source,
            3,
            context,
            offset,
            228.0,
            272.0,
            metadata_score
        ));
    }

    #[test]
    fn inverse_frame_matching_ignores_commented_and_escaped_boundaries() {
        let source = "% \\begin{frame}\n\\begin {frame}\n\
                      % \\end{frame}\n\\\\end{frame}\n\
                      Before \\% percent target. % hidden\n\n\n\n\n\\end {frame}";
        assert_eq!(
            source_word_location(source, 10, "percent target", 9, 0),
            Some((5, 18))
        );
        assert_eq!(source_word_location(source, 10, "hidden", 1, 4), None);
        let unterminated = "\\begin{frame}\ntarget\n\n\n\n\n";
        assert_eq!(source_word_location(unterminated, 6, "target", 1, 0), None);
    }

    #[test]
    fn inverse_caption_math_uses_literal_context_not_closing_line_proximity() {
        let source = "\\caption{Selected views.\n\
            cloud vertices at exact quotient distance at most \\(\\rho_q\\) from the\n\
            exact reference segment.\n\n\n\n\n\
            The transverse coordinate is in units of \\(\\rho_q\\), hence magnified.\n\
            End of caption.}\n";
        let line = source.lines().count() as u32;
        for (context, row) in [
            ("distance at most 𝜌𝑞 from the exact reference segment.", 2),
            ("coordinate is in units of 𝜌𝑞, hence magnified.", 8),
        ] {
            assert_eq!(
                source_word_location(source, line, context, context.find('𝜌').unwrap(), 4),
                Some((
                    row,
                    source
                        .lines()
                        .nth(row as usize - 1)
                        .unwrap()
                        .find("\\rho")
                        .unwrap()
                ))
            );
        }
        // The glyph alone cannot identify which repeated expression was clicked.
        assert_eq!(source_word_location(source, line, "𝜌𝑞", 0, 4), None);
    }

    #[test]
    fn inverse_inline_math_does_not_use_hidden_comment_context() {
        let source = "\\caption{\\emph{at most} \\(\\rho_q\\)\n\
                      % at most\n\\(\\rho_q\\)\n}";
        assert_eq!(
            source_word_location(source, 4, "at most 𝜌𝑞", "at most ".len(), 4),
            None
        );
    }

    #[test]
    fn parse_synctex_edit_finds_file_and_line() {
        let stdout = "Output: paper.pdf\nInput: /private/tmp/synctex-test/./paper.tex\nLine: 3\nColumn: -1\n";
        let target = parse_synctex_edit(stdout).expect("synctex match");
        assert_eq!(target.file, "/private/tmp/synctex-test/./paper.tex");
        assert_eq!(target.line, 3);
    }

    #[test]
    fn parse_synctex_edit_takes_last_input() {
        let stdout = "Input: preamble.tex\nLine: 9\nInput: paper.tex\nLine: 3\n";
        let target = parse_synctex_edit(stdout).expect("synctex match");
        assert_eq!(target.file, "paper.tex");
        assert_eq!(target.line, 3);
    }

    #[test]
    fn parse_synctex_edit_rejects_missing_input() {
        assert!(parse_synctex_edit("Output: paper.pdf\nLine: 3\n").is_none());
        assert!(parse_synctex_edit("Output: paper.pdf\nInput: paper.tex\n").is_none());
        assert!(parse_synctex_edit("").is_none());
    }

    #[test]
    fn forward_word_uses_scalar_columns_and_rejects_commands_and_comments() {
        let line = "école \\emph{naïve} word % hidden";
        let hint = ForwardWord::at(line, 13).unwrap();
        assert_eq!(hint.words[hint.selected], "naïve");
        for column in [0, 6, 8, 24, 26, u32::MAX] {
            assert!(ForwardWord::at(line, column).is_none(), "column {column}");
        }
        let mut hint = hint;
        hint.selected = hint.words.len();
        assert!(!hint.valid());
        let decomposed = "e\u{301}cole";
        for column in 1..=decomposed.chars().count() as u32 {
            let hint = ForwardWord::at(decomposed, column).unwrap();
            assert_eq!(hint.words[hint.selected], decomposed);
            assert!(hint.valid());
        }
    }

    #[test]
    fn literal_forward_word_uses_utf8_boundaries_and_unicode_context() {
        let revision = PdfRevision::read(Path::new(file!())).unwrap();
        let request = |text: &str, byte_column: usize| {
            parse_forward_request(
                &serde_json::json!({
                    "pdf": "/literal.pdf", "revision": revision, "page": 1,
                    "h": 72, "v": 120, "width": 0, "height": 0,
                    "word": { "text": text, "byte_column": byte_column }
                })
                .to_string(),
            )
        };
        let text = "one two three four café λ e\u{301}cole 漢字 nine ten";
        let selected = text.find("e\u{301}cole").unwrap();
        for byte in [selected, selected + 1] {
            let hint = request(text, byte).unwrap().word.unwrap();
            assert_eq!(
                hint.words,
                ["four", "café", "λ", "e\u{301}cole", "漢字", "nine", "ten"]
            );
            assert_eq!(hint.selected, 3);
        }
        assert_eq!(request("λ first", 0).unwrap().word.unwrap().words[0], "λ");
        assert_eq!(
            request("100% literal", 5).unwrap().word.unwrap().words[1],
            "literal"
        );
        assert!(request("λ first", 1).is_err());
        assert!(request(text, text.len() + 1).is_err());
        for byte in [3, text.len()] {
            assert!(request(text, byte).unwrap().word.is_none());
        }
        assert!(request(&"x".repeat(129), 0).unwrap().word.is_none());
    }

    #[test]
    fn forward_geometry_preserves_top_down_coordinates_and_rejects_overflow() {
        let revision = PdfRevision::read(Path::new(file!())).unwrap();
        let payload = format!(
            r#"{{"pdf":"/a:b.pdf","revision":{},"page":2,"h":72,"v":120,"width":250,"height":12}}"#,
            serde_json::to_string(&revision).unwrap()
        );
        let request = parse_forward_request(&payload).unwrap();
        assert_eq!(
            request.rect(),
            SearchRect {
                left: 72.0,
                top: 108.0,
                right: 322.0,
                bottom: 120.0
            }
        );
        assert_eq!(request.pdf, Path::new("/a:b.pdf"));
        let mut request = request;
        request.h = f32::MAX;
        request.width = f32::MAX;
        assert!(request.validate().is_err());
        for invalid in [
            payload.replace("\"page\":2", "\"page\":0"),
            payload.replace("\"/a:b.pdf\"", "\"relative.pdf\""),
            payload.replace("\"height\":12", "\"height\":-12"),
            payload.replace("\"h\":72", "\"h\":1e100"),
            payload.replace("\"h\":72", "\"h\":72,\"h\":73"),
            payload.replace("\"h\":72", "\"h\":72,\"unexpected\":0"),
            payload.replace("\"h\":72", r#""h":72,"word":{"words":[],"selected":0}"#),
            payload.replace(
                "\"h\":72",
                r#""h":72,"word":{"words":["text"],"selected":1}"#,
            ),
            payload.replace(
                "\"h\":72",
                r#""h":72,"word":{"words":["two words"],"selected":0}"#,
            ),
        ] {
            assert!(parse_forward_request(&invalid).is_err(), "{invalid}");
        }
    }
}

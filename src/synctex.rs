use std::io;
use std::path::Path;

use synctex_parser::resolve_forward_with_selector;
pub use synctex_parser::{
    DocumentRevision, ForwardCandidate, ForwardRequest, ForwardSource, ForwardWord, InversePoint,
    InverseResolution, MAX_WARNING_BYTES, PdfRevision, SourceLocation, SyncTeXCache,
    is_forward_unavailable, is_spacing_diacritic, parse_forward_request, resolve_inverse,
    resolve_inverse_with_cache, set_build_directory, word_index_at, words,
};

#[cfg(test)]
pub fn resolve_forward(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
) -> io::Result<ForwardRequest> {
    resolve_forward_with_library(pdf, file, line, column, None, ForwardSource::default())
}

pub fn resolve_forward_with_library(
    pdf: &Path,
    file: &Path,
    line: u32,
    column: u32,
    library: Option<&Path>,
    context: ForwardSource<'_>,
) -> io::Result<ForwardRequest> {
    resolve_forward_with_selector(
        pdf,
        file,
        line,
        column,
        context,
        |pdf, candidates, hint, anchor| {
            crate::pdf::select_forward_line_candidate(pdf, candidates, hint, anchor, library)
        },
    )
}

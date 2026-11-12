use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

const MAX_RECURSIVE_DEPTH: usize = 32;
const MAX_RECURSIVE_PDFS: usize = 10_000;
const MAX_RECURSIVE_VISITED: usize = 50_000;

#[derive(Debug)]
pub struct BrowserEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_recent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserEntrySource {
    Recent,
    Here,
    Subdirectory,
}

impl BrowserEntrySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recent => "RECENT",
            Self::Here => "HERE",
            Self::Subdirectory => "SUBDIR",
        }
    }
}

/// Fuzzy-matching scratch state reused across draws. `Matcher` owns a large
/// scratch buffer, so allocating it per highlighted row is wasteful; the
/// per-term patterns are reparsed only when the filter text changes.
struct FuzzyCache {
    matcher: Matcher,
    buffer: Vec<char>,
    filter: String,
    term_patterns: Vec<Pattern>,
    indices: Vec<u32>,
}

impl FuzzyCache {
    fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buffer: Vec::new(),
            filter: String::new(),
            term_patterns: Vec::new(),
            indices: Vec::new(),
        }
    }

    fn sync_terms(&mut self, filter: &str) {
        if self.filter == filter {
            return;
        }
        self.filter.clear();
        self.filter.push_str(filter);
        self.term_patterns = filter
            .split_whitespace()
            .map(|term| Pattern::parse(term, CaseMatching::Ignore, Normalization::Smart))
            .collect();
    }
}

pub struct BrowserState {
    pub current_dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub filter: String,
    pub filtered_indices: Vec<usize>,
    root_dir: PathBuf,
    recents: Vec<PathBuf>,
    recursive_entries: Vec<BrowserEntry>,
    recursive_loaded: bool,
    recursive_rx: Option<Receiver<Vec<BrowserEntry>>>,
    recursive_cancel: Arc<AtomicBool>,
    fuzzy: RefCell<FuzzyCache>,
}

impl BrowserState {
    #[cfg(test)]
    pub fn new(dir: PathBuf) -> Self {
        Self::with_recents(dir, Vec::new())
    }

    pub fn with_recents(dir: PathBuf, recents: Vec<PathBuf>) -> Self {
        let mut state = Self {
            current_dir: dir.clone(),
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            filter: String::new(),
            filtered_indices: Vec::new(),
            root_dir: dir,
            recents,
            recursive_entries: Vec::new(),
            recursive_loaded: false,
            recursive_rx: None,
            recursive_cancel: Arc::new(AtomicBool::new(false)),
            fuzzy: RefCell::new(FuzzyCache::new()),
        };
        state.load_dir();
        state
    }

    /// Supplies the recently opened documents, which are listed first in the
    /// starting directory and remain available to recursive filtering.
    #[cfg(test)]
    pub fn set_recents(&mut self, recents: Vec<PathBuf>) {
        self.recents = recents;
        self.load_dir();
    }

    pub fn load_dir(&mut self) {
        self.recursive_cancel.store(true, Ordering::Relaxed);
        self.entries.clear();
        self.filter.clear();
        self.recursive_loaded = false;
        self.recursive_rx = None;

        if let Some(parent) = self.current_dir.parent() {
            self.entries.push(BrowserEntry {
                name: "../".into(),
                path: parent.to_path_buf(),
                is_dir: true,
                is_recent: false,
            });
        }

        let Ok(read_dir) = std::fs::read_dir(&self.current_dir) else {
            self.selected = 0;
            self.scroll_offset = 0;
            self.rebuild_filter();
            return;
        };
        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                directories.push(BrowserEntry {
                    name: format!("{name}/"),
                    path: entry.path(),
                    is_dir: true,
                    is_recent: false,
                });
            } else if is_pdf(&entry.path()) {
                files.push(BrowserEntry {
                    name,
                    path: entry.path(),
                    is_dir: false,
                    is_recent: false,
                });
            }
        }
        directories.sort_by(|left, right| left.name.cmp(&right.name));
        files.sort_by(|left, right| left.name.cmp(&right.name));
        self.entries.extend(directories);
        self.entries.extend(files);
        self.prepend_recents();
        self.selected = 0;
        self.scroll_offset = 0;
        self.rebuild_filter();
    }

    /// Inserts recent documents that still exist at the top of the listing, but
    /// only in the directory the picker started in and only when they are not
    /// already shown there.
    fn prepend_recents(&mut self) {
        if self.current_dir != self.root_dir || self.recents.is_empty() {
            return;
        }
        let insert_at = usize::from(
            self.entries
                .first()
                .is_some_and(|entry| entry.name == "../"),
        );
        let mut recent_entries = Vec::new();
        for path in &self.recents {
            if !path.is_file() || !is_pdf(path) {
                continue;
            }
            if let Some(position) = self.entries.iter().position(|entry| &entry.path == path) {
                let mut entry = self.entries.remove(position);
                entry.is_recent = true;
                recent_entries.push(entry);
                continue;
            }
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            recent_entries.push(BrowserEntry {
                name: file_name,
                path: path.clone(),
                is_dir: false,
                is_recent: true,
            });
        }
        for (offset, entry) in recent_entries.into_iter().enumerate() {
            self.entries.insert(insert_at + offset, entry);
        }
    }

    pub fn preload_recursive(&mut self) {
        if self.recursive_loaded || self.recursive_rx.is_some() {
            return;
        }
        self.recursive_cancel.store(true, Ordering::Relaxed);
        self.recursive_cancel = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&self.recursive_cancel);
        let directory = self.current_dir.clone();
        let root_dir = self.root_dir.clone();
        let recents = self.recents.clone();
        let (sender, receiver) = mpsc::channel();
        self.recursive_rx = Some(receiver);
        thread::spawn(move || {
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            let canonical_root_dir =
                std::fs::canonicalize(&root_dir).unwrap_or_else(|_| root_dir.clone());
            let canonical_dir = if directory == root_dir {
                canonical_root_dir.clone()
            } else {
                std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone())
            };
            let root = find_git_root(&directory)
                .filter(|git_root| {
                    git_root.starts_with(&root_dir) || git_root.starts_with(&canonical_root_dir)
                })
                .unwrap_or_else(|| directory.clone());
            let canonical_scan_root = if root == directory {
                canonical_dir.clone()
            } else {
                std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone())
            };
            let mut canonical_recents = Vec::new();
            for path in recents.into_iter().take(crate::recent::MAX_RECENTS) {
                if cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                canonical_recents.push((path, canonical));
            }
            let recent_set: HashSet<&Path> = canonical_recents
                .iter()
                .map(|(_, canonical)| canonical.as_path())
                .collect();
            let mut seen = HashSet::new();
            let mut entries = Vec::new();
            if let Ok(read_dir) = std::fs::read_dir(&directory) {
                for entry in read_dir
                    .flatten()
                    .take_while(|_| !cancelled.load(Ordering::Relaxed))
                    .filter(|entry| {
                        !entry.file_name().to_string_lossy().starts_with('.')
                            && entry.file_type().is_ok_and(|ft| !ft.is_dir())
                            && is_pdf(&entry.path())
                    })
                    .take(MAX_RECURSIVE_PDFS)
                {
                    let file_name = entry.file_name();
                    let key = canonical_dir.join(&file_name);
                    let is_recent = recent_set.contains(key.as_path());
                    if seen.insert(key) {
                        entries.push(BrowserEntry {
                            name: file_name.to_string_lossy().into_owned(),
                            path: entry.path(),
                            is_dir: false,
                            is_recent,
                        });
                    }
                }
            }
            for (name, path) in collect_pdf_files(&root, &cancelled) {
                let key = path
                    .strip_prefix(&root)
                    .map(|rel| canonical_scan_root.join(rel))
                    .unwrap_or_else(|_| path.clone());
                let is_recent = recent_set.contains(key.as_path());
                if seen.insert(key) {
                    entries.push(BrowserEntry {
                        name,
                        path,
                        is_dir: false,
                        is_recent,
                    });
                }
            }
            for (path, key) in canonical_recents {
                if cancelled.load(Ordering::Relaxed) {
                    return;
                }
                if !path.is_file() || !is_pdf(&path) || !seen.insert(key) {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                entries.push(BrowserEntry {
                    name,
                    path,
                    is_dir: false,
                    is_recent: true,
                });
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            if !cancelled.load(Ordering::Relaxed) {
                let _ = sender.send(entries);
            }
        });
    }

    pub fn poll_recursive(&mut self) -> bool {
        let Some(receiver) = self.recursive_rx.as_ref() else {
            return false;
        };
        let Ok(entries) = receiver.try_recv() else {
            return false;
        };
        let selected_path = self.selected_entry().map(|entry| entry.path.clone());
        self.recursive_entries = entries;
        self.recursive_loaded = true;
        self.recursive_rx = None;
        if !self.filter.is_empty() {
            self.rebuild_filter();
            self.restore_selection(selected_path.as_deref());
        }
        true
    }

    pub fn recursive_loading(&self) -> bool {
        self.recursive_rx.is_some()
    }

    pub fn rebuild_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
            return;
        }

        let pattern = Pattern::parse(&self.filter, CaseMatching::Ignore, Normalization::Smart);
        let query = self.filter.trim().to_lowercase();
        let current_dir = self.current_dir.clone();
        let canonical_dir = std::fs::canonicalize(&current_dir).ok();
        let filtered_indices = {
            let source = self.active_source();
            let mut fuzzy = self.fuzzy.borrow_mut();
            let FuzzyCache {
                matcher, buffer, ..
            } = &mut *fuzzy;
            let mut scored: Vec<_> = source
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    if entry.name == "../" {
                        return None;
                    }
                    let basename = entry
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| entry.name.as_str().into());
                    let basename_score =
                        pattern.score(Utf32Str::new(basename.as_ref(), buffer), matcher);
                    let score = basename_score.or_else(|| {
                        let relative_path = entry
                            .path
                            .strip_prefix(&current_dir)
                            .ok()
                            .or_else(|| {
                                canonical_dir
                                    .as_deref()
                                    .and_then(|dir| entry.path.strip_prefix(dir).ok())
                            })
                            .unwrap_or(&entry.path)
                            .to_string_lossy();
                        pattern.score(Utf32Str::new(&relative_path, buffer), matcher)
                    })?;
                    let basename_lower = basename.to_lowercase();
                    let match_tier = if basename_lower == query {
                        3
                    } else if basename_lower.starts_with(&query) {
                        2
                    } else if basename_score.is_some() {
                        1
                    } else {
                        0
                    };
                    Some((index, match_tier, score, entry.is_recent))
                })
                .collect();
            scored.sort_by(|left, right| {
                right
                    .1
                    .cmp(&left.1)
                    .then_with(|| right.2.cmp(&left.2))
                    .then_with(|| right.3.cmp(&left.3))
                    .then_with(|| source[left.0].path.cmp(&source[right.0].path))
            });
            scored.into_iter().map(|(index, _, _, _)| index).collect()
        };
        self.filtered_indices = filtered_indices;
    }

    pub fn filtered_entries(&self) -> impl Iterator<Item = &BrowserEntry> {
        self.filtered_indices
            .iter()
            .filter_map(|index| self.active_source().get(*index))
    }

    /// Returns the displayed index where the recent-files section begins.
    /// Fuzzy results are ranked by match quality, so they are not sectioned.
    pub fn recent_heading_index(&self) -> Option<usize> {
        if !self.filter.is_empty() {
            return None;
        }
        self.filtered_entries().position(|entry| entry.is_recent)
    }

    pub fn match_indices(&self, value: &str) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let mut fuzzy = self.fuzzy.borrow_mut();
        fuzzy.sync_terms(&self.filter);
        let FuzzyCache {
            matcher,
            buffer,
            term_patterns,
            indices,
            ..
        } = &mut *fuzzy;
        indices.clear();
        for pattern in term_patterns.iter() {
            let haystack = Utf32Str::new(value, buffer);
            let _ = pattern.indices(haystack, matcher, indices);
        }
        indices.sort_unstable();
        indices.dedup();
        indices
            .iter()
            .filter_map(|&index| usize::try_from(index).ok())
            .collect()
    }

    pub fn entry_source(&self, entry: &BrowserEntry) -> Option<BrowserEntrySource> {
        if entry.name == "../" {
            return None;
        }
        if entry.is_recent {
            return Some(BrowserEntrySource::Recent);
        }
        if entry.path.parent() == Some(self.current_dir.as_path()) {
            Some(BrowserEntrySource::Here)
        } else {
            Some(BrowserEntrySource::Subdirectory)
        }
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn adjust_scroll(&mut self, visible_height: usize) {
        let visible_height = visible_height.max(1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
            return;
        }

        let heading_index = self.recent_heading_index();
        while self.scroll_offset < self.selected {
            let entries = self.selected - self.scroll_offset + 1;
            let includes_heading = heading_index
                .is_some_and(|index| index >= self.scroll_offset && index <= self.selected);
            if entries + usize::from(includes_heading) <= visible_height {
                break;
            }
            self.scroll_offset += 1;
        }
    }

    pub fn enter_selected(&mut self) -> Option<PathBuf> {
        let real_index = *self.filtered_indices.get(self.selected)?;
        let entry = self.active_source().get(real_index)?;
        if entry.is_dir {
            self.current_dir = entry.path.clone();
            self.load_dir();
            self.preload_recursive();
            None
        } else {
            Some(entry.path.clone())
        }
    }

    fn selected_entry(&self) -> Option<&BrowserEntry> {
        let real_index = *self.filtered_indices.get(self.selected)?;
        self.active_source().get(real_index)
    }

    fn restore_selection(&mut self, path: Option<&Path>) {
        self.selected = path
            .and_then(|path| {
                self.filtered_indices.iter().position(|index| {
                    self.active_source()
                        .get(*index)
                        .is_some_and(|entry| entry.path == path)
                })
            })
            .unwrap_or(0);
    }

    fn active_source(&self) -> &[BrowserEntry] {
        if !self.filter.is_empty() && self.recursive_loaded {
            &self.recursive_entries
        } else {
            &self.entries
        }
    }
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        self.recursive_cancel.store(true, Ordering::Relaxed);
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn collect_pdf_files(root: &Path, cancelled: &AtomicBool) -> Vec<(String, PathBuf)> {
    if cancelled.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let git_files = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.[pP][dD][fF]",
        ])
        .current_dir(root)
        .output();
    match git_files {
        Ok(output) if output.status.success() => {
            let mut files: Vec<_> = output
                .stdout
                .split(|byte| *byte == 0)
                .take_while(|_| !cancelled.load(Ordering::Relaxed))
                .filter(|relative| !relative.is_empty())
                .map(|relative| Path::new(OsStr::from_bytes(relative)))
                .filter(|relative| is_pdf(relative))
                .filter(|relative| relative.components().count() <= MAX_RECURSIVE_DEPTH + 1)
                .take(MAX_RECURSIVE_PDFS)
                .map(|relative| (relative.to_string_lossy().into_owned(), root.join(relative)))
                .collect();
            files.sort_by(|left, right| left.0.cmp(&right.0));
            return files;
        }
        _ => {}
    }

    let mut files = Vec::new();
    collect_pdf_files_recursive(root, root, &mut files, 0, cancelled, &mut 0);
    files
}

fn collect_pdf_files_recursive(
    directory: &Path,
    root: &Path,
    files: &mut Vec<(String, PathBuf)>,
    depth: usize,
    cancelled: &AtomicBool,
    visited: &mut usize,
) {
    let mut queue = std::collections::VecDeque::from([(directory.to_path_buf(), depth)]);
    let mut subdirs = Vec::new();
    while let Some((dir, current_depth)) = queue.pop_front() {
        if *visited >= MAX_RECURSIVE_VISITED
            || files.len() >= MAX_RECURSIVE_PDFS
            || cancelled.load(Ordering::Relaxed)
        {
            return;
        }
        let Some(mut entries) = bounded_dir_entries(&dir, cancelled, *visited) else {
            continue;
        };
        // `read_dir` order is filesystem-specific; sort so subdirectory
        // traversal and `MAX_RECURSIVE_PDFS` truncation are deterministic.
        entries.sort_by_key(std::fs::DirEntry::file_name);
        subdirs.clear();
        for entry in entries {
            if *visited >= MAX_RECURSIVE_VISITED
                || files.len() >= MAX_RECURSIVE_PDFS
                || cancelled.load(Ordering::Relaxed)
            {
                return;
            }
            *visited += 1;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if current_depth < MAX_RECURSIVE_DEPTH {
                    subdirs.push(path);
                }
            } else if is_pdf(&path) {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                files.push((relative.to_string_lossy().into_owned(), path));
            }
        }
        // Entries were visited in name order, so `subdirs` is already sorted.
        for subdir in subdirs.drain(..) {
            queue.push_back((subdir, current_depth + 1));
        }
    }
}

/// Entries of `dir`, at most the remaining visit budget, stopping early when
/// cancelled, so a huge directory is never collected whole.
fn bounded_dir_entries(
    dir: &Path,
    cancelled: &AtomicBool,
    visited: usize,
) -> Option<Vec<std::fs::DirEntry>> {
    let read_dir = std::fs::read_dir(dir).ok()?;
    Some(
        read_dir
            .take_while(|_| !cancelled.load(Ordering::Relaxed))
            .flatten()
            .take(MAX_RECURSIVE_VISITED.saturating_sub(visited))
            .collect(),
    )
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserState, MAX_RECURSIVE_DEPTH, MAX_RECURSIVE_PDFS, MAX_RECURSIVE_VISITED,
        bounded_dir_entries, collect_pdf_files_recursive,
    };
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_for_recursive_scan(browser: &mut BrowserState) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !browser.poll_recursive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(browser.recursive_loaded, "recursive scan timed out");
    }

    #[test]
    fn recursive_scan_obeys_depth_entry_and_cancellation_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut nested = root.to_path_buf();
        for _ in 0..MAX_RECURSIVE_DEPTH {
            nested.push("d");
            fs::create_dir(&nested).unwrap();
        }
        let boundary = nested.join("edge.pdf");
        fs::write(&boundary, b"synthetic").unwrap();
        let deeper = nested.join("d");
        fs::create_dir(&deeper).unwrap();
        fs::write(deeper.join("deep.pdf"), b"synthetic").unwrap();

        let cancelled = AtomicBool::new(false);
        let mut files = Vec::new();
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut 0);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, boundary);

        fs::write(root.join("a.pdf"), b"synthetic").unwrap();
        fs::write(root.join("b.pdf"), b"synthetic").unwrap();
        files = vec![(String::new(), root.join("seed.pdf")); MAX_RECURSIVE_PDFS - 1];
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut 0);
        assert_eq!(files.len(), MAX_RECURSIVE_PDFS);

        cancelled.store(true, Ordering::Relaxed);
        files.clear();
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut 0);
        assert!(files.is_empty());
    }

    #[test]
    fn recursive_scan_counts_non_pdf_entries_across_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("d/leaf.pdf"), b"synthetic").unwrap();
        let cancelled = AtomicBool::new(false);
        let mut files = Vec::new();
        let mut visited = MAX_RECURSIVE_VISITED - 1;
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut visited);
        assert_eq!(visited, MAX_RECURSIVE_VISITED);
        assert!(
            files.is_empty(),
            "scan exceeded its budget while descending"
        );

        visited = MAX_RECURSIVE_VISITED - 2;
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut visited);
        assert_eq!(visited, MAX_RECURSIVE_VISITED);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn recursive_scan_reads_only_the_remaining_visit_budget() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for index in 0..64 {
            fs::write(root.join(format!("wug{index:02}.pdf")), b"synthetic").unwrap();
        }
        let cancelled = AtomicBool::new(false);
        let budget = MAX_RECURSIVE_VISITED - 1;
        assert_eq!(
            bounded_dir_entries(root, &cancelled, budget).unwrap().len(),
            1
        );
        assert!(
            bounded_dir_entries(root, &cancelled, MAX_RECURSIVE_VISITED)
                .unwrap()
                .is_empty()
        );
        cancelled.store(true, Ordering::Relaxed);
        assert!(bounded_dir_entries(root, &cancelled, 0).unwrap().is_empty());
        cancelled.store(false, Ordering::Relaxed);

        let mut files = Vec::new();
        let mut visited = budget;
        collect_pdf_files_recursive(root, root, &mut files, 0, &cancelled, &mut visited);
        assert_eq!(visited, MAX_RECURSIVE_VISITED);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn recursive_worker_sorts_deduplicates_and_browser_cancels_on_reload_and_drop() {
        let directory = tempfile::tempdir().unwrap();
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        let path = directory.path().join("x.pdf");
        for name in ["z.pdf", "x.pdf", "a.pdf"] {
            fs::write(directory.path().join(name), b"synthetic").unwrap();
        }
        browser.set_recents(vec![path.clone(), path.clone()]);
        browser.preload_recursive();
        let entries = browser
            .recursive_rx
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["a.pdf", "x.pdf", "z.pdf"]
        );
        assert!(entries[1].is_recent);
        let (sender, receiver) = mpsc::channel();
        browser.recursive_rx = Some(receiver);
        sender.send(entries).unwrap();
        assert!(browser.poll_recursive());
        assert_eq!(browser.recursive_entries.len(), 3);

        let cancelled = Arc::new(AtomicBool::new(false));
        browser.recursive_cancel = Arc::clone(&cancelled);
        browser.load_dir();
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(browser.recursive_rx.is_none());
        let cancelled = Arc::new(AtomicBool::new(false));
        browser.recursive_cancel = Arc::clone(&cancelled);
        drop(browser);
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn browser_lists_directories_and_pdf_files_only() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("docs")).expect("directory");
        fs::write(directory.path().join("one.pdf"), b"synthetic").expect("pdf");
        fs::write(directory.path().join("notes.txt"), b"synthetic").expect("text file");

        let browser = BrowserState::new(directory.path().to_path_buf());
        let names: Vec<_> = browser
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();

        assert!(names.contains(&"docs/"));
        assert!(names.contains(&"one.pdf"));
        assert!(!names.contains(&"notes.txt"));
    }

    #[test]
    fn recents_from_other_directories_are_listed_first() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("b.pdf"), b"synthetic").expect("pdf");
        let other = tempfile::tempdir().expect("other directory");
        let recent = other.path().join("z-recent.pdf");
        fs::write(&recent, b"synthetic").expect("recent pdf");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent.clone()]);

        let recent_position = browser
            .entries
            .iter()
            .position(|entry| entry.name == "z-recent.pdf" && entry.is_recent)
            .expect("recent entry present");
        let listed = browser
            .entries
            .iter()
            .position(|entry| entry.name == "b.pdf")
            .expect("directory entry present");
        assert!(recent_position < listed);
    }

    #[test]
    fn recents_already_in_the_directory_are_not_duplicated() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("here.pdf");
        fs::write(&path, b"synthetic").expect("pdf");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![path]);

        let count = browser
            .entries
            .iter()
            .filter(|entry| entry.name.contains("here.pdf"))
            .count();
        assert_eq!(count, 1);
        let entry = browser
            .entries
            .iter()
            .find(|entry| entry.name == "here.pdf")
            .expect("recent entry");
        assert!(entry.is_recent);
    }

    #[test]
    fn recursive_filter_keeps_recents_outside_the_search_root() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("local.pdf"), b"synthetic").expect("local pdf");
        let other = tempfile::tempdir().expect("other directory");
        let recent = other.path().join("remote-report.pdf");
        fs::write(&recent, b"synthetic").expect("recent pdf");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent.clone()]);
        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);

        browser.filter = "remote report".into();
        browser.rebuild_filter();
        let matches: Vec<_> = browser.filtered_entries().collect();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, recent);
        assert!(matches[0].is_recent);
    }

    #[test]
    fn recursive_filter_matches_parent_directories() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let archive = directory.path().join("research-archive");
        fs::create_dir(&archive).expect("archive directory");
        let report = archive.join("report.pdf");
        fs::write(&report, b"synthetic").expect("report");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);
        browser.filter = "research-archive".into();
        browser.rebuild_filter();

        let matches: Vec<_> = browser.filtered_entries().collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, report);
    }

    #[test]
    fn git_recursive_filter_preserves_quoted_and_non_utf8_paths() {
        use std::ffi::OsString;
        #[cfg(target_os = "linux")]
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;

        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(directory.path())
                .args(["config", "core.quotePath", "true"])
                .status()
                .unwrap()
                .success()
        );
        let docs = directory.path().join("docs");
        fs::create_dir(&docs).unwrap();
        let docs = docs.canonicalize().unwrap();
        let cases = [
            ("résumé", OsString::from("résumé.pdf")),
            ("quoted", OsString::from("quoted\"name.pdf")),
            ("newline", OsString::from("newline\nname.pdf")),
            // macOS filesystems reject non-UTF-8 filenames.
            #[cfg(target_os = "linux")]
            ("raw", OsString::from_vec(b"raw-\xff.pdf".to_vec())),
        ];
        for (_, name) in &cases {
            fs::write(docs.join(name), b"synthetic").unwrap();
        }
        fs::write(directory.path().join("top.pdf"), b"synthetic").unwrap();
        let mut browser = BrowserState::new(directory.path().to_owned());
        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);
        browser.filter = "top".into();
        browser.rebuild_filter();
        assert_eq!(browser.filtered_entries().count(), 1);
        for (filter, name) in cases {
            browser.filter = filter.to_owned();
            browser.rebuild_filter();
            let matches: Vec<_> = browser
                .filtered_entries()
                .map(|entry| &entry.path)
                .collect();
            assert_eq!(matches, vec![&docs.join(name)], "{filter}");
        }
    }

    #[test]
    fn basename_matches_rank_above_parent_directory_matches() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let direct = directory.path().join("report.pdf");
        fs::write(&direct, b"synthetic").expect("direct report");
        let archive = directory.path().join("report-archive");
        fs::create_dir(&archive).expect("archive directory");
        fs::write(archive.join("notes.pdf"), b"synthetic").expect("nested PDF");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);
        browser.filter = "report".into();
        browser.rebuild_filter();

        assert_eq!(browser.filtered_entries().next().unwrap().path, direct);
    }

    #[test]
    fn recursive_results_preserve_the_selected_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let selected = directory.path().join("zeta.pdf");
        fs::write(&selected, b"synthetic").expect("selected PDF");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).expect("nested directory");
        fs::write(nested.join("alpha.pdf"), b"synthetic").expect("nested PDF");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "pdf".into();
        browser.rebuild_filter();
        assert_eq!(browser.selected_entry().unwrap().path, selected);

        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);

        assert_eq!(browser.selected_entry().unwrap().path, selected);
        assert_eq!(browser.selected, 1);
    }

    #[test]
    fn recent_heading_starts_at_the_first_recent_entry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recent = directory.path().join("recent.pdf");
        fs::write(&recent, b"synthetic").expect("recent pdf");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent]);

        let heading_index = browser.recent_heading_index().expect("recent heading");
        assert!(
            browser
                .filtered_entries()
                .nth(heading_index)
                .unwrap()
                .is_recent
        );

        browser.filter = "recent".into();
        browser.rebuild_filter();
        assert_eq!(browser.recent_heading_index(), None);
    }

    #[test]
    fn fuzzy_match_indices_identify_highlighted_characters() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "gss".to_string();

        assert_eq!(browser.match_indices("genesis.pdf"), vec![0, 4, 6]);
    }
}

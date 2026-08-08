use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

#[derive(Debug)]
pub struct BrowserEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_recent: bool,
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
}

impl BrowserState {
    pub fn new(dir: PathBuf) -> Self {
        let mut state = Self {
            current_dir: dir.clone(),
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            filter: String::new(),
            filtered_indices: Vec::new(),
            root_dir: dir,
            recents: Vec::new(),
            recursive_entries: Vec::new(),
            recursive_loaded: false,
            recursive_rx: None,
        };
        state.load_dir();
        state
    }

    /// Supplies the recently opened documents, which are listed first in the
    /// starting directory when no filter is active.
    pub fn set_recents(&mut self, recents: Vec<PathBuf>) {
        self.recents = recents;
        self.load_dir();
    }

    pub fn load_dir(&mut self) {
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
        let directory = self.current_dir.clone();
        let recents = self.recents.clone();
        let (sender, receiver) = mpsc::channel();
        self.recursive_rx = Some(receiver);
        thread::spawn(move || {
            let root = find_git_root(&directory).unwrap_or_else(|| directory.clone());
            let mut entries: Vec<_> = collect_pdf_files(&root)
                .into_iter()
                .map(|(name, path)| BrowserEntry {
                    name,
                    is_recent: recents.contains(&path),
                    path,
                    is_dir: false,
                })
                .collect();
            if let Ok(read_dir) = std::fs::read_dir(&directory) {
                for entry in read_dir.flatten().filter(|entry| is_pdf(&entry.path())) {
                    if !entries.iter().any(|existing| existing.path == entry.path()) {
                        entries.push(BrowserEntry {
                            name: entry.file_name().to_string_lossy().into_owned(),
                            path: entry.path(),
                            is_dir: false,
                            is_recent: false,
                        });
                    }
                }
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            let _ = sender.send(entries);
        });
    }

    pub fn poll_recursive(&mut self) -> bool {
        let Some(receiver) = self.recursive_rx.as_ref() else {
            return false;
        };
        let Ok(entries) = receiver.try_recv() else {
            return false;
        };
        self.recursive_entries = entries;
        self.recursive_loaded = true;
        self.recursive_rx = None;
        if !self.filter.is_empty() {
            self.rebuild_filter();
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
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut scored: Vec<_> = self
            .active_source()
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let haystack = Utf32Str::new(&entry.name, &mut buffer);
                pattern
                    .score(haystack, &mut matcher)
                    .map(|score| (index, score))
            })
            .collect();
        scored.sort_by_key(|entry| Reverse(entry.1));
        self.filtered_indices = scored.into_iter().map(|(index, _)| index).collect();
    }

    pub fn filtered_entries(&self) -> impl Iterator<Item = &BrowserEntry> {
        self.filtered_indices
            .iter()
            .filter_map(|index| self.active_source().get(*index))
    }

    pub fn match_indices(&self, name: &str) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let pattern = Pattern::parse(&self.filter, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let haystack = Utf32Str::new(name, &mut buffer);
        let mut indices = Vec::new();
        let _ = pattern.indices(haystack, &mut matcher, &mut indices);
        indices.sort_unstable();
        indices.dedup();
        indices
            .into_iter()
            .filter_map(|index| usize::try_from(index).ok())
            .collect()
    }

    pub fn select_down(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered_indices.len() - 1);
        }
    }

    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.filtered_indices.len().saturating_sub(1);
    }

    pub fn page_down(&mut self, page_size: usize) {
        if !self.filtered_indices.is_empty() {
            self.selected = (self.selected + page_size).min(self.filtered_indices.len() - 1);
        }
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.selected = self.selected.saturating_sub(page_size);
    }

    pub fn adjust_scroll(&mut self, visible_height: usize) {
        let visible_height = visible_height.max(1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected - visible_height + 1;
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

    fn active_source(&self) -> &[BrowserEntry] {
        if !self.filter.is_empty() && self.recursive_loaded {
            &self.recursive_entries
        } else {
            &self.entries
        }
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

fn collect_pdf_files(root: &Path) -> Vec<(String, PathBuf)> {
    let git_files = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output();
    match git_files {
        Ok(output) if output.status.success() => {
            let mut files: Vec<_> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|relative| is_pdf(Path::new(relative)))
                .map(|relative| (relative.to_string(), root.join(relative)))
                .collect();
            files.sort_by(|left, right| left.0.cmp(&right.0));
            return files;
        }
        _ => {}
    }

    let mut files = Vec::new();
    collect_pdf_files_recursive(root, root, &mut files);
    files
}

fn collect_pdf_files_recursive(directory: &Path, root: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(read_dir) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_pdf_files_recursive(&entry.path(), root, files);
        } else if is_pdf(&entry.path()) {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            files.push((relative.to_string_lossy().into_owned(), path));
        }
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::BrowserState;
    use std::fs;

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
    fn fuzzy_match_indices_identify_highlighted_characters() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "gss".to_string();

        assert_eq!(browser.match_indices("genesis.pdf"), vec![0, 4, 6]);
    }
}

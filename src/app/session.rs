use super::*;

pub(super) struct Session {
    pub(super) tabs: Vec<Tab>,
    pub(super) active_tab: usize,
    pub(super) next_document_id: DocumentId,
    pub(super) pending_open: Option<PendingOpen>,
}

pub(super) struct Tab {
    pub(super) document_id: DocumentId,
    pub(super) path: PathBuf,
    pub(super) revision: PdfRevision,
    pub(super) watcher: FileWatcher,
    pub(super) page_count: u32,
    pub(super) page: u32,
    pub(super) fit: FitMode,
    pub(super) zoom: u16,
    pub(super) invert: bool,
    pub(super) dark_mode_style: DarkModeStyle,
    pub(super) search_highlight: [u8; 3],
    pub(super) link_highlight: [u8; 3],
    // Content alignment may put the first visible page outside its own crop
    // range while a wider following page retains the viewport's horizontal pan.
    pub(super) scroll_x: i64,
    pub(super) scroll_y: u32,
    pub(super) center_content: bool,
    pub(super) outline: Arc<Vec<OutlineItem>>,
    pub(super) cache: HashMap<RenderKey, Arc<Frame>>,
    pub(super) search: SearchState,
    pub(super) link_history: JumpList,
    pub(super) pending_destination: Option<LinkDestination>,
    pub(super) link_index: LinkIndexState,
}

impl Tab {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        document_id: DocumentId,
        path: PathBuf,
        revision: PdfRevision,
        watcher: FileWatcher,
        page_count: u32,
        page: u32,
        fit: FitMode,
        invert: bool,
        dark_mode_style: DarkModeStyle,
        search_highlight: [u8; 3],
        link_highlight: [u8; 3],
        outline: Vec<OutlineItem>,
    ) -> Self {
        Self {
            document_id,
            path,
            revision,
            watcher,
            page_count,
            page,
            fit,
            zoom: ZOOM_DEFAULT,
            invert,
            dark_mode_style,
            search_highlight,
            link_highlight,
            scroll_x: 0,
            scroll_y: 0,
            center_content: true,
            outline: Arc::new(outline),
            cache: HashMap::new(),
            search: SearchState::default(),
            link_history: JumpList::default(),
            pending_destination: None,
            link_index: LinkIndexState::new(page_count),
        }
    }

    pub(super) fn render_key(
        &self,
        viewport: Viewport,
        link_mode: bool,
        selected_link_ordinal: Option<u32>,
    ) -> RenderKey {
        RenderKey {
            document_id: self.document_id,
            page: self.page,
            width: viewport.pixel_width,
            height: viewport.pixel_height,
            zoom: self.zoom,
            fit: self.fit,
            invert: self.invert,
            dark_mode_style: self.dark_mode_style,
            search_request_id: self.search.highlight_request_id(self.page),
            search_highlight: self.search_highlight,
            link_mode,
            link_highlight: self.link_highlight,
            selected_link_ordinal,
        }
    }

    pub(super) fn view_position(&self) -> ViewPosition {
        ViewPosition {
            page: self.page,
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            center_content: self.center_content,
        }
    }

    pub(super) fn set_view_position(&mut self, position: ViewPosition) {
        self.page = position.page.min(self.page_count.saturating_sub(1));
        self.scroll_x = position.scroll_x;
        self.scroll_y = position.scroll_y;
        self.center_content = position.center_content;
    }
}

// Watch both files: a companion published after its PDF must also trigger reload.
pub(super) type FileFingerprint = crate::synctex::DocumentRevision;

pub(super) struct FileWatcher {
    pub(super) accepted: FileFingerprint,
    pub(super) failed: Option<FileFingerprint>,
    pub(super) candidate: Option<(FileFingerprint, Instant)>,
    pub(super) next_poll: Instant,
}

impl FileWatcher {
    #[cfg(test)]
    pub(super) fn new(path: &Path) -> io::Result<Self> {
        Ok(Self::from_revision(FileFingerprint::read(path)?))
    }

    pub(super) fn from_revision(accepted: FileFingerprint) -> Self {
        Self {
            accepted,
            failed: None,
            candidate: None,
            next_poll: Instant::now() + FILE_POLL_INTERVAL,
        }
    }

    pub(super) fn poll(&mut self, path: &Path) -> Option<FileFingerprint> {
        let now = Instant::now();
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + FILE_POLL_INTERVAL;

        let fingerprint = match FileFingerprint::read(path) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                self.candidate = None;
                return None;
            }
        };
        self.observe(fingerprint, now)
    }

    pub(super) fn observe(
        &mut self,
        fingerprint: FileFingerprint,
        now: Instant,
    ) -> Option<FileFingerprint> {
        if fingerprint == self.accepted || self.failed == Some(fingerprint) {
            self.candidate = None;
            return None;
        }

        match self.candidate {
            Some((candidate, since))
                if candidate == fingerprint && now.duration_since(since) >= FILE_STABLE_FOR =>
            {
                Some(fingerprint)
            }
            Some((candidate, _)) if candidate == fingerprint => None,
            _ => {
                self.candidate = Some((fingerprint, now));
                None
            }
        }
    }

    pub(super) fn accept(&mut self, fingerprint: FileFingerprint) {
        self.accepted = fingerprint;
        self.failed = None;
        if self
            .candidate
            .is_some_and(|(candidate, _)| candidate == fingerprint)
        {
            self.candidate = None;
        }
    }

    pub(super) fn mark_failed(&mut self, fingerprint: FileFingerprint) {
        self.failed = Some(fingerprint);
        self.candidate = None;
        self.next_poll = Instant::now() + RELOAD_RETRY_DELAY;
    }
}

pub(super) struct TabView {
    position: ViewPosition,
    fit: FitMode,
    zoom: u16,
    invert: bool,
}

pub(super) enum PendingOpen {
    Reload {
        document_id: DocumentId,
        fingerprint: FileFingerprint,
    },
    Selection {
        document_id: DocumentId,
        path: PathBuf,
        view: Option<TabView>,
    },
}

impl PendingOpen {
    fn document_id(&self) -> DocumentId {
        match self {
            Self::Reload { document_id, .. } | Self::Selection { document_id, .. } => *document_id,
        }
    }
}

impl App {
    pub(super) fn poll_file_change(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let change = self.session.tabs.iter_mut().find_map(|tab| {
            tab.watcher
                .poll(&tab.path)
                .map(|fingerprint| (tab.document_id, tab.path.clone(), fingerprint))
        });
        let Some((document_id, path, fingerprint)) = change else {
            return Ok(());
        };

        self.begin_reload(document_id, path, fingerprint, output)
    }

    pub(super) fn begin_reload(
        &mut self,
        document_id: DocumentId,
        path: PathBuf,
        fingerprint: FileFingerprint,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.clear_document_flash(document_id);
        if self.tab().document_id == document_id {
            self.navigation.inverse.take();
            self.bump_generation(true);
            self.reset_render_state();
            self.pending_scale = None;
        } else {
            self.pending.retain(|key, _| key.document_id != document_id);
        }
        self.worker
            .open(document_id, path)
            .map_err(AppError::Renderer)?;
        self.session.pending_open = Some(PendingOpen::Reload {
            document_id,
            fingerprint,
        });
        if self.tab().document_id == document_id {
            let viewport = self.prepare_viewport(output)?;
            self.draw_status(output, viewport, "reloading")?;
        }
        Ok(())
    }

    pub(super) fn finish_open(
        &mut self,
        document_id: DocumentId,
        pages: u32,
        outline: Vec<OutlineItem>,
        revision: FileFingerprint,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(pending) = self
            .session
            .pending_open
            .take_if(|pending| pending.document_id() == document_id)
        else {
            return Ok(());
        };
        match pending {
            PendingOpen::Reload { fingerprint, .. } => {
                let Some(index) = self.tab_index(document_id) else {
                    return Ok(());
                };
                self.clear_document_flash(document_id);
                let tab = &mut self.session.tabs[index];
                tab.watcher.accept(fingerprint);
                tab.revision = revision.pdf;
                tab.page_count = pages;
                tab.page = tab.page.min(pages - 1);
                tab.outline = Arc::new(outline);
                tab.cache.clear();
                tab.search = SearchState::default();
                tab.link_history.clamp_pages(pages);
                tab.pending_destination = None;
                tab.link_index = LinkIndexState::new(pages);
                if index == self.session.active_tab {
                    self.reset_active_tab_pickers();
                    self.reset_render_state();
                    self.ensure_link_index();
                    self.request_current(output)?;
                }
            }
            PendingOpen::Selection { path, view, .. } => {
                self.restore_active_picker_preview();
                crate::recent::record(&path);
                let watcher = FileWatcher::from_revision(revision);
                self.session.tabs.push(Tab::new(
                    document_id,
                    path,
                    revision.pdf,
                    watcher,
                    pages,
                    0,
                    self.default_fit,
                    self.default_invert,
                    DarkModeStyle::new(
                        self.theme.document.background,
                        self.theme.document.foreground,
                    ),
                    terminal_color_rgb(self.theme.yellow),
                    terminal_color_rgb(self.theme.cyan),
                    outline,
                ));
                if let Some(view) = view {
                    let tab = self.session.tabs.last_mut().expect("new tab");
                    tab.set_view_position(view.position);
                    tab.fit = view.fit;
                    tab.zoom = view.zoom;
                    tab.invert = view.invert;
                }
                self.session.active_tab = self.session.tabs.len() - 1;
                self.reset_active_tab_pickers();
                self.reset_render_state();
                self.ensure_link_index();
                self.request_current(output)?;
            }
        }
        Ok(())
    }

    pub(super) fn fail_open(
        &mut self,
        document_id: DocumentId,
        error: &str,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let Some(pending) = self
            .session
            .pending_open
            .take_if(|pending| pending.document_id() == document_id)
        else {
            return Ok(());
        };
        let (failed_path, state) = match pending {
            PendingOpen::Reload { fingerprint, .. } => {
                let Some(index) = self.tab_index(document_id) else {
                    return Ok(());
                };
                self.session.tabs[index].watcher.mark_failed(fingerprint);
                let path = self.session.tabs[index].path.clone();
                let state =
                    (index == self.session.active_tab).then(|| format!("reload failed: {error}"));
                (path, state)
            }
            PendingOpen::Selection { path, .. } => (path, Some(format!("open failed: {error}"))),
        };
        if self.navigation.forward.as_ref().is_some_and(|fwd| {
            fwd.request.pdf == failed_path
                || fs::canonicalize(&fwd.request.pdf).ok().as_deref() == Some(failed_path.as_path())
        }) {
            self.finish_forward(Some(format!("document open failed: {error}")));
        }
        if let Some(state) = state {
            self.request_current(output)?;
            self.draw_status(output, self.viewport()?, &state)?;
        }
        Ok(())
    }

    pub(super) fn begin_open(
        &mut self,
        path: PathBuf,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.open_tab(path, None, output)
    }

    pub(super) fn duplicate_tab(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        if self.session.pending_open.is_some() {
            return Ok(());
        }
        let tab = self.tab();
        let view = TabView {
            position: tab.view_position(),
            fit: tab.fit,
            zoom: tab.zoom,
            invert: tab.invert,
        };
        self.open_tab(tab.path.clone(), Some(view), output)
    }

    fn open_tab(
        &mut self,
        path: PathBuf,
        view: Option<TabView>,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        self.navigation.inverse.take();
        let document_id = self.session.next_document_id;
        self.session.next_document_id = self.session.next_document_id.wrapping_add(1).max(1);
        self.worker
            .open(document_id, path.clone())
            .map_err(AppError::Renderer)?;
        self.session.pending_open = Some(PendingOpen::Selection {
            document_id,
            path,
            view,
        });
        let viewport = self.prepare_viewport(output)?;
        self.draw_status(output, viewport, "opening")?;
        Ok(())
    }

    pub(super) fn switch_tab(
        &mut self,
        direction: i32,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if self.session.tabs.len() < 2 || self.session.pending_open.is_some() {
            return Ok(());
        }
        let index = cycled_tab_index(self.session.active_tab, self.session.tabs.len(), direction);
        self.select_tab(index, output)
    }

    pub(super) fn select_tab(
        &mut self,
        index: usize,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        if index >= self.session.tabs.len()
            || index == self.session.active_tab
            || self.session.pending_open.is_some()
        {
            return Ok(());
        }
        self.restore_active_picker_preview();
        self.navigation.inverse.take();
        self.session.active_tab = index;
        self.reset_active_tab_pickers();
        self.reset_render_state();
        self.ensure_link_index();
        self.request_current(output)
    }

    pub(super) fn close_current(&mut self, output: &mut impl Write) -> Result<bool, AppError> {
        if self.session.pending_open.is_some() {
            return Ok(false);
        }
        self.navigation.inverse.take();
        if self.session.tabs.len() == 1 {
            return Ok(true);
        }
        let removed = self.session.tabs.remove(self.session.active_tab);
        self.clear_document_flash(removed.document_id);
        self.worker.close(removed.document_id);
        if self.session.active_tab == self.session.tabs.len() {
            self.session.active_tab -= 1;
        }
        self.reset_active_tab_pickers();
        self.reset_render_state();
        self.ensure_link_index();
        self.request_current(output)?;
        Ok(false)
    }

    pub(super) fn tab(&self) -> &Tab {
        &self.session.tabs[self.session.active_tab]
    }

    pub(super) fn tab_mut(&mut self) -> &mut Tab {
        &mut self.session.tabs[self.session.active_tab]
    }

    pub(super) fn tab_index(&self, document_id: DocumentId) -> Option<usize> {
        self.session
            .tabs
            .iter()
            .position(|tab| tab.document_id == document_id)
    }

    pub(super) fn tab_index_for_path(&self, path: &Path) -> Option<usize> {
        self.session
            .tabs
            .get(self.session.active_tab)
            .filter(|tab| tab.path == path)
            .map(|_| self.session.active_tab)
            .or_else(|| self.session.tabs.iter().position(|tab| tab.path == path))
    }
}

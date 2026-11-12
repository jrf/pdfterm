use super::*;

pub(super) fn pick_pdf(
    directory: PathBuf,
    output: &mut impl Write,
    theme: Palette,
    interrupt: impl Fn() -> io::Result<bool>,
) -> Result<Option<PathBuf>, AppError> {
    let mut browser = BrowserState::with_recents(directory, crate::recent::load());
    browser.preload_recursive();
    with_modal_picker_terminal(output, Some(theme), |terminal| {
        let mut redraw = true;
        let mut visible_height = 1;
        let mut filtering = false;

        let selection = loop {
            if interrupt()? {
                break None;
            }
            if browser.poll_recursive() {
                redraw = true;
            }
            if redraw {
                visible_height = modal_picker_list_height(terminal)?;
                browser.adjust_scroll(visible_height);
                terminal.draw(|frame| draw_picker(frame, &browser, filtering, theme))?;
                redraw = false;
            }

            let Some(key) = next_modal_picker_key(&mut redraw, false)? else {
                continue;
            };
            redraw = true;
            if apply_picker_navigation(&mut browser, key, visible_height, filtering) {
                continue;
            }
            match key.code {
                KeyCode::Esc if filtering => {
                    clear_picker_filter(&mut browser);
                    filtering = false;
                }
                KeyCode::Esc if !clear_picker_filter(&mut browser) => break None,
                KeyCode::Esc => {}
                KeyCode::Char('/') if is_plain_key(key) => filtering = true,
                KeyCode::Enter => {
                    if let Some(path) = browser.enter_selected() {
                        break Some(path);
                    }
                }
                _ if filtering && edit_picker_filter(&mut browser.filter, key) => {
                    browser.rebuild_filter();
                    browser.select_first();
                    browser.scroll_offset = 0;
                }
                _ => {}
            }
        };
        Ok(selection)
    })
}

/// Runs a full-screen modal picker on a ratatui terminal borrowed from
/// `output`. The terminal is released (restoring its cursor state) before the
/// screen is cleared with `clear_theme`, when given; pickers whose caller
/// repaints with a different theme pass `None`.
pub(super) fn with_modal_picker_terminal<W: Write, T>(
    output: &mut W,
    clear_theme: Option<Palette>,
    run: impl FnOnce(&mut Terminal<CrosstermBackend<&mut W>>) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let mut terminal = Terminal::new(CrosstermBackend::new(&mut *output))?;
    let value = run(&mut terminal)?;
    drop(terminal);
    if let Some(theme) = clear_theme {
        clear_picker(output, theme)?;
    }
    Ok(value)
}

pub(super) fn clear_picker(output: &mut impl Write, theme: Palette) -> io::Result<()> {
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Hide,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    output.flush()
}

pub(super) fn show_help(
    output: &mut impl Write,
    theme: Palette,
    interrupt: impl Fn() -> io::Result<bool>,
) -> Result<(), AppError> {
    with_modal_picker_terminal(output, Some(theme), |terminal| {
        let mut redraw = true;

        loop {
            if interrupt()? {
                break;
            }
            if redraw {
                terminal.autoresize()?;
                terminal.draw(|frame| draw_help_menu(frame, theme))?;
                redraw = false;
            }

            if let Some(key) = next_modal_picker_key(&mut redraw, false)?
                && matches!(key.code, KeyCode::Char('?' | 'q') | KeyCode::Esc)
            {
                break;
            }
        }
        Ok(())
    })
}

#[allow(clippy::type_complexity)]
struct FilteredModalOptions<'a, T> {
    clear_theme: Option<Palette>,
    wheel: bool,
    edge: ListEdge,
    initial_selected: usize,
    refilter: &'a mut dyn FnMut(&str) -> Vec<usize>,
    on_escape_clear: &'a mut dyn FnMut(&[usize], usize) -> usize,
    draw: &'a mut dyn FnMut(&mut RatatuiFrame, &[usize], usize, usize, PickerFilter<'_>),
    on_enter: &'a mut dyn FnMut(&[usize], usize) -> Option<Option<T>>,
}

fn run_filtered_modal_picker<T>(
    output: &mut impl Write,
    interrupt: impl Fn() -> io::Result<bool>,
    opts: FilteredModalOptions<'_, T>,
) -> Result<Option<T>, AppError> {
    let mut filter = String::new();
    let mut filtered = (opts.refilter)(&filter);
    let mut selected = opts.initial_selected;
    let mut scroll_offset = 0usize;
    let mut filtering = false;
    with_modal_picker_terminal(output, opts.clear_theme, |terminal| {
        let mut redraw = true;
        let mut visible_height = 1;
        let selection = loop {
            if interrupt()? {
                break None;
            }
            if redraw {
                visible_height = modal_picker_list_height(terminal)?;
                scroll_into_view(selected, &mut scroll_offset, visible_height);
                let pf = PickerFilter {
                    query: &filter,
                    active: filtering,
                };
                terminal
                    .draw(|frame| (opts.draw)(frame, &filtered, selected, scroll_offset, pf))?;
                redraw = false;
            }
            let Some(key) = next_modal_picker_key(&mut redraw, opts.wheel)? else {
                continue;
            };
            if let Some(next) = picker_navigation_index(
                selected,
                filtered.len(),
                key,
                visible_height,
                !filtering,
                opts.edge,
            ) {
                selected = next;
                redraw = true;
                continue;
            }
            match key.code {
                KeyCode::Esc if filtering => {
                    let restored = (opts.on_escape_clear)(&filtered, selected);
                    filter.clear();
                    filtered = (opts.refilter)(&filter);
                    selected = filtered.iter().position(|i| *i == restored).unwrap_or(0);
                    scroll_offset = 0;
                    filtering = false;
                }
                KeyCode::Esc => break None,
                KeyCode::Char('/') if is_plain_key(key) => filtering = true,
                KeyCode::Enter => {
                    if let Some(choice) = (opts.on_enter)(&filtered, selected) {
                        break choice;
                    }
                    continue;
                }
                _ if filtering && edit_picker_filter(&mut filter, key) => {
                    filtered = (opts.refilter)(&filter);
                    selected = 0;
                    scroll_offset = 0;
                }
                _ => continue,
            }
            redraw = true;
        };
        Ok(selection)
    })
}

pub(super) fn pick_theme(
    themes: &[(String, Palette)],
    current: usize,
    output: &mut impl Write,
    interrupt: impl Fn() -> io::Result<bool>,
) -> Result<Option<usize>, AppError> {
    let lower_names: Vec<String> = themes.iter().map(|(name, _)| name.to_lowercase()).collect();
    let initial_selected = if current < themes.len() { current } else { 0 };
    run_filtered_modal_picker(
        output,
        interrupt,
        FilteredModalOptions {
            clear_theme: None,
            wheel: false,
            edge: ListEdge::Wrap,
            initial_selected,
            refilter: &mut |filter| filter_theme_indices_lower(&lower_names, filter),
            on_escape_clear: &mut |filtered, selected| {
                filtered.get(selected).copied().unwrap_or(current)
            },
            draw: &mut |frame, filtered, selected, scroll_offset, pf| {
                draw_theme_picker(
                    frame,
                    themes,
                    filtered,
                    selected,
                    scroll_offset,
                    pf.query,
                    pf.active,
                )
            },
            on_enter: &mut |filtered, selected| Some(filtered.get(selected).copied()),
        },
    )
}

/// Draws a side picker into a fixed viewport ending at `area` and hides the
/// cursor afterwards. Callers outside an already-synchronized frame wrap this
/// in `synchronized_output`.
pub(super) fn draw_side_picker_terminal(
    output: &mut impl Write,
    area: Rect,
    draw: impl FnOnce(&mut RatatuiFrame),
) -> io::Result<()> {
    let backend = CrosstermBackend::new(&mut *output);
    let mut terminal = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, area.right(), area.bottom())),
        },
    )?;
    terminal.draw(draw)?;
    drop(terminal);
    execute!(output, Hide)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PositionedImage {
    pub(super) left: u16,
    pub(super) top: u16,
    pub(super) placement: Placement,
}

pub(super) fn clear_image_canvas(output: &mut impl Write, viewport: Viewport) -> io::Result<()> {
    execute!(output, ResetColor)?;
    for row in viewport.top..viewport.top.saturating_add(viewport.rows) {
        execute!(output, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LinkPickerImage {
    pub(super) image_id: u32,
    pub(super) source_width: u32,
    pub(super) source_height: u32,
    pub(super) crop: Option<kitty::Crop>,
    pub(super) cell_width: u32,
    pub(super) cell_height: u32,
    pub(super) original: PositionedImage,
}

impl LinkPickerImage {
    pub(super) fn new(
        image_id: u32,
        frame: &Frame,
        placement: ImagePlacement,
        viewport: Viewport,
    ) -> Self {
        let (source_width, source_height) = placement
            .crop
            .map(|crop| (crop.width, crop.height))
            .unwrap_or((frame.width, frame.height));
        let (cell_width, cell_height) = viewport.cell_size();
        Self {
            image_id,
            source_width,
            source_height,
            crop: placement.crop,
            cell_width,
            cell_height,
            original: PositionedImage {
                left: placement.left,
                top: viewport.top,
                placement: Placement {
                    image_id,
                    columns: if placement.native_cell.is_some() {
                        0
                    } else {
                        placement.columns
                    },
                    rows: if placement.native_cell.is_some() {
                        0
                    } else {
                        placement.rows
                    },
                    offset_y: placement.offset_y,
                    z_index: PAGE_IMAGE_Z_INDEX,
                    crop: placement.crop,
                },
            },
        }
    }

    fn fit(self, pane: Rect) -> PositionedImage {
        let max_width = u32::from(pane.width).saturating_mul(self.cell_width);
        let max_height = u32::from(pane.height).saturating_mul(self.cell_height);
        let scale = (max_width as f64 / f64::from(self.source_width))
            .min(max_height as f64 / f64::from(self.source_height))
            .min(1.0);
        let pixel_width = (f64::from(self.source_width) * scale).round().max(1.0) as u32;
        let pixel_height = (f64::from(self.source_height) * scale).round().max(1.0) as u32;
        let columns = pixel_width
            .div_ceil(self.cell_width)
            .min(u32::from(pane.width))
            .max(1) as u16;
        let rows = pixel_height
            .div_ceil(self.cell_height)
            .min(u32::from(pane.height))
            .max(1) as u16;
        PositionedImage {
            left: pane.x + pane.width.saturating_sub(columns) / 2,
            top: pane.y + pane.height.saturating_sub(rows) / 2,
            placement: Placement {
                image_id: self.image_id,
                columns,
                rows,
                offset_y: 0,
                z_index: PAGE_IMAGE_Z_INDEX,
                crop: self.crop,
            },
        }
    }
}

pub(super) fn position_link_picker_image(
    image: LinkPickerImage,
    preview: Rect,
    layout: LinkPickerLayout,
) -> PositionedImage {
    if layout == LinkPickerLayout::Floating {
        image.original
    } else {
        image.fit(preview)
    }
}

pub(super) fn link_picker_area(viewport: Viewport) -> Rect {
    Rect::new(0, viewport.top, viewport.columns, viewport.rows)
}

pub(super) fn resolved_link_picker_layout(
    area: Rect,
    layout: LinkPickerLayout,
) -> LinkPickerLayout {
    match layout {
        LinkPickerLayout::Auto if u32::from(area.width) >= u32::from(area.height) * 2 => {
            LinkPickerLayout::Vertical
        }
        LinkPickerLayout::Auto => LinkPickerLayout::Horizontal,
        layout => layout,
    }
}

pub(super) fn next_link_picker_layout(area: Rect, layout: LinkPickerLayout) -> LinkPickerLayout {
    match resolved_link_picker_layout(area, layout) {
        LinkPickerLayout::Vertical => LinkPickerLayout::Horizontal,
        LinkPickerLayout::Horizontal => LinkPickerLayout::Floating,
        LinkPickerLayout::Floating => LinkPickerLayout::Vertical,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    }
}

pub(super) const fn link_picker_layout_name(layout: LinkPickerLayout) -> &'static str {
    match layout {
        LinkPickerLayout::Auto => "auto",
        LinkPickerLayout::Vertical => "vertical",
        LinkPickerLayout::Horizontal => "horizontal",
        LinkPickerLayout::Floating => "floating",
    }
}

pub(super) fn link_picker_layout_status(area: Rect, layout: LinkPickerLayout) -> String {
    if layout == LinkPickerLayout::Auto {
        format!(
            "auto/{}",
            link_picker_layout_name(resolved_link_picker_layout(area, layout))
        )
    } else {
        link_picker_layout_name(layout).to_string()
    }
}

pub(super) fn link_picker_focus_for_key(
    current: LinkPickerFocus,
    layout: LinkPickerLayout,
    key: KeyEvent,
) -> Option<LinkPickerFocus> {
    if layout == LinkPickerLayout::Floating {
        return None;
    }
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('h') if plain => Some(LinkPickerFocus::Document),
        KeyCode::Char('l') if plain => Some(LinkPickerFocus::Links),
        KeyCode::Tab | KeyCode::BackTab => Some(match current {
            LinkPickerFocus::Document => LinkPickerFocus::Links,
            LinkPickerFocus::Links => LinkPickerFocus::Document,
        }),
        _ => None,
    }
}

pub(super) fn link_picker_panes(area: Rect, geometry: LinkPickerGeometry) -> (Rect, Rect) {
    let picker_percent = geometry.split_percent.clamp(20, 80);
    let document_percent = 100 - picker_percent;
    let panes = match resolved_link_picker_layout(area, geometry.layout) {
        LinkPickerLayout::Vertical => Layout::horizontal([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Horizontal => Layout::vertical([
            Constraint::Percentage(document_percent),
            Constraint::Percentage(picker_percent),
        ])
        .split(area),
        LinkPickerLayout::Floating => return (area, picker_rect(area)),
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    (panes[0], panes[1])
}

pub(super) fn place_positioned_image(
    output: &mut impl Write,
    image: PositionedImage,
) -> io::Result<()> {
    execute!(output, MoveTo(image.left, image.top))?;
    kitty::place_image(output, image.placement)
}

fn fill_pane_rows(output: &mut impl Write, pane: Rect) -> io::Result<()> {
    let blank = " ".repeat(usize::from(pane.width));
    for row in pane.y..pane.y.saturating_add(pane.height) {
        execute!(output, MoveTo(pane.x, row), Print(&blank))?;
    }
    Ok(())
}

pub(super) fn show_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    let (preview, pane) = link_picker_panes(area, geometry);
    execute!(
        output,
        SetBackgroundColor(theme.bg_dark),
        SetForegroundColor(theme.fg)
    )?;
    fill_pane_rows(output, pane)?;
    if geometry.layout != LinkPickerLayout::Floating {
        place_positioned_image(output, image.fit(preview))?;
    }
    execute!(output, Hide)?;
    output.flush()
}

pub(super) fn clear_link_picker_pane(
    output: &mut impl Write,
    area: Rect,
    geometry: LinkPickerGeometry,
) -> io::Result<()> {
    let (_, pane) = link_picker_panes(area, geometry);
    execute!(output, ResetColor)?;
    fill_pane_rows(output, pane)?;
    output.flush()
}

pub(super) fn restore_link_picker_split(
    output: &mut impl Write,
    area: Rect,
    image: LinkPickerImage,
    geometry: LinkPickerGeometry,
    theme: Palette,
) -> io::Result<()> {
    clear_link_picker_pane(output, area, geometry)?;
    place_positioned_image(output, image.original)?;
    execute!(
        output,
        SetBackgroundColor(theme.bg),
        SetForegroundColor(theme.fg),
        Hide
    )?;
    output.flush()
}

pub(super) fn link_number_index(input: &str, link_count: usize) -> Option<usize> {
    input
        .parse::<usize>()
        .ok()
        .filter(|number| (1..=link_count).contains(number))
        .map(|number| number - 1)
}

pub(super) fn update_link_number_selection(
    input: &mut String,
    digit: char,
    link_count: usize,
) -> Option<usize> {
    let mut candidate = input.clone();
    candidate.push(digit);
    if let Some(index) = link_number_index(&candidate, link_count) {
        *input = candidate;
        return Some(index);
    }

    input.clear();
    input.push(digit);
    if let Some(index) = link_number_index(input, link_count) {
        Some(index)
    } else {
        input.clear();
        None
    }
}

/// What a single-step move past either end of a picker list does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ListEdge {
    Clamp,
    Wrap,
}

/// Returns the list position chosen by a navigation key, or `None` when `key`
/// does not navigate (or the list is empty). The vim letter keys (j/k/g/G)
/// only navigate when `letter_keys` is set, so pickers can reserve them for
/// filter input.
pub(super) fn picker_navigation_index(
    selected: usize,
    len: usize,
    key: KeyEvent,
    visible_height: usize,
    letter_keys: bool,
    edge: ListEdge,
) -> Option<usize> {
    let key = super::normalize_shifted_key(key);
    let last = len.checked_sub(1)?;
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let letters = letter_keys && is_plain_key(key);
    let wrap = edge == ListEdge::Wrap;
    let next = if wrap && selected >= last {
        0
    } else {
        (selected + 1).min(last)
    };
    let previous = if wrap && selected == 0 {
        last
    } else {
        selected.saturating_sub(1).min(last)
    };
    match key.code {
        KeyCode::Down => Some(next),
        KeyCode::Up => Some(previous),
        KeyCode::Char('j') if letters => Some(next),
        KeyCode::Char('k') if letters => Some(previous),
        KeyCode::Char('f' | 'd') if control => Some((selected + visible_height).min(last)),
        KeyCode::Char('b' | 'u') if control => Some(selected.saturating_sub(visible_height)),
        KeyCode::Char('g') if letters => Some(0),
        KeyCode::Char('G') if letters => Some(last),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(last),
        KeyCode::PageDown => Some((selected + visible_height).min(last)),
        KeyCode::PageUp => Some(selected.saturating_sub(visible_height)),
        _ => None,
    }
}

/// Whether `key` carries neither Control nor Alt (Shift is allowed).
pub(super) fn is_plain_key(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Applies Backspace or a plain typed character to a picker filter and
/// returns whether the key edited it.
pub(super) fn edit_picker_filter(filter: &mut String, key: KeyEvent) -> bool {
    let key = super::normalize_shifted_key(key);
    match key.code {
        KeyCode::Backspace => {
            filter.pop();
            true
        }
        KeyCode::Char(character) if is_plain_key(key) => {
            filter.push(character);
            true
        }
        _ => false,
    }
}

/// Waits briefly for the next key press in a modal picker. Resizes request a
/// redraw; with `wheel` set, mouse-wheel scrolling maps to Up/Down.
pub(super) fn next_modal_picker_key(
    redraw: &mut bool,
    wheel: bool,
) -> Result<Option<KeyEvent>, AppError> {
    if !event::poll(Duration::from_millis(50))? {
        return Ok(None);
    }
    let key = match read_event()? {
        Event::Key(key) => key,
        Event::Mouse(MouseEvent { kind, .. }) if wheel => match kind {
            MouseEventKind::ScrollUp => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            MouseEventKind::ScrollDown => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            _ => return Ok(None),
        },
        Event::Resize(_, _) => {
            *redraw = true;
            return Ok(None);
        }
        _ => return Ok(None),
    };
    Ok((key.kind == KeyEventKind::Press).then_some(key))
}

/// Fits a modal picker terminal to the screen and returns its list height.
pub(super) fn modal_picker_list_height<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
) -> io::Result<usize> {
    terminal.autoresize()?;
    let area = terminal.size()?;
    Ok(usize::from(
        picker_rect(area.into()).height.saturating_sub(4).max(1),
    ))
}

pub(super) fn filter_document_links(links: &[DocumentLink], filter: &str) -> Vec<usize> {
    let terms: Vec<_> = filter
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    if terms.is_empty() {
        return (0..links.len()).collect();
    }
    let mut haystack = String::new();
    links
        .iter()
        .enumerate()
        .filter_map(|(index, link)| {
            use std::fmt::Write as _;
            haystack.clear();
            match &link.target {
                LinkTarget::Internal { page, .. } => {
                    let _ = write!(
                        haystack,
                        "{} source page {} pdf page {}",
                        link.label,
                        link.source_page + 1,
                        page + 1
                    );
                }
                LinkTarget::Uri(uri) => {
                    let _ = write!(
                        haystack,
                        "{} source page {} {}",
                        link.label,
                        link.source_page + 1,
                        uri
                    );
                }
            }
            if haystack.is_ascii() {
                haystack.make_ascii_lowercase();
            } else {
                haystack = haystack.to_lowercase();
            }
            terms
                .iter()
                .all(|term| haystack.contains(term))
                .then_some(index)
        })
        .collect()
}

pub(super) fn scroll_into_view(selected: usize, scroll_offset: &mut usize, visible_height: usize) {
    let visible_height = visible_height.max(1);
    if selected < *scroll_offset {
        *scroll_offset = selected;
    } else if selected >= *scroll_offset + visible_height {
        *scroll_offset = selected + 1 - visible_height;
    }
}

fn draw_modal_block(
    frame: &mut RatatuiFrame,
    popup: Rect,
    title: &str,
    colors: PickerTheme,
) -> Rect {
    frame.render_widget(
        Block::default().style(Style::default().bg(colors.backdrop)),
        frame.area(),
    );
    frame.render_widget(RatatuiClear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(colors.border))
        .title(format!(" {title} "))
        .title_style(
            Style::default()
                .fg(colors.accent)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

fn draw_modal_picker_chrome(
    frame: &mut RatatuiFrame,
    title: &str,
    filter: PickerFilter<'_>,
    bindings: &[(&str, &str)],
    status: Option<(String, RatatuiColor)>,
    theme: Palette,
) -> (Rect, PickerTheme) {
    let colors = PickerTheme::from(theme);
    let inner = draw_modal_block(frame, picker_rect(frame.area()), title, colors);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let filter_line = if filter.query.is_empty() && !filter.active {
        Line::from(Span::styled(
            " / to filter",
            Style::default().fg(colors.muted).bg(colors.chrome),
        ))
    } else {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(colors.accent).bg(colors.chrome)),
            Span::styled(
                filter.query,
                Style::default().fg(colors.text).bg(colors.chrome),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(filter_line).style(Style::default().bg(colors.chrome)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(picker_hint_line(bindings, status, colors))
            .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
    (rows[1], colors)
}

pub(super) fn pick_outline(
    items: &[OutlineItem],
    current_page: u32,
    output: &mut impl Write,
    theme: Palette,
    interrupt: impl Fn() -> io::Result<bool>,
) -> Result<Option<u32>, AppError> {
    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut buffer = Vec::new();
    let start_index = outline_start_index(items, current_page);
    run_filtered_modal_picker(
        output,
        interrupt,
        FilteredModalOptions {
            clear_theme: Some(theme),
            wheel: true,
            edge: ListEdge::Clamp,
            initial_selected: start_index,
            refilter: &mut |filter| filter_outline_with(items, filter, &mut matcher, &mut buffer),
            on_escape_clear: &mut |_, _| start_index,
            draw: &mut |frame, filtered, selected, scroll_offset, pf| {
                draw_outline(frame, items, filtered, selected, scroll_offset, pf, theme)
            },
            on_enter: &mut |filtered, selected| {
                filtered.get(selected).map(|i| Some(items[*i].page))
            },
        },
    )
}

pub(super) fn outline_start_index(items: &[OutlineItem], current_page: u32) -> usize {
    items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| item.page <= current_page)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn filter_outline_with(
    items: &[OutlineItem],
    filter: &str,
    matcher: &mut nucleo_matcher::Matcher,
    buffer: &mut Vec<char>,
) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    use nucleo_matcher::Utf32Str;
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);
    let mut scored: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let haystack = Utf32Str::new(&item.title, buffer);
            pattern.score(haystack, matcher).map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
pub(super) fn filter_outline(items: &[OutlineItem], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut buffer = Vec::new();
    filter_outline_with(items, filter, &mut matcher, &mut buffer)
}

pub(super) fn draw_outline(
    frame: &mut RatatuiFrame,
    items: &[OutlineItem],
    filtered: &[usize],
    selected: usize,
    scroll_offset: usize,
    filter: PickerFilter<'_>,
    theme: Palette,
) {
    let (list_area, colors) = draw_modal_picker_chrome(
        frame,
        "Outline",
        filter,
        &[
            ("j/k/wheel", "select"),
            ("^b/^f", "page"),
            ("g/G", "ends"),
            ("/", "filter"),
            ("enter", "jump"),
            ("esc", if filter.active { "clear" } else { "close" }),
        ],
        None,
        theme,
    );
    let visible_height = usize::from(list_area.height);
    let width = usize::from(list_area.width);
    let mut lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(position, item_index)| {
            let item = &items[*item_index];
            let is_selected = position == selected;
            let style = Style::default()
                .fg(colors.text)
                .bg(row_bg(colors, is_selected));
            let indent = "  ".repeat(usize::from(item.depth).min(6) + 1);
            let page_label = format!(" {} ", item.page + 1);
            let mut line = Line::from(vec![
                Span::styled(if is_selected { "▌" } else { " " }, style.fg(colors.accent)),
                Span::styled(indent, style),
                Span::styled(item.title.clone(), style.add_modifier(Modifier::BOLD)),
            ]);
            let used = line.width();
            let page_width = page_label.chars().count();
            if used + page_width < width {
                let page_style = if is_selected {
                    style
                } else {
                    Style::default().fg(colors.muted).bg(colors.surface)
                };
                line.spans
                    .push(Span::styled(" ".repeat(width - used - page_width), style));
                line.spans.push(Span::styled(page_label, page_style));
            }
            line
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No matches",
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        list_area,
    );
}

pub(super) fn apply_picker_navigation(
    browser: &mut BrowserState,
    key: KeyEvent,
    visible_height: usize,
    filtering: bool,
) -> bool {
    let Some(next) = picker_navigation_index(
        browser.selected,
        browser.filtered_indices.len(),
        key,
        visible_height,
        !filtering,
        ListEdge::Clamp,
    ) else {
        return false;
    };
    browser.selected = next;
    true
}

pub(super) fn clear_picker_filter(browser: &mut BrowserState) -> bool {
    if browser.filter.is_empty() {
        return false;
    }
    browser.filter.clear();
    browser.rebuild_filter();
    browser.select_first();
    browser.scroll_offset = 0;
    true
}

pub(super) fn draw_picker(
    frame: &mut RatatuiFrame,
    browser: &BrowserState,
    filtering: bool,
    theme: Palette,
) {
    let entries: Vec<_> = browser.filtered_entries().collect();
    let colors = PickerTheme::from(theme);
    let status = if browser.recursive_loading() {
        Some((format!("scan • {}", entries.len()), colors.loading))
    } else {
        let position = usize::from(!entries.is_empty()) * (browser.selected + 1);
        Some((format!("{position}/{}", entries.len()), colors.muted))
    };
    let escape_action = if filtering || !browser.filter.is_empty() {
        "clear"
    } else {
        "close"
    };
    let directory = shorten_path(&browser.current_dir.display().to_string());
    let (list_area, colors) = draw_modal_picker_chrome(
        frame,
        &directory,
        PickerFilter {
            query: &browser.filter,
            active: filtering,
        },
        &[
            ("j/k", ""),
            ("^b/^f", "page"),
            ("g/G", "ends"),
            ("/", "filter"),
            ("enter", "open"),
            ("esc", escape_action),
        ],
        status,
        theme,
    );

    let visible_height = usize::from(list_area.height);
    let recent_heading_index = browser.recent_heading_index();
    let mut lines = Vec::with_capacity(visible_height);
    for (index, entry) in entries.iter().enumerate().skip(browser.scroll_offset) {
        if Some(index) == recent_heading_index && lines.len() + 1 < visible_height {
            lines.push(picker_recent_heading_line(
                usize::from(list_area.width),
                colors,
            ));
        }
        if lines.len() >= visible_height {
            break;
        }
        lines.push(picker_entry_line(
            entry,
            browser,
            index == browser.selected,
            usize::from(list_area.width),
            colors,
        ));
        if lines.len() >= visible_height {
            break;
        }
    }
    if lines.is_empty() {
        let message = if browser.filter.is_empty() {
            "   No PDF files found"
        } else {
            "   No matches"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(colors.muted).bg(colors.surface),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        list_area,
    );
}

fn side_picker_areas(
    area: Rect,
    geometry: LinkPickerGeometry,
    include_detail: bool,
) -> (LinkPickerLayout, Rect, Borders, [Rect; 4]) {
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let (_, pane) = link_picker_panes(area, geometry);
    let borders = match layout {
        LinkPickerLayout::Vertical => Borders::LEFT,
        LinkPickerLayout::Horizontal => Borders::TOP,
        LinkPickerLayout::Floating => Borders::ALL,
        LinkPickerLayout::Auto => unreachable!("auto layout is resolved above"),
    };
    let inner = Block::default().borders(borders).inner(pane);
    let detail_height = if include_detail {
        link_picker_detail_height(inner.height)
    } else {
        0
    };
    let footer_height = u16::from(layout == LinkPickerLayout::Floating);
    let split = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(detail_height),
        Constraint::Length(footer_height),
    ])
    .split(inner);
    (
        layout,
        pane,
        borders,
        [split[0], split[1], split[2], split[3]],
    )
}

fn side_picker_colors(theme: Palette) -> PickerTheme {
    let mut colors = PickerTheme::from(theme);
    colors.surface = picker_color(theme.bg_dark);
    colors.chrome = picker_color(theme.bg_dark1);
    colors
}

fn draw_side_picker_chrome(
    frame: &mut RatatuiFrame,
    area: Rect,
    geometry: LinkPickerGeometry,
    focused: bool,
    include_detail: bool,
    colors: PickerTheme,
    header: Vec<Span<'static>>,
) -> (LinkPickerLayout, [Rect; 4]) {
    let (layout, pane, borders, rows) = side_picker_areas(area, geometry, include_detail);
    frame.render_widget(RatatuiClear, pane);
    let block = Block::default()
        .borders(borders)
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .border_style(Style::default().fg(if focused {
            colors.accent
        } else {
            colors.border
        }));
    frame.render_widget(block, pane);
    frame.render_widget(
        Paragraph::new(Line::from(header))
            .style(Style::default().bg(colors.chrome))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(colors.border)),
            ),
        rows[0],
    );
    (layout, rows)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SearchPickerDisplayRow {
    Section(usize),
    Match(usize),
    Context(usize),
}

pub(super) fn append_search_picker_display_rows(
    rows: &mut Vec<SearchPickerDisplayRow>,
    matches: &[SearchPageMatch],
    start_index: usize,
    outline: &[OutlineItem],
) {
    let mut previous_section = start_index
        .checked_sub(1)
        .and_then(|prev| matches.get(prev))
        .and_then(|result| link_picker_section_index(outline, result.page));
    for (offset, result) in matches[start_index..].iter().enumerate() {
        let index = start_index + offset;
        let section = link_picker_section_index(outline, result.page);
        if section != previous_section {
            if let Some(section) = section {
                rows.push(SearchPickerDisplayRow::Section(section));
            }
            previous_section = section;
        }
        rows.push(SearchPickerDisplayRow::Match(index));
        if !result.context.is_empty() {
            rows.push(SearchPickerDisplayRow::Context(index));
        }
    }
}

fn picker_row_window(
    selected_row: usize,
    len: usize,
    visible_height: usize,
    divisor: usize,
) -> (usize, usize) {
    let height = visible_height.max(1).min(len);
    let start = selected_row
        .saturating_sub(height / divisor)
        .min(len.saturating_sub(height));
    (start, start + height)
}

pub(super) fn search_picker_row_window(
    rows: &[SearchPickerDisplayRow],
    selected: usize,
    visible_height: usize,
) -> (usize, usize) {
    let row = rows
        .iter()
        .position(|r| matches!(r, SearchPickerDisplayRow::Match(i) if *i == selected))
        .unwrap_or(0);
    picker_row_window(row, rows.len(), visible_height, 3)
}

fn row_bg(colors: PickerTheme, selected: bool) -> RatatuiColor {
    if selected {
        colors.selection
    } else {
        colors.surface
    }
}

fn selection_marker(colors: PickerTheme, selected: bool) -> Span<'static> {
    Span::styled(
        if selected { "▌ " } else { "  " },
        Style::default()
            .fg(colors.accent)
            .bg(row_bg(colors, selected)),
    )
}

fn side_picker_title_span(
    title: &'static str,
    focused: bool,
    colors: PickerTheme,
) -> Span<'static> {
    Span::styled(
        title,
        Style::default()
            .fg(if focused { colors.accent } else { colors.muted })
            .bg(colors.chrome)
            .add_modifier(Modifier::BOLD),
    )
}

fn push_unfocused_hint(header: &mut Vec<Span<'static>>, hint: &'static str, colors: PickerTheme) {
    header.push(Span::styled(
        hint,
        Style::default()
            .fg(colors.loading)
            .bg(colors.chrome)
            .add_modifier(Modifier::BOLD),
    ));
}

fn render_floating_footer(
    frame: &mut RatatuiFrame,
    area: Rect,
    bindings: &[(&str, &str)],
    status: String,
    colors: PickerTheme,
) {
    frame.render_widget(
        Paragraph::new(picker_hint_line(
            bindings,
            Some((status, colors.muted)),
            colors,
        ))
        .style(Style::default().bg(colors.chrome)),
        area,
    );
}

pub(super) fn draw_search_picker(
    frame: &mut RatatuiFrame,
    area: Rect,
    search: &SearchState,
    outline: &[OutlineItem],
    state: &SearchPickerState,
    geometry: LinkPickerGeometry,
    theme: Palette,
) {
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let results_focused =
        state.focus == LinkPickerFocus::Links || layout == LinkPickerLayout::Floating;
    let colors = side_picker_colors(theme);
    let mut header = vec![
        side_picker_title_span(" Search ", results_focused, colors),
        Span::styled(
            truncated_search_query(&search.query, 36),
            Style::default().fg(colors.text).bg(colors.chrome),
        ),
        Span::styled(
            format!(
                "  · {} hits on {} pages",
                search.total_occurrences,
                search.matches.len()
            ),
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
    ];
    if search.searching {
        header.push(Span::styled(
            format!("  scanning {}/{}", search.scanned, search.total_pages),
            Style::default().fg(colors.loading).bg(colors.chrome),
        ));
    }
    if !results_focused {
        push_unfocused_hint(&mut header, "  PDF focused  · l: results", colors);
    }
    let (layout, rows) = draw_side_picker_chrome(
        frame,
        area,
        geometry,
        results_focused,
        false,
        colors,
        header,
    );

    if search.matches.is_empty() {
        let message = if search.searching {
            "Scanning document text…"
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(colors.muted).bg(colors.surface)),
            rows[1],
        );
    } else {
        let display_rows = state.display_rows(search, outline);
        let (start, end) = search_picker_row_window(
            &display_rows,
            state.selected,
            usize::from(rows[1].height).max(1),
        );
        let width = usize::from(rows[1].width);
        let number_width = search.matches.len().to_string().len().max(1);
        let lines: Vec<_> = display_rows[start..end]
            .iter()
            .map(|row| match row {
                SearchPickerDisplayRow::Section(index) => {
                    link_picker_section_heading_line(&outline[*index].title, width, colors)
                }
                SearchPickerDisplayRow::Match(index) => search_picker_match_line(
                    *index,
                    &search.matches[*index],
                    state,
                    number_width,
                    width,
                    results_focused,
                    colors,
                ),
                SearchPickerDisplayRow::Context(index) => search_picker_context_line(
                    *index,
                    &search.matches[*index].context,
                    state,
                    width,
                    results_focused,
                    colors,
                ),
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(colors.surface)),
            rows[1],
        );
    }

    if layout == LinkPickerLayout::Floating {
        let position = usize::from(!search.matches.is_empty()) * (state.selected + 1);
        render_floating_footer(
            frame,
            rows[3],
            &[
                ("j/k C-b/f g/G", "navigate"),
                ("/", "new search"),
                ("enter", "jump"),
                ("s/a", "layout"),
                ("esc", "close"),
            ],
            format!("{position}/{}", search.matches.len()),
            colors,
        );
    }
}

pub(super) fn search_picker_match_line(
    index: usize,
    result: &SearchPageMatch,
    state: &SearchPickerState,
    number_width: usize,
    width: usize,
    focused: bool,
    colors: PickerTheme,
) -> Line<'static> {
    let selected = focused && index == state.selected;
    let background = row_bg(colors, selected);
    let hits = if result.occurrences == 1 {
        "hit"
    } else {
        "hits"
    };
    let current = if result.page == state.page {
        " · current"
    } else {
        ""
    };
    let label = format!(
        "{:>number_width$}  Page {} · {} {hits}{current}",
        index + 1,
        result.page + 1,
        result.occurrences
    );
    Line::from(vec![
        selection_marker(colors, selected),
        Span::styled(
            truncate_right(&label, width.saturating_sub(2)),
            Style::default()
                .fg(colors.text)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

pub(super) fn search_picker_context_line(
    index: usize,
    context: &str,
    state: &SearchPickerState,
    width: usize,
    focused: bool,
    colors: PickerTheme,
) -> Line<'static> {
    let background = row_bg(colors, focused && index == state.selected);
    Line::from(vec![
        Span::styled("    ", Style::default().bg(background)),
        Span::styled(
            truncate_right(context, width.saturating_sub(4)),
            Style::default().fg(colors.text_dim).bg(background),
        ),
    ])
}

pub(super) fn draw_link_picker(
    frame: &mut RatatuiFrame,
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    progress: LinkIndexProgress,
    geometry: LinkPickerGeometry,
    theme: Palette,
) {
    let LinkPickerDocument { links, outline, .. } = document;
    let selected = state.selected;
    let page = state.page;
    let layout = resolved_link_picker_layout(area, geometry.layout);
    let links_focused =
        state.focus == LinkPickerFocus::Links || layout == LinkPickerLayout::Floating;
    let cached_rows = state.rows(document);
    let filtered = &cached_rows.filtered;
    let selected_position = filtered.iter().position(|index| *index == selected);
    let colors = side_picker_colors(theme);

    let count_text = if state.filter.is_empty() {
        links.len().to_string()
    } else {
        format!("{}/{}", filtered.len(), links.len())
    };
    let mut header = vec![
        side_picker_title_span(" Links ", links_focused, colors),
        Span::styled(
            count_text,
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
        Span::styled(
            format!("  · {}", link_picker_layout_status(area, geometry.layout)),
            Style::default().fg(colors.muted).bg(colors.chrome),
        ),
    ];
    if !links_focused {
        push_unfocused_hint(&mut header, "  PDF focused  · l: links", colors);
    }
    if progress.indexing {
        header.push(Span::styled(
            format!("  indexing {}/{}", progress.scanned, progress.total_pages),
            Style::default().fg(colors.loading).bg(colors.chrome),
        ));
    }
    if state.filtering || !state.filter.is_empty() {
        header.push(Span::styled(
            "  / ",
            Style::default()
                .fg(colors.accent)
                .bg(colors.chrome)
                .add_modifier(Modifier::BOLD),
        ));
        let prompt = if state.filter.is_empty() {
            "type to filter…"
        } else {
            state.filter.as_str()
        };
        header.push(Span::styled(
            prompt.to_string(),
            Style::default().fg(colors.text).bg(colors.chrome),
        ));
    }
    let (layout, rows) =
        draw_side_picker_chrome(frame, area, geometry, links_focused, true, colors, header);

    if filtered.is_empty() {
        let message = if progress.indexing {
            format!(
                "Indexing document links · {}/{} pages",
                progress.scanned, progress.total_pages
            )
        } else if !state.filter.is_empty() {
            format!("No links match ‘{}’", state.filter)
        } else {
            "No annotated links in this document".to_string()
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(colors.muted).bg(colors.surface)),
            rows[1],
        );
    } else {
        let display_rows = &cached_rows.display;
        let visible_height = usize::from(rows[1].height).max(1);
        let (start, end) = link_picker_row_window(display_rows, selected, visible_height);
        let width = usize::from(rows[1].width);
        let number_width = filtered.len().to_string().len().max(1);
        let lines: Vec<_> = display_rows[start..end]
            .iter()
            .map(|display_row| match display_row {
                LinkPickerDisplayRow::Section(index) => {
                    link_picker_section_heading_line(&outline[*index].title, width, colors)
                }
                LinkPickerDisplayRow::Page(source_page) => {
                    link_picker_page_heading_line(*source_page, *source_page == page, width, colors)
                }
                LinkPickerDisplayRow::Link { index, ordinal } => link_picker_entry_line(
                    *ordinal,
                    *index,
                    &links[*index],
                    if links_focused { selected } else { usize::MAX },
                    number_width,
                    width,
                    colors,
                ),
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(colors.surface)),
            rows[1],
        );
    }

    if rows[2].height > 0
        && let Some(position) = selected_position
        && let Some(link) = links.get(selected)
    {
        draw_link_picker_detail(frame, rows[2], link, position, colors);
    }

    if layout == LinkPickerLayout::Floating {
        let status = if filtered.is_empty() {
            if state.filter.is_empty() {
                "no links"
            } else {
                "no matches"
            }
            .to_string()
        } else {
            format!("{}/{}", selected_position.unwrap_or(0) + 1, filtered.len())
        };
        let action = match selected_position
            .and_then(|_| links.get(selected))
            .map(|link| &link.target)
        {
            Some(LinkTarget::Uri(_)) => "copy URL",
            _ => "jump",
        };
        let escape_action = if state.filtering || !state.filter.is_empty() {
            "clear"
        } else {
            "close"
        };
        let compact = [("j/k", ""), ("/", ""), ("#", ""), ("↵", ""), ("esc", "")];
        let medium = [
            ("j/k C-b/f g/G", ""),
            ("/", ""),
            ("#", ""),
            ("enter", action),
            ("esc", ""),
        ];
        let full = [
            ("j/k C-b/f g/G", "navigate"),
            ("/", "filter"),
            ("#", "select"),
            ("enter", action),
            ("s/a", "layout"),
            ("esc", escape_action),
        ];
        let bindings = if rows[3].width < 48 {
            compact.as_slice()
        } else if rows[3].width < 110 {
            medium.as_slice()
        } else {
            full.as_slice()
        };
        render_floating_footer(frame, rows[3], bindings, status, colors);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LinkPickerDisplayRow {
    Section(usize),
    Page(u32),
    Link { index: usize, ordinal: usize },
}

pub(super) fn link_picker_display_rows(
    links: &[DocumentLink],
    filtered: &[usize],
    outline: &[OutlineItem],
) -> Vec<LinkPickerDisplayRow> {
    let mut rows = Vec::with_capacity(filtered.len().saturating_mul(2));
    let mut previous_section = None;
    let mut previous_page = None;
    for (ordinal, index) in filtered.iter().copied().enumerate() {
        let link = &links[index];
        let section = link_picker_section_index(outline, link.source_page);
        if section != previous_section {
            if let Some(section) = section {
                rows.push(LinkPickerDisplayRow::Section(section));
            }
            previous_section = section;
            previous_page = None;
        }
        if previous_page != Some(link.source_page) {
            rows.push(LinkPickerDisplayRow::Page(link.source_page));
            previous_page = Some(link.source_page);
        }
        rows.push(LinkPickerDisplayRow::Link { index, ordinal });
    }
    rows
}

pub(super) fn link_picker_link_at_position(
    area: Rect,
    document: LinkPickerDocument<'_>,
    state: &LinkPickerState,
    geometry: LinkPickerGeometry,
    column: u16,
    row: u16,
) -> Option<usize> {
    let list_area = link_picker_list_area(area, geometry);
    if column < list_area.x
        || column >= list_area.x.saturating_add(list_area.width)
        || row < list_area.y
        || row >= list_area.y.saturating_add(list_area.height)
    {
        return None;
    }
    let cached_rows = state.rows(document);
    let display_rows = &cached_rows.display;
    let (start, end) = link_picker_row_window(
        display_rows,
        state.selected,
        usize::from(list_area.height).max(1),
    );
    let offset = usize::from(row.saturating_sub(list_area.y));
    match display_rows
        .get(start + offset)
        .filter(|_| start + offset < end)
    {
        Some(LinkPickerDisplayRow::Link { index, .. }) => Some(*index),
        _ => None,
    }
}

pub(super) fn link_picker_list_area(area: Rect, geometry: LinkPickerGeometry) -> Rect {
    side_picker_areas(area, geometry, true).3[1]
}

pub(super) fn link_picker_section_index(outline: &[OutlineItem], page: u32) -> Option<usize> {
    outline
        .iter()
        .enumerate()
        .filter(|(_, item)| item.page <= page)
        .max_by_key(|(index, item)| (item.page, *index))
        .map(|(index, _)| index)
}

fn ruled_heading_line(
    spans: Vec<Span<'static>>,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let mut line = Line::from(spans);
    let used = line.width();
    if used < width {
        line.spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(colors.border).bg(colors.surface),
        ));
    }
    line
}

fn pad_line(mut line: Line<'static>, width: usize, bg: RatatuiColor) -> Line<'static> {
    let used = line.width();
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    line
}

pub(super) fn link_picker_row_window(
    rows: &[LinkPickerDisplayRow],
    selected: usize,
    visible_height: usize,
) -> (usize, usize) {
    let row = rows
        .iter()
        .position(|r| matches!(r, LinkPickerDisplayRow::Link { index, .. } if *index == selected))
        .unwrap_or(0);
    picker_row_window(row, rows.len(), visible_height, 2)
}

pub(super) fn link_picker_page_heading_line(
    page: u32,
    current: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let label = if current {
        format!(" Page {} · current ", page + 1)
    } else {
        format!(" Page {} ", page + 1)
    };
    let fg = if current { colors.recent } else { colors.muted };
    ruled_heading_line(
        vec![
            Span::styled("  ", Style::default().bg(colors.surface)),
            Span::styled(
                label,
                Style::default()
                    .fg(fg)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        width,
        colors,
    )
}

pub(super) fn link_picker_section_heading_line(
    title: &str,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let prefix = " Section  ";
    let label = truncate_right(title, width.saturating_sub(Line::raw(prefix).width() + 1));
    let bold = Style::default()
        .bg(colors.surface)
        .add_modifier(Modifier::BOLD);
    ruled_heading_line(
        vec![
            Span::styled(prefix, bold.fg(colors.loading)),
            Span::styled(format!("{label} "), bold.fg(colors.text)),
        ],
        width,
        colors,
    )
}

pub(super) fn link_picker_entry_line(
    ordinal: usize,
    index: usize,
    link: &DocumentLink,
    selected: usize,
    number_width: usize,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let is_selected = index == selected;
    let bg = if is_selected {
        colors.selection
    } else {
        colors.surface
    };
    let style = Style::default().fg(colors.text).bg(bg);
    let number = format!("{:>number_width$}  ", ordinal + 1);
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(2 + Line::raw(number.as_str()).width()),
    );
    pad_line(
        Line::from(vec![
            Span::styled(
                if is_selected { "▌ " } else { "  " },
                style.fg(colors.accent),
            ),
            Span::styled(number, style.fg(colors.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                label,
                if is_selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            ),
        ]),
        width,
        bg,
    )
}

pub(super) fn draw_link_picker_detail(
    frame: &mut RatatuiFrame,
    area: Rect,
    link: &DocumentLink,
    selected: usize,
    colors: PickerTheme,
) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(colors.border))
        .style(Style::default().bg(colors.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = usize::from(inner.width);
    let number = format!("{}  ", selected + 1);
    let label = truncate_right(
        &link_picker_label(&link.label),
        width.saturating_sub(Line::raw(number.as_str()).width()),
    );
    let target = match &link.target {
        LinkTarget::Internal { page, .. } => format!("PDF page {}", page + 1),
        LinkTarget::Uri(uri) => uri.clone(),
    };
    let source = format!("Page {}", link.source_page + 1);
    let fixed_width = Line::raw(source.as_str()).width() + 3;
    let target = truncate_right(&target, width.saturating_sub(fixed_width));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                number,
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label,
                Style::default()
                    .fg(colors.text)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                source,
                Style::default().fg(colors.recent).bg(colors.surface),
            ),
            Span::styled(" → ", Style::default().fg(colors.muted).bg(colors.surface)),
            Span::styled(
                target,
                Style::default().fg(colors.text_dim).bg(colors.surface),
            ),
        ]),
    ];
    let mut lines = lines;
    let truncate_context = inner.height <= 4;
    if let Some(context) = link.source_context.as_deref() {
        lines.push(link_picker_context_line(
            "Context",
            context,
            width,
            truncate_context,
            colors.loading,
            colors,
        ));
    }
    if let Some(context) = link.reference_context.as_deref() {
        lines.push(link_picker_context_line(
            "Reference",
            context,
            width,
            truncate_context,
            colors.recent,
            colors,
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

pub(super) fn link_picker_context_line(
    label: &'static str,
    context: &str,
    width: usize,
    truncate: bool,
    label_color: RatatuiColor,
    colors: PickerTheme,
) -> Line<'static> {
    let label = format!("{label}  ");
    let context = if truncate {
        truncate_right(
            context,
            width.saturating_sub(Line::raw(label.as_str()).width()),
        )
    } else {
        context.to_string()
    };
    Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .bg(colors.surface)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(context, Style::default().fg(colors.text).bg(colors.surface)),
    ])
}

pub(super) fn link_picker_label(label: &str) -> String {
    let citation = label.trim_matches(|character: char| {
        character.is_whitespace() || matches!(character, '[' | ']' | ',' | ';')
    });
    if !citation.is_empty() && citation.chars().all(|character| character.is_ascii_digit()) {
        format!("citation [{citation}]")
    } else {
        label.to_string()
    }
}

pub(super) fn draw_help_menu(frame: &mut RatatuiFrame, theme: Palette) {
    const NAVIGATION: &[(&str, &str)] = &[
        ("j/k · ↑/↓", "scroll vertically"),
        ("J / K", "next / previous page"),
        ("h/l · ←/→", "move horizontally"),
        ("d/u · ^d/^u · ^f/^b", "page viewport down / up"),
        ("Space/PgDn · Bksp/PgUp", "page viewport down / up"),
        ("g / G · [N]G / [N]gg", "first / last / page N"),
        (":", "go to page"),
        ("/", "search document"),
        ("n / N", "next / prev match"),
        ("Mouse wheel", "scroll vertically"),
        ("Enter", "browse PDF links"),
        ("Click", "follow PDF link"),
        ("h / l (split)", "focus PDF/pane"),
        ("/ (pane)", "filter/new search"),
        ("Ctrl-b/f/d/u (pane)", "page results"),
        ("g / G (pane)", "first/last"),
        ("Tab / Shift-Tab", "switch tabs"),
        ("Alt-1 … Alt-9", "select tab"),
    ];
    const VIEWER: &[(&str, &str)] = &[
        ("m", "fit page / width / height / content"),
        ("+ / -", "zoom in / out"),
        ("0", "reset zoom"),
        ("i", "toggle dark mode"),
        ("x", "find visible PDF text + SyncTeX jump"),
        ("Alt/Option-click", "word jump + focus"),
        ("S", "toggle smooth scroll"),
        ("p", "performance timings"),
        ("t", "table of contents"),
        ("T", "choose theme"),
        ("y", "copy page text"),
        ("L", "link mode + browser"),
        ("s / a", "cycle / auto layout"),
        ("b / B / ^o / ^i", "jump back / forward"),
        ("f", "open PDF in new tab"),
        ("D", "duplicate tab at current view"),
        ("R", "reload config"),
        ("q", "leave mode / close tab"),
        ("Esc", "leave mode / clear / exit"),
        ("?", "open help"),
    ];

    let colors = PickerTheme::from(theme);
    let area = frame.area();
    let popup = if area.width < 110 {
        area
    } else {
        picker_rect(area)
    };
    let inner = draw_modal_block(frame, popup, "Help", colors);
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if inner.width < 80 { 10 } else { 6 }),
        Constraint::Length(1),
    ])
    .split(inner);
    let columns =
        if inner.width < 90 && usize::from(rows[0].height) >= NAVIGATION.len() + VIEWER.len() + 4 {
            Layout::vertical([
                Constraint::Length(NAVIGATION.len() as u16 + 2),
                Constraint::Min(1),
            ])
            .split(rows[0])
        } else {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(2)
                .split(rows[0])
        };

    frame.render_widget(
        Paragraph::new(help_lines("Navigation", NAVIGATION, 18, colors))
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(help_lines("Viewer", VIEWER, 18, colors))
            .style(Style::default().bg(colors.surface))
            .wrap(Wrap { trim: true }),
        columns[1],
    );
    let path = crate::config::config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "HOME is unset".into());
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Config: {path}")),
            Line::from("[viewer]: smooth_scroll, scroll_frame_ms, scroll_ease_divisor"),
            Line::from("[viewer]: continuous_scroll, prefetch_pages, set_window_title, center_forward_search"),
            Line::from("[viewer]: flash_duration_ms, flash_label_font, word_precision, source_context_lines"),
            Line::from("[viewer]: flash_label_foreground, flash_label_background (#RRGGBB)"),
            Line::from("[nvim]: focus_on_forward, focus_on_inverse, compile; [nvim.keys]: editor keys"),
            Line::from("Commented defaults on first launch. Press R to reload config."),
        ])
        .style(Style::default().bg(colors.surface).fg(colors.text))
        .wrap(Wrap { trim: true }),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(picker_hint_line(&[("? / esc / q", "close")], None, colors))
            .style(Style::default().bg(colors.chrome)),
        rows[2],
    );
}

pub(super) fn help_lines(
    title: &'static str,
    bindings: &[(&str, &str)],
    key_width: usize,
    colors: PickerTheme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(bindings.len() + 2);
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(colors.directory)
            .bg(colors.surface)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(bindings.iter().map(|(key, action)| {
        Line::from(vec![
            Span::styled(
                format!(" {key:<key_width$}"),
                Style::default()
                    .fg(colors.accent)
                    .bg(colors.selection)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {action}"),
                Style::default().fg(colors.text).bg(colors.surface),
            ),
        ])
    }));
    lines
}

fn filter_theme_indices_lower(lower_names: &[String], filter: &str) -> Vec<usize> {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return (0..lower_names.len()).collect();
    }
    lower_names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| name.contains(&filter).then_some(index))
        .collect()
}

#[cfg(test)]
pub(super) fn filter_theme_indices(themes: &[(String, Palette)], filter: &str) -> Vec<usize> {
    let lower_names: Vec<String> = themes.iter().map(|(name, _)| name.to_lowercase()).collect();
    filter_theme_indices_lower(&lower_names, filter)
}

pub(super) fn draw_theme_picker(
    frame: &mut RatatuiFrame,
    themes: &[(String, Palette)],
    filtered: &[usize],
    selected: usize,
    scroll_offset: usize,
    filter: &str,
    filtering: bool,
) {
    let selected_theme = filtered.get(selected).copied().unwrap_or(0);
    let theme = themes
        .get(selected_theme)
        .map_or(crate::theme::TOKYO_NIGHT_MOON, |(_, theme)| *theme);
    let colors = PickerTheme::from(theme);
    let status = Some((
        format!(
            "{}/{}",
            usize::from(!filtered.is_empty()) * (selected + 1),
            filtered.len()
        ),
        colors.muted,
    ));
    let (list_area, colors) = draw_modal_picker_chrome(
        frame,
        "Themes",
        PickerFilter {
            query: filter,
            active: filtering,
        },
        &[
            ("j/k", "select"),
            ("^b/^f", "page"),
            ("g/G", "ends"),
            ("/", "filter"),
            ("enter", "apply"),
            ("esc", if filtering { "clear" } else { "cancel" }),
        ],
        status,
        theme,
    );

    let visible_height = usize::from(list_area.height).max(1);
    let mut first_visible = scroll_offset;
    scroll_into_view(selected, &mut first_visible, visible_height);
    let width = usize::from(list_area.width);
    let lines: Vec<_> = filtered
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_height)
        .map(|(position, index)| {
            let is_selected = position == selected;
            let bg = row_bg(colors, is_selected);
            let mut style = Style::default().fg(colors.text).bg(bg);
            if is_selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            pad_line(
                Line::from(vec![
                    selection_marker(colors, is_selected),
                    Span::styled(themes[*index].0.clone(), style),
                ]),
                width,
                bg,
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(colors.surface)),
        list_area,
    );
}

pub(super) fn picker_recent_heading_line(width: usize, colors: PickerTheme) -> Line<'static> {
    ruled_heading_line(
        vec![
            Span::styled("  ", Style::default().bg(colors.surface)),
            Span::styled(
                " Most Recent ",
                Style::default()
                    .fg(colors.recent)
                    .bg(colors.surface)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        width,
        colors,
    )
}

/// Display width of one character, measured without a heap allocation.
pub(super) fn char_width(character: char) -> usize {
    // `Line::raw` splits on line breaks, so they never contribute width there.
    if matches!(character, '\n' | '\r') {
        return 0;
    }
    let mut buffer = [0u8; 4];
    Span::raw(&*character.encode_utf8(&mut buffer)).width()
}

/// Appends `character` to `spans`, extending the last span when it already
/// carries `style` so runs of identically styled characters share one span.
pub(super) fn push_styled_char(spans: &mut Vec<Span<'static>>, character: char, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
        && let Cow::Owned(content) = &mut last.content
    {
        content.push(character);
        return;
    }
    spans.push(Span::styled(String::from(character), style));
}

fn truncate_with_ellipsis(value: &str, max_width: usize, from_end: bool) -> String {
    if Line::raw(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width.saturating_sub(1);
    let mut kept = Vec::new();
    let mut used = 0;
    let mut iter: Box<dyn Iterator<Item = char>> = if from_end {
        Box::new(value.chars().rev())
    } else {
        Box::new(value.chars())
    };
    for ch in &mut iter {
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        kept.push(ch);
        used += w;
    }
    if from_end {
        kept.reverse();
        format!("…{}", kept.into_iter().collect::<String>())
    } else {
        let mut out: String = kept.into_iter().collect();
        out.push('…');
        out
    }
}

pub(super) fn truncate_left(value: &str, max_width: usize) -> String {
    truncate_with_ellipsis(value, max_width, true)
}

pub(super) fn truncate_right(value: &str, max_width: usize) -> String {
    truncate_with_ellipsis(value, max_width, false)
}

pub(super) fn picker_entry_line(
    entry: &BrowserEntry,
    browser: &BrowserState,
    selected: bool,
    width: usize,
    colors: PickerTheme,
) -> Line<'static> {
    let background = row_bg(colors, selected);
    let icon = if entry.name == "../" {
        "↑ "
    } else if entry.is_dir {
        "› "
    } else {
        "  "
    };
    let icon_color = if entry.name == "../" {
        colors.text_dim
    } else if entry.is_dir {
        colors.directory
    } else if entry.is_recent {
        colors.recent
    } else {
        colors.text
    };
    let mut spans = vec![
        selection_marker(colors, selected),
        Span::styled(icon, Style::default().fg(icon_color).bg(background)),
    ];

    let display_name = if browser.filter.is_empty() || entry.name == "../" {
        entry.name.clone()
    } else {
        let mut name = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        if entry.is_dir && !name.ends_with('/') {
            name.push('/');
        }
        name
    };
    let matches = browser.match_indices(&display_name);
    let basename_start = if entry.is_dir || !browser.filter.is_empty() {
        0
    } else {
        display_name
            .char_indices()
            .rev()
            .find(|(_, character)| *character == '/')
            .map_or(0, |(index, _)| display_name[..=index].chars().count())
    };
    for (index, character) in display_name.chars().enumerate() {
        let foreground = if matches.binary_search(&index).is_ok() {
            colors.matched
        } else if index < basename_start || entry.name == "../" {
            colors.text_dim
        } else if entry.is_recent {
            colors.recent
        } else if entry.is_dir {
            colors.directory
        } else {
            colors.text
        };
        let mut style = Style::default().fg(foreground).bg(background);
        if matches.binary_search(&index).is_ok() || (selected && index >= basename_start) {
            style = style.add_modifier(Modifier::BOLD);
        }
        push_styled_char(&mut spans, character, style);
    }

    let mut line = Line::from(spans);
    let mut used = line.width();
    let source = (!browser.filter.is_empty())
        .then(|| browser.entry_source(entry))
        .flatten();
    let show_parent = entry.is_recent || source == Some(BrowserEntrySource::Subdirectory);
    let parent = show_parent
        .then(|| entry.path.parent())
        .flatten()
        .map(|parent| shorten_path(&parent.to_string_lossy()));
    if entry.is_recent || source.is_some() {
        let available = width.saturating_sub(used + 2);
        let label = source.map(BrowserEntrySource::label);
        let label_width = label.map_or(0, |label| Line::raw(label).width());
        let separator_width = usize::from(label.is_some() && parent.is_some()) * 2;
        if available >= label_width + separator_width + usize::from(parent.is_some()) {
            let parent_width = available.saturating_sub(label_width + separator_width);
            let parent = parent.map(|parent| truncate_left(&parent, parent_width));
            let context_width = label_width
                + separator_width
                + parent
                    .as_deref()
                    .map_or(0, |parent| Line::raw(parent).width());
            let gap = width.saturating_sub(used + context_width);
            line.spans.push(Span::styled(
                " ".repeat(gap),
                Style::default().bg(background),
            ));
            if let Some(label) = label {
                let foreground = match source {
                    Some(BrowserEntrySource::Recent) => colors.recent,
                    Some(BrowserEntrySource::Here) => colors.loading,
                    Some(BrowserEntrySource::Subdirectory) => colors.directory,
                    None => colors.muted,
                };
                line.spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(foreground)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if label.is_some() && parent.is_some() {
                line.spans
                    .push(Span::styled("  ", Style::default().bg(background)));
            }
            if let Some(parent) = parent {
                let matches = browser.match_indices(&parent);
                for (index, character) in parent.chars().enumerate() {
                    let matched = matches.binary_search(&index).is_ok();
                    let mut style = Style::default()
                        .fg(if matched {
                            colors.matched
                        } else {
                            colors.text_dim
                        })
                        .bg(background);
                    if matched {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    push_styled_char(&mut line.spans, character, style);
                }
            }
            used = width;
        }
    }
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(background),
        ));
    }
    line
}

pub(super) fn picker_hint_line(
    bindings: &[(&str, &str)],
    status: Option<(String, RatatuiColor)>,
    colors: PickerTheme,
) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", Style::default().bg(colors.chrome))];
    for (key, action) in bindings {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(colors.accent)
                .bg(colors.selection)
                .add_modifier(Modifier::BOLD),
        ));
        if !action.is_empty() {
            spans.push(Span::styled(
                format!(" {action}  "),
                Style::default().fg(colors.muted).bg(colors.chrome),
            ));
        }
    }
    if let Some((status, color)) = status {
        spans.push(Span::styled(
            status,
            Style::default().fg(color).bg(colors.chrome),
        ));
    }
    Line::from(spans)
}

pub(super) fn picker_rect(area: Rect) -> Rect {
    let width = if area.width > 4 {
        (area.width * 3 / 4).max(50).min(area.width - 4)
    } else {
        area.width.max(1)
    };
    let height = if area.height > 4 {
        (area.height * 3 / 4).max(6).min(area.height - 2)
    } else {
        area.height.max(1)
    };
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(super) fn link_picker_detail_height(height: u16) -> u16 {
    if height >= 24 {
        8
    } else if height >= 14 {
        6
    } else if height >= 9 {
        5
    } else {
        0
    }
}

pub(super) fn link_picker_visible_height(area: Rect, geometry: LinkPickerGeometry) -> usize {
    usize::from(side_picker_areas(area, geometry, true).3[1].height.max(1))
}

pub(super) fn search_picker_visible_height(area: Rect, geometry: LinkPickerGeometry) -> usize {
    usize::from(side_picker_areas(area, geometry, false).3[1].height.max(1))
}

pub(super) fn shorten_path_with_home(path: &str, home: Option<&Path>) -> String {
    let Some(home) = home.filter(|home| {
        !home.as_os_str().is_empty() && home.is_absolute() && home.parent().is_some()
    }) else {
        return path.to_string();
    };
    let Ok(rest) = Path::new(path).strip_prefix(home) else {
        return path.to_string();
    };
    if rest.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", rest.display())
    }
}

pub(super) fn shorten_path(path: &str) -> String {
    shorten_path_with_home(path, crate::config::home_dir().as_deref())
}

pub(super) fn picker_color(color: crossterm::style::Color) -> RatatuiColor {
    match color {
        crossterm::style::Color::Rgb { r, g, b } => RatatuiColor::Rgb(r, g, b),
        _ => RatatuiColor::Reset,
    }
}

#[derive(Clone, Copy)]
pub(super) struct PickerTheme {
    backdrop: RatatuiColor,
    surface: RatatuiColor,
    chrome: RatatuiColor,
    selection: RatatuiColor,
    border: RatatuiColor,
    accent: RatatuiColor,
    directory: RatatuiColor,
    recent: RatatuiColor,
    matched: RatatuiColor,
    loading: RatatuiColor,
    text: RatatuiColor,
    text_dim: RatatuiColor,
    muted: RatatuiColor,
}

impl From<Palette> for PickerTheme {
    fn from(theme: Palette) -> Self {
        Self {
            backdrop: picker_color(theme.bg_dark1),
            surface: picker_color(theme.bg),
            chrome: picker_color(theme.bg_dark),
            selection: picker_color(theme.bg_highlight),
            border: picker_color(theme.blue7),
            accent: picker_color(theme.blue),
            directory: picker_color(theme.blue1),
            recent: picker_color(theme.yellow),
            matched: picker_color(theme.magenta),
            loading: picker_color(theme.cyan),
            text: picker_color(theme.fg),
            text_dim: picker_color(theme.fg_dark),
            muted: picker_color(theme.comment),
        }
    }
}

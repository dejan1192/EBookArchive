//! Search book catalogs and download what you pick.
//!
//!   cargo run
//!
//! Type a query, Enter to search. Tab/Down moves to the results, Space marks
//! books, `d` downloads the marked ones. `/` returns to the search box.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Style, Stylize},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};
use throbber_widgets_tui::{BRAILLE_EIGHT_DOUBLE, Throbber, ThrobberState};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use tui::source::{self, Book, Format, SearchResults, Source};

const DOWNLOAD_DIR: &str = "downloads";
const TICK: Duration = Duration::from_millis(80);

/// Work finished on a background task, on its way back to the UI.
enum Msg {
    Results {
        generation: u64,
        source: &'static str,
        result: Result<SearchResults, String>,
    },
    Progress {
        generation: u64,
        idx: usize,
        seen: u64,
        total: Option<u64>,
    },
    Done {
        generation: u64,
        idx: usize,
        path: PathBuf,
    },
    Failed {
        generation: u64,
        idx: usize,
        error: String,
    },
}

#[derive(Debug, Clone)]
enum Status {
    Idle,
    /// Task spawned, response not started streaming yet.
    Queued,
    Downloading {
        seen: u64,
        total: Option<u64>,
    },
    Done(PathBuf),
    Failed(String),
}

struct Entry {
    book: Book,
    marked: bool,
    status: Status,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Search,
    Browse,
    /// Picking which catalogs to query.
    Sources,
    /// Browsing books already present in the download directory.
    Library,
}

struct LibraryEntry {
    path: PathBuf,
    size: u64,
}

/// A catalog plus whether the user wants it searched.
struct SourceEntry {
    source: Arc<dyn Source>,
    enabled: bool,
}

struct App {
    rt: Handle,
    client: reqwest::Client,
    sources: Vec<SourceEntry>,
    source_cursor: usize,
    tx: UnboundedSender<Msg>,
    rx: UnboundedReceiver<Msg>,

    input: Input,
    mode: Mode,
    mode_before_sources: Mode,
    entries: Vec<Entry>,
    selected: usize,
    page: usize,
    total_pages: Option<usize>,
    source_total_pages: HashMap<&'static str, usize>,

    /// Bumped on every new search, so replies from an abandoned one are dropped.
    generation: u64,
    pending: usize,
    spinner: ThrobberState,
    status: String,
    library: Vec<LibraryEntry>,
    library_selected: usize,
    exit: bool,
}

impl App {
    fn new(rt: Handle) -> Result<Self> {
        let (tx, rx) = unbounded_channel();
        Ok(Self {
            rt,
            client: reqwest::Client::builder()
                .user_agent(concat!("tui-book-search/", env!("CARGO_PKG_VERSION")))
                .build()?,
            sources: source::all()
                .into_iter()
                .map(|source| SourceEntry {
                    source,
                    enabled: true,
                })
                .collect(),
            source_cursor: 0,
            tx,
            rx,
            input: Input::default(),
            mode: Mode::Search,
            mode_before_sources: Mode::Search,
            entries: Vec::new(),
            selected: 0,
            page: 1,
            total_pages: None,
            source_total_pages: HashMap::new(),
            generation: 0,
            pending: 0,
            spinner: ThrobberState::default(),
            status: "type a title or author, then press Enter".to_string(),
            library: Vec::new(),
            library_selected: 0,
            exit: false,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last = Instant::now();
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;

            // Never block longer than a frame: background tasks need the loop
            // to come back around so their messages get drained.
            if event::poll(TICK.saturating_sub(last.elapsed()))? {
                let ev = event::read()?;
                self.handle_event(&ev);
            }
            self.drain();

            if last.elapsed() >= TICK {
                self.spinner.calc_next();
                last = Instant::now();
            }
        }
        Ok(())
    }

    async fn close_sources(&self) {
        for entry in &self.sources {
            let _ = entry.source.close().await;
        }
    }

    // ---- input -----------------------------------------------------------

    fn handle_event(&mut self, ev: &Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.exit = true;
            return;
        }
        if (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s'))
            || key.code == KeyCode::F(2)
        {
            self.open_sources();
            return;
        }
        if key.code == KeyCode::Tab && self.mode != Mode::Sources {
            if self.mode == Mode::Library {
                self.mode = Mode::Search;
                self.status = "type a title or author, then press Enter".to_string();
            } else {
                self.open_library();
            }
            return;
        }

        match self.mode {
            Mode::Search => match key.code {
                KeyCode::Enter => self.start_search(),
                KeyCode::Esc | KeyCode::Down => {
                    if !self.entries.is_empty() {
                        self.mode = Mode::Browse;
                    }
                }
                // tui-input owns the rest: text, backspace, arrows, word jumps.
                _ => {
                    self.input.handle_event(ev);
                }
            },
            Mode::Browse => match key.code {
                KeyCode::Char('/') | KeyCode::Char('i') => self.mode = Mode::Search,
                KeyCode::Char('s') => self.open_sources(),
                KeyCode::Char('o') => self.open_selected(),
                KeyCode::Char('q') | KeyCode::Esc => self.exit = true,
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Right | KeyCode::Char('n') => self.change_page(1),
                KeyCode::Left | KeyCode::Char('p') => self.change_page(-1),
                KeyCode::Char(' ') => self.toggle(),
                KeyCode::Char('d') | KeyCode::Enter => self.download_marked(),
                _ => {}
            },
            Mode::Sources => match key.code {
                KeyCode::Esc | KeyCode::Char('s') | KeyCode::Enter | KeyCode::Tab => {
                    self.mode = self.mode_before_sources
                }
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h') => {
                    self.source_cursor = self.source_cursor.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j') | KeyCode::Char('l') => {
                    self.source_cursor =
                        (self.source_cursor + 1).min(self.sources.len().saturating_sub(1))
                }
                KeyCode::Char(' ') => {
                    if let Some(entry) = self.sources.get_mut(self.source_cursor) {
                        entry.enabled = !entry.enabled;
                    }
                }
                KeyCode::Char('q') => self.exit = true,
                _ => {}
            },
            Mode::Library => match key.code {
                KeyCode::Esc | KeyCode::Char('/') => self.mode = Mode::Search,
                KeyCode::Char('q') => self.exit = true,
                KeyCode::Down | KeyCode::Char('j') => self.step_library(1),
                KeyCode::Up | KeyCode::Char('k') => self.step_library(-1),
                KeyCode::Enter | KeyCode::Char('o') => self.open_library_selected(),
                KeyCode::Char('r') => self.open_library(),
                _ => {}
            },
        }
    }

    /// Open the highlighted download in the installed desktop book reader.
    fn open_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        let Status::Done(path) = &entry.status else {
            self.status = "download it first (d)".to_string();
            return;
        };
        match launch_reader(path) {
            Ok(reader) => self.status = format!("opened with {reader}"),
            Err(e) => self.status = format!("could not open: {e}"),
        }
    }

    fn open_library(&mut self) {
        self.mode = Mode::Library;
        self.library = scan_library(Path::new(DOWNLOAD_DIR));
        self.library_selected = self
            .library_selected
            .min(self.library.len().saturating_sub(1));
        self.status = match self.library.len() {
            0 => format!("no ebooks in {DOWNLOAD_DIR}/"),
            1 => "1 downloaded book".to_string(),
            count => format!("{count} downloaded books"),
        };
    }

    fn step_library(&mut self, delta: isize) {
        if self.library.is_empty() {
            return;
        }
        self.library_selected = (self.library_selected as isize + delta)
            .rem_euclid(self.library.len() as isize) as usize;
    }

    fn open_library_selected(&mut self) {
        let Some(entry) = self.library.get(self.library_selected) else {
            return;
        };
        match launch_reader(&entry.path) {
            Ok(reader) => self.status = format!("opened with {reader}"),
            Err(e) => self.status = format!("could not open: {e}"),
        }
    }

    fn step(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    fn toggle(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.selected) {
            entry.marked = !entry.marked;
        }
    }

    fn open_sources(&mut self) {
        if self.mode != Mode::Sources {
            self.mode_before_sources = self.mode;
            self.mode = Mode::Sources;
        }
    }

    // ---- background work -------------------------------------------------

    fn start_search(&mut self) {
        self.page = 1;
        self.total_pages = None;
        self.source_total_pages.clear();
        self.load_page();
    }

    fn change_page(&mut self, delta: isize) {
        let next = self.page.saturating_add_signed(delta).max(1);
        if next == self.page || self.total_pages.is_some_and(|total| next > total) {
            return;
        }
        self.page = next;
        self.load_page();
    }

    fn load_page(&mut self) {
        let query = self.input.value().trim().to_string();
        if query.is_empty() {
            self.status = "type something first".to_string();
            return;
        }

        let enabled: Vec<Arc<dyn Source>> = self
            .sources
            .iter()
            .filter(|s| s.enabled)
            .filter(|s| {
                self.source_total_pages
                    .get(s.source.name())
                    .is_none_or(|total| self.page <= *total)
            })
            .map(|s| Arc::clone(&s.source))
            .collect();
        if enabled.is_empty() {
            self.status = "no sources enabled — press s".to_string();
            return;
        }

        self.generation += 1;
        let generation = self.generation;
        let page = self.page;
        self.entries.clear();
        self.selected = 0;
        self.pending = enabled.len();
        self.status = format!("searching for \"{query}\" — page {page}");

        for src in enabled {
            let client = self.client.clone();
            let tx = self.tx.clone();
            let query = query.clone();
            self.rt.spawn(async move {
                let result = src
                    .search(&client, &query, page, 25)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(Msg::Results {
                    generation,
                    source: src.name(),
                    result,
                });
            });
        }
    }

    fn download_marked(&mut self) {
        let prefer = [Format::Epub, Format::Pdf, Format::Txt];

        // Marked entries, or just the highlighted one if nothing is marked.
        let targets: Vec<usize> = match self.entries.iter().any(|e| e.marked) {
            true => self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.marked)
                .map(|(i, _)| i)
                .collect(),
            false if self.entries.is_empty() => Vec::new(),
            false => vec![self.selected],
        };
        if targets.is_empty() {
            self.status = "nothing to download".to_string();
            return;
        }

        // Collect everything the tasks need before mutating any entry, so the
        // borrows don't overlap.
        let mut jobs = Vec::new();
        for idx in targets {
            let entry = &self.entries[idx];
            if matches!(entry.status, Status::Downloading { .. }) {
                continue;
            }
            let Some(download) = entry.book.preferred(&prefer).cloned() else {
                continue;
            };
            let Some(src) = self
                .sources
                .iter()
                .find(|s| s.source.name() == entry.book.source)
                .map(|s| Arc::clone(&s.source))
            else {
                continue;
            };
            jobs.push((idx, entry.book.clone(), download, src));
        }

        let generation = self.generation;
        let started = jobs.len();
        for (idx, book, download, src) in jobs {
            self.entries[idx].status = Status::Queued;
            let client = self.client.clone();
            let tx = self.tx.clone();
            self.rt.spawn(async move {
                let progress_tx = tx.clone();
                let result = src
                    .fetch(
                        &client,
                        &book,
                        &download,
                        Path::new(DOWNLOAD_DIR),
                        &move |seen, total| {
                            let _ = progress_tx.send(Msg::Progress {
                                generation,
                                idx,
                                seen,
                                total,
                            });
                        },
                    )
                    .await;
                let _ = match result {
                    Ok(path) => tx.send(Msg::Done {
                        generation,
                        idx,
                        path,
                    }),
                    Err(e) => tx.send(Msg::Failed {
                        generation,
                        idx,
                        error: e.to_string(),
                    }),
                };
            });
        }
        self.status = format!("downloading {started} file(s) into {DOWNLOAD_DIR}/");
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Results {
                    generation,
                    source,
                    result,
                } => {
                    if generation != self.generation {
                        continue; // reply from a search the user moved on from
                    }
                    self.pending = self.pending.saturating_sub(1);
                    match result {
                        Ok(results) => {
                            if let Some(total) = results.total_pages {
                                self.source_total_pages.insert(source, total);
                            }
                            self.entries
                                .extend(results.books.into_iter().map(|book| Entry {
                                    book,
                                    marked: false,
                                    status: Status::Idle,
                                }));
                        }
                        Err(error) => self.status = format!("{source}: {error}"),
                    }
                    if self.pending == 0 {
                        self.total_pages = self.exact_total_pages();
                        if self.entries.is_empty()
                            && self.total_pages.is_some_and(|total| self.page > total)
                        {
                            self.page = self.total_pages.unwrap_or(1).max(1);
                            self.load_page();
                            continue;
                        }
                        self.status = match self.entries.len() {
                            0 => "no results".to_string(),
                            n => format!(
                                "{n} result(s) — page {}{} — Space marks, d downloads",
                                self.page,
                                self.total_pages
                                    .map(|total| format!("/{total}"))
                                    .unwrap_or_default()
                            ),
                        };
                        if !self.entries.is_empty() && self.mode == Mode::Search {
                            self.mode = Mode::Browse;
                        }
                    }
                }
                Msg::Progress {
                    generation,
                    idx,
                    seen,
                    total,
                } => {
                    if generation == self.generation
                        && let Some(entry) = self.entries.get_mut(idx)
                    {
                        entry.status = Status::Downloading { seen, total };
                    }
                }
                Msg::Done {
                    generation,
                    idx,
                    path,
                } => {
                    if generation == self.generation
                        && let Some(entry) = self.entries.get_mut(idx)
                    {
                        entry.status = Status::Done(path);
                    }
                }
                Msg::Failed {
                    generation,
                    idx,
                    error,
                } => {
                    if generation == self.generation
                        && let Some(entry) = self.entries.get_mut(idx)
                    {
                        entry.status = Status::Failed(error);
                    }
                }
            }
        }
    }

    // ---- rendering -------------------------------------------------------

    fn draw(&mut self, frame: &mut Frame) {
        if self.mode == Mode::Library {
            self.draw_library(frame);
            return;
        }

        let [tabs_area, search_area, sources_area, status_area, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(frame.area());
        self.draw_tabs(frame, tabs_area, false);

        // -- search box, with the real terminal cursor inside it
        let focused = self.mode == Mode::Search;
        let block = Block::bordered()
            .title(" Search ")
            .border_set(border::ROUNDED)
            .border_style(if focused {
                Style::new().cyan()
            } else {
                Style::new().dark_gray()
            });
        let inner = block.inner(search_area);
        let width = inner.width.max(1) as usize;
        let scroll = self.input.visual_scroll(width.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(self.input.value())
                .scroll((0, scroll as u16))
                .block(block),
            search_area,
        );
        if focused {
            let x = self.input.visual_cursor().saturating_sub(scroll) as u16;
            frame.set_cursor_position((inner.x + x, inner.y));
        }

        // -- source checkboxes: which catalogs get queried
        let picking = self.mode == Mode::Sources;
        let mut chips: Vec<Span> = vec!["Sources ".dark_gray()];
        for (i, entry) in self.sources.iter().enumerate() {
            let text = format!(
                "{}{} ",
                if entry.enabled { "[x] " } else { "[ ] " },
                entry.source.name()
            );
            chips.push(match (picking && i == self.source_cursor, entry.enabled) {
                (true, _) => text.black().on_cyan(),
                (false, true) => text.green(),
                (false, false) => text.dark_gray(),
            });
        }
        chips.push(
            if picking {
                " Space toggles, Esc done"
            } else if self.mode == Mode::Search {
                " (Ctrl+S/F2 to change)"
            } else {
                " (s to change)"
            }
            .dark_gray(),
        );
        frame.render_widget(Paragraph::new(Line::from(chips)), sources_area);

        // -- status line, with a spinner while any source is still in flight
        let mut spans: Vec<Span> = Vec::new();
        if let Some(phase) = self.phase() {
            spans.push(self.spin(Style::new().cyan().bold()));
            spans.push(phase.cyan().bold());
            spans.push("  ".into());
        }
        spans.push(self.status.clone().dark_gray());
        frame.render_widget(Paragraph::new(Line::from(spans)), status_area);

        // -- results
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| ListItem::new(self.row(e)))
            .collect();
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(Line::from(" Results ".bold()).centered())
                    .title_bottom(
                        Line::from(vec![
                            " Search ".into(),
                            "</>".blue().bold(),
                            " Mark ".into(),
                            "<Space>".blue().bold(),
                            " Download ".into(),
                            "<D>".blue().bold(),
                            " Open ".into(),
                            "<O>".blue().bold(),
                            " Quit ".into(),
                            "<Q> ".blue().bold(),
                            format!(
                                " Page {}{} ",
                                self.page,
                                self.total_pages
                                    .map(|total| format!("/{total}"))
                                    .unwrap_or_default()
                            )
                            .into(),
                            "[p previous] ".blue().bold(),
                            "[n next] ".blue().bold(),
                        ])
                        .centered(),
                    )
                    .border_set(border::THICK),
            )
            .highlight_symbol("> ")
            .highlight_style(Style::new().bold());
        let mut state =
            ListState::default().with_selected((!self.entries.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    /// One frame of the braille spinner, styled.
    fn spin(&self, style: Style) -> Span<'static> {
        Throbber::default()
            .throbber_set(BRAILLE_EIGHT_DOUBLE)
            .throbber_style(style)
            .to_symbol_span(&self.spinner)
    }

    /// What the app is waiting on right now, if anything.
    fn phase(&self) -> Option<String> {
        if self.pending > 0 {
            return Some(match self.entries.is_empty() {
                true => "Searching...".to_string(),
                false => "Waiting for the response...".to_string(),
            });
        }
        let active = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Queued | Status::Downloading { .. }))
            .count();
        (active > 0).then(|| match active {
            1 => "Downloading...".to_string(),
            n => format!("Downloading {n} books..."),
        })
    }

    fn draw_tabs(&self, frame: &mut Frame, area: ratatui::layout::Rect, library: bool) {
        let search = if library {
            " Search ".dark_gray()
        } else {
            " Search ".black().on_cyan().bold()
        };
        let downloads = if library {
            " Downloads ".black().on_cyan().bold()
        } else {
            " Downloads ".dark_gray()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                search,
                "  ".into(),
                downloads,
                "    <Tab> switch".dark_gray(),
            ])),
            area,
        );
    }

    fn draw_library(&mut self, frame: &mut Frame) {
        let [tabs_area, status_area, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(frame.area());
        self.draw_tabs(frame, tabs_area, true);
        frame.render_widget(Paragraph::new(self.status.clone().dark_gray()), status_area);

        let items = self.library.iter().map(|entry| {
            let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
            let extension = entry
                .path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("?")
                .to_ascii_uppercase();
            ListItem::new(Line::from(vec![
                name.into_owned().into(),
                format!("  [{extension}] ").blue(),
                human_size(entry.size).dark_gray(),
            ]))
        });
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(Line::from(" Downloaded books ".bold()).centered())
                    .title_bottom(
                        Line::from(vec![
                            " Open ".into(),
                            "<Enter/O>".blue().bold(),
                            " Refresh ".into(),
                            "<R>".blue().bold(),
                            " Search ".into(),
                            "<Tab> ".blue().bold(),
                        ])
                        .centered(),
                    )
                    .border_set(border::THICK),
            )
            .highlight_symbol("> ")
            .highlight_style(Style::new().bold());
        let mut state = ListState::default()
            .with_selected((!self.library.is_empty()).then_some(self.library_selected));
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    fn row<'a>(&self, entry: &'a Entry) -> Line<'a> {
        let mut spans = vec![
            if entry.marked {
                "[x] ".green()
            } else {
                "[ ] ".into()
            },
            format!("[{}] ", entry.book.source).yellow().bold(),
            entry.book.title.as_str().into(),
            format!("  {}", entry.book.author_line()).dark_gray(),
        ];
        if let Some(language) = &entry.book.language {
            spans.push(format!("  {language}").magenta());
        }

        match &entry.status {
            Status::Idle => {
                let mut seen = Vec::new();
                let formats = entry
                    .book
                    .downloads
                    .iter()
                    .filter_map(|download| {
                        if seen.contains(&download.format) {
                            return None;
                        }
                        seen.push(download.format.clone());
                        Some(match download.size {
                            Some(size) => {
                                format!("{} {}", download.format, human_size(size))
                            }
                            None => download.format.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                spans.push(format!("  [{}]", formats.join(",")).blue());
            }
            Status::Queued => {
                spans.push("  ".into());
                spans.push(self.spin(Style::new().yellow()));
                spans.push("Waiting for the response...".yellow());
            }
            Status::Downloading { seen, total } => {
                spans.push("  ".into());
                spans.push(self.spin(Style::new().yellow()));
                spans.push(
                    match total {
                        Some(t) if *t > 0 => format!("Downloading... {}%", seen * 100 / t),
                        _ => format!("Downloading... {seen} B"),
                    }
                    .yellow(),
                );
            }
            Status::Done(path) => spans.push(
                format!(
                    "  Saved {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
                .green(),
            ),
            Status::Failed(error) => spans.push(format!("  Failed: {error}").red()),
        }

        Line::from(spans)
    }

    fn exact_total_pages(&self) -> Option<usize> {
        let enabled = self.sources.iter().filter(|entry| entry.enabled);
        let totals = enabled
            .map(|entry| self.source_total_pages.get(entry.source.name()).copied())
            .collect::<Option<Vec<_>>>()?;
        totals.into_iter().max()
    }
}

fn scan_library(directory: &Path) -> Vec<LibraryEntry> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut books = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            (metadata.is_file() && is_ebook(&path)).then_some(LibraryEntry {
                path,
                size: metadata.len(),
            })
        })
        .collect::<Vec<_>>();
    books.sort_by_cached_key(|entry| {
        entry
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase()
    });
    books
}

fn is_ebook(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "epub" | "pdf" | "txt" | "html" | "htm" | "mobi" | "azw3" | "djvu" | "fb2"
            )
        })
}

fn launch_reader(path: &Path) -> Result<String> {
    let mut readers = std::env::var("TUI_BOOK_READER")
        .ok()
        .filter(|reader| !reader.trim().is_empty())
        .into_iter()
        .collect::<Vec<_>>();
    let foliate_format = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| matches!(extension.as_str(), "epub" | "mobi" | "azw3" | "fb2"));
    if foliate_format && !readers.iter().any(|reader| reader == "foliate") {
        readers.push("foliate".to_string());
    }
    if !readers.iter().any(|reader| reader == "xdg-open") {
        readers.push("xdg-open".to_string());
    }

    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut failures = Vec::new();
    for reader in readers {
        match Command::new(&reader)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(reader),
            Err(error) => failures.push(format!("{reader}: {error}")),
        }
    }
    anyhow::bail!("no desktop reader available ({})", failures.join(", "))
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[("GB", 1_000_000_000), ("MB", 1_000_000), ("kB", 1_000)];
    for (unit, value) in UNITS {
        if bytes >= *value {
            return format!("{:.1} {unit}", bytes as f64 / *value as f64);
        }
    }
    format!("{bytes} B")
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let mut app = App::new(runtime.handle().clone())?;
    let result = ratatui::run(|terminal| app.run(terminal));
    runtime.block_on(app.close_sources());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use tui::source::Download;

    fn book(title: &str, author: &str) -> Book {
        Book {
            source: "gutenberg",
            id: "11".into(),
            title: title.into(),
            authors: vec![author.into()],
            language: None,
            downloads: vec![Download {
                format: Format::Epub,
                url: "https://example.invalid/x.epub".into(),
                size: Some(188_960),
            }],
        }
    }

    #[test]
    fn renders() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();

        app.input = Input::from("alice");
        app.mode = Mode::Browse;
        app.status = "3 result(s) — Space marks, d downloads".into();
        app.entries = vec![
            Entry {
                book: book("Alice's Adventures in Wonderland", "Carroll, Lewis"),
                marked: true,
                status: Status::Downloading {
                    seen: 94_480,
                    total: Some(188_960),
                },
            },
            Entry {
                book: book("Through the Looking-Glass", "Carroll, Lewis"),
                marked: true,
                status: Status::Queued,
            },
            Entry {
                book: book("Alice's Adventures Under Ground", "Carroll, Lewis"),
                marked: false,
                status: Status::Done("downloads/alice.epub".into()),
            },
        ];
        app.selected = 1;

        let mut term = Terminal::new(TestBackend::new(84, 13)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        println!("{}", term.backend());
    }

    #[test]
    fn renders_downloaded_books_tab_and_filters_non_ebooks() {
        assert!(is_ebook(Path::new("book.EPUB")));
        assert!(is_ebook(Path::new("book.pdf")));
        assert!(!is_ebook(Path::new("book.bin")));
        assert!(!is_ebook(Path::new("cover.jpg")));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();
        app.mode = Mode::Library;
        app.status = "1 downloaded book".into();
        app.library = vec![LibraryEntry {
            path: PathBuf::from("downloads/Marsovac.epub"),
            size: 374_890,
        }];

        let mut term = Terminal::new(TestBackend::new(70, 8)).unwrap();
        term.draw(|frame| app.draw(frame)).unwrap();
        let rendered = format!("{}", term.backend());
        assert!(rendered.contains("Downloads"));
        assert!(rendered.contains("Marsovac.epub"));
        assert!(rendered.contains("EPUB"));
        assert!(rendered.contains("374.9 kB"));
    }
}

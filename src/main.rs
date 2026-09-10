//! Search book catalogs and download what you pick.
//!
//!   cargo run
//!
//! Type a query, Enter to search. Tab/Down moves to the results, Space marks
//! books, `d` downloads the marked ones. `/` returns to the search box.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::Style,
    symbols::border,
    text::{Line, Span},
    widgets::{
        Block, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use serde::{Deserialize, Serialize};
use throbber_widgets_tui::{BRAILLE_EIGHT_DOUBLE, Throbber, ThrobberState};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use tui::source::{self, Book, Format, SearchResults, Source};

mod theme;

const DEFAULT_DOWNLOAD_DIR: &str = "downloads";
const TICK: Duration = Duration::from_millis(80);
const RESULTS_PER_PAGE: usize = 50;

#[derive(Debug, Deserialize, Serialize)]
struct Preferences {
    #[serde(default)]
    disabled_sources: Vec<String>,
    #[serde(default = "default_download_dir")]
    download_dir: String,
    #[serde(default)]
    google_drive_remote: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            disabled_sources: Vec::new(),
            download_dir: default_download_dir(),
            google_drive_remote: String::new(),
        }
    }
}

fn default_download_dir() -> String {
    DEFAULT_DOWNLOAD_DIR.to_string()
}

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
    Uploading {
        generation: u64,
        idx: usize,
    },
    Done {
        generation: u64,
        idx: usize,
        path: PathBuf,
        drive: Option<Result<String, String>>,
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
    Uploading,
    Done(PathBuf),
    Failed(String),
}

struct Entry {
    book: Book,
    marked: bool,
    status: Status,
    /// When the download was queued, for the rate and ETA readouts.
    started: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Search,
    Browse,
    /// Picking which catalogs to query.
    Sources,
    /// Browsing books already present in the download directory.
    Library,
    /// Editing persistent storage settings.
    Settings,
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
    preferences_path: Option<PathBuf>,
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
    /// Animation tick, bumped once per frame; drives the indeterminate bar.
    frame: u64,
    status: String,
    library: Vec<LibraryEntry>,
    library_selected: usize,
    results_area: Rect,
    results_offset: usize,
    library_area: Rect,
    library_offset: usize,
    download_dir_input: Input,
    drive_remote_input: Input,
    settings_cursor: usize,
    rclone_available: bool,
    exit: bool,
}

impl App {
    fn new(rt: Handle) -> Result<Self> {
        let (tx, rx) = unbounded_channel();
        let preferences_path = preferences_path();
        let preferences = preferences_path
            .as_deref()
            .and_then(|path| load_preferences(path).ok())
            .unwrap_or_default();
        Ok(Self {
            rt,
            client: reqwest::Client::builder()
                .user_agent(concat!("tui-book-search/", env!("CARGO_PKG_VERSION")))
                .build()?,
            sources: source::all()
                .into_iter()
                .map(|source| SourceEntry {
                    enabled: !preferences
                        .disabled_sources
                        .iter()
                        .any(|name| name == source.name()),
                    source,
                })
                .collect(),
            preferences_path,
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
            frame: 0,
            status: "type a title or author, then press Enter".to_string(),
            library: Vec::new(),
            library_selected: 0,
            results_area: Rect::default(),
            results_offset: 0,
            library_area: Rect::default(),
            library_offset: 0,
            download_dir_input: Input::from(preferences.download_dir),
            drive_remote_input: Input::from(preferences.google_drive_remote),
            settings_cursor: 0,
            rclone_available: command_available("rclone"),
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
                self.frame = self.frame.wrapping_add(1);
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
        if let Event::Mouse(mouse) = ev {
            self.handle_mouse(*mouse);
            return;
        }
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
        if key.code == KeyCode::F(3) {
            self.open_settings();
            return;
        }
        if key.code == KeyCode::Tab && self.mode != Mode::Sources {
            self.next_tab();
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
                    if let Err(error) = self.save_preferences() {
                        self.status = format!("could not save source settings: {error}");
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
            Mode::Settings => match key.code {
                KeyCode::Esc => {
                    if self.commit_settings() {
                        self.mode = Mode::Search;
                    }
                }
                KeyCode::Enter => {
                    self.commit_settings();
                }
                KeyCode::Up | KeyCode::Char('k') => self.settings_cursor = 0,
                KeyCode::Down | KeyCode::Char('j') => self.settings_cursor = 1,
                KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.exit = true
                }
                _ => {
                    if self.settings_cursor == 0 {
                        self.download_dir_input.handle_event(ev);
                    } else {
                        self.drive_remote_input.handle_event(ev);
                    }
                }
            },
        }
    }

    fn next_tab(&mut self) {
        match self.mode {
            Mode::Library => self.open_settings(),
            Mode::Settings => {
                if self.commit_settings() {
                    self.mode = Mode::Search;
                    self.status = "type a title or author, then press Enter".to_string();
                }
            }
            _ => self.open_library(),
        }
    }

    fn open_settings(&mut self) {
        self.mode = Mode::Settings;
        self.status = "edit a field, then press Enter to save".to_string();
    }

    fn commit_settings(&mut self) -> bool {
        let directory = self.download_dir_input.value().trim();
        if directory.is_empty() {
            self.status = "download folder cannot be empty".to_string();
            return false;
        }
        let directory = expand_tilde(directory);
        if let Err(error) = fs::create_dir_all(&directory) {
            self.status = format!("could not create {}: {error}", directory.display());
            return false;
        }
        let drive = self.drive_remote_input.value().trim();
        if !drive.is_empty() && !drive.contains(':') {
            self.status = "Google Drive remote must look like gdrive:EBookArchive".to_string();
            return false;
        }
        self.download_dir_input = Input::from(directory.to_string_lossy().into_owned());
        match self.save_preferences() {
            Ok(()) => {
                self.status = "settings saved".to_string();
                true
            }
            Err(error) => {
                self.status = format!("could not save settings: {error}");
                false
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => match self.mode {
                Mode::Library => self.step_library_clamped(3),
                Mode::Browse | Mode::Search if !self.entries.is_empty() => {
                    self.mode = Mode::Browse;
                    self.step_clamped(3);
                }
                _ => {}
            },
            MouseEventKind::ScrollUp => match self.mode {
                Mode::Library => self.step_library_clamped(-3),
                Mode::Browse | Mode::Search if !self.entries.is_empty() => {
                    self.mode = Mode::Browse;
                    self.step_clamped(-3);
                }
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if self.mode == Mode::Library {
                    if let Some(index) = clicked_row(
                        mouse.column,
                        mouse.row,
                        self.library_area,
                        self.library_offset,
                        self.library.len(),
                    ) {
                        self.library_selected = index;
                    }
                } else if let Some(index) = clicked_row(
                    mouse.column,
                    mouse.row,
                    self.results_area,
                    self.results_offset,
                    self.entries.len(),
                ) {
                    self.mode = Mode::Browse;
                    self.selected = index;
                    self.toggle();
                }
            }
            _ => {}
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
        let directory = self.download_dir();
        self.library = scan_library(&directory);
        self.library_selected = self
            .library_selected
            .min(self.library.len().saturating_sub(1));
        self.status = match self.library.len() {
            0 => format!("no ebooks in {}", directory.display()),
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

    fn step_library_clamped(&mut self, delta: isize) {
        self.library_selected = clamped_index(self.library_selected, delta, self.library.len());
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

    fn step_clamped(&mut self, delta: isize) {
        self.selected = clamped_index(self.selected, delta, self.entries.len());
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

    fn save_preferences(&self) -> Result<()> {
        let Some(path) = &self.preferences_path else {
            return Ok(());
        };
        let mut disabled_sources = self
            .sources
            .iter()
            .filter(|entry| !entry.enabled)
            .map(|entry| entry.source.name().to_string())
            .collect::<Vec<_>>();
        disabled_sources.sort();
        save_preferences(
            path,
            &Preferences {
                disabled_sources,
                download_dir: self.download_dir_input.value().trim().to_string(),
                google_drive_remote: self.drive_remote_input.value().trim().to_string(),
            },
        )
    }

    fn download_dir(&self) -> PathBuf {
        expand_tilde(self.download_dir_input.value().trim())
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
                    .search(&client, &query, page, RESULTS_PER_PAGE)
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
        let dest_dir = self.download_dir();
        let drive_remote = self.drive_remote_input.value().trim().to_string();
        for (idx, book, download, src) in jobs {
            self.entries[idx].status = Status::Queued;
            self.entries[idx].started = Some(Instant::now());
            let client = self.client.clone();
            let tx = self.tx.clone();
            let dest_dir = dest_dir.clone();
            let drive_remote = drive_remote.clone();
            self.rt.spawn(async move {
                let progress_tx = tx.clone();
                let result = src
                    .fetch(&client, &book, &download, &dest_dir, &move |seen, total| {
                        let _ = progress_tx.send(Msg::Progress {
                            generation,
                            idx,
                            seen,
                            total,
                        });
                    })
                    .await;
                let _ = match result {
                    Ok(path) => {
                        let drive = if drive_remote.is_empty() {
                            None
                        } else {
                            let _ = tx.send(Msg::Uploading { generation, idx });
                            Some(
                                upload_to_google_drive(path.clone(), drive_remote)
                                    .await
                                    .map_err(|error| error.to_string()),
                            )
                        };
                        tx.send(Msg::Done {
                            generation,
                            idx,
                            path,
                            drive,
                        })
                    }
                    Err(e) => tx.send(Msg::Failed {
                        generation,
                        idx,
                        error: e.to_string(),
                    }),
                };
            });
        }
        self.status = format!("downloading {started} file(s) into {}", dest_dir.display());
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
                                    started: None,
                                }));
                        }
                        Err(error) => self.status = format!("{source}: {error}"),
                    }
                    if self.pending == 0 {
                        self.entries.truncate(RESULTS_PER_PAGE);
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
                            n => {
                                let more = if n == RESULTS_PER_PAGE {
                                    " — n for next page"
                                } else {
                                    ""
                                };
                                format!(
                                    "{n}/{RESULTS_PER_PAGE} results — page {}{}{} — Space marks, d downloads",
                                    self.page,
                                    self.total_pages
                                        .map(|total| format!("/{total}"))
                                        .unwrap_or_default(),
                                    more,
                                )
                            }
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
                Msg::Uploading { generation, idx } => {
                    if generation == self.generation
                        && let Some(entry) = self.entries.get_mut(idx)
                    {
                        entry.status = Status::Uploading;
                    }
                }
                Msg::Done {
                    generation,
                    idx,
                    path,
                    drive,
                } => {
                    if generation == self.generation
                        && let Some(entry) = self.entries.get_mut(idx)
                    {
                        entry.status = Status::Done(path);
                        entry.marked = false;
                        if let Some(result) = drive {
                            self.status = match result {
                                Ok(target) => format!("uploaded to Google Drive: {target}"),
                                Err(error) => format!("saved locally; Google Drive: {error}"),
                            };
                        }
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
        if self.mode == Mode::Settings {
            self.draw_settings(frame);
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

        self.draw_tabs(frame, tabs_area, Mode::Search);
        self.draw_search_box(frame, search_area);
        self.draw_sources(frame, sources_area);
        self.draw_status(frame, status_area);
        self.draw_results(frame, list_area);
    }

    /// The query box, with the real terminal cursor parked inside it.
    fn draw_search_box(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.mode == Mode::Search;
        let accent = if focused { theme::ACCENT } else { theme::FAINT };
        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::new().fg(accent))
            .title(Span::styled(" Search ", Style::new().fg(accent).bold()));
        let inner = block.inner(area);
        let width = inner.width.max(1) as usize;
        let scroll = self.input.visual_scroll(width.saturating_sub(1));

        // An empty, unfocused box says what to do with it rather than sitting blank.
        let body = match self.input.value().is_empty() && !focused {
            true => Paragraph::new(Span::styled(
                "  press / to search",
                Style::new().fg(theme::FAINT),
            )),
            false => Paragraph::new(self.input.value()).scroll((0, scroll as u16)),
        };
        frame.render_widget(body.block(block), area);

        if focused {
            let x = self.input.visual_cursor().saturating_sub(scroll) as u16;
            frame.set_cursor_position((inner.x + x, inner.y));
        }
    }

    /// One chip per catalog, lit when it will be queried.
    fn draw_sources(&self, frame: &mut Frame, area: Rect) {
        let picking = self.mode == Mode::Sources;
        let mut chips: Vec<Span> = vec![Span::styled("Sources", Style::new().fg(theme::MUTED))];
        for (i, entry) in self.sources.iter().enumerate() {
            let text = format!(
                " {} {} ",
                if entry.enabled { "●" } else { "○" },
                entry.source.name()
            );
            chips.push(match (picking && i == self.source_cursor, entry.enabled) {
                (true, _) => Span::styled(
                    text,
                    Style::new()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::ACCENT_BRIGHT)
                        .bold(),
                ),
                (false, true) => Span::styled(text, Style::new().fg(theme::OK)),
                (false, false) => Span::styled(text, Style::new().fg(theme::FAINT)),
            });
        }
        chips.push(theme::hint(if picking {
            "  space toggles, esc done"
        } else if self.mode == Mode::Search {
            "  (ctrl+s to change)"
        } else {
            "  (s to change)"
        }));
        frame.render_widget(Paragraph::new(Line::from(chips)), area);
    }

    /// Spinner plus whatever the app is waiting on, then the free-text status.
    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        if let Some(phase) = self.phase() {
            spans.push(self.spin(Style::new().fg(theme::ACCENT).bold()));
            spans.push(Span::styled(
                format!(" {phase}"),
                Style::new().fg(theme::ACCENT).bold(),
            ));
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            self.status.clone(),
            Style::new().fg(theme::MUTED),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_results(&mut self, frame: &mut Frame, area: Rect) {
        self.results_area = area;
        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::new().fg(theme::FAINT))
            .title(Span::styled(
                " Results ",
                Style::new().fg(theme::ACCENT).bold(),
            ))
            .title_bottom(self.results_legend().centered());

        if self.entries.is_empty() {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let lines = match self.pending > 0 {
                true => vec![Line::from(Span::styled(
                    "Searching the catalogs…",
                    Style::new().fg(theme::MUTED),
                ))],
                false => vec![
                    Line::from(Span::styled(
                        "Nothing here yet",
                        Style::new().fg(theme::MUTED).bold(),
                    )),
                    Line::from(""),
                    Line::from(theme::hint("Results from the ticked sources land here")),
                ],
            };
            self.draw_placeholder(frame, inner, lines);
            return;
        }

        // Two cells go to the cursor gutter, two to the borders.
        let content_width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|entry| ListItem::new(self.row(entry, content_width)))
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol(theme::CURSOR)
            .highlight_style(Style::new().bg(theme::SELECTION_BG).bold());
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
        self.results_offset = state.offset();

        self.draw_scrollbar(frame, area, self.entries.len(), self.selected);
    }

    /// A slim scrollbar down the right edge, only once the list overflows.
    fn draw_scrollbar(&self, frame: &mut Frame, area: Rect, len: usize, position: usize) {
        let visible = area.height.saturating_sub(2) as usize;
        if len <= visible {
            return;
        }
        let mut state = ScrollbarState::new(len).position(position);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(Style::new().fg(theme::FAINT))
                .thumb_style(Style::new().fg(theme::ACCENT)),
            area.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            &mut state,
        );
    }

    /// Centre a short message in an otherwise empty list body.
    fn draw_placeholder(&self, frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
        if area.height == 0 {
            return;
        }
        let top = area.height.saturating_sub(lines.len() as u16) / 2;
        let area = Rect {
            y: area.y + top,
            height: area.height - top,
            ..area
        };
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn results_legend(&self) -> Line<'static> {
        let page = format!(
            " {}{} ",
            self.page,
            self.total_pages
                .map(|total| format!("/{total}"))
                .unwrap_or_default()
        );
        Line::from(vec![
            theme::hint(" search "),
            theme::key("/"),
            theme::hint("  mark "),
            theme::key("space"),
            theme::hint("  get "),
            theme::key("d"),
            theme::hint("  open "),
            theme::key("o"),
            theme::hint("  quit "),
            theme::key("q"),
            theme::hint("   page"),
            Span::styled(page, Style::new().fg(theme::ACCENT_BRIGHT).bold()),
            theme::key("p"),
            theme::hint("/"),
            theme::key("n"),
            theme::hint(" "),
        ])
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
                true => "Searching".to_string(),
                false => "Waiting for the rest".to_string(),
            });
        }
        let active = self
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.status,
                    Status::Queued | Status::Downloading { .. } | Status::Uploading
                )
            })
            .count();
        (active > 0).then(|| match active {
            1 => "Downloading".to_string(),
            n => format!("Downloading {n} books"),
        })
    }

    /// Widest catalog name, so every title starts at the same column.
    fn tag_width(&self) -> usize {
        self.sources
            .iter()
            .map(|entry| entry.source.name().len())
            .max()
            .unwrap_or(0)
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect, active: Mode) {
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(16)]).areas(area);

        let tab = |label: &str, active: bool| match active {
            true => Span::styled(
                format!(" {label} "),
                Style::new().fg(theme::ON_ACCENT).bg(theme::ACCENT).bold(),
            ),
            false => Span::styled(format!(" {label} "), Style::new().fg(theme::MUTED)),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                tab(
                    "Search",
                    matches!(active, Mode::Search | Mode::Browse | Mode::Sources),
                ),
                Span::raw(" "),
                tab("Downloads", active == Mode::Library),
                Span::raw(" "),
                tab("Settings", active == Mode::Settings),
                theme::hint("   tab switches"),
            ])),
            left_area,
        );

        let count = match active {
            Mode::Library => format!("{} on disk ", self.library.len()),
            Mode::Settings => "configuration ".to_string(),
            _ => format!("{} found ", self.entries.len()),
        };
        frame.render_widget(
            Paragraph::new(theme::hint(&count)).alignment(Alignment::Right),
            right_area,
        );
    }

    fn draw_library(&mut self, frame: &mut Frame) {
        let [tabs_area, status_area, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(frame.area());
        self.library_area = list_area;
        self.draw_tabs(frame, tabs_area, Mode::Library);
        frame.render_widget(
            Paragraph::new(Span::styled(
                self.status.clone(),
                Style::new().fg(theme::MUTED),
            )),
            status_area,
        );

        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::new().fg(theme::FAINT))
            .title(Span::styled(
                " Downloaded books ",
                Style::new().fg(theme::ACCENT).bold(),
            ))
            .title_bottom(
                Line::from(vec![
                    theme::hint(" open "),
                    theme::key("enter"),
                    theme::hint("  refresh "),
                    theme::key("r"),
                    theme::hint("  search "),
                    theme::key("tab"),
                    theme::hint(" "),
                ])
                .centered(),
            );

        if self.library.is_empty() {
            let inner = block.inner(list_area);
            frame.render_widget(block, list_area);
            self.draw_placeholder(
                frame,
                inner,
                vec![
                    Line::from(Span::styled(
                        "No books downloaded yet",
                        Style::new().fg(theme::MUTED).bold(),
                    )),
                    Line::from(""),
                    Line::from(theme::hint("Find one on the Search tab and press d")),
                ],
            );
            return;
        }

        let content_width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .library
            .iter()
            .map(|entry| ListItem::new(Self::library_row(entry, content_width)))
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol(theme::CURSOR)
            .highlight_style(Style::new().bg(theme::SELECTION_BG).bold());
        let mut state = ListState::default().with_selected(Some(self.library_selected));
        frame.render_stateful_widget(list, list_area, &mut state);
        self.library_offset = state.offset();

        self.draw_scrollbar(frame, list_area, self.library.len(), self.library_selected);
    }

    fn draw_settings(&mut self, frame: &mut Frame) {
        let [tabs_area, status_area, download_area, drive_area, help_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .areas(frame.area());
        self.draw_tabs(frame, tabs_area, Mode::Settings);
        self.draw_status(frame, status_area);
        Self::draw_setting_input(
            frame,
            download_area,
            " Download folder ",
            &self.download_dir_input,
            self.settings_cursor == 0,
        );
        Self::draw_setting_input(
            frame,
            drive_area,
            " Google Drive remote (optional) ",
            &self.drive_remote_input,
            self.settings_cursor == 1,
        );

        let rclone = if self.rclone_available {
            Span::styled("rclone is installed", Style::new().fg(theme::OK).bold())
        } else {
            Span::styled(
                "rclone is not installed",
                Style::new().fg(theme::ERR).bold(),
            )
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    theme::hint("Drive target example: "),
                    Span::styled("gdrive:EBookArchive", Style::new().fg(theme::BLUE)),
                ]),
                Line::from(""),
                Line::from(vec![
                    rclone,
                    theme::hint(". Configure it with: rclone config"),
                ]),
                Line::from(""),
                Line::from(theme::hint(
                    "up/down chooses a field  •  Enter saves  •  Tab opens Search",
                )),
            ])
            .block(
                Block::bordered()
                    .border_set(border::ROUNDED)
                    .border_style(Style::new().fg(theme::FAINT))
                    .title(Span::styled(
                        " Storage ",
                        Style::new().fg(theme::ACCENT).bold(),
                    )),
            )
            .wrap(Wrap { trim: true }),
            help_area,
        );
    }

    fn draw_setting_input(
        frame: &mut Frame,
        area: Rect,
        title: &'static str,
        input: &Input,
        focused: bool,
    ) {
        let color = if focused { theme::ACCENT } else { theme::FAINT };
        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::new().fg(color))
            .title(Span::styled(title, Style::new().fg(color).bold()));
        let inner = block.inner(area);
        let width = inner.width.max(1) as usize;
        let scroll = input.visual_scroll(width.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(input.value())
                .scroll((0, scroll as u16))
                .block(block),
            area,
        );
        if focused {
            let x = input.visual_cursor().saturating_sub(scroll) as u16;
            frame.set_cursor_position((inner.x + x, inner.y));
        }
    }

    /// Filename on the left, format and size lined up on the right.
    fn library_row(entry: &LibraryEntry, width: usize) -> Line<'static> {
        let name = entry
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let extension = entry
            .path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("?")
            .to_ascii_uppercase();

        let right = vec![
            Span::styled(format!("[{extension}]"), Style::new().fg(theme::BLUE)),
            Span::styled(
                format!("  {:>9}", human_size(entry.size)),
                Style::new().fg(theme::MUTED),
            ),
        ];
        Self::justify(vec![Span::raw(name)], right, width)
    }

    /// A result row: mark, catalog, title and author left; live status right.
    fn row(&self, entry: &Entry, width: usize) -> Line<'static> {
        let mut left: Vec<Span> = vec![
            match entry.marked {
                true => Span::styled("✓ ", Style::new().fg(theme::OK).bold()),
                false => Span::styled("· ", Style::new().fg(theme::FAINT)),
            },
            Span::styled(
                format!("{:<width$}  ", entry.book.source, width = self.tag_width()),
                Style::new().fg(theme::VIOLET),
            ),
            Span::raw(entry.book.title.clone()),
            Span::styled(
                format!("  {}", entry.book.author_line()),
                Style::new().fg(theme::MUTED),
            ),
        ];
        if let Some(language) = &entry.book.language {
            left.push(Span::styled(
                format!("  {language}"),
                Style::new().fg(theme::BLUE),
            ));
        }
        Self::justify(left, self.status_cell(entry), width)
    }

    /// Push `right` against the right edge, clipping `left` if the two collide.
    fn justify(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
        let right_width: usize = right.iter().map(|span| span.width()).sum();
        let mut spans = theme::clip(left, width.saturating_sub(right_width + 2));
        let used: usize = spans.iter().map(|span| span.width()).sum();
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(used + right_width)),
        ));
        spans.extend(right);
        Line::from(spans)
    }

    /// The right-hand column of a result row: available formats, or live progress.
    fn status_cell(&self, entry: &Entry) -> Vec<Span<'static>> {
        /// Cells given to the progress bar itself.
        const BAR: usize = 14;

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
                            Some(size) => format!("{} {}", download.format, human_size(size)),
                            None => download.format.to_string(),
                        })
                    })
                    .collect::<Vec<_>>();
                vec![Span::styled(
                    formats.join("  "),
                    Style::new().fg(theme::BLUE),
                )]
            }
            Status::Queued => vec![
                self.spin(Style::new().fg(theme::WARN)),
                Span::styled(" queued", Style::new().fg(theme::WARN)),
            ],
            Status::Downloading { seen, total } => {
                let elapsed = entry.started.map(|at| at.elapsed()).unwrap_or_default();
                let mut spans = Vec::new();
                match total {
                    // Known length: a real bar, a percentage, a rate and an ETA.
                    Some(total) if *total > 0 => {
                        let fraction = *seen as f64 / *total as f64;
                        spans.extend(theme::progress_bar(fraction, BAR, theme::WARN));
                        spans.push(Span::styled(
                            format!(" {:>3.0}%", fraction * 100.0),
                            Style::new().fg(theme::WARN).bold(),
                        ));
                        spans.push(Span::styled(
                            format!("  {:>9}", theme::rate(*seen, elapsed)),
                            Style::new().fg(theme::MUTED),
                        ));
                        spans.push(Span::styled(
                            format!("  {:>5}", theme::eta(*seen, *total, elapsed)),
                            Style::new().fg(theme::MUTED),
                        ));
                    }
                    // No Content-Length: sweep, rather than pretend to know.
                    _ => {
                        spans.extend(theme::pulse(self.frame, BAR, theme::WARN));
                        spans.push(Span::styled(
                            format!(" {:>9}", human_size(*seen)),
                            Style::new().fg(theme::WARN).bold(),
                        ));
                        spans.push(Span::styled(
                            format!("  {:>9}", theme::rate(*seen, elapsed)),
                            Style::new().fg(theme::MUTED),
                        ));
                    }
                }
                spans
            }
            Status::Uploading => vec![
                self.spin(Style::new().fg(theme::BLUE)),
                Span::styled(" uploading to Drive", Style::new().fg(theme::BLUE)),
            ],
            Status::Done(path) => vec![Span::styled(
                format!(
                    "✓ {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                Style::new().fg(theme::OK),
            )],
            Status::Failed(error) => vec![Span::styled(
                format!("✗ {error}"),
                Style::new().fg(theme::ERR),
            )],
        }
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

fn clicked_row(column: u16, row: u16, area: Rect, offset: usize, len: usize) -> Option<usize> {
    let inside = column > area.x
        && column < area.x.saturating_add(area.width).saturating_sub(1)
        && row > area.y
        && row < area.y.saturating_add(area.height).saturating_sub(1);
    if !inside {
        return None;
    }
    let index = offset + usize::from(row - area.y - 1);
    (index < len).then_some(index)
}

fn clamped_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(len - 1)
}

fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

fn command_available(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
    })
}

async fn upload_to_google_drive(path: PathBuf, remote: String) -> Result<String> {
    let target = google_drive_target(&path, &remote)?;
    let command_target = target.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("rclone")
            .arg("copyto")
            .arg("--")
            .arg(path)
            .arg(command_target)
            .output()
    })
    .await
    .map_err(|error| anyhow::anyhow!("could not run rclone: {error}"))?
    .map_err(|error| anyhow::anyhow!("rclone is not installed: {error}"))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        anyhow::bail!("rclone failed: {error}");
    }
    Ok(target)
}

fn google_drive_target(path: &Path, remote: &str) -> Result<String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("downloaded filename is not valid UTF-8"))?;
    let remote = remote.trim().trim_end_matches('/');
    if remote.is_empty() {
        anyhow::bail!("Google Drive remote is empty");
    }
    Ok(format!("{remote}/{filename}"))
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

fn preferences_path() -> Option<PathBuf> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config).join("tui-book-search/preferences.json"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/tui-book-search/preferences.json"))
}

fn load_preferences(path: &Path) -> Result<Preferences> {
    let contents = fs::read(path)?;
    Ok(serde_json::from_slice(&contents)?)
}

fn save_preferences(path: &Path, preferences: &Preferences) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(preferences)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let mut app = App::new(runtime.handle().clone())?;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = ratatui::run(|terminal| app.run(terminal));
    let mouse_result = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    runtime.block_on(app.close_sources());
    result?;
    mouse_result?;
    Ok(())
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
                started: Some(Instant::now() - Duration::from_secs(3)),
            },
            Entry {
                book: book("Through the Looking-Glass", "Carroll, Lewis"),
                marked: true,
                status: Status::Queued,
                started: None,
            },
            Entry {
                book: book("Alice's Adventures Under Ground", "Carroll, Lewis"),
                marked: false,
                status: Status::Done("downloads/alice.epub".into()),
                started: None,
            },
            Entry {
                book: book("The Nursery Alice", "Carroll, Lewis"),
                marked: false,
                status: Status::Downloading {
                    seen: 41_233,
                    total: None,
                },
                started: Some(Instant::now() - Duration::from_secs(2)),
            },
            Entry {
                book: book("Sylvie and Bruno", "Carroll, Lewis"),
                marked: false,
                status: Status::Failed("404 Not Found".into()),
                started: None,
            },
        ];
        app.selected = 1;

        let mut term = Terminal::new(TestBackend::new(110, 15)).unwrap();
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
        println!("{}", term.backend());
        assert!(rendered.contains("Downloads"));
        assert!(rendered.contains("Marsovac.epub"));
        assert!(rendered.contains("EPUB"));
        assert!(rendered.contains("374.9 kB"));
    }

    #[test]
    fn successful_download_clears_the_mark() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();
        app.entries.push(Entry {
            book: book("Marsovac", "Andy Weir"),
            marked: true,
            status: Status::Queued,
            started: Some(Instant::now()),
        });
        app.tx
            .send(Msg::Done {
                generation: app.generation,
                idx: 0,
                path: PathBuf::from("downloads/Marsovac.epub"),
                drive: None,
            })
            .unwrap();

        app.drain();

        assert!(!app.entries[0].marked);
        assert!(matches!(app.entries[0].status, Status::Done(_)));
    }

    #[test]
    fn mouse_click_selects_and_marks_a_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();
        app.mode = Mode::Browse;
        app.entries = vec![
            Entry {
                book: book("First", "Author"),
                marked: false,
                status: Status::Idle,
                started: None,
            },
            Entry {
                book: book("Second", "Author"),
                marked: false,
                status: Status::Idle,
                started: None,
            },
        ];
        app.results_area = Rect::new(0, 4, 80, 8);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 6,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.selected, 1);
        assert!(app.entries[1].marked);
        assert!(!app.entries[0].marked);
    }

    #[test]
    fn mouse_scroll_stops_at_list_edges() {
        assert_eq!(clamped_index(0, -3, 10), 0);
        assert_eq!(clamped_index(8, 3, 10), 9);
        assert_eq!(clamped_index(9, 3, 10), 9);
        assert_eq!(clamped_index(0, 3, 0), 0);
    }

    #[test]
    fn saves_and_loads_disabled_sources() {
        let path = std::env::temp_dir().join(format!(
            "tui-book-search-preferences-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let expected = Preferences {
            disabled_sources: vec!["annas-archive".into(), "libgen".into()],
            download_dir: "/tmp/ebooks".into(),
            google_drive_remote: "gdrive:EBookArchive".into(),
        };

        save_preferences(&path, &expected).unwrap();
        let loaded = load_preferences(&path).unwrap();

        assert_eq!(loaded.disabled_sources, expected.disabled_sources);
        assert_eq!(loaded.download_dir, expected.download_dir);
        assert_eq!(loaded.google_drive_remote, expected.google_drive_remote);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn old_preferences_keep_default_storage_settings() {
        let loaded: Preferences =
            serde_json::from_str(r#"{"disabled_sources":["libgen"]}"#).unwrap();

        assert_eq!(loaded.download_dir, DEFAULT_DOWNLOAD_DIR);
        assert!(loaded.google_drive_remote.is_empty());
    }

    #[test]
    fn settings_page_saves_download_folder_and_drive_remote() {
        let root = std::env::temp_dir().join(format!(
            "tui-book-settings-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let preferences = root.join("preferences.json");
        let downloads = root.join("my-books");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();
        app.preferences_path = Some(preferences.clone());
        app.download_dir_input = Input::from(downloads.to_string_lossy().into_owned());
        app.drive_remote_input = Input::from("gdrive:EBookArchive");

        app.commit_settings();

        let saved = load_preferences(&preferences).unwrap();
        assert_eq!(saved.download_dir, downloads.to_string_lossy());
        assert_eq!(saved.google_drive_remote, "gdrive:EBookArchive");
        assert!(downloads.is_dir());

        app.mode = Mode::Settings;
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("Settings"));
        assert!(rendered.contains("Download folder"));
        assert!(rendered.contains("Google Drive remote"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builds_google_drive_destination_without_changing_filename() {
        assert_eq!(
            google_drive_target(Path::new("/tmp/Marsovac.epub"), "gdrive:EBookArchive/").unwrap(),
            "gdrive:EBookArchive/Marsovac.epub"
        );
    }

    /// Both lists have to say something useful when they have nothing to show.
    #[test]
    fn renders_empty_states() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = App::new(rt.handle().clone()).unwrap();

        let mut term = Terminal::new(TestBackend::new(70, 12)).unwrap();
        term.draw(|frame| app.draw(frame)).unwrap();
        let rendered = format!("{}", term.backend());
        println!("{}", term.backend());
        assert!(rendered.contains("Nothing here yet"));

        // Once focus leaves the box, it says how to get back to it.
        app.mode = Mode::Browse;
        term.draw(|frame| app.draw(frame)).unwrap();
        let rendered = format!("{}", term.backend());
        println!("{}", term.backend());
        assert!(rendered.contains("press / to search"));

        app.mode = Mode::Library;
        term.draw(|frame| app.draw(frame)).unwrap();
        let rendered = format!("{}", term.backend());
        println!("{}", term.backend());
        assert!(rendered.contains("No books downloaded yet"));
    }

    /// A bar that cannot fill past its width, and reads 0% and 100% exactly.
    #[test]
    fn progress_bar_stays_within_its_width() {
        for (fraction, expected_full) in [(0.0, 0), (0.5, 5), (1.0, 10)] {
            let spans = theme::progress_bar(fraction, 10, theme::WARN);
            let width: usize = spans.iter().map(|span| span.width()).sum();
            assert_eq!(width, 10, "fraction {fraction} drew {width} cells");
            assert_eq!(spans[0].content.chars().count(), expected_full);
        }
        // Out-of-range input is clamped rather than overflowing the row.
        let spans = theme::progress_bar(4.2, 10, theme::WARN);
        assert_eq!(spans.iter().map(|s| s.width()).sum::<usize>(), 10);
    }

    /// The sweep never runs off either end of its track.
    #[test]
    fn pulse_stays_within_its_width() {
        for frame in 0..64 {
            let spans = theme::pulse(frame, 12, theme::WARN);
            let width: usize = spans.iter().map(|span| span.width()).sum();
            assert_eq!(width, 12, "frame {frame} drew {width} cells");
        }
    }

    /// The right-hand column keeps its place even when the title is too long.
    #[test]
    fn justify_clips_the_left_side_to_fit() {
        let long = "a".repeat(200);
        let line = App::justify(vec![Span::raw(long)], vec![Span::raw("100%")], 40);
        assert_eq!(line.width(), 40);
        assert!(line.to_string().ends_with("100%"));
        assert!(line.to_string().contains('…'));
    }
}

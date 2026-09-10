//! A gallery of every spinner in `throbber-widgets-tui`.
//!
//!   cargo run --example spinners            # interactive
//!   cargo run --example spinners -- --dump  # print a few frames and exit
//!
//! Keys: [space] pause   [+]/[-] speed   [q] quit

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols::border,
    text::Line,
    widgets::{Block, Paragraph, StatefulWidget},
    DefaultTerminal, Frame,
};
use throbber_widgets_tui::{symbols::throbber::Set, Throbber, ThrobberState};

/// Every set the crate ships, with a display name.
const SETS: &[(&str, Set)] = &[
    ("ASCII", throbber_widgets_tui::ASCII),
    ("BOX_DRAWING", throbber_widgets_tui::BOX_DRAWING),
    ("ARROW", throbber_widgets_tui::ARROW),
    ("DOUBLE_ARROW", throbber_widgets_tui::DOUBLE_ARROW),
    ("VERTICAL_BLOCK", throbber_widgets_tui::VERTICAL_BLOCK),
    ("HORIZONTAL_BLOCK", throbber_widgets_tui::HORIZONTAL_BLOCK),
    ("QUADRANT_BLOCK", throbber_widgets_tui::QUADRANT_BLOCK),
    ("QUADRANT_CRACK", throbber_widgets_tui::QUADRANT_BLOCK_CRACK),
    ("WHITE_SQUARE", throbber_widgets_tui::WHITE_SQUARE),
    ("WHITE_CIRCLE", throbber_widgets_tui::WHITE_CIRCLE),
    ("BLACK_CIRCLE", throbber_widgets_tui::BLACK_CIRCLE),
    ("CLOCK", throbber_widgets_tui::CLOCK),
    ("BRAILLE_ONE", throbber_widgets_tui::BRAILLE_ONE),
    ("BRAILLE_DOUBLE", throbber_widgets_tui::BRAILLE_DOUBLE),
    ("BRAILLE_SIX", throbber_widgets_tui::BRAILLE_SIX),
    ("BRAILLE_SIX_DOUBLE", throbber_widgets_tui::BRAILLE_SIX_DOUBLE),
    ("BRAILLE_EIGHT", throbber_widgets_tui::BRAILLE_EIGHT),
    ("BRAILLE_EIGHT_DBL", throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE),
    ("OGHAM_A", throbber_widgets_tui::OGHAM_A),
    ("OGHAM_B", throbber_widgets_tui::OGHAM_B),
    ("OGHAM_C", throbber_widgets_tui::OGHAM_C),
    ("PARENTHESIS", throbber_widgets_tui::PARENTHESIS),
    ("CANADIAN", throbber_widgets_tui::CANADIAN),
];

const PALETTE: &[Color] = &[
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::LightBlue,
    Color::LightRed,
];

struct App {
    /// One state per spinner: `render` normalizes the index against the set's
    /// own length, so a single shared state would corrupt every other spinner.
    states: Vec<ThrobberState>,
    interval: Duration,
    paused: bool,
    exit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            states: SETS.iter().map(|_| ThrobberState::default()).collect(),
            interval: Duration::from_millis(80),
            paused: false,
            exit: false,
        }
    }
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut last = Instant::now();
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;

            // Wait for input, but never longer than one animation frame --
            // this is what lets the UI tick while nothing is being typed.
            let timeout = self.interval.saturating_sub(last.elapsed());
            if event::poll(timeout)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code);
            }

            if last.elapsed() >= self.interval {
                self.tick();
                last = Instant::now();
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.exit = true,
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.interval = self.interval.saturating_sub(Duration::from_millis(10))
                    .max(Duration::from_millis(10));
            }
            KeyCode::Char('-') => {
                self.interval = (self.interval + Duration::from_millis(10))
                    .min(Duration::from_millis(500));
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if self.paused {
            return;
        }
        for state in &mut self.states {
            state.calc_next();
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::bordered()
            .title(Line::from(" Spinner Gallery ".bold()).centered())
            .title_bottom(Line::from(vec![
                " Pause ".into(), "<Space>".blue().bold(),
                " Speed ".into(), "<+/->".blue().bold(),
                " Quit ".into(), "<Q> ".blue().bold(),
            ]).centered())
            .border_set(border::THICK), area);

        let inner = Block::bordered().inner(area);
        let [status, grid] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
        ]).areas(inner);

        let label = if self.paused { "paused".to_string() } else {
            format!("{}ms/frame", self.interval.as_millis())
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                format!(" {} sets  ", SETS.len()).dark_gray(),
                label.yellow(),
            ])),
            status,
        );

        self.render_grid(grid, frame.buffer_mut());
    }

    fn render_grid(&mut self, area: Rect, buf: &mut Buffer) {
        let cols = 3usize;
        let rows = SETS.len().div_ceil(cols);
        if area.height == 0 {
            return;
        }

        let col_areas = Layout::horizontal(
            (0..cols).map(|_| Constraint::Ratio(1, cols as u32)),
        ).split(area);

        for (c, col_area) in col_areas.iter().enumerate() {
            let row_areas = Layout::vertical(
                (0..rows).map(|_| Constraint::Length(1)),
            ).split(*col_area);

            for (r, row_area) in row_areas.iter().enumerate() {
                let Some(i) = c.checked_mul(rows).map(|base| base + r) else { continue };
                let Some((name, set)) = SETS.get(i) else { continue };

                let throbber = Throbber::default()
                    .label(*name)
                    .throbber_set(set.clone())
                    .style(Style::new().dark_gray())
                    .throbber_style(Style::new().fg(PALETTE[i % PALETTE.len()]).bold());

                // StatefulWidget, not Widget -- the plain Widget impl picks a
                // *random* symbol each frame instead of animating.
                StatefulWidget::render(throbber, *row_area, buf, &mut self.states[i]);
            }
        }
    }
}

fn main() -> io::Result<()> {
    if std::env::args().any(|a| a == "--dump") {
        return dump();
    }
    let mut app = App::default();
    ratatui::run(|terminal| app.run(terminal))
}

/// Render a few frames to an off-screen buffer and print them, so the gallery
/// can be inspected without a TTY.
fn dump() -> io::Result<()> {
    use ratatui::{backend::TestBackend, Terminal};

    let mut app = App::default();
    let mut terminal = Terminal::new(TestBackend::new(74, 12)).unwrap();
    for frame in 0..4 {
        terminal.draw(|f| app.draw(f)).unwrap();
        println!("--- frame {frame} ---");
        println!("{}", terminal.backend());
        for _ in 0..3 {
            app.tick();
        }
    }
    Ok(())
}

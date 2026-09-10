use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    symbols::border,
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget, Widget},
    DefaultTerminal, Frame,
};

#[derive(Debug)]
struct Item {
    label: String,
    checked: bool,
}

impl Item {
    fn new(label: &str) -> Self {
        Self { label: label.into(), checked: false }
    }
}

#[derive(Debug)]
pub struct App {
    items: Vec<Item>,
    selected: usize,
    counter: u8,
    exit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            items: vec![
                Item::new("Enable logging"),
                Item::new("Dark mode"),
                Item::new("Auto-save"),
            ],
            selected: 0,
            counter: 0,
            exit: false,
        }
    }
}

impl App {

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn next(&mut self) {
        if self.items.is_empty() { return; }
        self.selected = (self.selected + 1) % self.items.len();
    }

    fn previous(&mut self) {
        if self.items.is_empty() { return; }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    fn toggle(&mut self) {
        if let Some(item) = self.items.get_mut(self.selected) {
            item.checked = !item.checked;
        }
    }

    fn draw(&self, frame: &mut Frame) {
     frame.render_widget(self, frame.area());
    }

    // -- snip --

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Down | KeyCode::Tab => self.next(),
            KeyCode::Up => self.previous(),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle(),
            KeyCode::Char('q') => self.exit(),
            KeyCode::Left => self.decrement_counter(),
            KeyCode::Right => self.increment_counter(),
            _ => {}
        }
    }

  /// updates the application's state based on user input
    fn handle_events(&mut self) -> io::Result<()> {
        match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(key_event)
            }
            _ => {}
        };
        Ok(())
    }
}

impl App {

    // -- snip --

    fn exit(&mut self) {
        self.exit = true;
    }

    fn increment_counter(&mut self) {
        self.counter += 1;
    }

    fn decrement_counter(&mut self) {
        if self.counter > 0 {
            self.counter -= 1;
        }
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = Line::from(" Counter App Tutorial ".bold());
        let instructions = Line::from(vec![
            " Move ".into(),
            "<Up/Down>".blue().bold(),
            " Toggle ".into(),
            "<Space>".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);
        let block = Block::bordered()
            .title(title.centered())
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        let [banner, counter, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ]).areas(area);

        Paragraph::new(Line::from(" WOOWOWOOWO ".bold()))
            .render(banner, buf);

        let counter_line = Line::from(vec![
            "Value: ".into(),
            self.counter.to_string().yellow(),
            "  (<Left>/<Right>)".dark_gray(),
        ]);
        Paragraph::new(counter_line).render(counter, buf);

        let items: Vec<ListItem> = self.items.iter()
            .map(|item| {
                ListItem::new(Line::from(vec![
                    if item.checked { "[x] ".green() } else { "[ ] ".into() },
                    item.label.clone().into(),
                ]))
            }).collect();

        let list = List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_style(Style::new().bold());

        let mut state = ListState::default().with_selected(Some(self.selected));
        StatefulWidget::render(&list, body, buf, &mut state);
    }
}

fn main() -> io::Result<()>{
    ratatui::run(|terminal| App::default().run(terminal))
}

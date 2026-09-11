//! Interactive terminal presentation for concurrent task execution.
//!
//! The scheduler sends ordinary execution events to this module. The renderer
//! owns the terminal and keeps one bounded scrollback buffer per task, so task
//! output never has to compete for the process-global terminal cursor.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::events::{ExecutionEvent, TaskStatus, TaskStream};
use crate::runner::CancellationToken;

const MAX_LINES_PER_TASK: usize = 1_000;

/// Handle held by the output sink while the TUI renderer owns the terminal.
pub(crate) struct TuiController {
    sender: Sender<TuiMessage>,
    thread: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

enum TuiMessage {
    Event(ExecutionEvent),
    Finish,
}

impl TuiController {
    pub(crate) fn start(cancellation: CancellationToken) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("mono-tui".to_owned())
            .spawn(move || run_tui(receiver, cancellation, ready_sender))
            .map_err(io::Error::other)?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(io::Error::other(format!("TUI failed to start: {error}")))
            }
        }
    }

    pub(crate) fn send(&self, event: ExecutionEvent) -> io::Result<()> {
        self.sender
            .send(TuiMessage::Event(event))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        self.sender
            .send(TuiMessage::Finish)
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        self.join()
    }

    fn join(&self) -> io::Result<()> {
        let Some(thread) = self
            .thread
            .lock()
            .expect("TUI thread lock is not poisoned")
            .take()
        else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| io::Error::other("TUI renderer thread panicked"))?
    }
}

impl Drop for TuiController {
    fn drop(&mut self) {
        if self
            .thread
            .lock()
            .expect("TUI thread lock is not poisoned")
            .is_some()
        {
            let _ = self.sender.send(TuiMessage::Finish);
            let _ = self.join();
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = stdout.flush();
    }
}

fn run_tui(
    receiver: Receiver<TuiMessage>,
    cancellation: CancellationToken,
    ready: mpsc::SyncSender<io::Result<()>>,
) -> io::Result<()> {
    if let Err(error) = terminal::enable_raw_mode() {
        let _ = ready.send(Err(error));
        return Ok(());
    }

    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide) {
        let _ = terminal::disable_raw_mode();
        let _ = ready.send(Err(error));
        return Ok(());
    }
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    ready.send(Ok(())).map_err(io::Error::other)?;

    let mut app = TuiApp::default();
    loop {
        while let Ok(message) = receiver.try_recv() {
            if app.apply(message) {
                terminal.draw(|frame| app.draw(frame))?;
                return Ok(());
            }
        }

        if event::poll(Duration::from_millis(25))?
            && let Event::Key(key) = event::read()?
            && handle_key(&mut app, key, &cancellation)
        {
            terminal.draw(|frame| app.draw(frame))?;
            return Ok(());
        }

        terminal.draw(|frame| app.draw(frame))?;
    }
}

fn handle_key(app: &mut TuiApp, key: KeyEvent, cancellation: &CancellationToken) -> bool {
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        cancellation.cancel();
        return false;
    }

    match key.code {
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::Char('g') => app.scroll_to_start(),
        KeyCode::Char('G') => app.follow_tail = true,
        _ => {}
    }
    false
}

#[derive(Default)]
struct TuiApp {
    tasks: Vec<TaskView>,
    selected: usize,
    follow_tail: bool,
    scroll: u16,
    finished: bool,
}

impl TuiApp {
    fn apply(&mut self, message: TuiMessage) -> bool {
        match message {
            TuiMessage::Finish => {
                for task in &mut self.tasks {
                    task.flush_partial();
                }
                self.finished = true;
                true
            }
            TuiMessage::Event(event) => {
                self.apply_event(event);
                false
            }
        }
    }

    fn apply_event(&mut self, event: ExecutionEvent) {
        match event {
            ExecutionEvent::RunStarted { .. } => {}
            ExecutionEvent::TaskStarted { task, .. } => {
                self.task_mut(&task);
            }
            ExecutionEvent::TaskOutput {
                task,
                stream,
                bytes,
                ..
            } => self.task_mut(&task).append(stream, &bytes),
            ExecutionEvent::TaskAttemptStarted { task, .. } => {
                self.task_mut(&task);
            }
            ExecutionEvent::TaskFinished {
                task,
                status,
                elapsed_ms,
                ..
            } => {
                let view = self.task_mut(&task);
                view.flush_partial();
                view.status = Some(status);
                view.elapsed_ms = Some(elapsed_ms);
            }
            ExecutionEvent::RunFinished { .. } => {}
        }
    }

    fn task_mut(&mut self, task: &str) -> &mut TaskView {
        if let Some(index) = self.tasks.iter().position(|view| view.id == task) {
            return &mut self.tasks[index];
        }
        let index = self.tasks.len();
        self.tasks.push(TaskView::new(task));
        &mut self.tasks[index]
    }

    fn select_next(&mut self) {
        if !self.tasks.is_empty() {
            self.selected = (self.selected + 1).min(self.tasks.len() - 1);
            self.follow_tail = true;
        }
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.follow_tail = true;
    }

    fn scroll_down(&mut self) {
        if !self.follow_tail {
            self.scroll = self.scroll.saturating_add(5);
        }
    }

    fn scroll_up(&mut self) {
        if self.follow_tail {
            self.follow_tail = false;
            self.scroll = 0;
        } else {
            self.scroll = self.scroll.saturating_sub(5);
        }
    }

    fn scroll_to_start(&mut self) {
        self.follow_tail = false;
        self.scroll = 0;
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let outer = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(outer);
        let header = Paragraph::new(self.header()).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(header, layout[0]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(layout[1]);
        self.draw_task_list(frame, body[0]);
        self.draw_task_output(frame, body[1]);
    }

    fn header(&self) -> String {
        let running = self
            .tasks
            .iter()
            .filter(|task| task.status.is_none())
            .count();
        let completed = self
            .tasks
            .iter()
            .filter(|task| matches!(task.status, Some(TaskStatus::Completed)))
            .count();
        let failed = self
            .tasks
            .iter()
            .filter(|task| {
                task.status.is_some_and(|status| {
                    matches!(
                        status,
                        TaskStatus::Failed | TaskStatus::TimedOut | TaskStatus::OutputLimit
                    )
                })
            })
            .count();
        format!(
            " mono · {running} running · {completed} completed · {failed} failed{}",
            if self.finished { " · finished" } else { "" }
        )
    }

    fn draw_task_list(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let items = self
            .tasks
            .iter()
            .map(|task| ListItem::new(task.label()))
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" tasks "))
            .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
        let mut state = ListState::default();
        state.select((!self.tasks.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_task_output(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(task) = self.tasks.get(self.selected) else {
            frame.render_widget(
                Paragraph::new("Waiting for tasks...")
                    .block(Block::default().borders(Borders::ALL).title(" output ")),
                area,
            );
            return;
        };
        let title = format!(" {} ", task.id);
        let text = task
            .lines
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect::<Vec<_>>();
        let visible_lines = area.height.saturating_sub(2) as usize;
        let max_scroll = task.lines.len().saturating_sub(visible_lines) as u16;
        let scroll = if self.follow_tail {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }
}

struct TaskView {
    id: String,
    status: Option<TaskStatus>,
    elapsed_ms: Option<u128>,
    lines: VecDeque<String>,
    stdout_partial: String,
    stderr_partial: String,
}

impl TaskView {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            status: None,
            elapsed_ms: None,
            lines: VecDeque::new(),
            stdout_partial: String::new(),
            stderr_partial: String::new(),
        }
    }

    fn append(&mut self, stream: TaskStream, bytes: &[u8]) {
        let prefix = matches!(stream, TaskStream::Stderr).then_some("! ");
        let mut completed = Vec::new();
        {
            let partial = match stream {
                TaskStream::Stdout => &mut self.stdout_partial,
                TaskStream::Stderr => &mut self.stderr_partial,
            };
            partial.push_str(&String::from_utf8_lossy(bytes));
            while let Some(newline) = partial.find('\n') {
                let line = partial.drain(..=newline).collect::<String>();
                completed.push(format_line(prefix, line.trim_end_matches('\n')));
            }
        }
        for line in completed {
            self.push_line(line);
        }
    }

    fn flush_partial(&mut self) {
        let stdout = std::mem::take(&mut self.stdout_partial);
        let stderr = std::mem::take(&mut self.stderr_partial);
        if !stdout.is_empty() {
            self.push_line(stdout);
        }
        if !stderr.is_empty() {
            self.push_line(format_line(Some("! "), &stderr));
        }
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() == MAX_LINES_PER_TASK {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn label(&self) -> String {
        let icon = match self.status {
            None => "⠹",
            Some(TaskStatus::Completed | TaskStatus::Cached) => "✓",
            Some(TaskStatus::Failed | TaskStatus::TimedOut | TaskStatus::OutputLimit) => "✗",
            Some(TaskStatus::Cancelled) => "×",
            Some(TaskStatus::Blocked) => "·",
        };
        let status = self.status.map(TaskStatus::label).unwrap_or("running");
        let elapsed = self
            .elapsed_ms
            .map(|millis| format!(" {millis}ms"))
            .unwrap_or_default();
        format!("{icon} {:<24} {status}{elapsed}", self.id)
    }
}

fn format_line(prefix: Option<&str>, line: &str) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}{line}"),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_view_keeps_output_separate_by_stream_and_flushes_partial_lines() {
        let mut task = TaskView::new("build");
        task.append(TaskStream::Stdout, b"hello");
        task.append(TaskStream::Stderr, b"warning\n");
        task.flush_partial();

        assert_eq!(
            task.lines.into_iter().collect::<Vec<_>>(),
            vec!["! warning".to_owned(), "hello".to_owned()]
        );
    }

    #[test]
    fn task_view_evicts_old_lines_at_the_scrollback_limit() {
        let mut task = TaskView::new("build");
        for index in 0..=MAX_LINES_PER_TASK {
            task.append(TaskStream::Stdout, format!("{index}\n").as_bytes());
        }

        assert_eq!(task.lines.len(), MAX_LINES_PER_TASK);
        assert_eq!(task.lines.front().expect("oldest line"), "1");
    }
}

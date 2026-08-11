use std::{io, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    control::ControlClient,
    upstream::{LogEntry, ServerStatus, State},
};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enter alternate screen");
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Default)]
struct App {
    servers: Vec<ServerStatus>,
    logs: Vec<LogEntry>,
    selected: usize,
    filter: Option<String>,
    status: String,
    error: Option<String>,
    help: bool,
}

impl App {
    fn selected(&self) -> Option<&ServerStatus> {
        self.servers.get(self.selected)
    }
    fn move_by(&mut self, amount: isize) {
        self.selected = move_selection(self.selected, self.servers.len(), amount);
    }
    fn replace_servers(&mut self, servers: impl IntoIterator<Item = ServerStatus>) {
        let selected_id = self.selected().map(|server| server.id.clone());
        self.servers = servers.into_iter().collect();
        self.selected = selected_id
            .and_then(|id| self.servers.iter().position(|server| server.id == id))
            .unwrap_or_else(|| self.selected.min(self.servers.len().saturating_sub(1)));
        if self
            .filter
            .as_ref()
            .is_some_and(|id| !self.servers.iter().any(|s| &s.id == id))
        {
            self.filter = None;
        }
    }
    fn toggle_filter(&mut self) {
        if let Some(id) = self.selected().map(|server| server.id.clone()) {
            self.filter = if self.filter.as_ref() == Some(&id) {
                None
            } else {
                Some(id)
            };
        }
    }
}

fn move_selection(current: usize, len: usize, amount: isize) -> usize {
    if len == 0 {
        return 0;
    }
    current.saturating_add_signed(amount).min(len - 1)
}

pub async fn run(path: Option<PathBuf>) -> Result<()> {
    let client = ControlClient::load(path)?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to initialize terminal")?;
    terminal.clear().context("failed to clear terminal")?;
    let mut app = App {
        status: "connecting".into(),
        ..App::default()
    };
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    loop {
        terminal
            .draw(|frame| draw(frame, &app, client.endpoint()))
            .context("failed to draw dashboard")?;
        tokio::select! {
            _ = refresh.tick() => refresh_data(&client, &mut app).await,
            result = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
                if event::poll(Duration::from_millis(100))? { event::read().map(Some) } else { Ok(None) }
            }) => {
                match result.context("terminal event task failed")?.context("failed to read terminal event")? {
                    Some(Event::Key(key)) if handle_key(key, &client, &mut app).await => break,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn refresh_data(client: &ControlClient, app: &mut App) {
    match client.status().await {
        Ok(status) => {
            app.replace_servers(status.servers.into_values());
            app.status = "refreshed".into();
            app.error = None;
        }
        Err(error) => app.error = Some(error.to_string()),
    }
    let after = app.logs.last().map(|entry| entry.id);
    match client.logs(after, None).await {
        Ok(logs) => {
            app.logs.extend(logs.logs);
            if app.logs.len() > 500 {
                app.logs.drain(..app.logs.len() - 500);
            }
        }
        Err(error) => app.error = Some(error.to_string()),
    }
}

async fn handle_key(key: KeyEvent, client: &ControlClient, app: &mut App) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Down | KeyCode::Char('j') => app.move_by(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_by(-1),
        KeyCode::Char('f') => app.toggle_filter(),
        KeyCode::Char('?') => app.help = !app.help,
        KeyCode::Char('R') => action(app, "reloading", client.reload()).await,
        KeyCode::Char('r') => {
            if let Some(id) = app.selected().map(|s| s.id.clone()) {
                action(app, &format!("restarting {id}"), client.restart(&id)).await;
            }
        }
        KeyCode::Char('e') => {
            if let Some(server) = app.selected() {
                let id = server.id.clone();
                let disabled = matches!(server.state, State::Disabled);
                if disabled {
                    action(app, &format!("enabling {id}"), client.enable(&id)).await;
                } else {
                    action(app, &format!("disabling {id}"), client.disable(&id)).await;
                }
            }
        }
        _ => {}
    }
    refresh_data(client, app).await;
    false
}

async fn action(app: &mut App, label: &str, future: impl Future<Output = Result<()>>) {
    app.status = label.into();
    app.error = None;
    if let Err(error) = future.await {
        app.error = Some(error.to_string());
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App, endpoint: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(55),
            Constraint::Min(5),
        ])
        .split(frame.area());
    let filter = app.filter.as_deref().map_or("all", |id| id);
    let header = if let Some(error) = &app.error {
        format!("{endpoint} | ERROR: {error} | logs: {filter}")
    } else {
        format!("{endpoint} | {} | logs: {filter}", app.status)
    };
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::ALL).title(" mcplex ")),
        chunks[0],
    );
    let rows = app.servers.iter().map(|server| {
        Row::new(vec![
            server.id.clone(),
            format!("{:?}", server.state).to_lowercase(),
            server.restarts.to_string(),
            server.tools.to_string(),
            server.resources.to_string(),
            server.prompts.to_string(),
            server
                .last_call_ms
                .map_or("-".into(), |ms| format!("{ms} ms")),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(28),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Min(9),
        ],
    )
    .header(
        Row::new([
            "server",
            "state",
            "restarts",
            "tools",
            "resources",
            "prompts",
            "latency",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("▶ ")
    .block(Block::default().borders(Borders::ALL).title(" servers "));
    let mut state =
        TableState::default().with_selected((!app.servers.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(table, chunks[1], &mut state);
    let lines: Vec<Line<'_>> = app
        .logs
        .iter()
        .filter(|entry| {
            app.filter
                .as_ref()
                .is_none_or(|id| entry.server.as_ref() == Some(id))
        })
        .map(|entry| Line::raw(entry.message.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" live logs ")),
        chunks[2],
    );
    if app.help {
        let area = centered(66, 50, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new("q/Esc quit\n↑/↓ or j/k select\ne enable/disable\nr restart\nR reload config\nf filter logs by selected server\n? close help")
            .block(Block::default().borders(Borders::ALL).title(" keys ")), area);
    }
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server(id: &str) -> ServerStatus {
        ServerStatus {
            id: id.into(),
            state: State::Ready,
            restarts: 0,
            error: None,
            tools: 0,
            resources: 0,
            prompts: 0,
            last_call_ms: None,
        }
    }
    #[test]
    fn selection_is_bounded() {
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(2, 3, 1), 2);
        assert_eq!(move_selection(4, 0, -1), 0);
    }
    #[test]
    fn selection_survives_reordering() {
        let mut app = App {
            servers: vec![server("a"), server("b")],
            selected: 1,
            ..App::default()
        };
        app.replace_servers(vec![server("b"), server("a")]);
        assert_eq!(app.selected().unwrap().id, "b");
    }
    #[test]
    fn filter_toggles_and_clears_for_removed_server() {
        let mut app = App {
            servers: vec![server("a")],
            ..App::default()
        };
        app.toggle_filter();
        assert_eq!(app.filter.as_deref(), Some("a"));
        app.toggle_filter();
        assert!(app.filter.is_none());
        app.toggle_filter();
        app.replace_servers(Vec::new());
        assert!(app.filter.is_none());
    }
}
